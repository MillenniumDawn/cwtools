use cwtools_parser::ast::{Child, ParsedFile, Value};
use cwtools_rules::rules_types::*;
use cwtools_string_table::string_table::StringTable;

/// A diagnostic error from validation.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationError {
    pub message: String,
    pub severity: ErrorSeverity,
    pub line: u32,
    pub col: u16,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ErrorSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

/// Validate an AST (ParsedFile) against a set of rules.
/// For every root child that matches a type definition, runs the subtypes' rules.
pub fn validate_ast(
    ast: &ParsedFile,
    ruleset: &RuleSet,
    table: &StringTable,
) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    for child in &ast.root_children {
        let children_to_validate = match child {
            Child::Node(node_idx) => {
                let node = &ast.arena.nodes[*node_idx as usize];
                let key = table.get_string(node.key.normal).unwrap_or_default();
                if ruleset.types.iter().any(|t| t.name == key) {
                    Some((key, &node.children))
                } else {
                    None
                }
            }
            Child::Leaf(leaf_idx) => {
                let leaf = &ast.arena.leaves[*leaf_idx as usize];
                let key = table.get_string(leaf.key.normal).unwrap_or_default();
                if let Value::Clause(children) = &leaf.value {
                    if ruleset.types.iter().any(|t| t.name == key) {
                        Some((key, children))
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            _ => None,
        };

        if let Some((type_key, children)) = children_to_validate {
            let type_def = ruleset.types.iter().find(|t| t.name == type_key).unwrap();
            for subtype in &type_def.subtypes {
                validate_children_against_rules(
                    children, ast, &subtype.rules, ruleset, table, &mut errors,
                );
            }
        }
    }

    errors
}

/// Validate children against a list of rules.
fn validate_children_against_rules(
    children: &[Child],
    ast: &ParsedFile,
    rules: &[(RuleType, Options)],
    _ruleset: &RuleSet,
    table: &StringTable,
    errors: &mut Vec<ValidationError>,
) {
    use std::collections::HashMap;

    // Count occurrences of each key
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

    // Check each child against applicable rules
    for child in children {
        match child {
            Child::Leaf(idx) => {
                let leaf = &ast.arena.leaves[*idx as usize];
                let key = table.get_string(leaf.key.normal).unwrap_or_default();

                let matching_rules: Vec<&(RuleType, Options)> = rules
                    .iter()
                    .filter(|(rt, _)| rule_matches_leaf_key(rt, &key))
                    .collect();

                if matching_rules.is_empty() {
                    errors.push(ValidationError {
                        message: format!("Unexpected field '{}'", key),
                        severity: ErrorSeverity::Error,
                        line: leaf.pos.start.line,
                        col: leaf.pos.start.col,
                    });
                } else {
                    for (rule_type, _opts) in matching_rules {
                        validate_leaf_against_rule(leaf, rule_type, table, errors);
                    }
                }
            }
            Child::Node(idx) => {
                let node = &ast.arena.nodes[*idx as usize];
                let key = table.get_string(node.key.normal).unwrap_or_default();

                let matching_rules: Vec<&(RuleType, Options)> = rules
                    .iter()
                    .filter(|(rt, _)| rule_matches_node_key(rt, &key))
                    .collect();

                if matching_rules.is_empty() {
                    errors.push(ValidationError {
                        message: format!("Unexpected block '{}'", key),
                        severity: ErrorSeverity::Error,
                        line: node.pos.start.line,
                        col: node.pos.start.col,
                    });
                } else {
                    for (rule_type, _opts) in matching_rules {
                        if let RuleType::NodeRule { rules: inner_rules, .. } = rule_type {
                            validate_children_against_rules(
                                &node.children,
                                ast,
                                inner_rules,
                                _ruleset,
                                table,
                                errors,
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Check cardinality (min/max)
    for (rule_type, opts) in rules {
        if let Some(key) = get_rule_key(rule_type) {
            let count = key_counts.get(&key).copied().unwrap_or(0) as i32;
            if count < opts.min {
                errors.push(ValidationError {
                    message: format!(
                        "Field '{}' appears {} time(s), expected at least {}",
                        key, count, opts.min
                    ),
                    severity: ErrorSeverity::Error,
                    line: 0,
                    col: 0,
                });
            }
            if count > opts.max {
                errors.push(ValidationError {
                    message: format!(
                        "Field '{}' appears {} time(s), expected at most {}",
                        key, count, opts.max
                    ),
                    severity: ErrorSeverity::Error,
                    line: 0,
                    col: 0,
                });
            }
        }
    }
}

fn rule_matches_leaf_key(rule_type: &RuleType, key: &str) -> bool {
    match rule_type {
        RuleType::LeafRule { left, .. } | RuleType::NodeRule { left, .. } => {
            field_matches_key(left, key)
        }
        _ => false,
    }
}

fn rule_matches_node_key(rule_type: &RuleType, key: &str) -> bool {
    match rule_type {
        RuleType::NodeRule { left, .. } => field_matches_key(left, key),
        _ => false,
    }
}

fn field_matches_key(field: &NewField, key: &str) -> bool {
    match field {
        NewField::SpecificField(s) => s == key,
        NewField::AliasField(_) => true,
        NewField::ScalarField => true,
        _ => false,
    }
}

fn get_rule_key(rule_type: &RuleType) -> Option<String> {
    match rule_type {
        RuleType::LeafRule { left, .. } => field_to_key(left),
        RuleType::NodeRule { left, .. } => field_to_key(left),
        _ => None,
    }
}

fn field_to_key(field: &NewField) -> Option<String> {
    match field {
        NewField::SpecificField(s) => Some(s.clone()),
        _ => None,
    }
}

fn validate_leaf_against_rule(
    leaf: &cwtools_parser::ast::Leaf,
    rule_type: &RuleType,
    table: &StringTable,
    errors: &mut Vec<ValidationError>,
) {
    if let RuleType::LeafRule { right, .. } = rule_type {
        if !field_matches_value(right, &leaf.value, table) {
            let expected = field_to_description(right);
            let actual = leaf_value_to_string(&leaf.value, table);
            let key = table.get_string(leaf.key.normal).unwrap_or_default();
            errors.push(ValidationError {
                message: format!(
                    "Field '{}' has value '{}', expected {}",
                    key, actual, expected
                ),
                severity: ErrorSeverity::Error,
                line: leaf.pos.start.line,
                col: leaf.pos.start.col,
            });
        }
    }
}

fn field_matches_value(field: &NewField, value: &Value, table: &StringTable) -> bool {
    match (field, value) {
        (NewField::ValueField(ValueType::Bool), Value::Bool(_)) => true,
        (NewField::ValueField(ValueType::Int { min, max }), Value::Int(v)) => {
            let v_i = *v as i32;
            v_i >= *min && v_i <= *max
        }
        (NewField::ValueField(ValueType::Float { min, max }), Value::Float(v)) => {
            *v >= *min && *v <= *max
        }
        (NewField::ValueField(ValueType::Enum(_)), Value::String(_))
        | (NewField::ValueField(ValueType::Enum(_)), Value::QString(_)) => {
            // Enum values always accepted (definitions loaded separately)
            true
        }
        (NewField::ScalarField, _) => true,
        (NewField::SpecificField(s), Value::String(t))
        | (NewField::SpecificField(s), Value::QString(t)) => {
            let text = table.get_string(t.normal).unwrap_or_default();
            &text == s
        }
        (NewField::TypeField(_), _) => true,
        (NewField::ScopeField(_), Value::String(t))
        | (NewField::ScopeField(_), Value::QString(t)) => {
            let text = table.get_string(t.normal).unwrap_or_default();
            text.starts_with("scope[")
                || ["root", "this", "from", "prev"].contains(&text.as_str())
        }
        (NewField::VariableField { min, max, .. }, Value::Float(v)) => {
            *v >= *min && *v <= *max
        }
        (NewField::LocalisationField { .. }, Value::String(_) | Value::QString(_)) => true,
        (NewField::FilepathField { .. }, Value::String(_) | Value::QString(_)) => true,
        (NewField::ValueField(ValueType::Bool), Value::String(t))
        | (NewField::ValueField(ValueType::Bool), Value::QString(t)) => {
            // Allow "yes"/"no" strings as bools (Paradox script convention)
            let text = table.get_string(t.normal).unwrap_or_default();
            text == "yes" || text == "no"
        }
        (NewField::ValueField(ValueType::Int { .. }), Value::String(t))
        | (NewField::ValueField(ValueType::Int { .. }), Value::QString(t)) => {
            // Reject: string is not an int
            let text = table.get_string(t.normal).unwrap_or_default();
            text.parse::<i32>().is_ok()
        }
        (NewField::ValueField(ValueType::Float { .. }), Value::String(t))
        | (NewField::ValueField(ValueType::Float { .. }), Value::QString(t)) => {
            // Reject: string is not a float
            let text = table.get_string(t.normal).unwrap_or_default();
            text.parse::<f64>().is_ok()
        }
        _ => false,
    }
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
        NewField::LocalisationField { synced, .. } => {
            format!("localisation (synced={})", synced)
        },
        _ => "unknown field type".to_string(),
    }
}
