use crate::ValidationError;
use cwtools_parser::ast::ParsedFile;
use cwtools_rules::rules_types::RuleSet;
use cwtools_string_table::string_table::StringTable;

/// EU4-specific validators.
/// Ported from CWTools/Validation/EU4/EU4Validation.fs
pub fn validate_eu4(
    _ast: &ParsedFile,
    _ruleset: &RuleSet,
    _table: &StringTable,
    _file_path: &str,
    _errors: &mut Vec<ValidationError>,
) {
    // TODO: validate EU4 country_decisions, events, etc.
}
