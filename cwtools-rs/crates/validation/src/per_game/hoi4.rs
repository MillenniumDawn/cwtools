//! HOI4-specific cleanup hints.
//!
//! Currently: CW280, flagging a field whose body is exactly `{ always = <bool> }`
//! where that bool matches the field's game default, so the whole field is a
//! no-op and can be removed (e.g. `allowed_civil_war = { always = no }`).
//!
//! The table is deliberately explicit, not "any `{ always = no }` is redundant":
//! `always = no` is a recommended guard in other contexts (an event's
//! `trigger = { always = no }`, see CW107), so only fields whose default is
//! known are listed.

use super::common::as_block;
use crate::{ValidationError, error_codes};
use cwtools_parser::ast::{Child, ParsedFile, Value};
use cwtools_string_table::string_table::StringTable;

/// If the block's only non-comment child is `always = <bool>`, return that bool;
/// otherwise `None` (anything else means the block does real work).
fn sole_always_value(children: &[Child], ast: &ParsedFile, table: &StringTable) -> Option<bool> {
    let mut found: Option<bool> = None;
    for child in children {
        match child {
            Child::Comment(_) => {}
            Child::Leaf(idx) => {
                let l = &ast.arena.leaves[*idx as usize];
                if !table
                    .with_string(l.key.lower, |k| k == "always")
                    .unwrap_or(false)
                {
                    return None;
                }
                match l.value {
                    Value::Bool(b) if found.is_none() => found = Some(b),
                    _ => return None,
                }
            }
            _ => return None,
        }
    }
    found
}

fn walk(
    children: &[Child],
    ast: &ParsedFile,
    table: &StringTable,
    file_path: &str,
    errors: &mut Vec<ValidationError>,
) {
    for child in children {
        let Some(block) = as_block(child, ast) else {
            continue;
        };
        let key = block.key_string_lower(table);

        // Fields whose body `{ always = <bool> }` matches the game default (so the
        // field is a no-op) -> the default the `always` value must equal. Listed
        // explicitly: an idea/spirit's allowed_civil_war defaults to "no".
        let default = match key.as_str() {
            "allowed_civil_war" => Some(false),
            _ => None,
        };
        if let Some(default) = default
            && sole_always_value(block.children, ast, table) == Some(default)
        {
            errors.push(ValidationError::from_code(
                &error_codes::CW280_REDUNDANT_DEFAULT_FIELD,
                file_path,
                block.range.start.line,
                block.range.start.col,
                &[&key],
            ));
        }

        walk(block.children, ast, table, file_path, errors);
    }
}

/// Run the HOI4-specific cleanup hints over a whole file.
pub fn validate_hoi4(
    ast: &ParsedFile,
    _ruleset: &cwtools_rules::rules_types::RuleSet,
    table: &StringTable,
    file_path: &str,
    errors: &mut Vec<ValidationError>,
) {
    walk(&ast.root_children, ast, table, file_path, errors);
}

#[cfg(test)]
mod tests {
    use super::*;
    use cwtools_parser::parser::parse_string;

    fn run(src: &str) -> Vec<ValidationError> {
        let table = StringTable::new();
        let ast = parse_string(src, &table).expect("parse");
        let ruleset = cwtools_rules::rules_types::RuleSet::new();
        let mut errors = Vec::new();
        validate_hoi4(&ast, &ruleset, &table, "test.txt", &mut errors);
        errors
    }

    #[test]
    fn flags_redundant_allowed_civil_war() {
        let errors = run("my_idea = {\n allowed_civil_war = { always = no }\n}\n");
        assert_eq!(errors.len(), 1, "expected one CW280");
        assert_eq!(errors[0].code, Some("CW280"));
    }

    #[test]
    fn ignores_non_default_value() {
        // always = yes is not the default for allowed_civil_war, so not redundant.
        let errors = run("my_idea = {\n allowed_civil_war = { always = yes }\n}\n");
        assert!(errors.is_empty());
    }

    #[test]
    fn ignores_real_trigger_body() {
        // A real trigger (not a bare always) does work — leave it alone.
        let errors = run("my_idea = {\n allowed_civil_war = { has_war = no }\n}\n");
        assert!(errors.is_empty());
    }

    #[test]
    fn flags_mixed_case_key() {
        // Paradox keys are case-insensitive; dispatch must be too.
        let errors = run("my_idea = {\n Allowed_Civil_War = { Always = no }\n}\n");
        assert_eq!(errors.len(), 1, "expected one CW280");
        assert_eq!(errors[0].code, Some("CW280"));
    }

    #[test]
    fn ignores_unlisted_fields() {
        // always = no on an unlisted field is not flagged (e.g. event guard).
        let errors = run("country_event = {\n trigger = { always = no }\n}\n");
        assert!(errors.is_empty());
    }
}
