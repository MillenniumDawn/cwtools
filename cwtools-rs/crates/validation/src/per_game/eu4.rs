use crate::{ValidationError, ErrorSeverity};
use cwtools_parser::ast::{Child, ParsedFile, Value};
use cwtools_rules::rules_types::RuleSet;
use cwtools_string_table::string_table::StringTable;

/// EU4-specific validators.
/// Ported from CWTools/Validation/EU4/EU4Validation.fs
pub fn validate_eu4(
    ast: &ParsedFile,
    _ruleset: &RuleSet,
    table: &StringTable,
    file_path: &str,
    errors: &mut Vec<ValidationError>,
) {
    for child in &ast.root_children {
        match child {
            Child::Node(idx) => {
                let node = &ast.arena.nodes[*idx as usize];
                let key = table.get_string(node.key.normal).unwrap_or_default();
                match key.as_str() {
                    "country_decisions" => validate_country_decisions(node, ast, table, file_path, errors),
                    "events" => validate_eu4_event(node, ast, table, file_path, errors),
                    _ => {}
                }
            }
            Child::Leaf(idx) => {
                let leaf = &ast.arena.leaves[*idx as usize];
                let key = table.get_string(leaf.key.normal).unwrap_or_default();
                if key == "country_decisions" || key == "events" {
                    if let Value::Clause(children) = &leaf.value {
                        // TODO: validate EU4 event/decision leaf clauses
                    }
                }
            }
            _ => {}
        }
    }
}

fn validate_country_decisions(
    _node: &cwtools_parser::ast::Node,
    _ast: &ParsedFile,
    _table: &StringTable,
    _file_path: &str,
    _errors: &mut Vec<ValidationError>,
) {
    // TODO: validate major decisions have major = yes, etc.
}

fn validate_eu4_event(
    _node: &cwtools_parser::ast::Node,
    _ast: &ParsedFile,
    _table: &StringTable,
    _file_path: &str,
    _errors: &mut Vec<ValidationError>,
) {
    // TODO: validate EU4 event structure
}
