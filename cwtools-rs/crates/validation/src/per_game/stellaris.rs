use super::common::as_block;
use crate::{ErrorSeverity, ValidationError, error_codes};
use cwtools_parser::ast::{Child, ParsedFile, Value};
use cwtools_rules::rules_types::RuleSet;
use cwtools_string_table::string_table::StringTable;

/// Stellaris-specific validators.
/// Ported from CWTools/Validation/Stellaris/STLValidation.fs
pub fn validate_stellaris(
    ast: &ParsedFile,
    _ruleset: &RuleSet,
    table: &StringTable,
    file_path: &str,
    errors: &mut Vec<ValidationError>,
) {
    for child in &ast.root_children {
        let Some(block) = as_block(child, ast) else {
            continue;
        };
        let key = block.key_string(table);
        match key.as_str() {
            k if k.ends_with("_event") || k == "event" => validate_event(
                block.children,
                block.range.start.line,
                ast,
                table,
                file_path,
                errors,
            ),
            "ship_size" => validate_ship_size(block.children, ast, table, file_path, errors),
            "technology" => validate_technology(block.children, ast, table, file_path, errors),
            _ => {}
        }
    }

    // Stellaris-specific structural hints (if/else 2.1, deprecated set_name).
    walk_if_else(&ast.root_children, ast, table, file_path, errors);
}

// ── If/Else & set_name structural hints (Item: Tier B Stellaris) ───────────
//
// Ported from CWTools/Validation/Stellaris/STLValidation.fs `validateIfElse210`
// (CW236/CW237), `validateIfElse` (CW238) and `validateDeprecatedSetName`
// (CW253). F# scopes these to classified effect blocks; this walk keys off the
// node names instead, which only appear in effect script.

/// Keys of a block's direct keyed children, in order.
fn child_keys(children: &[Child], ast: &ParsedFile, table: &StringTable) -> Vec<String> {
    children
        .iter()
        .filter_map(|c| match c {
            Child::Leaf(idx) => Some(
                table
                    .get_string(ast.arena.leaves[*idx as usize].key.normal)
                    .unwrap_or_default(),
            ),
            _ => None,
        })
        .collect()
}

fn walk_if_else(
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
        let key = block.key_string(table);
        let block_children = block.children;
        let line = block.range.start.line;
        let col = block.range.start.col;

        // CW253 — deprecated set_empire_name / set_planet_name.
        if key == "set_empire_name" || key == "set_planet_name" {
            errors.push(ValidationError {
                message: error_codes::CW253_DEPRECATED_SET_NAME
                    .message_template
                    .to_string(),
                severity: error_codes::CW253_DEPRECATED_SET_NAME.severity,
                line,
                col,
                file: file_path.to_string(),
                code: Some(error_codes::CW253_DEPRECATED_SET_NAME.id),
            });
        }

        if key != "limit" && key != "modifier" {
            let has_else = block_children
                .iter()
                .any(|c| child_key_eq(c, ast, table, "else"));
            let has_if = block_children
                .iter()
                .any(|c| child_key_eq(c, ast, table, "if"));
            let deprecated_else = (key == "if" || key == "else_if") && has_else && !has_if;

            // CW236 — old nested if/else style.
            if deprecated_else {
                errors.push(ValidationError {
                    message: error_codes::CW236_DEPRECATED_ELSE
                        .message_template
                        .to_string(),
                    severity: error_codes::CW236_DEPRECATED_ELSE.severity,
                    line,
                    col,
                    file: file_path.to_string(),
                    code: Some(error_codes::CW236_DEPRECATED_ELSE.id),
                });
            }

            // CW237 — ambiguous if = { if ... else }.
            if key == "if" && has_else && has_if {
                errors.push(ValidationError {
                    message: error_codes::CW237_AMBIGUOUS_IF_ELSE
                        .message_template
                        .to_string(),
                    severity: error_codes::CW237_AMBIGUOUS_IF_ELSE.severity,
                    line,
                    col,
                    file: file_path.to_string(),
                    code: Some(error_codes::CW237_AMBIGUOUS_IF_ELSE.id),
                });
            }

            // CW238 — else/else_if missing a preceding if (skip the deprecated case).
            if !deprecated_else {
                let mut prev_was_if = false;
                for k in child_keys(block_children, ast, table) {
                    if k != "if" && k != "else" && k != "else_if" {
                        continue;
                    }
                    if prev_was_if {
                        prev_was_if = k == "if" || k == "else_if";
                    } else if k == "if" {
                        prev_was_if = true;
                    } else {
                        // else / else_if with no preceding if.
                        errors.push(ValidationError {
                            message: error_codes::CW238_IF_ELSE_ORDER
                                .message_template
                                .to_string(),
                            severity: error_codes::CW238_IF_ELSE_ORDER.severity,
                            line,
                            col,
                            file: file_path.to_string(),
                            code: Some(error_codes::CW238_IF_ELSE_ORDER.id),
                        });
                        break;
                    }
                }
            }
        }

        walk_if_else(block_children, ast, table, file_path, errors);
    }
}

// ── Event Validation ───────────────────────────────────

/// Validate a Stellaris event body (children of `*_event = { ... }` or inline clause).
/// `event_line` is the line of the event key for anchoring the CW107 diagnostic.
fn validate_event(
    children: &[Child],
    event_line: u32,
    ast: &ParsedFile,
    table: &StringTable,
    file_path: &str,
    errors: &mut Vec<ValidationError>,
) {
    let mut has_mtth = false;
    let mut has_trig = false;
    let mut has_once = false;
    let mut has_base = false;
    let mut has_always_no = false;
    for c in children {
        if child_key_eq(c, ast, table, "mean_time_to_happen") {
            has_mtth = true;
        } else if child_key_eq(c, ast, table, "is_triggered_only") {
            has_trig = true;
        } else if child_key_eq(c, ast, table, "fire_only_once") {
            has_once = true;
        } else if child_key_eq(c, ast, table, "base") {
            has_base = true;
        } else if child_key_eq(c, ast, table, "trigger") && child_has_always_no(c, ast, table) {
            has_always_no = true;
        }
    }

    if !has_mtth && !has_trig && !has_once && !has_always_no && !has_base {
        errors.push(ValidationError {
            message: "Event is missing mean_time_to_happen, is_triggered_only, fire_only_once, or trigger={always=no}. Performance concern: event may fire every tick.".to_string(),
            severity: ErrorSeverity::Information,
            line: event_line,
            col: 0,
            file: file_path.to_string(),
            code: Some(error_codes::CW107_EVENT_EVERY_TICK.id),
        });
    }

    // CW301: pre-triggers found inside the trigger block should be moved to event
    // root for performance. Mirrors F# STLValidation.fs `validatePreTriggers` which
    // checks trigger block leaves, not event root leaves.
    const PRE_TRIGGERS: &[&str] = &[
        "has_owner",
        "is_homeworld",
        "original_owner",
        "is_ai",
        "has_ground_combat",
        "is_capital",
        "is_occupied_flag",
    ];
    for child in children {
        if !child_key_eq(child, ast, table, "trigger") {
            continue;
        }
        let trigger_children = match child {
            Child::Leaf(idx) => {
                if let Value::Clause(c) = &ast.arena.leaves[*idx as usize].value {
                    c.as_slice()
                } else {
                    continue;
                }
            }
            _ => continue,
        };
        for tc in trigger_children {
            let Child::Leaf(idx) = tc else { continue };
            let leaf = &ast.arena.leaves[*idx as usize];
            let is_pre = table
                .with_string(leaf.key.normal, |k| PRE_TRIGGERS.contains(&k))
                .unwrap_or(false);
            if is_pre {
                let key = table
                    .with_string(leaf.key.normal, |s| s.to_string())
                    .unwrap_or_default();
                errors.push(ValidationError {
                    message: format!(
                        "Trigger '{}' can be a pre-trigger at event root for better performance",
                        key
                    ),
                    severity: ErrorSeverity::Information,
                    line: child_line(tc, ast),
                    col: 0,
                    file: file_path.to_string(),
                    code: Some(error_codes::CW301_PRE_TRIGGER_LEVEL.id),
                });
            }
        }
    }
}

// ── Ship Size Validation ───────────────────────────────

fn validate_ship_size(
    _children: &[Child],
    _ast: &ParsedFile,
    _table: &StringTable,
    _file_path: &str,
    _errors: &mut Vec<ValidationError>,
) {
    // TODO: validate ship size has valid graphical_culture / section
}

// ── Technology Validation ──────────────────────────────

fn validate_technology(
    _children: &[Child],
    _ast: &ParsedFile,
    _table: &StringTable,
    _file_path: &str,
    _errors: &mut Vec<ValidationError>,
) {
    // TODO: validate tech prerequisites exist
}

// ── Helpers ────────────────────────────────────────────

fn child_key_eq(child: &Child, ast: &ParsedFile, table: &StringTable, expected: &str) -> bool {
    match child {
        Child::Leaf(idx) => {
            let leaf = &ast.arena.leaves[*idx as usize];
            table
                .with_string(leaf.key.normal, |k| k.eq_ignore_ascii_case(expected))
                .unwrap_or(false)
        }
        _ => false,
    }
}

fn child_line(child: &Child, ast: &ParsedFile) -> u32 {
    match child {
        Child::Leaf(idx) => ast.arena.leaves[*idx as usize].pos.start.line,
        _ => 0,
    }
}

fn child_has_always_no(child: &Child, ast: &ParsedFile, table: &StringTable) -> bool {
    as_block(child, ast).is_some_and(|block| {
        block
            .children
            .iter()
            .any(|c| child_key_eq(c, ast, table, "always") && child_is_bool(c, ast, table, false))
    })
}

fn child_is_bool(child: &Child, ast: &ParsedFile, table: &StringTable, expected: bool) -> bool {
    match child {
        Child::Leaf(idx) => {
            let leaf = &ast.arena.leaves[*idx as usize];
            match &leaf.value {
                Value::Bool(b) => *b == expected,
                Value::String(t) | Value::QString(t) => table
                    .with_string(t.normal, |s| {
                        (expected && s.eq_ignore_ascii_case("yes"))
                            || (!expected && s.eq_ignore_ascii_case("no"))
                    })
                    .unwrap_or(false),
                _ => false,
            }
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cwtools_parser::parser::parse_string;

    fn codes(script: &str) -> Vec<(String, u32, u16)> {
        let table = StringTable::new();
        let ast = parse_string(script, &table).unwrap();
        let ruleset = RuleSet::new();
        let mut errors = Vec::new();
        validate_stellaris(&ast, &ruleset, &table, "test.txt", &mut errors);
        errors
            .into_iter()
            .filter_map(|e| e.code.map(|c| (c.to_string(), e.line, e.col)))
            .collect()
    }

    fn has_code(codes: &[(String, u32, u16)], code: &str) -> bool {
        codes.iter().any(|(c, _, _)| c == code)
    }

    #[test]
    fn child_key_eq_is_case_insensitive() {
        let table = StringTable::new();
        let ast = parse_string("root = {\n IF = {}\n Trigger = {}\n}\n", &table).unwrap();
        let block = as_block(&ast.root_children[0], &ast).expect("root is a block");
        assert!(
            block
                .children
                .iter()
                .any(|c| child_key_eq(c, &ast, &table, "if")),
            "`IF` should match expected `if`"
        );
        assert!(
            block
                .children
                .iter()
                .any(|c| child_key_eq(c, &ast, &table, "trigger")),
            "`Trigger` should match expected `trigger`"
        );
    }

    // ── Event validation (CW107) ──────────────────────────────────────────────

    #[test]
    fn event_without_mtth_or_trigger_is_cw107() {
        let c = codes("my_event = { }\n");
        assert!(
            has_code(&c, "CW107"),
            "event with no MTTH/trigger/once should emit CW107, got: {:?}",
            c
        );
    }

    #[test]
    fn event_with_mtth_is_clean() {
        let c = codes("my_event = { mean_time_to_happen = { years = 5 } }\n");
        assert!(!has_code(&c, "CW107"), "got: {:?}", c);
    }

    #[test]
    fn event_is_triggered_only_is_clean() {
        let c = codes("my_event = { is_triggered_only = yes }\n");
        assert!(!has_code(&c, "CW107"), "got: {:?}", c);
    }

    #[test]
    fn event_fire_only_once_is_clean() {
        let c = codes("my_event = { fire_only_once = yes }\n");
        assert!(!has_code(&c, "CW107"), "got: {:?}", c);
    }

    #[test]
    fn event_trigger_always_no_is_clean() {
        let c = codes("my_event = { trigger = { always = no } }\n");
        assert!(!has_code(&c, "CW107"), "got: {:?}", c);
    }

    #[test]
    fn event_trigger_always_yes_still_cw107() {
        // `trigger = { always = yes }` does NOT suppress CW107; only always=no does.
        let c = codes("my_event = { trigger = { always = yes } }\n");
        assert!(has_code(&c, "CW107"), "got: {:?}", c);
    }

    #[test]
    fn non_event_root_is_not_cw107() {
        // The CW107 check is scoped to *_event / event keys only.
        let c = codes("foo = { }\n");
        assert!(!has_code(&c, "CW107"), "got: {:?}", c);
    }

    // ── Pre-trigger placement (CW301) ───────────────────────────────────────

    #[test]
    fn pre_trigger_inside_trigger_is_cw301() {
        let c = codes(
            "my_event = {\n\
             mean_time_to_happen = { years = 5 }\n\
             trigger = { is_ai = yes }\n\
             }\n",
        );
        assert!(
            has_code(&c, "CW301"),
            "pre-trigger inside trigger block should emit CW301, got: {:?}",
            c
        );
    }

    #[test]
    fn pre_trigger_at_root_is_clean() {
        // `is_ai` at the event root is the preferred (pre-trigger) location.
        let c = codes(
            "my_event = {\n\
             mean_time_to_happen = { years = 5 }\n\
             is_ai = yes\n\
             }\n",
        );
        assert!(!has_code(&c, "CW301"), "got: {:?}", c);
    }

    // ── Deprecated set_name (CW253) ───────────────────────────────────────────

    #[test]
    fn set_empire_name_is_cw253() {
        let c = codes("foo = { set_empire_name = { key = \"X\" } }\n");
        assert!(has_code(&c, "CW253"), "got: {:?}", c);
    }

    #[test]
    fn set_planet_name_is_cw253() {
        let c = codes("foo = { set_planet_name = { key = \"Y\" } }\n");
        assert!(has_code(&c, "CW253"), "got: {:?}", c);
    }

    // ── If/else structural hints (CW236/CW237/CW238) ──────────────────────────

    #[test]
    fn deprecated_nested_else_is_cw236() {
        // Old Stellaris style: if = { else = { ... } } without an inner if.
        let c = codes("foo = { if = { limit = { } else = { a = 1 } } }\n");
        assert!(has_code(&c, "CW236"), "got: {:?}", c);
    }

    #[test]
    fn ambiguous_if_with_else_and_inner_if_is_cw237() {
        // `if = { if ... else }` is ambiguous nesting.
        let c = codes("foo = { if = { limit = { } if = { a = 1 } else = { b = 2 } } }\n");
        assert!(has_code(&c, "CW237"), "got: {:?}", c);
    }

    #[test]
    fn else_without_preceding_if_is_cw238() {
        let c = codes("foo = { else = { a = 1 } }\n");
        assert!(has_code(&c, "CW238"), "got: {:?}", c);
    }

    #[test]
    fn properly_ordered_if_else_if_is_clean() {
        let c = codes("foo = { if = { limit = { } a = 1 } else_if = { limit = { } b = 2 } }\n");
        assert!(!has_code(&c, "CW238"), "got: {:?}", c);
    }

    #[test]
    fn nested_limit_and_modifier_do_not_false_positive() {
        // `limit` and `modifier` blocks are excluded from the if/else order walk.
        let c = codes("foo = { limit = { } modifier = { } }\n");
        assert!(!has_code(&c, "CW236") && !has_code(&c, "CW237") && !has_code(&c, "CW238"));
    }
}
