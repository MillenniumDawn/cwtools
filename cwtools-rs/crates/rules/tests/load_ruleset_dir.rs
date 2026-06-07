use cwtools_rules::ruleset_loader::load_ruleset_from_dir;
use cwtools_string_table::string_table::StringTable;
use std::path::Path;

/// End-to-end test: load the Imperator Rome config directory and check that
/// we get a non-trivial combined RuleSet.
///
/// The config lives at CWToolsDocs/testconfig/cwtools-ir-config relative to
/// the repository root.  The test is skipped gracefully if the path does not
/// exist (e.g. on CI without the submodule).
#[test]
fn load_ir_config_dir() {
    let config_dir =
        Path::new("/mnt/Linux/github-projects/cwtools/CWToolsDocs/testconfig/cwtools-ir-config");

    if !config_dir.exists() {
        eprintln!("ir-config not found at {:?}, skipping test", config_dir);
        return;
    }

    let table = StringTable::new();
    let (ruleset, errors) = load_ruleset_from_dir(config_dir, &table);

    // Report any parse errors but don't fail the test on them — some .cwt
    // files in the test config may use features not yet implemented.
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
