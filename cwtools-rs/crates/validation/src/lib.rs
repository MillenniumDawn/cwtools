use cwtools_game::scope_engine::{ScopeContext, ScopeId, ScopeResult};
use cwtools_game::constants::Game;
use cwtools_parser::ast::{Child, ParsedFile, Value};
use cwtools_rules::rules_types::*;
use cwtools_string_table::string_table::StringTable;
use std::collections::HashMap;

pub mod per_game;

#[derive(Debug, Clone, PartialEq)]
pub struct ValidationError {
    pub message: String,
    pub severity: ErrorSeverity,
    pub line: u32,
    pub col: u16,
    pub file: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ErrorSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

pub fn validate_ast(
    ast: &ParsedFile,
    ruleset: &RuleSet,
    table: &StringTable,
    file_path: &str,
    game: Option<Game>,
) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    let enum_map: HashMap<&str, &EnumDefinition> = ruleset
        .enums
        .iter()
        .map(|e| (e.key.as_str(), e))
        .collect();

    // Create a default scope context if game is provided
    let mut scope_context = game.map(|g| ScopeContext::new(g, ScopeId(100)));

    for child in &ast.root_children {
        let children_to_validate = match child {
            Child::Node(node_idx) => {
                let node = &ast.arena.nodes[*node_idx as usize];
                let key = table.get_string(node.key.normal).unwrap_or_default();
                find_matching_type(&key, ruleset)
                    .map(|t| (key.clone(), t, node.children.as_slice()))
            }
            Child::Leaf(leaf_idx) => {
                let leaf = &ast.arena.leaves[*leaf_idx as usize];
                let key = table.get_string(leaf.key.normal).unwrap_or_default();
                if let Value::Clause(children) = &leaf.value {
                    find_matching_type(&key, ruleset)
                        .map(|t| (key.clone(), t, children.as_slice()))
                } else {
                    None
                }
            }
            _ => None,
        };

        if let Some((_type_key, type_def, children)) = children_to_validate {
            if type_def.subtypes.is_empty() {
                validate_children(children, ast, &[], &enum_map, table, &mut errors, file_path, &mut scope_context);
            } else {
                for subtype in &type_def.subtypes {
                    let should_apply = if let Some(ref filter_key) = subtype.type_key_field {
                        children.iter().any(|c| child_key_matches(c, ast, table, filter_key))
                    } else {
                        true
                    };
                    if should_apply {
                        // Apply subtype push_scope if present
                        let saved = scope_context.as_ref().map(|ctx| ctx.save());
                        if let Some(ref mut ctx) = scope_context {
                            if let Some(ref push_scope) = subtype.push_scope {
                                ctx.change_scope(push_scope);
                            }
                        }
                        validate_children(children, ast, &subtype.rules, &enum_map, table, &mut errors, file_path, &mut scope_context);
                        if let (Some(saved), Some(ref mut ctx)) = (saved, scope_context.as_mut()) {
                            ctx.restore(saved);
                        }
                    }
                }
            }
        }
    }
    // Run game-specific validators if game is provided
    if let Some(g) = game {
        let game_errors = per_game::run_game_validators(ast, ruleset, table, file_path, g);
        errors.extend(game_errors);
    }

    errors
}

fn find_matching_type<'a>(key: &str, ruleset: &'a RuleSet) -> Option<&'a TypeDefinition> {
    ruleset.types.iter().find(|t| t.name == key)
}

fn child_key_matches(child: &Child, ast: &ParsedFile, table: &StringTable, filter_key: &str) -> bool {
    match child {
        Child::Leaf(idx) => {
            let leaf = &ast.arena.leaves[*idx as usize];
            table.get_string(leaf.key.normal).unwrap_or_default() == filter_key
        }
        Child::Node(idx) => {
            let node = &ast.arena.nodes[*idx as usize];
            table.get_string(node.key.normal).unwrap_or_default() == filter_key
        }
        _ => false,
    }
}

fn validate_children(
    children: &[Child],
    ast: &ParsedFile,
    rules: &[(RuleType, Options)],
    enum_map: &HashMap<&str, &EnumDefinition>,
    table: &StringTable,
    errors: &mut Vec<ValidationError>,
    file_path: &str,
    scope_context: &mut Option<ScopeContext>,
) {
    let mut key_counts: HashMap<String, usize> = HashMap::new();
    for child in children {
        match child {
            Child::Leaf(idx) => {
                let leaf = &ast.arena.leaves[*idx as usize];
                let key = table.get_string(leaf.key.normal).unwrap_or_default();
                *key_counts.entry(key).or_insert(0) += 1;
            }
            Child::Node(idx) => {
                let node = &ast.arena.nodes[*idx as usize];
                let key = table.get_string(node.key.normal).unwrap_or_default();
                *key_counts.entry(key).or_insert(0) += 1;
            }
            _ => {}
        }
    }

    for child in children {
        match child {
            Child::Leaf(idx) => {
                let leaf = &ast.arena.leaves[*idx as usize];
                let key = table.get_string(leaf.key.normal).unwrap_or_default();
                let mut matched = false;
                for (rule_type, opts) in rules {
                    if rule_matches_leaf_key(rule_type, &key) {
                        matched = true;
                        // Check required_scopes before validating value
                        if let Some(ctx) = scope_context {
                            if let Some(current) = ctx.current() {
                                let scope_name = format!("{:?}", current); // placeholder
                                if !opts.required_scopes.is_empty() && !opts.required_scopes.iter().any(|s| scope_name.eq_ignore_ascii_case(s)) {
                                    errors.push(ValidationError {
                                        message: format!("Field '{}' requires scope {:?}, but current scope is {:?}", key, opts.required_scopes, current),
                                        severity: ErrorSeverity::Warning,
                                        line: leaf.pos.start.line,
                                        col: leaf.pos.start.col,
                                        file: file_path.to_string(),
                                    });
                                }
                            }
                        }
                        validate_leaf(leaf, rule_type, table, enum_map, errors, file_path);
                    }
                }
                if !matched {
                    errors.push(ValidationError {
                        message: format!("Unexpected field '{}'", key),
                        severity: ErrorSeverity::Error,
                        line: leaf.pos.start.line,
                        col: leaf.pos.start.col,
                        file: file_path.to_string(),
                    });
                }
            }
            Child::Node(idx) => {
                let node = &ast.arena.nodes[*idx as usize];
                let key = table.get_string(node.key.normal).unwrap_or_default();
                let mut matched = false;
                for (rule_type, opts) in rules {
                    if rule_matches_node_key(rule_type, &key) {
                        matched = true;
                        match rule_type {
                            RuleType::NodeRule { rules: inner_rules, .. } => {
                                // Apply push_scope / replace_scopes before recursing
                                let saved = scope_context.as_ref().map(|ctx| ctx.save());
                                if let Some(ctx) = scope_context.as_mut() {
                                    if let Some(ref push) = opts.push_scope {
                                        ctx.change_scope(push);
                                    }
                                    if let Some(ref replace) = opts.replace_scopes {
                                        // Apply replace_scopes: update root, this, from, prev
                                        apply_replace_scopes(ctx, replace);
                                    }
                                }
                                validate_children(&node.children, ast, inner_rules, enum_map, table, errors, file_path, scope_context);
                                // Restore scope after recursing
                                if let (Some(saved), Some(ref mut ctx)) = (saved, scope_context.as_mut()) {
                                    ctx.restore(saved);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                if !matched {
                    errors.push(ValidationError {
                        message: format!("Unexpected block '{}'", key),
                        severity: ErrorSeverity::Error,
                        line: node.pos.start.line,
                        col: node.pos.start.col,
                        file: file_path.to_string(),
                    });
                }
            }
            _ => {}
        }
    }

    for (rule_type, opts) in rules {
        if let Some(key) = get_rule_key(rule_type) {
            let count = key_counts.get(&key).copied().unwrap_or(0) as i32;
            let sev = opts.severity.as_ref().map(|s| severity_to_error(s.clone())).unwrap_or(ErrorSeverity::Error);
            if count < opts.min {
                errors.push(ValidationError {
                    message: format!("Field '{}' appears {} time(s), expected at least {}", key, count, opts.min),
                    severity: sev, line: 0, col: 0, file: file_path.to_string(),
                });
            }
            if count > opts.max {
                errors.push(ValidationError {
                    message: format!("Field '{}' appears {} time(s), expected at most {}", key, count, opts.max),
                    severity: sev, line: 0, col: 0, file: file_path.to_string(),
                });
            }
        }
    }
}

fn apply_replace_scopes(_ctx: &mut ScopeContext, replace: &ReplaceScopes) {
    // TODO: implement proper replace_scopes
    // For now, just note that it was applied (placeholder)
    if let Some(ref root) = replace.root {
        // Would need to map scope name to ScopeId
        let _ = root;
    }
}

fn rule_matches_leaf_key(rule_type: &RuleType, key: &str) -> bool {
    match rule_type {
        RuleType::LeafRule { left, .. } | RuleType::NodeRule { left, .. } => field_matches_key(left, key),
        _ => false,
    }
}

fn rule_matches_node_key(rule_type: &RuleType, key: &str) -> bool {
    match rule_type {
        RuleType::NodeRule { left, .. } | RuleType::LeafRule { left, .. } => field_matches_key(left, key),
        _ => false,
    }
}

fn field_matches_key(field: &NewField, key: &str) -> bool {
    match field {
        NewField::SpecificField(s) => {
            if s.starts_with("alias_name[") && s.ends_with(']') {
                return true; // alias_name[effect] matches any key
            }
            if s.starts_with("alias_match_left[") && s.ends_with(']') {
                return true; // alias_match_left[effect] matches any key
            }
            s == key
        }
        NewField::AliasField(_) => true,
        NewField::ScalarField => true,
        _ => false,
    }
}

fn get_rule_key(rule_type: &RuleType) -> Option<String> {
    match rule_type {
        RuleType::LeafRule { left, .. } | RuleType::NodeRule { left, .. } => field_to_key(left),
        _ => None,
    }
}

fn field_to_key(field: &NewField) -> Option<String> {
    match field {
        NewField::SpecificField(s) => Some(s.clone()),
        _ => None,
    }
}

fn validate_leaf(
    leaf: &cwtools_parser::ast::Leaf,
    rule_type: &RuleType,
    table: &StringTable,
    enum_map: &HashMap<&str, &EnumDefinition>,
    errors: &mut Vec<ValidationError>,
    file_path: &str,
) {
    if let RuleType::LeafRule { right, .. } = rule_type {
        if !field_matches_value(right, &leaf.value, table, enum_map) {
            let expected = field_to_description(right);
            let actual = leaf_value_to_string(&leaf.value, table);
            let key = table.get_string(leaf.key.normal).unwrap_or_default();
            errors.push(ValidationError {
                message: format!("Field '{}' has value '{}', expected {}", key, actual, expected),
                severity: ErrorSeverity::Error,
                line: leaf.pos.start.line, col: leaf.pos.start.col, file: file_path.to_string(),
            });
        }
    }
}

fn field_matches_value(field: &NewField, value: &Value, table: &StringTable, enum_map: &HashMap<&str, &EnumDefinition>) -> bool {
    match (field, value) {
        (NewField::ValueField(ValueType::Bool), Value::Bool(_)) => true,
        (NewField::ValueField(ValueType::Int { min, max }), Value::Int(v)) => { let v_i = *v as i32; v_i >= *min && v_i <= *max }
        (NewField::ValueField(ValueType::Float { min, max }), Value::Float(v)) => { *v >= *min && *v <= *max }
        (NewField::ValueField(ValueType::Enum(enum_name)), Value::String(t))
        | (NewField::ValueField(ValueType::Enum(enum_name)), Value::QString(t)) => {
            let text = table.get_string(t.normal).unwrap_or_default();
            match enum_map.get(enum_name.as_str()) {
                Some(enum_def) => enum_def.values.contains(&text),
                None => true,
            }
        }
        (NewField::ScalarField, _) => true,
        (NewField::SpecificField(s), Value::String(t)) | (NewField::SpecificField(s), Value::QString(t)) => {
            table.get_string(t.normal).unwrap_or_default() == *s
        }
        (NewField::TypeField(TypeType::Simple(type_name)), Value::String(t))
        | (NewField::TypeField(TypeType::Simple(type_name)), Value::QString(t)) => {
            validate_type_reference(&table.get_string(t.normal).unwrap_or_default(), type_name)
        }
        (NewField::TypeField(TypeType::Complex { name, .. }), Value::String(t))
        | (NewField::TypeField(TypeType::Complex { name, .. }), Value::QString(t)) => {
            validate_type_reference(&table.get_string(t.normal).unwrap_or_default(), name)
        }
        (NewField::ScopeField(_), Value::String(t)) | (NewField::ScopeField(_), Value::QString(t)) => {
            let text = table.get_string(t.normal).unwrap_or_default();
            text.starts_with("scope[") || ["root","this","from","prev","capital","random","trigger"].contains(&text.as_str())
        }
        (NewField::VariableField { min, max, .. }, Value::Float(v)) => { *v >= *min && *v <= *max }
        (NewField::LocalisationField { .. }, Value::String(_) | Value::QString(_)) => true,
        (NewField::FilepathField { .. }, Value::String(_) | Value::QString(_)) => true,
        (NewField::ValueField(ValueType::Bool), Value::String(t)) | (NewField::ValueField(ValueType::Bool), Value::QString(t)) => { table.get_string(t.normal).unwrap_or_default() == "yes" || table.get_string(t.normal).unwrap_or_default() == "no" }
        (NewField::ValueField(ValueType::Int { .. }), Value::String(t)) | (NewField::ValueField(ValueType::Int { .. }), Value::QString(t)) => { table.get_string(t.normal).unwrap_or_default().parse::<i32>().is_ok() }
        (NewField::ValueField(ValueType::Float { .. }), Value::String(t)) | (NewField::ValueField(ValueType::Float { .. }), Value::QString(t)) => { table.get_string(t.normal).unwrap_or_default().parse::<f64>().is_ok() }
        _ => false,
    }
}

fn validate_type_reference(text: &str, expected_type: &str) -> bool {
    if text.is_empty() { return false; }
    let clean = text.trim_start_matches('<').trim_end_matches('>');
    if clean.contains('.') {
        let parts: Vec<&str> = clean.split('.').collect();
        if !parts.is_empty() && parts[0] == expected_type { return true; }
    }
    clean == expected_type || text == expected_type
}

fn leaf_value_to_string(value: &Value, table: &StringTable) -> String {
    match value {
        Value::String(t) | Value::QString(t) => table.get_string(t.normal).unwrap_or_default(),
        Value::Float(f) => f.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Clause(_) => "{...}".to_string(),
    }
}

fn field_to_description(field: &NewField) -> String {
    match field {
        NewField::ValueField(vt) => format!("{:?}", vt),
        NewField::ScalarField => "any value".to_string(),
        NewField::SpecificField(s) => format!("'{}'", s),
        NewField::TypeField(tt) => format!("{:?}", tt),
        NewField::ScopeField(scopes) => format!("scope {:?}", scopes),
        NewField::LocalisationField { synced, .. } => format!("localisation (synced={})", synced),
        _ => "unknown field type".to_string(),
    }
}

fn severity_to_error(sev: Severity) -> ErrorSeverity {
    match sev {
        Severity::Error => ErrorSeverity::Error,
        Severity::Warning => ErrorSeverity::Warning,
        Severity::Information => ErrorSeverity::Information,
        Severity::Hint => ErrorSeverity::Hint,
    }
}

pub fn error_hash(error: &ValidationError) -> String {
    let sev_str = match error.severity {
        ErrorSeverity::Error => "error",
        ErrorSeverity::Warning => "warning",
        ErrorSeverity::Information => "information",
        ErrorSeverity::Hint => "hint",
    };
    format!("{}|{}|{}|{}", sev_str, error.file, error.line, error.message)
}
