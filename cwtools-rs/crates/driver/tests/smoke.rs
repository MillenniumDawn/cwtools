//! Smoke tests for the shared driver pipeline (Session + primitives).
//!
//! The driver is the anti-drift hub between the CLI and the LSP: both call its
//! Session/pipeline primitives so the load sequence can't diverge. These tests
//! pin the pipeline against the checked-in `performancetest2` fixture (a
//! Stellaris mod slice with its own `.cwtools/config` ruleset) plus a couple of
//! synthesized temp dirs for the discovery-config helper. They assert the
//! pipeline loads, indexes, and validates without panicking, and that its
//! output is deterministic across runs.

use std::collections::HashSet;
use std::path::PathBuf;

use cwtools_driver::{RulesInput, Session, SessionConfig, index_game_dir, search_config_for};
use cwtools_game::constants::Game;
use cwtools_index::variable_defining_effects;
use cwtools_rules::ruleset_loader::load_ruleset_from_dir;
use cwtools_string_table::string_table::StringTable;

fn testfiles() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("testfiles")
}

fn perf_mod() -> PathBuf {
    testfiles().join("performancetest2")
}

fn perf_rules() -> PathBuf {
    perf_mod().join(".cwtools").join("config")
}

fn total_instances(index: &cwtools_index::TypeIndex) -> usize {
    index.map.values().map(|v| v.len()).sum()
}

// ── search_config_for ────────────────────────────────────────────────────────

/// A directory whose own name is a known script folder is searched directly:
/// `include_dirs = ["."]`, root set to the directory itself.
#[test]
fn search_config_known_folder_searches_directly() {
    let dir = perf_mod().join("common");
    let config = search_config_for(&dir);
    assert_eq!(config.root, dir);
    assert_eq!(config.include_dirs, vec![".".to_string()]);
}

/// A mod root (no top-level script files, name not a known folder) is searched
/// as a workspace: the engine's default subfolder list, not `["."]`.
#[test]
fn search_config_mod_root_uses_default_subfolders() {
    let dir = perf_mod();
    let config = search_config_for(&dir);
    assert_eq!(config.root, dir);
    assert_ne!(config.include_dirs, vec![".".to_string()]);
    assert!(
        config.include_dirs.iter().any(|d| d == "common"),
        "mod-root branch should keep the default subfolder list, got {:?}",
        config.include_dirs
    );
}

/// A directory that itself holds loose script files is searched directly even
/// when its name is not a known folder.
#[test]
fn search_config_loose_script_files_search_directly() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("modroot");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("stuff.txt"), "foo = { x = 1 }\n").unwrap();

    let config = search_config_for(&root);
    assert_eq!(config.include_dirs, vec![".".to_string()]);
}

/// A directory with only subfolders (no top-level script files, non-known name)
/// falls to the workspace branch.
#[test]
fn search_config_subfolders_only_uses_default() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("modroot");
    std::fs::create_dir_all(root.join("sub")).unwrap();

    let config = search_config_for(&root);
    assert_ne!(config.include_dirs, vec![".".to_string()]);
}

// ── index_game_dir ───────────────────────────────────────────────────────────

/// Indexing a fixture dir parses + collects type instances, and the instance
/// count is stable across two runs (the merge order is deterministic).
#[test]
fn index_game_dir_is_populated_and_stable() {
    let table = StringTable::new();
    let (ruleset, _errors) = load_ruleset_from_dir(&perf_rules(), &table);
    let var_effects = variable_defining_effects(&ruleset);

    let first = index_game_dir(&perf_mod(), &ruleset, &table, &var_effects);
    let second = index_game_dir(&perf_mod(), &ruleset, &table, &var_effects);

    let n1 = total_instances(&first);
    assert!(n1 > 0, "expected the fixture to yield type instances");
    assert_eq!(
        n1,
        total_instances(&second),
        "instance count must be deterministic across runs"
    );
    // Events are the largest type in the fixture; the config defines `event`.
    assert!(
        first.map.contains_key("event"),
        "expected `event` instances, got types: {:?}",
        first.map.keys().collect::<Vec<_>>()
    );
}

/// Missing rules degrade gracefully: an empty ruleset yields an empty index,
/// not a panic.
#[test]
fn index_game_dir_empty_ruleset_yields_empty_index() {
    let table = StringTable::new();
    let ruleset = cwtools_rules::rules_types::RuleSet::new();
    let index = index_game_dir(&perf_mod(), &ruleset, &table, &HashSet::new());
    assert_eq!(total_instances(&index), 0);
}

// ── Session::load / validate_all ─────────────────────────────────────────────

fn load_perf_session() -> cwtools_driver::SessionWithFiles {
    Session::load(SessionConfig {
        game: Game::Stellaris,
        rules: RulesInput::Dir(perf_rules()),
        directory: perf_mod(),
        vanilla: None,
        vanilla_cache: None,
        ignore_files: &[],
        ignore_dirs: &[],
        loc_languages: None,
        on_rules_warning: None,
    })
}

/// The full load pipeline runs end to end on the fixture: discovery succeeds,
/// a non-empty type index is built, the scope registry is prebuilt, and files
/// are resident for the batch path.
#[test]
fn session_load_builds_indexes() {
    let session = load_perf_session();
    assert!(!session.discovery_failed, "discovery should not fail");
    assert!(!session.parsed_files().is_empty(), "mod files should parse");
    assert!(
        !session.type_index().map.is_empty(),
        "type index should be populated"
    );
    assert!(
        !session.ruleset().types.is_empty(),
        "ruleset should carry type definitions"
    );
    assert!(
        session.registry().is_some(),
        "a game is set, so the scope registry should be prebuilt"
    );
}

/// validate_all runs the whole batch without panicking and returns one entry
/// per parsed file. The total error count is deterministic across two loads.
#[test]
fn session_validate_all_is_deterministic() {
    let s1 = load_perf_session();
    let r1 = s1.validate_all();
    assert_eq!(
        r1.len(),
        s1.parsed_files().len(),
        "validate_all returns one result per parsed file"
    );
    let errors1: usize = r1.iter().map(|(_, e)| e.len()).sum();

    let s2 = load_perf_session();
    let errors2: usize = s2.validate_all().iter().map(|(_, e)| e.len()).sum();

    assert_eq!(
        errors1, errors2,
        "validation output must be deterministic across runs"
    );
}
