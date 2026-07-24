use cwtools_rules::rules_types::RootRule;
use cwtools_rules::ruleset_loader::load_ruleset_from_dir;
use cwtools_string_table::string_table::StringTable;
use std::path::PathBuf;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ruleset")
}

/// The directory walk itself: every `.cwt` under the root, subdirectories
/// included, merged into one RuleSet, with `folders.cwt` taking its own path and
/// non-`.cwt` files left alone.
///
/// This is the always-on half of the pair below. It runs on a committed fixture
/// so CI asserts something real about `load_ruleset_from_dir` without a checkout
/// of the game config.
#[test]
fn load_ruleset_dir_merges_every_cwt_file() {
    let table = StringTable::new();
    let (ruleset, errors) = load_ruleset_from_dir(&fixture_dir(), &table);

    assert!(
        errors.is_empty(),
        "fixture should parse clean, got {errors:?}"
    );

    // Types from the top level and from `nested/` both land in one RuleSet.
    let type_names: Vec<&str> = ruleset.types.iter().map(|t| t.name.as_str()).collect();
    assert!(
        type_names.contains(&"guard_idea"),
        "top-level types.cwt missing from {type_names:?}"
    );
    assert!(
        type_names.contains(&"guard_decision"),
        "nested/effects.cwt missing from {type_names:?}, the walk is not recursive"
    );
    assert!(
        !type_names.contains(&"should_never_load"),
        "loader parsed a non-.cwt file: {type_names:?}"
    );

    // A type's own fields survive the merge, not just its name.
    let idea = ruleset
        .types
        .iter()
        .find(|t| t.name == "guard_idea")
        .expect("guard_idea");
    assert_eq!(idea.name_field.as_deref(), Some("id"));
    // The loader strips the config's `game/` prefix off type paths.
    assert_eq!(idea.path_options.paths, vec!["common/ideas".to_string()]);

    let gender = ruleset
        .enums
        .iter()
        .find(|e| e.key == "guard_gender")
        .expect("enums.cwt did not contribute enum[guard_gender]");
    assert_eq!(
        gender.values,
        vec!["male".to_string(), "female".to_string()]
    );

    let alias_names: Vec<&str> = ruleset.aliases.iter().map(|(k, _)| k.as_str()).collect();
    assert!(
        alias_names.contains(&"effect:guard_set_flag") && alias_names.contains(&"effect:guard_if"),
        "aliases from nested/effects.cwt missing from {alias_names:?}"
    );
    assert!(
        ruleset
            .single_aliases
            .iter()
            .any(|(k, _)| k == "guard_limit"),
        "single_alias[guard_limit] missing"
    );

    // Root rules are keyed by type name, one per `<type> = { ... }` block.
    let root_types: Vec<&str> = ruleset
        .root_rules
        .iter()
        .filter_map(|r| match r {
            RootRule::TypeRule(name, _) => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        root_types.contains(&"guard_idea") && root_types.contains(&"guard_decision"),
        "root rules missing from {root_types:?}"
    );

    // folders.cwt is a plain line list, not .cwt syntax, and has its own branch.
    assert_eq!(
        ruleset.folders,
        vec![
            "common".to_string(),
            "events".to_string(),
            "localisation".to_string()
        ]
    );
}

/// End-to-end load of the real HOI4 config (`cwtools-hoi4-config`), which is
/// its own repo and not vendored here.
///
/// Ignored by default: without the checkout there is nothing to load, and a test
/// that quietly returns instead would report as passing while asserting nothing.
/// Run it with `cargo test -p cwtools_rules -- --ignored`, from a sibling
/// checkout (`<github-projects>/cwtools-hoi4-config/Config`) or with
/// `CWTOOLS_HOI4_CONFIG` pointing elsewhere. A missing directory is a hard
/// failure once you have asked for the test by name.
#[test]
#[ignore = "needs a cwtools-hoi4-config checkout; run with --ignored"]
fn load_hoi4_config_dir() {
    let config_dir = std::env::var_os("CWTOOLS_HOI4_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../../cwtools-hoi4-config/Config")
        });

    assert!(
        config_dir.exists(),
        "hoi4-config not found at {}; clone it as a sibling or set CWTOOLS_HOI4_CONFIG",
        config_dir.display()
    );

    let table = StringTable::new();
    let (ruleset, errors) = load_ruleset_from_dir(&config_dir, &table);

    // Report parse errors but don't fail on them — some .cwt files may use
    // features the Rust loader doesn't implement yet.
    for err in &errors {
        eprintln!("warn: {}", err);
    }

    println!("  types:         {}", ruleset.types.len());
    println!("  enums:         {}", ruleset.enums.len());
    println!("  aliases:       {}", ruleset.aliases.len());
    println!("  single_aliases:{}", ruleset.single_aliases.len());
    println!("  complex_enums: {}", ruleset.complex_enums.len());
    println!("  root_rules:    {}", ruleset.root_rules.len());
    println!("  values:        {}", ruleset.values.len());

    assert!(
        ruleset.types.len() > 20,
        "expected > 20 types, got {}",
        ruleset.types.len()
    );
    assert!(
        !ruleset.enums.is_empty(),
        "expected at least one enum, got {}",
        ruleset.enums.len()
    );
}
