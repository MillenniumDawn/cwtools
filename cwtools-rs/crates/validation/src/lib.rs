use cwtools_game::scope_engine::{ScopeContext, ScopeId, ScopeResult};
use cwtools_game::constants::Game;
use cwtools_game::scope::{Scope};
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

/// Iterate grandchildren of a skip_root_key wrapper and validate each one uniformly.
/// Both the Node-root and Leaf-root shapes delegate here so behaviour is identical.
#[allow(clippy::too_many_arguments)]
fn validate_wrapper_grandchildren(
    grandchildren: &[Child],
    type_def: &TypeDefinition,
    ast: &ParsedFile,
    inner_rules: &[(RuleType, Options)],
    enum_map: &HashMap<&str, &EnumDefinition>,
    table: &StringTable,
    errors: &mut Vec<ValidationError>,
    file_path: &str,
    scope_context: &mut Option<ScopeContext>,
    game: Option<Game>,
    ruleset: &RuleSet,
) {
    for grandchild in grandchildren {
        match grandchild {
            Child::Node(gc_idx) => {
                let gc_node = &ast.arena.nodes[*gc_idx as usize];
                validate_with_type(type_def, gc_node.children.as_slice(), ast, inner_rules, enum_map, table, errors, file_path, scope_context, game, ruleset);
            }
            Child::Leaf(gc_idx) => {
                let gc_leaf = &ast.arena.leaves[*gc_idx as usize];
                if let Value::Clause(gc_children) = &gc_leaf.value {
                    validate_with_type(type_def, gc_children.as_slice(), ast, inner_rules, enum_map, table, errors, file_path, scope_context, game, ruleset);
                }
                // Non-clause scalar leaf inside wrapper: leave as-is (no error)
            }
            Child::LeafValue(idx) => {
                let lv = &ast.arena.leaf_values[*idx as usize];
                let value = leaf_value_to_string(&lv.value, table);
                errors.push(ValidationError {
                    message: format!("Unexpected bare value '{}'", value),
                    severity: ErrorSeverity::Warning,
                    line: lv.pos.start.line,
                    col: lv.pos.start.col,
                    file: file_path.to_string(),
                });
            }
            _ => {}
        }
    }
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

    let mut scope_context = game.map(|g| ScopeContext::new(g, ScopeId(100)));

    // Pre-compute path-based type match (most specific wins)
    let path_type = find_type_by_path(file_path, ruleset);

    for child in &ast.root_children {
        // 1. Try exact root key match (e.g. ai_strategy_plan = { ... })
        let exact_match = match child {
            Child::Node(node_idx) => {
                let node = &ast.arena.nodes[*node_idx as usize];
                let key = table.get_string(node.key.normal).unwrap_or_default();
                find_type_and_rules(&key, ruleset)
                    .map(|(td, rules)| (key.clone(), td, node.children.as_slice(), rules))
            }
            Child::Leaf(leaf_idx) => {
                let leaf = &ast.arena.leaves[*leaf_idx as usize];
                let key = table.get_string(leaf.key.normal).unwrap_or_default();
                if let Value::Clause(children) = &leaf.value {
                    find_type_and_rules(&key, ruleset)
                        .map(|(td, rules)| (key.clone(), td, children.as_slice(), rules))
                } else {
                    None
                }
            }
            _ => None,
        };

        if let Some((type_key, type_def, children, inner_rules)) = exact_match {
                validate_with_type(type_def, children, ast, inner_rules, &enum_map, table, &mut errors, file_path, &mut scope_context, game, ruleset);
            continue;
        }

        // 2. Fallback: path-based matching
        if let Some(type_def) = path_type {
            let inner_rules = find_rules_by_name(&type_def.name, ruleset);

            // Determine if the root node should be treated as a wrapper.
            // Official CWT rules use skip_root_key = any for this, but many
            // upstream configs lack it.  Heuristic: if the root key matches
            // a subtype name of the path-matched type, treat the root as a
            // wrapper and validate its children directly.
            let root_key = match child {
                Child::Node(node_idx) => table.get_string(ast.arena.nodes[*node_idx as usize].key.normal).unwrap_or_default(),
                Child::Leaf(leaf_idx) => table.get_string(ast.arena.leaves[*leaf_idx as usize].key.normal).unwrap_or_default(),
                _ => String::new(),
            };
            let subtype_wrapper = !root_key.is_empty() && type_def.subtypes.iter().any(|st| st.name == root_key);

            // If skip_root_key = any (or heuristic matches), the root node is a WRAPPER — validate its children individually
            if should_skip_root_key(&root_key, type_def) || subtype_wrapper {
                let grandchildren: &[Child] = match child {
                    Child::Node(node_idx) => {
                        &ast.arena.nodes[*node_idx as usize].children
                    }
                    Child::Leaf(leaf_idx) => {
                        let leaf = &ast.arena.leaves[*leaf_idx as usize];
                        if let Value::Clause(ref ch) = leaf.value {
                            ch.as_slice()
                        } else {
                            &[]
                        }
                    }
                    _ => &[],
                };
                validate_wrapper_grandchildren(grandchildren, type_def, ast, inner_rules, &enum_map, table, &mut errors, file_path, &mut scope_context, game, ruleset);
                continue;
            }

            // No skip_root_key — validate the root node itself normally
            match child {
                Child::Node(node_idx) => {
                    let node = &ast.arena.nodes[*node_idx as usize];
                    validate_with_type(type_def, node.children.as_slice(), ast, inner_rules, &enum_map, table, &mut errors, file_path, &mut scope_context, game, ruleset);
                }
                Child::Leaf(leaf_idx) => {
                    let leaf = &ast.arena.leaves[*leaf_idx as usize];
                    if let Value::Clause(children) = &leaf.value {
                        validate_with_type(type_def, children.as_slice(), ast, inner_rules, &enum_map, table, &mut errors, file_path, &mut scope_context, game, ruleset);
                    }
                }
                _ => {}
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

/// Validate a set of children against a type's rules, handling subtypes.
fn validate_with_type(
    type_def: &TypeDefinition,
    children: &[Child],
    ast: &ParsedFile,
    inner_rules: &[(RuleType, Options)],
    enum_map: &HashMap<&str, &EnumDefinition>,
    table: &StringTable,
    errors: &mut Vec<ValidationError>,
    file_path: &str,
    scope_context: &mut Option<ScopeContext>,
    game: Option<Game>,
    ruleset: &RuleSet,
) {
    if type_def.subtypes.is_empty() {
        validate_children(children, ast, inner_rules, enum_map, table, errors, file_path, scope_context, game, ruleset);
    } else {
        for subtype in &type_def.subtypes {
            let should_apply = if let Some(ref filter_key) = subtype.type_key_field {
                children.iter().any(|c| child_key_matches(c, ast, table, filter_key))
            } else {
                true
            };
            if should_apply {
                // Look up the actual validation rules for this subtype from the root rule.
                // The SubtypeRule in inner_rules carries the real parsed rules, whereas
                // the type_def's SubTypeDefinition only stores metadata.
                let subtype_rules = find_subtype_rules(subtype.name.as_str(), inner_rules)
                    .unwrap_or(&subtype.rules);
                
                let saved = scope_context.as_ref().map(|ctx| ctx.save());
                if let Some(ctx) = scope_context.as_mut() {
                    if let Some(ref push_scope) = subtype.push_scope {
                        ctx.change_scope(push_scope);
                    }
                }
                validate_children(children, ast, subtype_rules, enum_map, table, errors, file_path, scope_context, game, ruleset);
                if let (Some(saved), Some(ctx)) = (saved, scope_context.as_mut()) {
                    ctx.restore(saved);
                }
            }
        }
    }
}

/// Look up the validation rules for a named subtype from a set of inner rules.
/// Returns the rules slice from the matching SubtypeRule, or None if not found.
fn find_subtype_rules<'a>(name: &str, inner_rules: &'a [(RuleType, Options)]) -> Option<&'a [(RuleType, Options)]> {
    for (rule_type, _opts) in inner_rules {
        if let RuleType::SubtypeRule { name: rule_name, rules, .. } = rule_type {
            if rule_name == name {
                return Some(rules.as_slice());
            }
        }
    }
    None
}

/// Check if this type says its root key should be skipped (children are the real entries).
fn should_skip_root_key(_key: &str, type_def: &TypeDefinition) -> bool {
    type_def.skip_root_key.iter().any(|sk| match sk {
        SkipRootKey::AnyKey => true,
        SkipRootKey::SpecificKey(v) => v == _key,
        SkipRootKey::MultipleKeys(keys, _) => keys.iter().any(|k| k == _key),
    })
}

/// Look up both the TypeDefinition and the actual validation rules for a given type name.
fn find_type_and_rules<'a>(name: &str, ruleset: &'a RuleSet) -> Option<(&'a TypeDefinition, &'a [(RuleType, Options)])> {
    let type_def = ruleset.types.iter().find(|t| t.name == name)?;
    let rules = find_rules_by_name(name, ruleset);
    Some((type_def, rules))
}

/// Map a ScopeId to a human-readable name for validation purposes.
fn get_scope_name(scope: ScopeId, game: Game) -> String {
    for def in game.scope_defs() {
        if def.id.0 == scope.0 {
            return def.aliases.first().unwrap_or(&def.name).to_string();
        }
    }
    format!("scope_{}", scope.0)
}

fn scope_matches_required(current: ScopeId, game: Game, required: &[String]) -> bool {
    let name = get_scope_name(current, game);
    required.iter().any(|s| s.eq_ignore_ascii_case(&name))
}

/// Find the actual validation rules for a type by looking in root_rules.
fn find_rules_by_name<'a>(name: &str, ruleset: &'a RuleSet) -> &'a [(RuleType, Options)] {
    for rr in &ruleset.root_rules {
        if let RootRule::TypeRule(rule_name, (rule, _opts)) = rr {
            if rule_name == name {
                if let RuleType::NodeRule { rules, .. } = rule {
                    return rules.as_slice();
                }
            }
        }
    }
    &[]
}

/// Returns true only when `needle` appears in `haystack` as a whole sequence of
/// path segments (bounded by '/' or start/end on both sides). Both inputs must
/// already be lowercased and use '/' separators (clean_path normalizes these).
/// This prevents `events` from matching `.../my_events_backup/x.txt`.
fn path_contains_segment(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs = start + pos;
        let left_ok = abs == 0 || haystack.as_bytes().get(abs - 1) == Some(&b'/');
        let right = abs + needle.len();
        let right_ok = right == haystack.len() || haystack.as_bytes().get(right) == Some(&b'/');
        if left_ok && right_ok {
            return true;
        }
        start = abs + 1;
        if start >= haystack.len() {
            break;
        }
    }
    false
}

/// Find a type whose path_options match the given file path.
/// Returns the MOST SPECIFIC match (longest path string) so that
/// `common/ai_strategy_plans` wins over generic `common`.
fn find_type_by_path<'a>(file_path: &str, ruleset: &'a RuleSet) -> Option<&'a TypeDefinition> {
    let path_lower = file_path.to_lowercase();
    let mut best: Option<&TypeDefinition> = None;
    let mut best_len = 0usize;

    for t in &ruleset.types {
        for p in &t.path_options.paths {
            let p_lower = p.to_lowercase();
            if path_contains_segment(&path_lower, &p_lower) && p_lower.len() > best_len {
                best = Some(t);
                best_len = p_lower.len();
            }
        }
    }
    best
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
    game: Option<Game>,
    ruleset: &RuleSet,
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
                    if rule_matches_leaf_key(rule_type, &key, ruleset) {
                        matched = true;
                        // Check required_scopes before validating value
                        if let Some(ctx) = scope_context {
                            if let Some(current) = ctx.current() {
                                if let Some(g) = game {
                                    if !opts.required_scopes.is_empty() && !scope_matches_required(current, g, &opts.required_scopes) {
                                        errors.push(ValidationError {
                                            message: format!("Field '{}' requires scope {:?}, but current scope is {:?}", key, opts.required_scopes, get_scope_name(current, g)),
                                            severity: ErrorSeverity::Warning,
                                            line: leaf.pos.start.line,
                                            col: leaf.pos.start.col,
                                            file: file_path.to_string(),
                                        });
                                    }
                                }
                            }
                        }
                        match rule_type {
                            RuleType::LeafRule { .. } => {
                                validate_leaf(leaf, rule_type, table, enum_map, errors, file_path);
                            }
                            RuleType::NodeRule { rules: inner_rules, .. } => {
                                // A Leaf with a Clause value is effectively a Node — recurse into its children
                                if let Value::Clause(children) = &leaf.value {
                                    let saved = scope_context.as_ref().map(|ctx| ctx.save());
                                    if let Some(ctx) = scope_context.as_mut() {
                                        if let Some(ref push) = opts.push_scope {
                                            ctx.change_scope(push);
                                        }
                                        if let Some(ref replace) = opts.replace_scopes {
                                            apply_replace_scopes(ctx, replace);
                                        }
                                    }
        validate_children(children, ast, inner_rules, enum_map, table, errors, file_path, scope_context, game, ruleset);
                                    if let (Some(saved), Some(ref mut ctx)) = (saved, scope_context.as_mut()) {
                                        ctx.restore(saved);
                                    }
                                }
                            }
                            _ => {}
                        }
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
                    if rule_matches_node_key(rule_type, &key, ruleset) {
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
                                validate_children(
                                    &node.children, ast, inner_rules, enum_map, table, errors,
                                    file_path, scope_context, game, ruleset,
                                );
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

fn rule_matches_leaf_key(rule_type: &RuleType, key: &str, ruleset: &RuleSet) -> bool {
    match rule_type {
        // Cross-kind fallback: a NodeRule can also match a leaf key (e.g. alias blocks)
        RuleType::LeafRule { left, .. } | RuleType::NodeRule { left, .. } => field_matches_key(left, key, ruleset),
        _ => false,
    }
}

fn rule_matches_node_key(rule_type: &RuleType, key: &str, ruleset: &RuleSet) -> bool {
    match rule_type {
        // Cross-kind fallback: a LeafRule can also match a node key
        RuleType::NodeRule { left, .. } | RuleType::LeafRule { left, .. } => field_matches_key(left, key, ruleset),
        _ => false,
    }
}

fn field_matches_key(field: &NewField, key: &str, ruleset: &RuleSet) -> bool {
    match field {
        NewField::SpecificField(s) => s == key,
        NewField::AliasField(category) => {
            // Check if this key matches any alias in the given category.
            // Aliases are stored as "category:name" in ruleset.aliases.
            let prefix = format!("{}:", category);
            let has_any = ruleset.aliases.iter().any(|(name, _)| name.starts_with(&prefix));
            if !has_any {
                // Category entirely unloaded — be permissive to avoid false-positive floods.
                return true;
            }
            ruleset.aliases.iter().any(|(name, _)| {
                name.starts_with(&prefix) && name.split(':').nth(1) == Some(key)
            })
        }
        NewField::SingleAliasField(alias_name) => {
            // SingleAliasField matches if the key is exactly this alias name.
            alias_name == key
        }
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
                // Unknown enums (complex enums, unloaded enums) can't be statically validated here.
                // Accept them to avoid false positives.
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
        (NewField::VariableField { min, max, .. }, Value::Int(v)) => { (*v as f64) >= *min && (*v as f64) <= *max }
        (NewField::VariableField { min, max, .. }, Value::String(t)) | (NewField::VariableField { min, max, .. }, Value::QString(t)) => {
            let text = table.get_string(t.normal).unwrap_or_default();
            if let Ok(v) = text.parse::<f64>() {
                v >= *min && v <= *max
            } else {
                false
            }
        }
        (NewField::LocalisationField { .. }, Value::String(_) | Value::QString(_)) => true,
        (NewField::FilepathField { .. }, Value::String(_) | Value::QString(_)) => true,
        (NewField::ValueField(ValueType::Bool), Value::String(t)) | (NewField::ValueField(ValueType::Bool), Value::QString(t)) => { let v = table.get_string(t.normal).unwrap_or_default().to_lowercase(); v == "yes" || v == "no" }
        (NewField::ValueField(ValueType::Int { .. }), Value::String(t)) | (NewField::ValueField(ValueType::Int { .. }), Value::QString(t)) => { table.get_string(t.normal).unwrap_or_default().parse::<i32>().is_ok() }
        (NewField::ValueField(ValueType::Float { .. }), Value::String(t)) | (NewField::ValueField(ValueType::Float { .. }), Value::QString(t)) => { table.get_string(t.normal).unwrap_or_default().parse::<f64>().is_ok() }
        // AliasField and SingleAliasField on the right side accept clauses and strings
        // TODO: implement deep validation against the alias / single_alias definition
        (NewField::AliasField(_), Value::Clause(_)) => true,
        (NewField::AliasField(_), Value::String(_) | Value::QString(_)) => true,
        (NewField::SingleAliasField(_), Value::Clause(_)) => true,
        (NewField::SingleAliasField(_), Value::String(_) | Value::QString(_)) => true,
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
