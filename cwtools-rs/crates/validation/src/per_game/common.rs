use crate::{ValidationError, error_codes};
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
        if let Some(type_def) = find_matching_type(&key, ruleset)
            && type_def.unique
        {
            let count = type_counts.get(&key).copied().unwrap_or(0);
            if count > 1 {
                // CW261 (DuplicateTypeDef). F#'s message is
                // "Key {id} of type {typename} is defined multiple times";
                // this per-file detection keys off the type name appearing
                // as repeated sibling keys, so `id` and `typename` collapse
                // to the same token. F#'s check is project-wide and grouped
                // by extracted instance id — a known refinement gap.
                let code = &error_codes::CW261_DUPLICATE_TYPE_DEF;
                errors.push(ValidationError {
                    message: code.format(&[&key, &key]),
                    severity: code.severity,
                    line: 0,
                    col: 0,
                    file: file_path.to_string(),
                    code: Some(code.id.to_string()),
                });
            }
        }
    }
}

fn find_matching_type<'a>(key: &str, ruleset: &'a RuleSet) -> Option<&'a TypeDefinition> {
    ruleset.type_by_name.get(key).map(|&i| &ruleset.types[i])
}
