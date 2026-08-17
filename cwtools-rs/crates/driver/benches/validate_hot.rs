//! Batch validation inner loop over a real ruleset and a pinned corpus file.
//!
//! `rules_hot` covers the work a keystroke triggers. This one times
//! `validate_prepared` on a fixed Kaiserreich scripted-effects file, the shape
//! a CLI or workspace scan walks per file. Combined with the TRACE spans on
//! `count_and_validate_children`, `validate_leaf` and `validate_alias_usage`,
//! a later change to those loops can land with before/after numbers.
//!
//! Inputs are the same two checkouts the corpus guard uses:
//!
//!   CWTOOLS_RULES=/path/to/cwtools-hoi4-config/Config \
//!   CWTOOLS_CORPUS=/path/to/Kaiserreich-4-Development \
//!     cargo bench -p cwtools_driver --bench validate_hot
//!
//! Either can also be found under `CWTOOLS_PROJECTS`, or as a sibling of this
//! repo. Without both there is nothing to validate against and the bench
//! prints why and measures nothing.

use std::hint::black_box;
use std::path::{Path, PathBuf};

use criterion::{Criterion, criterion_group, criterion_main};
use cwtools_driver::{RulesInput, load_rules};
use cwtools_game::constants::Game;
use cwtools_index::TypeIndex;
use cwtools_parser::parser::parse_string;
use cwtools_string_table::string_table::StringTable;
use cwtools_validation::{Prepared, build_scope_registry_arc, validate_prepared};

/// Alias-heavy scripted-effects file from the pinned Kaiserreich corpus.
/// Large enough for the inner loop to show up, small enough to iterate.
const CORPUS_FILE: &str = "common/scripted_effects/RUS effects (Russia).txt";

fn projects_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CWTOOLS_PROJECTS") {
        return Some(PathBuf::from(dir));
    }
    // crates/driver -> repo root is ../../.., siblings sit next to it.
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    repo.join("..").canonicalize().ok()
}

fn rules_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CWTOOLS_RULES") {
        return Some(PathBuf::from(dir));
    }
    let dir = projects_dir()?.join("cwtools-hoi4-config/Config");
    dir.is_dir().then_some(dir)
}

fn corpus_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CWTOOLS_CORPUS") {
        return Some(PathBuf::from(dir));
    }
    let dir = projects_dir()?.join("Kaiserreich-4-Development");
    dir.is_dir().then_some(dir)
}

fn read_corpus_file(root: &Path) -> Option<(String, String)> {
    let path = root.join(CORPUS_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => Some((CORPUS_FILE.to_string(), text)),
        Err(e) => {
            eprintln!("validate_hot: could not read {}: {e}", path.display());
            None
        }
    }
}

fn bench_validate_hot(c: &mut Criterion) {
    let Some(rules) = rules_dir() else {
        eprintln!(
            "validate_hot: no ruleset. Set CWTOOLS_RULES to a cwtools-hoi4-config/Config checkout"
        );
        return;
    };
    let Some(corpus) = corpus_dir() else {
        eprintln!(
            "validate_hot: no corpus. Set CWTOOLS_CORPUS to a Kaiserreich-4-Development checkout"
        );
        return;
    };
    let Some((file_path, source)) = read_corpus_file(&corpus) else {
        return;
    };

    let table = StringTable::new();
    let (ruleset, rule_errors) = match load_rules(&RulesInput::Dir(rules), &table) {
        Ok(loaded) => loaded,
        Err(e) => {
            eprintln!("validate_hot: could not load rules: {e}");
            return;
        }
    };
    assert!(
        rule_errors.is_empty(),
        "validate_hot: rules problems: {rule_errors:?}"
    );

    let ast = parse_string(&source, &table);
    assert!(
        !ast.arena.leaves.is_empty(),
        "validate_hot: {CORPUS_FILE} parsed to no leaves"
    );

    let type_index = TypeIndex::default();
    let scope_registry = build_scope_registry_arc(&ruleset, Some(Game::Hoi4));
    let prepared = Prepared {
        ruleset: &ruleset,
        table: &table,
        game: Some(Game::Hoi4),
        type_index: Some(&type_index),
        modifier_keys: None,
        loc_index: None,
        extra_loc_keys: None,
        inline_scripts: None,
        registry: scope_registry.as_ref(),
        scope_checks: true,
        var_checks: true,
    };

    // One dry run so a panic fails setup instead of the first criterion sample.
    black_box(validate_prepared(&ast, &file_path, &prepared));

    c.bench_function("validate_prepared/scripted_effects", |b| {
        b.iter(|| {
            black_box(validate_prepared(
                black_box(&ast),
                black_box(file_path.as_str()),
                black_box(&prepared),
            ))
        })
    });
}

criterion_group!(benches, bench_validate_hot);
criterion_main!(benches);
