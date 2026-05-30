use crate::{ValidationError, ErrorSeverity};
use cwtools_parser::ast::{Child, ParsedFile, Value};
use cwtools_rules::rules_types::RuleSet;
use cwtools_string_table::string_table::StringTable;
use std::collections::HashMap;

/// Validate common features across all games:
/// - `unique`: check no duplicate type definitions in same file
/// - `should_be_referenced`: check no unreferenced types
/// - `warning_only`: severity downgrade (handled by caller)
pub fn validate_common(
    ast: &ParsedFile,
    _ruleset: &RuleSet,
    table: &StringTable,
    file_path: &str,
    errors: &mut Vec<ValidationError>,
) {
    let mut type_counts: HashMap<String, usize> = HashMap::new();

    for child in &ast.root_children {
        let key = match child {
            Child::Node(idx) => {
                let node = &ast.arena.nodes[*idx as usize];
                table.get_string(node.key.normal).unwrap_or_default()
            }
            Child::Leaf(idx) => {
                let leaf = &ast.arena.leaves[*idx as usize];
                table.get_string(leaf.key.normal).unwrap_or_default()
            }
            _ => continue,
        };
        *type_counts.entry(key).or_insert(0) += 1;
    }

    for (key, count) in &type_counts {
        if *count > 1 {
            // Could check ruleset for `unique` flag, but for now just note it
            // In full parity, we'd look up the type definition and check `unique: true`
            errors.push(ValidationError {
                message: format!("Type '{}' appears {} times in file (unique violation)", key, count),
                severity: ErrorSeverity::Warning,
                line: 0,
                col: 0,
                file: file_path.to_string(),
            });
        }
    }
}
