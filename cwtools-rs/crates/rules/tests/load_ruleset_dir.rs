use cwtools_rules::ruleset_loader::load_ruleset_from_dir;
use cwtools_string_table::string_table::StringTable;
use std::path::PathBuf;

/// End-to-end test: load the real HOI4 config (`cwtools-hoi4-config`) and check
/// that we get a non-trivial combined RuleSet.
///
/// The config is its own repo. By default it's expected as a sibling checkout
/// (`<github-projects>/cwtools-hoi4-config/Config`); set `CWTOOLS_HOI4_CONFIG`
/// to point elsewhere. The test is skipped gracefully if it isn't present
/// (e.g. CI without the clone, or a git worktree that isn't a sibling).
#[test]
fn load_hoi4_config_dir() {
    let config_dir = std::env::var_os("CWTOOLS_HOI4_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../../cwtools-hoi4-config/Config")
        });

    if !config_dir.exists() {
        eprintln!("hoi4-config not found at {:?}, skipping test", config_dir);
        return;
    }

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
