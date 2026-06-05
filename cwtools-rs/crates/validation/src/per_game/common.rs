use crate::{ValidationError, ErrorSeverity, error_codes};
use cwtools_parser::ast::{Child, ParsedFile};
use cwtools_rules::rules_types::{RuleSet, TypeDefinition};
use cwtools_string_table::string_table::StringTable;
use std::collections::HashMap;

/// Validate common features across all games.
pub fn validate_common(
    ast: &ParsedFile,
    ruleset: &RuleSet,
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
            Child::LeafValue(_) | Child::ValueClause(_) | Child::Comment(_) => continue,
        };
        *type_counts.entry(key.clone()).or_insert(0) += 1;

        // Check if this type is defined with unique=true
        if let Some(type_def) = find_matching_type(&key, ruleset) {
            if type_def.unique {
                let count = type_counts.get(&key).copied().unwrap_or(0);
                if count > 1 {
                    errors.push(ValidationError {
                        message: format!("Type '{}' appears {} times in file (unique violation)", key, count),
                        severity: ErrorSeverity::Warning,
                        line: 0,
                        col: 0,
                        file: file_path.to_string(),
                        code: Some(error_codes::CW501_DUPLICATE_TYPE.id.to_string()),
                    });
                }
            }
        }
    }
}

fn find_matching_type<'a>(key: &str, ruleset: &'a RuleSet) -> Option<&'a TypeDefinition> {
    ruleset.type_by_name.get(key).map(|&i| &ruleset.types[i])
}
