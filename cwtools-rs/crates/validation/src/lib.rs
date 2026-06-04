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

        if let Some((_type_key, type_def, children, inner_rules)) = exact_match {
            // Only content-validate when the matched type actually has rules; a
            // type[x] declared solely for instance indexing (path/name_field, no
            // rule body) must not flag its instance fields as unexpected.
            let has_content_rules = !inner_rules.is_empty()
                || type_def.subtypes.iter().any(|st| !st.rules.is_empty());
            if has_content_rules {
                validate_with_type(type_def, children, ast, inner_rules, &enum_map, table, &mut errors, file_path, &mut scope_context, game, ruleset);
                continue;
            }
            // matched by name but instance-only: fall through to path matching
        }

        // 2. Fallback: path-based matching
        if let Some(type_def) = path_type {
            let inner_rules = find_rules_by_name(&type_def.name, ruleset);

            // A `type[x] = { path = ... name_field = ... }` with no associated rule
            // body exists only to index instances of that type; its instances are
            // not content-validated (matching F#). Skip when there is nothing to
            // validate against, otherwise every field reads as "unexpected".
            let has_content_rules = !inner_rules.is_empty()
                || type_def.subtypes.iter().any(|st| !st.rules.is_empty());
            if !has_content_rules {
                continue;
            }

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
///
/// Follows F# memoizeRules logic: collect the base rules (non-SubtypeRule entries from
/// inner_rules) plus the rules of every matching subtype into a single merged list, then
/// validate the children once against that union.  This means:
///   - cardinality is counted over the merged rule set, not per-subtype in isolation
///   - a field that exists in any matching subtype is not "unexpected"
///   - SubtypeRule entries that don't match are silently skipped
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
        let pre_count = errors.len();
        validate_children(children, ast, inner_rules, enum_map, table, errors, file_path, scope_context, game, ruleset);
        // Item 9: warning_only
        if type_def.warning_only {
            for err in errors[pre_count..].iter_mut() {
                if err.severity == ErrorSeverity::Error {
                    err.severity = ErrorSeverity::Warning;
                }
            }
        }
        return;
    }

    // Step 1: determine which subtypes match (F# testSubtype logic).
    // A subtype matches when:
    //   (a) type_key_field is None, OR the children contain a field whose key equals type_key_field
    //   (b) starts_with is None, OR (no-op here; starts_with filters by the node's OWN key which
    //       we don't have at this point — conservative: treat as matching)
    // Mutual-exclusion via only_if_not is applied after the initial pass.
    let mut matched_subtype_names: Vec<&str> = Vec::new();
    for subtype in &type_def.subtypes {
        let key_ok = subtype.type_key_field.as_ref()
            .map(|fk| children.iter().any(|c| child_key_matches(c, ast, table, fk)))
            .unwrap_or(true);
        if key_ok {
            matched_subtype_names.push(subtype.name.as_str());
        }
    }
    // Apply only_if_not: remove a subtype if any of its only_if_not names are in the matched set.
    let all_names_copy: Vec<&str> = matched_subtype_names.clone();
    matched_subtype_names.retain(|name| {
        let st = type_def.subtypes.iter().find(|s| s.name == *name).unwrap();
        !st.only_if_not.iter().any(|excl| all_names_copy.contains(&excl.as_str()))
    });

    // Step 2: collect base rules (non-SubtypeRule entries) + matching SubtypeRule entries.
    // This mirrors F# memoizeRules which expands SubtypeRule(key, shouldMatch, cfs) based on
    // whether key is in the active subtypes list.
    //
    // Two sources of rules:
    //   (A) inner_rules — from a separate `type_name = { ... }` TypeRule in the ruleset.
    //       SubtypeRule entries inside it are expanded per the active subtype set.
    //   (B) type_def.subtypes[i].rules — rules stored directly on SubTypeDefinition.
    //       These are populated when the type is defined ONLY via `types = { type[x] = { subtype[y] = { ... } } }`
    //       with no separate `x = { subtype[y] = { ... } }` rule block.
    //
    // If inner_rules has SubtypeRule entries, use path (A).  Otherwise fall back to (B).
    let inner_has_subtype_rules = inner_rules.iter().any(|(rt, _)| matches!(rt, RuleType::SubtypeRule { .. }));

    let mut merged: Vec<(RuleType, Options)> = Vec::new();
    if inner_has_subtype_rules {
        // Path A: expand SubtypeRule entries from inner_rules
        for (rule_type, opts) in inner_rules {
            match rule_type {
                RuleType::SubtypeRule { name, positive, rules: st_rules } => {
                    let is_active = matched_subtype_names.contains(&name.as_str());
                    let should_include = if *positive { is_active } else { !is_active };
                    if should_include {
                        merged.extend(st_rules.iter().cloned());
                    }
                }
                _ => {
                    merged.push((rule_type.clone(), opts.clone()));
                }
            }
        }
    } else {
        // Path B: pull rules directly from the matching SubTypeDefinition entries.
        // Base (non-subtype) rules come from inner_rules as-is.
        merged.extend(inner_rules.iter().cloned());
        for subtype in &type_def.subtypes {
            if matched_subtype_names.contains(&subtype.name.as_str()) {
                merged.extend(subtype.rules.iter().cloned());
            }
        }
    }

    // Step 3: if no subtypes matched and there are no base rules, there's nothing to validate.
    // This handles the case where a type is defined purely via subtypes: a script object that
    // doesn't match any subtype discriminator is silently accepted.
    if matched_subtype_names.is_empty() && merged.is_empty() {
        return;
    }

    // Step 4: pick push_scope from the first matching subtype that has one.
    let push_scope: Option<&str> = type_def.subtypes.iter()
        .filter(|s| matched_subtype_names.contains(&s.name.as_str()))
        .find_map(|s| s.push_scope.as_deref());

    let saved = scope_context.as_ref().map(|ctx| ctx.save());
    if let (Some(ps), Some(ctx)) = (push_scope, scope_context.as_mut()) {
        ctx.change_scope(ps);
    }

    // Step 5: validate children once against the merged rule set.
    let pre_count = errors.len();
    validate_children(children, ast, &merged, enum_map, table, errors, file_path, scope_context, game, ruleset);

    // Item 9: warning_only — downgrade all newly-added errors to warnings (F# RuleValidationService.fs:916).
    if type_def.warning_only {
        for err in errors[pre_count..].iter_mut() {
            if err.severity == ErrorSeverity::Error {
                err.severity = ErrorSeverity::Warning;
            }
        }
    }

    if let (Some(saved), Some(ctx)) = (saved, scope_context.as_mut()) {
        ctx.restore(saved);
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
    // Track occurrence counts for cardinality checking.
    // Keyed children (Leaf/Node): key string -> count.
    let mut key_counts: HashMap<String, usize> = HashMap::new();
    // Item 5: LeafValues — count per LeafValueRule index.
    let mut leafvalue_counts: Vec<usize> = vec![0usize; rules.len()];
    // Item 5: ValueClause — count per ValueClauseRule index.
    let mut valueclause_counts: Vec<usize> = vec![0usize; rules.len()];

    // First pass: count occurrences of all children kinds.
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
            Child::LeafValue(lvidx) => {
                let lv = &ast.arena.leaf_values[*lvidx as usize];
                for (rule_idx, (rule_type, _)) in rules.iter().enumerate() {
                    if let RuleType::LeafValueRule { right } = rule_type {
                        if field_matches_value(right, &lv.value, table, enum_map) {
                            leafvalue_counts[rule_idx] += 1;
                            break;
                        }
                    }
                }
            }
            Child::ValueClause(_) => {
                for (rule_idx, (rule_type, _)) in rules.iter().enumerate() {
                    if matches!(rule_type, RuleType::ValueClauseRule { .. }) {
                        valueclause_counts[rule_idx] += 1;
                        break;
                    }
                }
            }
            _ => {}
        }
    }

    // Second pass: validate each child.
    for child in children {
        match child {
            Child::Leaf(idx) => {
                let leaf = &ast.arena.leaves[*idx as usize];
                let key = table.get_string(leaf.key.normal).unwrap_or_default();
                let mut matched = false;
                for (rule_type, opts) in rules {
                    if rule_matches_leaf_key(rule_type, &key, ruleset) {
                        matched = true;
                        // Item 8: required_scopes check for Leaf
                        if let Some(current) = scope_context.as_ref().and_then(|ctx| ctx.current()) {
                            if let Some(g) = game {
                                if !opts.required_scopes.is_empty() && !scope_matches_required(current, g, &opts.required_scopes) {
                                    errors.push(ValidationError {
                                        message: format!(
                                            "Field '{}' requires scope {:?}, but current scope is '{}'",
                                            key, opts.required_scopes, get_scope_name(current, g)
                                        ),
                                        severity: ErrorSeverity::Warning,
                                        line: leaf.pos.start.line,
                                        col: leaf.pos.start.col,
                                        file: file_path.to_string(),
                                    });
                                }
                            }
                        }
                        match rule_type {
                            RuleType::LeafRule { left, .. } => {
                                // Item 6: alias expansion — when the rule is AliasField(cat), look up
                                // the specific `cat:key` alias and validate against its right side.
                                if let NewField::AliasField(category) = left {
                                    let alias_key = format!("{}:{}", category, key);
                                    if let Some((_, alias_rule)) = ruleset.aliases.iter().find(|(n, _)| n == &alias_key) {
                                        match alias_rule {
                                            (RuleType::LeafRule { .. }, _) => {
                                                validate_leaf(leaf, &alias_rule.0, table, enum_map, errors, file_path);
                                            }
                                            (RuleType::NodeRule { rules: alias_inner, .. }, alias_opts) => {
                                                if let Value::Clause(clause_children) = &leaf.value {
                                                    let saved = scope_context.as_ref().map(|ctx| ctx.save());
                                                    if let Some(ctx) = scope_context.as_mut() {
                                                        if let Some(ref push) = alias_opts.push_scope {
                                                            ctx.change_scope(push);
                                                        }
                                                        if let Some(ref replace) = alias_opts.replace_scopes {
                                                            apply_replace_scopes(ctx, replace, game);
                                                        }
                                                    }
                                                    validate_children(clause_children, ast, alias_inner, enum_map, table, errors, file_path, scope_context, game, ruleset);
                                                    if let (Some(saved), Some(ref mut ctx)) = (saved, scope_context.as_mut()) {
                                                        ctx.restore(saved);
                                                    }
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                    // If alias category is unloaded (empty), accept silently.
                                } else {
                                    validate_leaf(leaf, rule_type, table, enum_map, errors, file_path);
                                }
                            }
                            RuleType::NodeRule { left, rules: inner_rules, .. } => {
                                // Item 6: alias expansion for NodeRule with AliasField left
                                if let NewField::AliasField(category) = left {
                                    let alias_key = format!("{}:{}", category, key);
                                    if let Some((_, alias_rule)) = ruleset.aliases.iter().find(|(n, _)| n == &alias_key) {
                                        if let (RuleType::NodeRule { rules: alias_inner, .. }, alias_opts) = alias_rule {
                                            if let Value::Clause(clause_children) = &leaf.value {
                                                let saved = scope_context.as_ref().map(|ctx| ctx.save());
                                                if let Some(ctx) = scope_context.as_mut() {
                                                    if let Some(ref push) = alias_opts.push_scope {
                                                        ctx.change_scope(push);
                                                    }
                                                    if let Some(ref replace) = alias_opts.replace_scopes {
                                                        apply_replace_scopes(ctx, replace, game);
                                                    }
                                                }
                                                validate_children(clause_children, ast, alias_inner, enum_map, table, errors, file_path, scope_context, game, ruleset);
                                                if let (Some(saved), Some(ref mut ctx)) = (saved, scope_context.as_mut()) {
                                                    ctx.restore(saved);
                                                }
                                            }
                                        }
                                    }
                                } else {
                                    // A Leaf with a Clause value is effectively a Node — recurse into its children.
                                    if let Value::Clause(clause_children) = &leaf.value {
                                        let saved = scope_context.as_ref().map(|ctx| ctx.save());
                                        if let Some(ctx) = scope_context.as_mut() {
                                            if let Some(ref push) = opts.push_scope {
                                                ctx.change_scope(push);
                                            }
                                            if let Some(ref replace) = opts.replace_scopes {
                                                apply_replace_scopes(ctx, replace, game);
                                            }
                                        }
                                        validate_children(clause_children, ast, inner_rules, enum_map, table, errors, file_path, scope_context, game, ruleset);
                                        if let (Some(saved), Some(ref mut ctx)) = (saved, scope_context.as_mut()) {
                                            ctx.restore(saved);
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                        break; // first matching rule wins
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
                        // Item 8: required_scopes check for Node
                        if let Some(current) = scope_context.as_ref().and_then(|ctx| ctx.current()) {
                            if let Some(g) = game {
                                if !opts.required_scopes.is_empty() && !scope_matches_required(current, g, &opts.required_scopes) {
                                    errors.push(ValidationError {
                                        message: format!(
                                            "Block '{}' requires scope {:?}, but current scope is '{}'",
                                            key, opts.required_scopes, get_scope_name(current, g)
                                        ),
                                        severity: ErrorSeverity::Warning,
                                        line: node.pos.start.line,
                                        col: node.pos.start.col,
                                        file: file_path.to_string(),
                                    });
                                }
                            }
                        }
                        match rule_type {
                            // Item 6: alias expansion for Node children
                            RuleType::NodeRule { left, rules: inner_rules, .. } => {
                                if let NewField::AliasField(category) = left {
                                    let alias_key = format!("{}:{}", category, key);
                                    if let Some((_, alias_rule)) = ruleset.aliases.iter().find(|(n, _)| n == &alias_key) {
                                        if let (RuleType::NodeRule { rules: alias_inner, .. }, alias_opts) = alias_rule {
                                            let saved = scope_context.as_ref().map(|ctx| ctx.save());
                                            if let Some(ctx) = scope_context.as_mut() {
                                                if let Some(ref push) = alias_opts.push_scope {
                                                    ctx.change_scope(push);
                                                }
                                                if let Some(ref replace) = alias_opts.replace_scopes {
                                                    apply_replace_scopes(ctx, replace, game);
                                                }
                                            }
                                            validate_children(&node.children, ast, alias_inner, enum_map, table, errors, file_path, scope_context, game, ruleset);
                                            if let (Some(saved), Some(ref mut ctx)) = (saved, scope_context.as_mut()) {
                                                ctx.restore(saved);
                                            }
                                        }
                                    }
                                    // If the alias is unloaded, accept silently.
                                } else {
                                    let saved = scope_context.as_ref().map(|ctx| ctx.save());
                                    if let Some(ctx) = scope_context.as_mut() {
                                        if let Some(ref push) = opts.push_scope {
                                            ctx.change_scope(push);
                                        }
                                        if let Some(ref replace) = opts.replace_scopes {
                                            apply_replace_scopes(ctx, replace, game);
                                        }
                                    }
                                    validate_children(
                                        &node.children, ast, inner_rules, enum_map, table, errors,
                                        file_path, scope_context, game, ruleset,
                                    );
                                    if let (Some(saved), Some(ref mut ctx)) = (saved, scope_context.as_mut()) {
                                        ctx.restore(saved);
                                    }
                                }
                            }
                            _ => {}
                        }
                        break; // first matching rule wins
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
            // Item 5: LeafValue validation
            Child::LeafValue(lvidx) => {
                let lv = &ast.arena.leaf_values[*lvidx as usize];
                let mut matched = false;
                for (rule_type, _opts) in rules {
                    if let RuleType::LeafValueRule { right } = rule_type {
                        if field_matches_value(right, &lv.value, table, enum_map) {
                            matched = true;
                            break;
                        }
                    }
                }
                if !matched {
                    let val_str = leaf_value_to_string(&lv.value, table);
                    errors.push(ValidationError {
                        message: format!("Unexpected bare value '{}'", val_str),
                        severity: ErrorSeverity::Warning,
                        line: lv.pos.start.line,
                        col: lv.pos.start.col,
                        file: file_path.to_string(),
                    });
                }
            }
            // Item 5: ValueClause validation
            Child::ValueClause(vcidx) => {
                let vc = &ast.arena.value_clauses[*vcidx as usize];
                let mut matched = false;
                for (rule_type, _opts) in rules {
                    if let RuleType::ValueClauseRule { rules: vc_rules } = rule_type {
                        matched = true;
                        validate_children(&vc.children, ast, vc_rules, enum_map, table, errors, file_path, scope_context, game, ruleset);
                        break;
                    }
                }
                if !matched {
                    errors.push(ValidationError {
                        message: "Unexpected value clause '{...}'".to_string(),
                        severity: ErrorSeverity::Warning,
                        line: vc.pos.start.line,
                        col: vc.pos.start.col,
                        file: file_path.to_string(),
                    });
                }
            }
            _ => {}
        }
    }

    // Cardinality enforcement
    for (rule_idx, (rule_type, opts)) in rules.iter().enumerate() {
        // Item 7: when strict_min=false, missing required fields are warnings not errors.
        let missing_sev = if opts.strict_min {
            opts.severity.as_ref().map(|s| severity_to_error(s.clone())).unwrap_or(ErrorSeverity::Error)
        } else {
            ErrorSeverity::Warning
        };
        let max_sev = opts.severity.as_ref().map(|s| severity_to_error(s.clone())).unwrap_or(ErrorSeverity::Error);

        match rule_type {
            RuleType::LeafRule { .. } | RuleType::NodeRule { .. } => {
                if let Some(key) = get_rule_key(rule_type) {
                    let count = key_counts.get(&key).copied().unwrap_or(0) as i32;
                    if count < opts.min {
                        errors.push(ValidationError {
                            message: format!("Field '{}' appears {} time(s), expected at least {}", key, count, opts.min),
                            severity: missing_sev, line: 0, col: 0, file: file_path.to_string(),
                        });
                    }
                    if count > opts.max {
                        errors.push(ValidationError {
                            message: format!("Field '{}' appears {} time(s), expected at most {}", key, count, opts.max),
                            severity: max_sev, line: 0, col: 0, file: file_path.to_string(),
                        });
                    }
                }
            }
            // Item 5: LeafValueRule cardinality
            RuleType::LeafValueRule { right } => {
                let count = leafvalue_counts[rule_idx] as i32;
                if count < opts.min {
                    errors.push(ValidationError {
                        message: format!("LeafValue {:?} appears {} time(s), expected at least {}", right, count, opts.min),
                        severity: missing_sev, line: 0, col: 0, file: file_path.to_string(),
                    });
                }
                if count > opts.max {
                    errors.push(ValidationError {
                        message: format!("LeafValue {:?} appears {} time(s), expected at most {}", right, count, opts.max),
                        severity: max_sev, line: 0, col: 0, file: file_path.to_string(),
                    });
                }
            }
            // Item 5: ValueClauseRule cardinality
            RuleType::ValueClauseRule { .. } => {
                let count = valueclause_counts[rule_idx] as i32;
                if count < opts.min {
                    errors.push(ValidationError {
                        message: format!("ValueClause appears {} time(s), expected at least {}", count, opts.min),
                        severity: missing_sev, line: 0, col: 0, file: file_path.to_string(),
                    });
                }
                if count > opts.max {
                    errors.push(ValidationError {
                        message: format!("ValueClause appears {} time(s), expected at most {}", count, opts.max),
                        severity: max_sev, line: 0, col: 0, file: file_path.to_string(),
                    });
                }
            }
            _ => {}
        }
    }
}

fn apply_replace_scopes(ctx: &mut ScopeContext, replace: &ReplaceScopes, game: Option<Game>) {
    if let Some(g) = game {
        ctx.apply_replace_scope(
            replace.root.as_deref(),
            replace.this.as_deref(),
            &replace.froms,
            &replace.prevs,
            g,
        );
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

/// Check that a string has the YYYY.MM.DD shape for a CW date field.
fn is_date_shape(s: &str) -> bool {
    // Accept YYYY.MM.DD or YYYY.M.D — split by '.' and check 3 numeric parts
    let parts: Vec<&str> = s.splitn(4, '.').collect();
    parts.len() >= 3 && parts[0].parse::<i32>().is_ok()
        && parts[1].parse::<u32>().is_ok()
        && parts[2].parse::<u32>().is_ok()
}

/// Check that a string has the YYYY.MM.DD.HH shape for a CW datetime field.
fn is_datetime_shape(s: &str) -> bool {
    // Allow 3 or 4 dot-separated numeric parts
    is_date_shape(s)
}

fn field_matches_value(field: &NewField, value: &Value, table: &StringTable, enum_map: &HashMap<&str, &EnumDefinition>) -> bool {
    // Item 2: VALUE-VALIDATOR BYPASSES (F# FieldValidators.fs:82-83, 836-839).
    // Before any type-specific checks, accept scripted variables (@...), localisation
    // references ($$), and inline math ([...]).  These are valid CW script idioms that
    // can legitimately appear in place of any typed value.
    match value {
        Value::String(t) | Value::QString(t) => {
            let text = table.get_string(t.normal).unwrap_or_default();
            if text.starts_with('@') || text.contains("$$") || text.starts_with('[') {
                return true;
            }
        }
        _ => {}
    }

    match (field, value) {
        // --- Boolean ---
        (NewField::ValueField(ValueType::Bool), Value::Bool(_)) => true,
        (NewField::ValueField(ValueType::Bool), Value::String(t)) | (NewField::ValueField(ValueType::Bool), Value::QString(t)) => {
            let v = table.get_string(t.normal).unwrap_or_default().to_lowercase();
            v == "yes" || v == "no"
        }

        // --- Int with range enforcement (item 4) ---
        (NewField::ValueField(ValueType::Int { min, max }), Value::Int(v)) => {
            let v_i = *v as i32;
            v_i >= *min && v_i <= *max
        }
        (NewField::ValueField(ValueType::Int { min, max }), Value::String(t)) | (NewField::ValueField(ValueType::Int { min, max }), Value::QString(t)) => {
            let text = table.get_string(t.normal).unwrap_or_default();
            if let Ok(v) = text.parse::<i32>() {
                v >= *min && v <= *max
            } else {
                false
            }
        }

        // --- Float with range enforcement (item 4) ---
        (NewField::ValueField(ValueType::Float { min, max }), Value::Float(v)) => { *v >= *min && *v <= *max }
        (NewField::ValueField(ValueType::Float { min, max }), Value::String(t)) | (NewField::ValueField(ValueType::Float { min, max }), Value::QString(t)) => {
            let text = table.get_string(t.normal).unwrap_or_default();
            if let Ok(v) = text.parse::<f64>() {
                v >= *min && v <= *max
            } else {
                false
            }
        }

        // --- Enum ---
        (NewField::ValueField(ValueType::Enum(enum_name)), Value::String(t))
        | (NewField::ValueField(ValueType::Enum(enum_name)), Value::QString(t)) => {
            let text = table.get_string(t.normal).unwrap_or_default();
            match enum_map.get(enum_name.as_str()) {
                Some(enum_def) => enum_def.values.contains(&text),
                None => true, // unknown/complex enums can't be validated statically
            }
        }

        // --- Percent (item 3): value ends with '%' or is a number ---
        (NewField::ValueField(ValueType::Percent), Value::String(t)) | (NewField::ValueField(ValueType::Percent), Value::QString(t)) => {
            let text = table.get_string(t.normal).unwrap_or_default();
            text.ends_with('%') || text.parse::<f64>().is_ok()
        }
        (NewField::ValueField(ValueType::Percent), Value::Float(_) | Value::Int(_)) => true,

        // --- Date / DateTime (item 3): basic YYYY.MM.DD[.HH] shape ---
        (NewField::ValueField(ValueType::Date), Value::String(t)) | (NewField::ValueField(ValueType::Date), Value::QString(t)) => {
            is_date_shape(&table.get_string(t.normal).unwrap_or_default())
        }
        (NewField::ValueField(ValueType::DateTime), Value::String(t)) | (NewField::ValueField(ValueType::DateTime), Value::QString(t)) => {
            is_datetime_shape(&table.get_string(t.normal).unwrap_or_default())
        }

        // --- Ck2Dna (item 3): exactly 32 hex chars (F# FieldValidators.fs:194-204) ---
        (NewField::ValueField(ValueType::Ck2Dna), Value::String(t)) | (NewField::ValueField(ValueType::Ck2Dna), Value::QString(t)) => {
            let text = table.get_string(t.normal).unwrap_or_default();
            text.len() == 32 && text.chars().all(|c| c.is_ascii_hexdigit())
        }

        // --- Ck2DnaProperty (item 3): length 8 or 32, hex chars (F# FieldValidators.fs:205-211) ---
        (NewField::ValueField(ValueType::Ck2DnaProperty), Value::String(t)) | (NewField::ValueField(ValueType::Ck2DnaProperty), Value::QString(t)) => {
            let text = table.get_string(t.normal).unwrap_or_default();
            (text.len() == 8 || text.len() == 32) && text.chars().all(|c| c.is_ascii_hexdigit())
        }

        // --- IrFamilyName / StlNameFormat (item 3): accept any string ---
        (NewField::ValueField(ValueType::IrFamilyName), Value::String(_) | Value::QString(_)) => true,
        (NewField::ValueField(ValueType::StlNameFormat(_)), Value::String(_) | Value::QString(_)) => true,

        // --- Scalar: accept anything ---
        (NewField::ScalarField, _) => true,

        // --- SpecificField: exact string match ---
        (NewField::SpecificField(s), Value::String(t)) | (NewField::SpecificField(s), Value::QString(t)) => {
            table.get_string(t.normal).unwrap_or_default() == *s
        }

        // --- TypeField: accept string (cross-file existence is a separate pass) ---
        (NewField::TypeField(TypeType::Simple(type_name)), Value::String(t))
        | (NewField::TypeField(TypeType::Simple(type_name)), Value::QString(t)) => {
            validate_type_reference(&table.get_string(t.normal).unwrap_or_default(), type_name)
        }
        (NewField::TypeField(TypeType::Complex { name, .. }), Value::String(t))
        | (NewField::TypeField(TypeType::Complex { name, .. }), Value::QString(t)) => {
            validate_type_reference(&table.get_string(t.normal).unwrap_or_default(), name)
        }

        // --- ScopeField ---
        (NewField::ScopeField(_), Value::String(t)) | (NewField::ScopeField(_), Value::QString(t)) => {
            let text = table.get_string(t.normal).unwrap_or_default();
            text.starts_with("scope[") || ["root","this","from","prev","capital","random","trigger"].contains(&text.as_str())
        }

        // --- VariableField with range enforcement (item 4) ---
        (NewField::VariableField { min, max, .. }, Value::Float(v)) => { *v >= *min && *v <= *max }
        (NewField::VariableField { min, max, .. }, Value::Int(v)) => { (*v as f64) >= *min && (*v as f64) <= *max }
        (NewField::VariableField { min, max, .. }, Value::String(t)) | (NewField::VariableField { min, max, .. }, Value::QString(t)) => {
            let text = table.get_string(t.normal).unwrap_or_default();
            if let Ok(v) = text.parse::<f64>() {
                v >= *min && v <= *max
            } else {
                // non-numeric string: accept (could be a scripted variable not caught by bypass)
                true
            }
        }

        // --- LocalisationField / FilepathField ---
        (NewField::LocalisationField { .. }, Value::String(_) | Value::QString(_)) => true,
        (NewField::FilepathField { .. }, Value::String(_) | Value::QString(_)) => true,

        // --- IconField (item 3): accept any string ---
        (NewField::IconField(_), Value::String(_) | Value::QString(_)) => true,

        // --- VariableGetField / VariableSetField (item 3): accept any string or numeric ---
        (NewField::VariableGetField(_), _) => true,
        (NewField::VariableSetField(_), _) => true,

        // --- ValueScopeField / ValueScopeMarkerField (item 3): accept number, @var, or scope chain ---
        (NewField::ValueScopeField { .. }, Value::Float(_) | Value::Int(_)) => true,
        (NewField::ValueScopeField { .. }, Value::String(_) | Value::QString(_)) => true,
        (NewField::ValueScopeMarkerField { .. }, Value::Float(_) | Value::Int(_)) => true,
        (NewField::ValueScopeMarkerField { .. }, Value::String(_) | Value::QString(_)) => true,

        // --- AliasValueKeysField (item 3): accept any string key ---
        (NewField::AliasValueKeysField(_), Value::String(_) | Value::QString(_)) => true,

        // --- AliasField / SingleAliasField: accept clause or string (deep validation TODO) ---
        (NewField::AliasField(_), Value::Clause(_)) => true,
        (NewField::AliasField(_), Value::String(_) | Value::QString(_)) => true,
        (NewField::SingleAliasField(_), Value::Clause(_)) => true,
        (NewField::SingleAliasField(_), Value::String(_) | Value::QString(_)) => true,

        // --- MarkerField: accept anything (validated elsewhere) ---
        (NewField::MarkerField(_), _) => true,

        // --- IgnoreMarkerField / IgnoreField: always accept ---
        (NewField::IgnoreMarkerField, _) => true,
        (NewField::IgnoreField(_), _) => true,

        _ => false,
    }
}

fn validate_type_reference(text: &str, _expected_type: &str) -> bool {
    // A TypeField references an *instance* of the named type (e.g. a `node_type`
    // rule is satisfied by `node_type_one`, a defined instance), not the literal
    // type name. Verifying the instance actually exists needs a cross-file type
    // index (built in the info crate); until that is wired in, accept any
    // non-empty token rather than flag every valid reference as an error.
    !text.is_empty()
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
