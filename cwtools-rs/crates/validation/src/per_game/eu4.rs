use crate::ValidationError;
use cwtools_parser::ast::{Child, ParsedFile, Value};
use cwtools_rules::rules_types::RuleSet;
use cwtools_string_table::string_table::StringTable;

/// EU4-specific validators.
/// Ported from CWTools/Validation/EU4/EU4Validation.fs
pub fn validate_eu4(
    ast: &ParsedFile,
    _ruleset: &RuleSet,
    table: &StringTable,
    _file_path: &str,
    _errors: &mut Vec<ValidationError>,
) {
    for child in &ast.root_children {
        match child {
            Child::Node(idx) => {
                let node = &ast.arena.nodes[*idx as usize];
                let _key = table.get_string(node.key.normal).unwrap_or_default();
                // TODO: validate EU4 country_decisions, events, etc.
            }
            Child::Leaf(idx) => {
                let leaf = &ast.arena.leaves[*idx as usize];
                let _key = table.get_string(leaf.key.normal).unwrap_or_default();
                if let Value::Clause(_children) = &leaf.value {
                    // TODO: validate EU4 event/decision leaf clauses
                }
            }
            Child::LeafValue(_) | Child::ValueClause(_) | Child::Comment(_) => {}
        }
    }
}
