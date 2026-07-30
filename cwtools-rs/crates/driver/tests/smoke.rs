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

use cwtools_driver::{
    RulesInput, Session, SessionConfig, VanillaCacheAuto, index_game_dir, search_config_for,
};
use cwtools_game::constants::Game;
use cwtools_index::variable_defining_effects;
use cwtools_localization::Lang;
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
        vanilla_cache_auto: None,
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

// ── CW100 loc gate ───────────────────────────────────────────────────────────

const LOC_RULES: &str = r#"
types = {
    type[thing] = {
        path = "game/common/things"
        localisation = {
            ## required
            name = "$"
        }
    }
}
"#;

/// A temp workspace: `rules/` with a `## required` loc rule, and `mod/` with one
/// `thing` instance plus an optional `localisation/` file.
fn loc_gate_workspace(loc: Option<&str>) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("rules")).unwrap();
    std::fs::write(tmp.path().join("rules").join("things.cwt"), LOC_RULES).unwrap();
    let things = tmp.path().join("mod").join("common").join("things");
    std::fs::create_dir_all(&things).unwrap();
    std::fs::write(things.join("x.txt"), "my_thing = { }\n").unwrap();
    if let Some(text) = loc {
        let loc_dir = tmp.path().join("mod").join("localisation");
        std::fs::create_dir_all(&loc_dir).unwrap();
        std::fs::write(loc_dir.join("l_english.yml"), text).unwrap();
    }
    tmp
}

fn cw100_count(workspace: &std::path::Path) -> usize {
    let session = Session::load(SessionConfig {
        game: Game::Hoi4,
        rules: RulesInput::Dir(workspace.join("rules")),
        directory: workspace.join("mod"),
        vanilla: None,
        vanilla_cache: None,
        vanilla_cache_auto: None,
        ignore_files: &[],
        ignore_dirs: &[],
        loc_languages: None,
        on_rules_warning: None,
    });
    session
        .validate_all()
        .iter()
        .flat_map(|(_, errs)| errs.iter())
        .filter(|e| e.code == Some("CW100"))
        .count()
}

/// A mod with no `localisation/` at all must not report CW100 for every object:
/// an empty loc index means "loc not loaded", not "nothing is localised". Same
/// gate the LSP applies in `append_missing_loc_errors`.
#[test]
fn cw100_is_suppressed_when_the_loc_index_is_empty() {
    let tmp = loc_gate_workspace(None);
    assert_eq!(
        cw100_count(tmp.path()),
        0,
        "CW100 must not fire when no loc files were loaded"
    );
}

/// With loc data present the gate is open: an object whose required key is
/// missing from a non-empty loc index still reports CW100.
#[test]
fn cw100_still_fires_when_loc_data_exists() {
    let tmp = loc_gate_workspace(Some("l_english:\n unrelated_key:0 \"text\"\n"));
    assert_eq!(
        cw100_count(tmp.path()),
        1,
        "CW100 must still fire for a missing key once loc data is loaded"
    );
}

fn load_loc_session(
    workspace: &std::path::Path,
    parse_cache_dir: Option<PathBuf>,
) -> cwtools_driver::SessionWithFiles {
    Session::load_with_parse_cache(
        SessionConfig {
            game: Game::Hoi4,
            rules: RulesInput::Dir(workspace.join("rules")),
            directory: workspace.join("mod"),
            vanilla: None,
            vanilla_cache: None,
            vanilla_cache_auto: None,
            ignore_files: &[],
            ignore_dirs: &[],
            loc_languages: None,
            on_rules_warning: None,
        },
        parse_cache_dir,
    )
}

#[test]
fn parse_cache_preserves_cold_and_warm_validation_output() {
    let tmp = loc_gate_workspace(None);
    std::fs::write(
        tmp.path().join("mod/common/things/x.txt"),
        "my_thing = { broken =\n",
    )
    .unwrap();
    let uncached = load_loc_session(tmp.path(), None).validate_all();
    let cache_dir = tmp.path().join("cache");
    let cold = load_loc_session(tmp.path(), Some(cache_dir.clone())).validate_all();
    let cache_entries: usize = std::fs::read_dir(cache_dir.join("parse-cache"))
        .unwrap()
        .flatten()
        .map(|workspace| {
            std::fs::read_dir(workspace.path())
                .unwrap()
                .flatten()
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "cwb"))
                .count()
        })
        .sum();
    let warm = load_loc_session(tmp.path(), Some(cache_dir)).validate_all();

    assert!(cache_entries > 0);
    assert!(uncached.iter().any(|(_, errors)| !errors.is_empty()));
    assert_eq!(uncached, cold);
    assert_eq!(cold, warm);
}

#[test]
fn unusable_parse_cache_falls_back_to_uncached_validation() {
    let tmp = loc_gate_workspace(None);
    let uncached = load_loc_session(tmp.path(), None).validate_all();
    let blocker = tmp.path().join("not-a-cache-directory");
    std::fs::write(&blocker, b"x").unwrap();

    let fallback = load_loc_session(tmp.path(), Some(blocker)).validate_all();
    assert_eq!(uncached, fallback);
}

#[test]
fn changed_source_is_not_validated_against_a_stale_index() {
    let tmp = loc_gate_workspace(None);
    let session = Session::load(SessionConfig {
        game: Game::Hoi4,
        rules: RulesInput::Dir(tmp.path().join("rules")),
        directory: tmp.path().join("mod"),
        vanilla: None,
        vanilla_cache: None,
        vanilla_cache_auto: None,
        ignore_files: &[],
        ignore_dirs: &[],
        loc_languages: None,
        on_rules_warning: None,
    });
    std::fs::write(
        tmp.path().join("mod/common/things/x.txt"),
        "changed_thing = { different_value = yes }\n",
    )
    .unwrap();

    assert!(
        session
            .validate_all()
            .into_iter()
            .flat_map(|(_, errors)| errors)
            .any(|error| error.message.contains("changed after indexing"))
    );
}

// ── auto-managed vanilla cache ───────────────────────────────────────────────

const THING_RULES: &str = r#"
types = {
    type[thing] = {
        path = "game/common/things"
    }
}
"#;

/// Two-type variant of [`THING_RULES`]: same `thing` definition, different
/// ruleset shape, so it must not reuse the one-type cache.
const THING_RULES_V2: &str = r#"
types = {
    type[thing] = {
        path = "game/common/things"
    }
    type[other] = {
        path = "game/common/others"
    }
}
"#;

/// A temp workspace with `rules/`, a one-instance `mod/`, a one-instance
/// `vanilla/` install, and an empty `cache/` for the auto cache to write into.
fn cache_workspace() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("rules")).unwrap();
    std::fs::write(tmp.path().join("rules").join("things.cwt"), THING_RULES).unwrap();
    for (root, instance) in [("mod", "my_thing"), ("vanilla", "vanilla_thing")] {
        let things = tmp.path().join(root).join("common").join("things");
        std::fs::create_dir_all(&things).unwrap();
        std::fs::write(things.join("x.txt"), format!("{instance} = {{ }}\n")).unwrap();
    }
    std::fs::create_dir_all(tmp.path().join("cache")).unwrap();
    tmp
}

fn load_cached(workspace: &std::path::Path, refresh: bool) -> cwtools_driver::SessionWithFiles {
    Session::load(SessionConfig {
        game: Game::Hoi4,
        rules: RulesInput::Dir(workspace.join("rules")),
        directory: workspace.join("mod"),
        vanilla: Some(workspace.join("vanilla")),
        vanilla_cache: None,
        vanilla_cache_auto: Some(VanillaCacheAuto {
            dir: workspace.join("cache"),
            refresh,
        }),
        ignore_files: &[],
        ignore_dirs: &[],
        loc_languages: None,
        on_rules_warning: None,
    })
}

fn cache_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "cwv"))
        .collect();
    files.sort();
    files
}

/// Emptying the install's only script file after the first load: a second load
/// that still resolves the base-game instance can only have read it from the
/// cache the first load wrote.
fn blank_the_install(workspace: &std::path::Path) {
    std::fs::write(
        workspace
            .join("vanilla")
            .join("common")
            .join("things")
            .join("x.txt"),
        "",
    )
    .unwrap();
}

#[test]
fn vanilla_cache_auto_writes_then_reuses() {
    let ws = cache_workspace();
    let first = load_cached(ws.path(), false);
    assert!(
        first.type_index().contains("thing", "vanilla_thing"),
        "the install walk should index the base-game instance"
    );
    assert_eq!(
        cache_files(&ws.path().join("cache")).len(),
        1,
        "the first run should write exactly one cache file"
    );

    blank_the_install(ws.path());
    let second = load_cached(ws.path(), false);
    assert!(
        second.type_index().contains("thing", "vanilla_thing"),
        "the second run should read the base-game instance from the cache"
    );
}

#[test]
fn vanilla_cache_auto_refresh_rebuilds_and_overwrites() {
    let ws = cache_workspace();
    load_cached(ws.path(), false);
    blank_the_install(ws.path());

    let refreshed = load_cached(ws.path(), true);
    assert!(
        !refreshed.type_index().contains("thing", "vanilla_thing"),
        "--refresh must re-index the install, not read the cache"
    );
    let after = load_cached(ws.path(), false);
    assert!(
        !after.type_index().contains("thing", "vanilla_thing"),
        "--refresh must also overwrite the stale cache it skipped"
    );
}

#[test]
fn vanilla_cache_auto_is_keyed_by_ruleset_shape() {
    let ws = cache_workspace();
    load_cached(ws.path(), false);

    // Same install, different rules: the cached instances are extracted by the
    // rules, so the old cache must not be reused for the new ones.
    std::fs::write(ws.path().join("rules").join("things.cwt"), THING_RULES_V2).unwrap();
    let reloaded = load_cached(ws.path(), false);
    assert!(
        reloaded.type_index().contains("thing", "vanilla_thing"),
        "a rules change should re-index, not lose base-game data"
    );
    assert_eq!(
        cache_files(&ws.path().join("cache")).len(),
        2,
        "each ruleset shape gets its own cache file"
    );
}

#[test]
fn vanilla_cache_auto_recovers_from_an_unreadable_file() {
    let ws = cache_workspace();
    load_cached(ws.path(), false);
    let cache = cache_files(&ws.path().join("cache")).remove(0);
    std::fs::write(&cache, b"not a cache file").unwrap();

    let session = load_cached(ws.path(), false);
    assert!(
        session.type_index().contains("thing", "vanilla_thing"),
        "an unreadable cache must fall back to indexing the install"
    );
    assert!(
        std::fs::metadata(&cache).unwrap().len() > "not a cache file".len() as u64,
        "the unreadable cache should have been replaced"
    );
}

// ── loc language scoping ─────────────────────────────────────────────────────

/// A temp workspace whose `mod/localisation/` holds one english file, one french
/// file, and one file with an unrecognised language header.
fn loc_language_workspace() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("rules")).unwrap();
    std::fs::write(tmp.path().join("rules").join("things.cwt"), THING_RULES).unwrap();
    let things = tmp.path().join("mod").join("common").join("things");
    std::fs::create_dir_all(&things).unwrap();
    std::fs::write(things.join("x.txt"), "my_thing = { }\n").unwrap();
    let loc = tmp.path().join("mod").join("localisation");
    std::fs::create_dir_all(&loc).unwrap();
    std::fs::write(
        loc.join("a_l_english.yml"),
        "l_english:\n english_key:0 \"e\"\n",
    )
    .unwrap();
    std::fs::write(
        loc.join("b_l_french.yml"),
        "l_french:\n french_key:0 \"f\"\n",
    )
    .unwrap();
    std::fs::write(
        loc.join("c_l_klingon.yml"),
        "l_klingon:\n other_key:0 \"k\"\n",
    )
    .unwrap();
    tmp
}

fn load_scoped(
    workspace: &std::path::Path,
    langs: Option<Vec<Lang>>,
) -> cwtools_driver::SessionWithFiles {
    Session::load(SessionConfig {
        game: Game::Hoi4,
        rules: RulesInput::Dir(workspace.join("rules")),
        directory: workspace.join("mod"),
        vanilla: None,
        vanilla_cache: None,
        vanilla_cache_auto: None,
        ignore_files: &[],
        ignore_dirs: &[],
        loc_languages: langs,
        on_rules_warning: None,
    })
}

#[test]
fn unscoped_loc_load_keeps_every_language() {
    let ws = loc_language_workspace();
    let session = load_scoped(ws.path(), None);
    assert!(session.loc_index().exists_any("english_key"));
    assert!(
        session.loc_index().exists_any("french_key"),
        "the default must keep loading every language"
    );
}

#[test]
fn scoped_loc_load_skips_other_languages() {
    let ws = loc_language_workspace();
    let session = load_scoped(ws.path(), Some(vec![Lang::English]));
    assert!(session.loc_index().exists_any("english_key"));
    assert!(
        !session.loc_index().exists_any("french_key"),
        "a language outside --loc-language should never be parsed"
    );
    assert_eq!(session.loc_index().languages_with_data(), &[Lang::English]);
}

#[test]
fn scoped_loc_load_still_validates_unrecognised_headers() {
    // A file whose header language can't be read isn't scoped out by the loc
    // validator, so the parse-time filter must not drop it either (CW256).
    let ws = loc_language_workspace();
    let session = load_scoped(ws.path(), Some(vec![Lang::English]));
    let diagnostics = session.loc_project_diagnostics();
    assert!(
        diagnostics
            .iter()
            .any(|d| d.file.ends_with("c_l_klingon.yml")),
        "unrecognised-header files must still be parsed and linted, got {:?}",
        diagnostics.iter().map(|d| &d.file).collect::<Vec<_>>()
    );
    assert!(
        !diagnostics
            .iter()
            .any(|d| d.file.ends_with("b_l_french.yml")),
        "a scoped-out language reports nothing, same as before"
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
