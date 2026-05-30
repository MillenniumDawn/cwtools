use crate::{ValidationError, ErrorSeverity};
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
        match child {
            Child::Node(idx) => {
                let node = &ast.arena.nodes[*idx as usize];
                let key = table.get_string(node.key.normal).unwrap_or_default();
                match key.as_str() {
                    "event" => validate_event(node, ast, table, file_path, errors),
                    "ship_size" => validate_ship_size(node, ast, table, file_path, errors),
                    "technology" => validate_technology(node, ast, table, file_path, errors),
                    _ => {}
                }
            }
            Child::Leaf(idx) => {
                let leaf = &ast.arena.leaves[*idx as usize];
                let key = table.get_string(leaf.key.normal).unwrap_or_default();
                if key == "event" {
                    if let Value::Clause(children) = &leaf.value {
                        validate_event_clause(children, ast, table, file_path, errors);
                    }
                }
            }
            _ => {}
        }
    }
}

// ── Event Validation ───────────────────────────────────

fn validate_event(
    node: &cwtools_parser::ast::Node,
    ast: &ParsedFile,
    table: &StringTable,
    file_path: &str,
    errors: &mut Vec<ValidationError>,
) {
    let has_mtth = node.children.iter().any(|c| child_key_eq(c, ast, table, "mean_time_to_happen"));
    let has_trig = node.children.iter().any(|c| child_key_eq(c, ast, table, "is_triggered_only"));
    let has_once = node.children.iter().any(|c| child_key_eq(c, ast, table, "fire_only_once"));
    let has_base = node.children.iter().any(|c| child_key_eq(c, ast, table, "base"));
    let has_always_no = node.children.iter().any(|c| {
        child_key_eq(c, ast, table, "trigger") && child_has_always_no(c, ast, table)
    });

    if !has_mtth && !has_trig && !has_once && !has_always_no && !has_base {
        errors.push(ValidationError {
            message: "Event is missing mean_time_to_happen, is_triggered_only, fire_only_once, or trigger={always=no}. Performance concern: event may fire every tick.".to_string(),
            severity: ErrorSeverity::Warning,
            line: node.pos.start.line,
            col: node.pos.start.col,
            file: file_path.to_string(),
        });
    }

    // Check pre-triggers: must be in event's direct children, not inside trigger block
    let pre_triggers = [
        "has_owner", "is_homeworld", "original_owner", "is_ai",
        "has_ground_combat", "is_capital", "is_occupied_flag",
    ];
    for child in &node.children {
        let key = match child {
            Child::Leaf(idx) => table.get_string(ast.arena.leaves[*idx as usize].key.normal).unwrap_or_default(),
            Child::Node(idx) => table.get_string(ast.arena.nodes[*idx as usize].key.normal).unwrap_or_default(),
            _ => continue,
        };
        if pre_triggers.contains(&key.as_str()) {
            errors.push(ValidationError {
                message: format!("Pre-trigger '{}' should be inside a 'trigger' block, not at event root", key),
                severity: ErrorSeverity::Warning,
                line: child_line(child, ast),
                col: 0,
                file: file_path.to_string(),
            });
        }
    }
}

fn validate_event_clause(
    children: &[Child],
    ast: &ParsedFile,
    table: &StringTable,
    file_path: &str,
    errors: &mut Vec<ValidationError>,
) {
    let has_mtth = children.iter().any(|c| child_key_eq(c, ast, table, "mean_time_to_happen"));
    let has_trig = children.iter().any(|c| child_key_eq(c, ast, table, "is_triggered_only"));
    let has_once = children.iter().any(|c| child_key_eq(c, ast, table, "fire_only_once"));
    let has_base = children.iter().any(|c| child_key_eq(c, ast, table, "base"));
    let has_always_no = children.iter().any(|c| {
        child_key_eq(c, ast, table, "trigger") && child_has_always_no(c, ast, table)
    });

    if !has_mtth && !has_trig && !has_once && !has_always_no && !has_base {
        // Find the event leaf's position
        let line = children.first().map(|c| child_line(c, ast)).unwrap_or(0);
        errors.push(ValidationError {
            message: "Event is missing mean_time_to_happen, is_triggered_only, fire_only_once, or trigger={always=no}. Performance concern: event may fire every tick.".to_string(),
            severity: ErrorSeverity::Warning,
            line,
            col: 0,
            file: file_path.to_string(),
        });
    }
}

// ── Ship Size Validation ───────────────────────────────

fn validate_ship_size(
    _node: &cwtools_parser::ast::Node,
    _ast: &ParsedFile,
    _table: &StringTable,
    _file_path: &str,
    _errors: &mut Vec<ValidationError>,
) {
    // TODO: validate ship size has valid graphical_culture / section
}

// ── Technology Validation ──────────────────────────────

fn validate_technology(
    _node: &cwtools_parser::ast::Node,
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
        Child::Node(idx) => {
            let node = &ast.arena.nodes[*idx as usize];
            table.get_string(node.key.normal).unwrap_or_default() == expected
        }
        _ => false,
    }
}

fn child_line(child: &Child, ast: &ParsedFile) -> u32 {
    match child {
        Child::Leaf(idx) => ast.arena.leaves[*idx as usize].pos.start.line,
        Child::Node(idx) => ast.arena.nodes[*idx as usize].pos.start.line,
        _ => 0,
    }
}

fn child_has_always_no(child: &Child, ast: &ParsedFile, table: &StringTable) -> bool {
    match child {
        Child::Node(idx) => {
            let node = &ast.arena.nodes[*idx as usize];
            node.children.iter().any(|c| {
                child_key_eq(c, ast, table, "always") && child_is_bool(c, ast, table, false)
            })
        }
        _ => false,
    }
}

fn child_is_bool(child: &Child, ast: &ParsedFile, table: &StringTable, expected: bool) -> bool {
    match child {
        Child::Leaf(idx) => {
            let leaf = &ast.arena.leaves[*idx as usize];
            match &leaf.value {
                Value::Bool(b) => *b == expected,
                Value::String(t) | Value::QString(t) => {
                    let text = table.get_string(t.normal).unwrap_or_default().to_lowercase();
                    (expected && text == "yes") || (!expected && text == "no")
                }
                _ => false,
            }
        }
        _ => false,
    }
}
