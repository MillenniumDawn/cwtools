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
                code: Some(error_codes::CW253_DEPRECATED_SET_NAME.id.to_string()),
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
                    code: Some(error_codes::CW236_DEPRECATED_ELSE.id.to_string()),
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
                    code: Some(error_codes::CW237_AMBIGUOUS_IF_ELSE.id.to_string()),
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
                            code: Some(error_codes::CW238_IF_ELSE_ORDER.id.to_string()),
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
    let has_mtth = children
        .iter()
        .any(|c| child_key_eq(c, ast, table, "mean_time_to_happen"));
    let has_trig = children
        .iter()
        .any(|c| child_key_eq(c, ast, table, "is_triggered_only"));
    let has_once = children
        .iter()
        .any(|c| child_key_eq(c, ast, table, "fire_only_once"));
    let has_base = children.iter().any(|c| child_key_eq(c, ast, table, "base"));
    let has_always_no = children
        .iter()
        .any(|c| child_key_eq(c, ast, table, "trigger") && child_has_always_no(c, ast, table));

    if !has_mtth && !has_trig && !has_once && !has_always_no && !has_base {
        errors.push(ValidationError {
            message: "Event is missing mean_time_to_happen, is_triggered_only, fire_only_once, or trigger={always=no}. Performance concern: event may fire every tick.".to_string(),
            severity: ErrorSeverity::Information,
            line: event_line,
            col: 0,
            file: file_path.to_string(),
            code: Some(error_codes::CW107_EVENT_EVERY_TICK.id.to_string()),
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
            let key = match tc {
                Child::Leaf(idx) => table
                    .get_string(ast.arena.leaves[*idx as usize].key.normal)
                    .unwrap_or_default(),
                _ => continue,
            };
            if PRE_TRIGGERS.contains(&key.as_str()) {
                errors.push(ValidationError {
                    message: format!(
                        "Trigger '{}' can be a pre-trigger at event root for better performance",
                        key
                    ),
                    severity: ErrorSeverity::Information,
                    line: child_line(tc, ast),
                    col: 0,
                    file: file_path.to_string(),
                    code: Some(error_codes::CW301_PRE_TRIGGER_LEVEL.id.to_string()),
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
            table.get_string(leaf.key.normal).unwrap_or_default() == expected
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
                Value::String(t) | Value::QString(t) => {
                    let text = table
                        .get_string(t.normal)
                        .unwrap_or_default()
                        .to_lowercase();
                    (expected && text == "yes") || (!expected && text == "no")
                }
                _ => false,
            }
        }
        _ => false,
    }
}

// ── Localisation validators (Item 6) ─────────────────────────────────────────
//
// Ported from CWTools/Validation/Stellaris/STLLocalisationValidation.fs
// (checkKeyAndDesc).  These require a set of known localisation keys — if the
// caller doesn't supply one, the checks are skipped entirely.

/// Check that every named instance of `type_name` found in `ast` has both a
/// `<instance_name>` loc key AND a `<instance_name>_desc` loc key present in
/// `loc_keys`.  Mirrors F# `checkKeyAndDesc`.
///
/// The `name_getter` closure extracts the instance name from a node's children.
/// If `loc_keys` is None the function is a no-op.
pub fn check_key_and_desc(
    ast: &ParsedFile,
    table: &StringTable,
    file_path: &str,
    loc_keys: Option<&std::collections::HashSet<String>>,
    node_key_filter: &[&str],
    errors: &mut Vec<ValidationError>,
) {
    let loc_keys = match loc_keys {
        Some(k) => k,
        None => return,
    };

    for child in &ast.root_children {
        if let Child::Leaf(idx) = child {
            let leaf = &ast.arena.leaves[*idx as usize];
            let key = table.get_string(leaf.key.normal).unwrap_or_default();
            if !node_key_filter.is_empty() && !node_key_filter.contains(&key.as_str()) {
                continue;
            }
            if let Value::Clause(_) = &leaf.value {
                check_loc_key_pair(&key, leaf.pos.start.line, loc_keys, file_path, errors);
            }
        }
    }
}

fn check_loc_key_pair(
    name: &str,
    line: u32,
    loc_keys: &std::collections::HashSet<String>,
    file_path: &str,
    errors: &mut Vec<ValidationError>,
) {
    if !loc_keys.contains(name) {
        errors.push(ValidationError {
            message: format!(
                "Missing localisation key '{}' for instance '{}'",
                name, name
            ),
            severity: ErrorSeverity::Warning,
            line,
            col: 0,
            file: file_path.to_string(),
            code: Some(error_codes::CW100_MISSING_LOCALISATION.id.to_string()),
        });
    }
    let desc_key = format!("{}_desc", name);
    if !loc_keys.contains(&desc_key) {
        errors.push(ValidationError {
            message: format!(
                "Missing localisation key '{}' for instance '{}'",
                desc_key, name
            ),
            severity: ErrorSeverity::Warning,
            line,
            col: 0,
            file: file_path.to_string(),
            code: Some(error_codes::CW100_MISSING_LOCALISATION.id.to_string()),
        });
    }
}

/// Port of F# `valTechLocs`: validate that each technology node has its
/// localisation keys.  Requires loc_keys; no-op if None.
/// valPolicies follows the same pattern.
///
/// Not yet wired into [`run_game_validators`]. The loc-key plumbing it cited as
/// missing IS now available (the dispatcher takes a `ValidationCtx` carrying
/// `loc_index`), so the only remaining gap is the entry matching: F# operated on
/// the rules engine's *classified* tech/policy nodes, whereas `check_key_and_desc`
/// matches a literal root key. Stellaris tech entries are keyed by tech name (not
/// wrapped in a `technology` node), so the filter below needs porting to a
/// path/type-driven match (and a Stellaris corpus to verify) before this fires.
pub fn validate_stellaris_loc(
    ast: &ParsedFile,
    table: &StringTable,
    file_path: &str,
    loc_keys: Option<&std::collections::HashSet<String>>,
    errors: &mut Vec<ValidationError>,
) {
    // Technology localisation check
    check_key_and_desc(ast, table, file_path, loc_keys, &["technology"], errors);
    // Policy localisation check
    check_key_and_desc(ast, table, file_path, loc_keys, &["policy"], errors);
}
