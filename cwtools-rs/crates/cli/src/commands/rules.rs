//! `rules`: parse a `.cwt` file or directory and print a summary. Owns the
//! rules loader and the summary printer, which `parse` and `cache` reuse.

use cwtools_rules::rules_types::RuleSet;
use cwtools_string_table::string_table::StringTable;
use std::path::PathBuf;

pub(super) fn run(file: PathBuf) {
    let table = StringTable::new();
    let ruleset = load_rules(&file, &table);
    let label = if file.is_dir() {
        format!("rule directory: {}", file.display())
    } else {
        format!("rules file: {}", file.display())
    };
    println!("Parsed {}", label);
    print_ruleset_summary(&ruleset);
}

/// Load a RuleSet from either a single `.cwt` file or a directory of `.cwt`
/// files (shared loader in the driver). Rules-load failure is fatal in the CLI.
pub(super) fn load_rules(rules_path: &std::path::Path, table: &StringTable) -> RuleSet {
    let mut warn = |w: String| eprintln!("warn: {}", w);
    cwtools_driver::load_rules(
        &cwtools_driver::RulesInput::from_path(rules_path.to_path_buf()),
        table,
        Some(&mut warn),
    )
    .unwrap_or_else(|e| {
        eprintln!("Error loading rules: {}", e);
        std::process::exit(1);
    })
}

/// Print a compact summary of a loaded RuleSet. Shared by the Parse-on-directory
/// and Rules subcommands (previously copy-pasted between them).
pub(super) fn print_ruleset_summary(ruleset: &RuleSet) {
    println!("  Types:         {}", ruleset.types.len());
    for t in &ruleset.types {
        println!(
            "    - {} (path: {:?}, subtypes: {})",
            t.name,
            t.path_options.paths,
            t.subtypes.len()
        );
    }
    println!("  Enums:         {}", ruleset.enums.len());
    for e in &ruleset.enums {
        println!("    - {} ({} values)", e.key, e.values.len());
    }
    println!("  Aliases:       {}", ruleset.aliases.len());
    println!("  SingleAliases: {}", ruleset.single_aliases.len());
    println!("  ComplexEnums:  {}", ruleset.complex_enums.len());
}
