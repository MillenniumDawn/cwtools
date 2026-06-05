use cwtools_game::scope_engine::{ScopeContext, ScopeId};
use cwtools_game::constants::Game;
use cwtools_parser::ast::{Child, ParsedFile, Value};
use cwtools_rules::rules_types::*;
use cwtools_string_table::string_table::{StringTable, StringTokens};
use std::collections::{HashMap, HashSet};

pub mod error_codes;
pub mod per_game;

#[derive(Debug, Clone, PartialEq)]
pub struct ValidationError {
    pub message: String,
    pub severity: ErrorSeverity,
    pub line: u32,
    pub col: u16,
    pub file: String,
    /// CW### error code, e.g. "CW201" for unexpected field.
    pub code: Option<String>,
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
    type_index: Option<&cwtools_info::TypeIndex>,
    modifier_keys: Option<&HashSet<String>>,
) {
    for grandchild in grandchildren {
        match grandchild {
            Child::Node(gc_idx) => {
                let gc_node = &ast.arena.nodes[*gc_idx as usize];
                let gc_key = table.get_string(gc_node.key.normal).unwrap_or_default();
                validate_with_type(type_def, gc_node.children.as_slice(), ast, inner_rules, enum_map, table, errors, file_path, scope_context, game, ruleset, type_index, modifier_keys, Some(&gc_key));
            }
            Child::Leaf(gc_idx) => {
                let gc_leaf = &ast.arena.leaves[*gc_idx as usize];
                if let Value::Clause(gc_children) = &gc_leaf.value {
                    let gc_key = table.get_string(gc_leaf.key.normal).unwrap_or_default();
                    validate_with_type(type_def, gc_children.as_slice(), ast, inner_rules, enum_map, table, errors, file_path, scope_context, game, ruleset, type_index, modifier_keys, Some(&gc_key));
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
                    code: Some(error_codes::CW201_UNEXPECTED_FIELD.id.to_string()),
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
    type_index: Option<&cwtools_info::TypeIndex>,
    modifier_keys: Option<&HashSet<String>>,
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

    // type_per_file: the WHOLE file is a single instance of this type (e.g. an
    // OOB file). Its root children ARE the instance body — there is no per-entry
    // wrapper key — so validate them once against the type's rules and stop.
    if let Some(td) = path_type {
        if td.type_per_file {
            let inner_rules = find_rules_by_name(&td.name, ruleset);
            let has_content_rules = !inner_rules.is_empty()
                || td.subtypes.iter().any(|st| !st.rules.is_empty());
            if has_content_rules {
                validate_with_type(td, &ast.root_children, ast, inner_rules, &enum_map, table, &mut errors, file_path, &mut scope_context, game, ruleset, type_index, modifier_keys, None);
            }
            if let Some(g) = game {
                errors.extend(per_game::run_game_validators(ast, ruleset, table, file_path, g));
            }
            return errors;
        }
    }

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
            // Only content-validate when the matched type actually has rules; a
            // type[x] declared solely for instance indexing (path/name_field, no
            // rule body) must not flag its instance fields as unexpected.
            let has_content_rules = !inner_rules.is_empty()
                || type_def.subtypes.iter().any(|st| !st.rules.is_empty());
            if has_content_rules {
                // When the matched key is itself a skip_root_key wrapper for this
                // type (e.g. `ability = { force_attack = { ... } }` where the type
                // is `ability` AND skip_root_key = ability), the key is a wrapper,
                // not an instance: its children are the instances. Validate them as
                // grandchildren instead of treating them as the type's content.
                if should_skip_root_key(&type_key, type_def) {
                    validate_wrapper_grandchildren(children, type_def, ast, inner_rules, &enum_map, table, &mut errors, file_path, &mut scope_context, game, ruleset, type_index, modifier_keys);
                } else {
                    validate_with_type(type_def, children, ast, inner_rules, &enum_map, table, &mut errors, file_path, &mut scope_context, game, ruleset, type_index, modifier_keys, Some(&type_key));
                }
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
            // A wrapper is ONLY signalled by skip_root_key. A subtype whose name
            // equals the root key is NOT a wrapper — that's the type_key_filter
            // discriminator pattern (e.g. `country_event = { ... }` selects the
            // `country_event` subtype of `event`); the node is the instance and
            // its children are the content, not a wrapper layer to skip.
            let root_key = match child {
                Child::Node(node_idx) => table.get_string(ast.arena.nodes[*node_idx as usize].key.normal).unwrap_or_default(),
                Child::Leaf(leaf_idx) => table.get_string(ast.arena.leaves[*leaf_idx as usize].key.normal).unwrap_or_default(),
                _ => String::new(),
            };

            // If skip_root_key = any, the root node is a WRAPPER — validate its children individually
            if should_skip_root_key(&root_key, type_def) {
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
                validate_wrapper_grandchildren(grandchildren, type_def, ast, inner_rules, &enum_map, table, &mut errors, file_path, &mut scope_context, game, ruleset, type_index, modifier_keys);
                continue;
            }

            // The type declares skip_root_key(s) but this root matches none of them:
            // the type does not apply to this root (F# `skiprootkey` gate). Skip it
            // rather than validating the root directly — otherwise an unrelated file
            // sharing the path (e.g. `leader_skills` under common/unit_leader, where
            // the only type is `unit_leader_trait` with skip_root_key = leader_traits)
            // gets its children flagged as unexpected.
            if !type_def.skip_root_key.is_empty() {
                continue;
            }

            // No skip_root_key — validate the root node itself normally
            match child {
                Child::Node(node_idx) => {
                    let node = &ast.arena.nodes[*node_idx as usize];
                    validate_with_type(type_def, node.children.as_slice(), ast, inner_rules, &enum_map, table, &mut errors, file_path, &mut scope_context, game, ruleset, type_index, modifier_keys, Some(&root_key));
                }
                Child::Leaf(leaf_idx) => {
                    let leaf = &ast.arena.leaves[*leaf_idx as usize];
                    if let Value::Clause(children) = &leaf.value {
                        validate_with_type(type_def, children.as_slice(), ast, inner_rules, &enum_map, table, &mut errors, file_path, &mut scope_context, game, ruleset, type_index, modifier_keys, Some(&root_key));
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
    type_index: Option<&cwtools_info::TypeIndex>,
    modifier_keys: Option<&HashSet<String>>,
    node_key: Option<&str>,
) {
    if type_def.subtypes.is_empty() {
        let pre_count = errors.len();
        validate_children(children, ast, inner_rules, enum_map, table, errors, file_path, scope_context, game, ruleset, type_index, modifier_keys);
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
        if subtype_matches(subtype, children, ast, table, enum_map, node_key, type_index) {
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
    validate_children(children, ast, &merged, enum_map, table, errors, file_path, scope_context, game, ruleset, type_index, modifier_keys);

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
    let basename = path_lower.rsplit('/').next().unwrap_or(&path_lower);
    // The file's directory (no filename, no trailing slash).
    let dir = path_lower.strip_suffix(basename).unwrap_or(&path_lower).trim_end_matches('/');
    let mut best: Option<&TypeDefinition> = None;
    let mut best_len = 0usize;

    for t in &ruleset.types {
        // path_file pins the type to one specific filename (e.g. several types
        // share path "map" but only airports.txt is the `airports` type).
        if let Some(pf) = &t.path_options.path_file {
            if basename != pf.to_lowercase() {
                continue;
            }
        }
        // path_extension restricts the type to files with a given extension
        // (e.g. sound types require `.asset`, so a `.txt` combat-sounds file must
        // NOT match them). F# treats the extension as a hard filter.
        if let Some(ext) = &t.path_options.path_extension {
            let ext = ext.to_lowercase();
            let ext = ext.strip_prefix('.').unwrap_or(&ext);
            if !basename.rsplit('.').next().is_some_and(|e| e == ext) {
                continue;
            }
        }
        for p in &t.path_options.paths {
            let p_lower = p.to_lowercase();
            // path_strict: the file must be DIRECTLY in this directory (so
            // `path_strict` type[unit] at common/units does NOT swallow files in
            // common/units/names/). Otherwise it may be in a subdirectory.
            let matches = if t.path_options.path_strict {
                dir == p_lower || dir.ends_with(&format!("/{}", p_lower))
            } else {
                path_contains_segment(dir, &p_lower)
            };
            // A path_file match is more specific than any bare directory match.
            let weight = p_lower.len() + if t.path_options.path_file.is_some() { 1000 } else { 0 };
            if matches && weight > best_len {
                best = Some(t);
                best_len = weight;
            }
        }
    }
    best
}

/// Test a subtype's rules against an entity's children, following F#
/// `testSubtype` / `applyClauseField` with `enforceCardinality = false`.
///
/// A subtype is active unless one of its rules is violated:
///   - a required rule (min >= 1) whose key is absent (or under-count),
///   - a key present more than its max,
///   - a PRESENT field whose value doesn't match the rule.
/// Fields the rules don't mention are ignored (no "unexpected" check here), so a
/// subtype whose rules are all optional (`## cardinality = 0..1`) and absent
/// matches vacuously — exactly how F# unions optional subtypes. The real
/// discriminators are the un-annotated rules (default `1..1`, required) and any
/// present field whose value contradicts a rule (e.g. `is_archetype = no` is
/// contradicted by a present `is_archetype = yes`).
fn subtype_rules_match(
    rules: &[(RuleType, Options)],
    children: &[Child],
    ast: &ParsedFile,
    table: &StringTable,
    enum_map: &HashMap<&str, &EnumDefinition>,
    type_index: Option<&cwtools_info::TypeIndex>,
) -> bool {
    // A subtype with discriminators must be *positively activated* by the entity:
    // F# matches a subtype when its rules apply cleanly, but a subtype whose
    // discriminators are all optional (`0..1`) and absent would otherwise match
    // every entity and wrongly impose its required body fields. So we additionally
    // require some discriminator to be actively met. A present field that fails a
    // discriminator still *blocks* the match (contradiction), and a missing
    // required (`min>=1`) discriminator still fails it.
    //
    // Discriminators are grouped by key. Several rules can share a key as a
    // disjunction — both same-kind (`trait_type = assignable_trait` / `trait_type =
    // assignable_terrain_trait`) and cross-kind (`type = enum[air_units]` as a leaf
    // OR `type = { enum[air_units] }` as a block). F# counts cardinality by key
    // across leaves AND nodes, and a present field is a contradiction only when it
    // matches NONE of the key's rules. So we collect both leaf and node rules under
    // one key and evaluate them together.
    #[derive(Default)]
    struct KeyGroup<'a> {
        leaf_rights: Vec<(&'a NewField, &'a Options)>,
        node_inners: Vec<(&'a [(RuleType, Options)], &'a Options)>,
    }
    let mut groups: HashMap<&str, KeyGroup> = HashMap::new();
    for (rt, opts) in rules {
        match rt {
            RuleType::LeafRule { left: NewField::SpecificField(k), right } => {
                groups.entry(k.as_str()).or_default().leaf_rights.push((right, opts));
            }
            RuleType::NodeRule { left: NewField::SpecificField(k), rules: inner } => {
                groups.entry(k.as_str()).or_default().node_inners.push((inner.as_slice(), opts));
            }
            _ => {}
        }
    }
    if groups.is_empty() {
        // No discriminators at all → pure-marker subtype, matches vacuously.
        return true;
    }
    let mut activated = false;

    for (k, group) in &groups {
        let mut count: i32 = 0;
        let mut any_match = false;
        for c in children {
            // Resolve this child's key and decide which discriminator kind applies:
            // a scalar leaf checks the leaf rules; a block (node or clause-leaf)
            // checks the node rules.
            let (matches_key, leaf_value, clause): (bool, Option<&Value>, Option<&[Child]>) = match c {
                Child::Leaf(idx) => {
                    let leaf = &ast.arena.leaves[*idx as usize];
                    if table.get_string(leaf.key.normal).unwrap_or_default() == *k {
                        match &leaf.value {
                            Value::Clause(ch) => (true, None, Some(ch.as_slice())),
                            v => (true, Some(v), None),
                        }
                    } else { (false, None, None) }
                }
                Child::Node(idx) => {
                    let node = &ast.arena.nodes[*idx as usize];
                    if table.get_string(node.key.normal).unwrap_or_default() == *k {
                        (true, None, Some(node.children.as_slice()))
                    } else { (false, None, None) }
                }
                _ => (false, None, None),
            };
            if !matches_key { continue; }
            count += 1;
            if let Some(v) = leaf_value {
                for (right, _) in &group.leaf_rights {
                    if field_matches_value(right, v, table, enum_map) {
                        any_match = true;
                        // A present field activates the subtype. A bare `<type>` ref
                        // doesn't activate on shape alone (the key is common), but it
                        // DOES when the value is a verified instance of that type —
                        // e.g. `category = <peace_action_categories>` with a real
                        // category. A `<type.subtype>` ref has no plain type entry so
                        // this naturally declines (keeps `air_equip` from activating
                        // on every `archetype = ...`).
                        if field_activates_on_presence(right)
                            || typefield_value_is_instance(right, v, table, type_index)
                        {
                            activated = true;
                        }
                    }
                }
            }
            if let Some(ic) = clause {
                if group.node_inners.iter().any(|(inner, _)| subtype_rules_match(inner, ic, ast, table, enum_map, type_index)) {
                    any_match = true;
                    activated = true;
                }
            }
        }
        // Present but matching none of the disjuncts (of the applicable kind) → contradiction.
        if count > 0 && !any_match { return false; }
        // Cardinality is counted by key across both kinds: required if any disjunct
        // demands it, capped by the tightest max.
        let all_opts = group.leaf_rights.iter().map(|(_, o)| *o)
            .chain(group.node_inners.iter().map(|(_, o)| *o));
        let min_required = all_opts.clone().map(|o| o.min).max().unwrap_or(0);
        let max_allowed = all_opts.map(|o| o.max).min().unwrap_or(i32::MAX);
        if min_required > count || count > max_allowed { return false; }
        // Absent but a disjunct is the field's default value (`= no`/`false`/`0`).
        if count == 0 && group.leaf_rights.iter().any(|(r, _)| is_default_satisfied_literal(r)) {
            activated = true;
        }
    }

    activated
}

/// Whether a present field matching this discriminator activates the subtype.
/// Most discriminators are presence signals (`days_remove = scalar`, `is_archetype
/// = yes`, `type = enum[...]`). The exception is a bare `<type>` reference and the
/// alias/ignore placeholders: those keys are common and their value check is
/// permissive, so presence alone is not a reliable subtype signal.
fn field_activates_on_presence(right: &NewField) -> bool {
    !matches!(
        right,
        NewField::TypeField(_)
            | NewField::AliasField(_)
            | NewField::SingleAliasField(_)
            | NewField::IgnoreField(_)
            | NewField::IgnoreMarkerField
    )
}

/// A `field = literal` discriminator whose literal is the field's default value,
/// so an absent field satisfies it. Paradox booleans default to `no`/`false` and
/// numeric flags to `0`.
fn is_default_satisfied_literal(right: &NewField) -> bool {
    matches!(right, NewField::SpecificField(v) if v == "no" || v == "false" || v == "0")
}

/// Decide whether a subtype is active for an entity (F# `testSubtype`).
///
/// - `## type_key_filter`: active iff the instance's own node key is in the list.
/// - Explicit `type_key_field`: active iff the entity has a child with that key.
/// - Otherwise: apply the subtype's rules (cardinality-aware) — see
///   [`subtype_rules_match`]. An empty subtype matches vacuously.
fn subtype_matches(
    subtype: &SubTypeDefinition,
    children: &[Child],
    ast: &ParsedFile,
    table: &StringTable,
    enum_map: &HashMap<&str, &EnumDefinition>,
    node_key: Option<&str>,
    type_index: Option<&cwtools_info::TypeIndex>,
) -> bool {
    // `## type_key_filter` discriminates on the instance's own node key (e.g.
    // `shared_focus` selects subtype[shared], `joint_focus` selects subtype[joint_focus]).
    if !subtype.type_key_filter.is_empty() {
        return node_key.is_some_and(|k| subtype.type_key_filter.iter().any(|f| f == k));
    }
    if let Some(fk) = &subtype.type_key_field {
        return children.iter().any(|c| child_key_matches(c, ast, table, fk));
    }
    subtype_rules_match(&subtype.rules, children, ast, table, enum_map, type_index)
}

/// True when `right` is a plain `<type>` reference and `value` is a known instance
/// of that type. Used so a present typed discriminator activates its subtype only
/// when the value is real (not on the shape of the key alone). A `<type.subtype>`
/// reference has no plain type entry in the index, so it declines here.
fn typefield_value_is_instance(
    right: &NewField,
    value: &Value,
    table: &StringTable,
    type_index: Option<&cwtools_info::TypeIndex>,
) -> bool {
    let (NewField::TypeField(TypeType::Simple(tname)), Some(idx)) = (right, type_index) else {
        return false;
    };
    let v = match value {
        Value::String(t) | Value::QString(t) => match_text(table, t),
        _ => return false,
    };
    idx.contains(tname, &v)
}

/// Start (line, col) of a child node, for locating block-level diagnostics.
fn child_start_pos(child: &Child, ast: &ParsedFile) -> Option<(u32, u16)> {
    match child {
        Child::Leaf(i) => { let l = &ast.arena.leaves[*i as usize]; Some((l.pos.start.line, l.pos.start.col)) }
        Child::Node(i) => { let n = &ast.arena.nodes[*i as usize]; Some((n.pos.start.line, n.pos.start.col)) }
        Child::LeafValue(i) => { let lv = &ast.arena.leaf_values[*i as usize]; Some((lv.pos.start.line, lv.pos.start.col)) }
        Child::ValueClause(i) => { let vc = &ast.arena.value_clauses[*i as usize]; Some((vc.pos.start.line, vc.pos.start.col)) }
        _ => None,
    }
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

/// Validate one keyed Leaf child against a single matching rule, writing into
/// `errors`. Factored out so an overloaded key (several rules with the same key,
/// e.g. two `province = { ... }` definitions) can be validated as a disjunction.
#[allow(clippy::too_many_arguments)]
/// True when a rule's left-hand field is `IgnoreField` (`key = ignore_field`),
/// meaning the matched field/block is accepted without validating its contents.
fn rule_left_is_ignore(rule_type: &RuleType) -> bool {
    matches!(
        rule_type,
        RuleType::LeafRule { left: NewField::IgnoreField(_), .. }
            | RuleType::NodeRule { left: NewField::IgnoreField(_), .. }
    )
}

fn validate_leaf_against_rule(
    leaf: &cwtools_parser::ast::Leaf,
    key: &str,
    rule_type: &RuleType,
    opts: &Options,
    ast: &ParsedFile,
    enum_map: &HashMap<&str, &EnumDefinition>,
    table: &StringTable,
    errors: &mut Vec<ValidationError>,
    file_path: &str,
    scope_context: &mut Option<ScopeContext>,
    game: Option<Game>,
    ruleset: &RuleSet,
    type_index: Option<&cwtools_info::TypeIndex>,
    modifier_keys: Option<&HashSet<String>>,
) {
    // `key = ignore_field`: the field's value is accepted unvalidated.
    if rule_left_is_ignore(rule_type) {
        return;
    }
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
                    code: Some(error_codes::CW400_UNKNOWN_SCOPE.id.to_string()),
                });
            }
        }
    }
    match rule_type {
        RuleType::LeafRule { left, .. } => {
            if let NewField::AliasField(category) = left {
                validate_alias_usage(category, key, Some(leaf), None, ast, enum_map, table, errors, file_path, scope_context, game, ruleset, type_index, modifier_keys);
            } else {
                validate_leaf(leaf, rule_type, table, enum_map, errors, file_path, type_index);
            }
        }
        RuleType::NodeRule { left, rules: inner_rules, .. } => {
            if let NewField::AliasField(category) = left {
                validate_alias_usage(category, key, Some(leaf), None, ast, enum_map, table, errors, file_path, scope_context, game, ruleset, type_index, modifier_keys);
            } else if let Value::Clause(clause_children) = &leaf.value {
                let saved = scope_context.as_ref().map(|ctx| ctx.save());
                if let Some(ctx) = scope_context.as_mut() {
                    if let Some(ref push) = opts.push_scope {
                        ctx.change_scope(push);
                    }
                    if let Some(ref replace) = opts.replace_scopes {
                        apply_replace_scopes(ctx, replace, game);
                    }
                }
                validate_children(clause_children, ast, inner_rules, enum_map, table, errors, file_path, scope_context, game, ruleset, type_index, modifier_keys);
                if let (Some(saved), Some(ref mut ctx)) = (saved, scope_context.as_mut()) {
                    ctx.restore(saved);
                }
            }
        }
        _ => {}
    }
}

/// As `validate_leaf_against_rule` but for a parser `Node` child.
#[allow(clippy::too_many_arguments)]
fn validate_node_against_rule(
    node: &cwtools_parser::ast::Node,
    key: &str,
    rule_type: &RuleType,
    opts: &Options,
    ast: &ParsedFile,
    enum_map: &HashMap<&str, &EnumDefinition>,
    table: &StringTable,
    errors: &mut Vec<ValidationError>,
    file_path: &str,
    scope_context: &mut Option<ScopeContext>,
    game: Option<Game>,
    ruleset: &RuleSet,
    type_index: Option<&cwtools_info::TypeIndex>,
    modifier_keys: Option<&HashSet<String>>,
) {
    // `key = ignore_field`: the block is accepted unvalidated.
    if rule_left_is_ignore(rule_type) {
        return;
    }
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
                    code: Some(error_codes::CW400_UNKNOWN_SCOPE.id.to_string()),
                });
            }
        }
    }
    if let RuleType::NodeRule { left, rules: inner_rules, .. } = rule_type {
        if let NewField::AliasField(category) = left {
            validate_alias_usage(category, key, None, Some(&node.children), ast, enum_map, table, errors, file_path, scope_context, game, ruleset, type_index, modifier_keys);
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
            validate_children(&node.children, ast, inner_rules, enum_map, table, errors, file_path, scope_context, game, ruleset, type_index, modifier_keys);
            if let (Some(saved), Some(ref mut ctx)) = (saved, scope_context.as_mut()) {
                ctx.restore(saved);
            }
        }
    }
}

/// Run several candidate rules for one overloaded key as a disjunction: accept on
/// the first clean match, otherwise surface the fewest-errors candidate. With a
/// single candidate this is just a direct validation.
fn pick_best_candidate<F>(mut validate_one: F, errors: &mut Vec<ValidationError>, n: usize)
where
    F: FnMut(usize, &mut Vec<ValidationError>),
{
    if n == 1 {
        validate_one(0, errors);
        return;
    }
    let mut best: Option<Vec<ValidationError>> = None;
    for i in 0..n {
        let mut temp = Vec::new();
        validate_one(i, &mut temp);
        if temp.is_empty() {
            return; // clean match
        }
        match &best {
            Some(b) if b.len() <= temp.len() => {}
            _ => best = Some(temp),
        }
    }
    if let Some(b) = best {
        errors.extend(b);
    }
}

/// Collect the rules whose key matches `key`. If any rule keys on a literal
/// `SpecificField` equal to `key`, ONLY those are returned — a specific rule
/// (e.g. `milestones = { ... }`) wins over catch-all rules (`enum[x] = ...`,
/// `<type> = ...`, `alias_name[...]`) that match the same key permissively.
fn matching_candidates<'a, F>(
    rules: &'a [(RuleType, Options)],
    key: &str,
    ruleset: &RuleSet,
    type_index: Option<&cwtools_info::TypeIndex>,
    matcher: F,
) -> Vec<&'a (RuleType, Options)>
where
    F: Fn(&RuleType, &str, &RuleSet, Option<&cwtools_info::TypeIndex>) -> bool,
{
    let all: Vec<&(RuleType, Options)> = rules.iter()
        .filter(|(rt, _)| matcher(rt, key, ruleset, type_index))
        .collect();
    let specific: Vec<&(RuleType, Options)> = all.iter()
        .filter(|(rt, _)| matches!(rt,
            RuleType::LeafRule { left: NewField::SpecificField(s), .. }
            | RuleType::NodeRule { left: NewField::SpecificField(s), .. } if s == key))
        .copied()
        .collect();
    if specific.is_empty() { all } else { specific }
}

/// Expand nested `SubtypeRule` entries into their inner rules.
///
/// Top-level subtypes are resolved in `validate_with_type` against the entity
/// root, but a `subtype[x] = { ... }` block can also appear deep inside a rule
/// tree (e.g. `ai_weights = { scalar = { subtype[player_context] = { ai_will_do }
/// subtype[country_context] = { ai_will_do } } }`). At that depth the root's
/// active-subtype set isn't threaded down and the nested `SubtypeRule` carries
/// only its inner rules, not its discriminator (which lives on the root
/// TypeDefinition). So we union every branch: a field present in any subtype
/// branch is accepted, mirroring F#'s "a field in any matching subtype is not
/// unexpected". This is permissive across non-active branches, which is the safe
/// direction (no false-positive "Unexpected field").
fn flatten_nested_subtype_rules(rules: &[(RuleType, Options)]) -> Vec<(RuleType, Options)> {
    let mut out: Vec<(RuleType, Options)> = Vec::with_capacity(rules.len());
    for (rt, opts) in rules {
        // Both positive and negative (`subtype[!x]`) branches contribute fields by
        // union: a negative branch can't be resolved without the root set, so we
        // include its fields too rather than drop them.
        if let RuleType::SubtypeRule { rules: st_rules, .. } = rt {
            out.extend(flatten_nested_subtype_rules(st_rules));
        } else {
            out.push((rt.clone(), opts.clone()));
        }
    }
    out
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
    type_index: Option<&cwtools_info::TypeIndex>,
    modifier_keys: Option<&HashSet<String>>,
) {
    // Nested subtype blocks (a `subtype[x] = {...}` not at the entity root) carry
    // their fields inside SubtypeRule entries that the candidate matcher below
    // doesn't see. Flatten them in — but only pay the clone when any are present,
    // since this is a hot path called for every block.
    let flattened;
    let rules: &[(RuleType, Options)] = if rules.iter().any(|(rt, _)| matches!(rt, RuleType::SubtypeRule { .. })) {
        flattened = flatten_nested_subtype_rules(rules);
        &flattened
    } else {
        rules
    };

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
                let key = unquote_key(&table.get_string(leaf.key.normal).unwrap_or_default()).to_string();
                *key_counts.entry(key).or_insert(0) += 1;
            }
            Child::Node(idx) => {
                let node = &ast.arena.nodes[*idx as usize];
                let key = unquote_key(&table.get_string(node.key.normal).unwrap_or_default()).to_string();
                *key_counts.entry(key).or_insert(0) += 1;
            }
            Child::LeafValue(lvidx) => {
                let lv = &ast.arena.leaf_values[*lvidx as usize];
                // An anonymous `{ ... }` block parses as a clause-valued LeafValue;
                // count it toward a ValueClauseRule, not a LeafValueRule.
                if matches!(lv.value, Value::Clause(_)) {
                    for (rule_idx, (rule_type, _)) in rules.iter().enumerate() {
                        if matches!(rule_type, RuleType::ValueClauseRule { .. }) {
                            valueclause_counts[rule_idx] += 1;
                            break;
                        }
                    }
                } else {
                    // Count toward EVERY matching LeafValueRule, not just the
                    // first. Alternative leafvalue rules in one block are counted
                    // independently in F# (RuleValidationService.checkCardinality
                    // is a per-rule `Seq.sumBy`). Breaking on the first match lets
                    // a permissive earlier alternative (e.g. a `<type>` TypeField,
                    // which accepts any token) starve a later `enum[...]` rule,
                    // producing a spurious "appears 0 time(s)" cardinality error.
                    for (rule_idx, (rule_type, _)) in rules.iter().enumerate() {
                        if let RuleType::LeafValueRule { right } = rule_type {
                            if field_matches_value(right, &lv.value, table, enum_map) {
                                leafvalue_counts[rule_idx] += 1;
                            }
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
                let key = unquote_key(&table.get_string(leaf.key.normal).unwrap_or_default()).to_string();
                let candidates = matching_candidates(rules, &key, ruleset, type_index, rule_matches_leaf_key);
                if candidates.is_empty() {
                    // Item 5: dynamic modifier keys — if provided and this key is a
                    // known modifier, accept silently (modifier context mechanism).
                    let is_modifier = modifier_keys.map(|mk| mk.contains(&key)).unwrap_or(false);
                    if !is_modifier {
                        errors.push(ValidationError {
                            message: format!("Unexpected field '{}'", key),
                            severity: ErrorSeverity::Error,
                            line: leaf.pos.start.line,
                            col: leaf.pos.start.col,
                            file: file_path.to_string(),
                            code: Some(error_codes::CW201_UNEXPECTED_FIELD.id.to_string()),
                        });
                    }
                } else {
                    // An overloaded key (several rules with the same key, e.g. two
                    // `province = { ... }` forms) is a disjunction — accept if any
                    // candidate validates cleanly.
                    let n = candidates.len();
                    pick_best_candidate(|i, out| {
                        let (rt, opts) = candidates[i];
                        validate_leaf_against_rule(leaf, &key, rt, opts, ast, enum_map, table, out, file_path, scope_context, game, ruleset, type_index, modifier_keys);
                    }, errors, n);
                }
            }
            Child::Node(idx) => {
                let node = &ast.arena.nodes[*idx as usize];
                let key = unquote_key(&table.get_string(node.key.normal).unwrap_or_default()).to_string();
                let candidates = matching_candidates(rules, &key, ruleset, type_index, rule_matches_node_key);
                if candidates.is_empty() {
                    // Item 5: dynamic modifier keys — accept known modifier block keys silently.
                    let is_modifier = modifier_keys.map(|mk| mk.contains(&key)).unwrap_or(false);
                    if !is_modifier {
                        errors.push(ValidationError {
                            message: format!("Unexpected block '{}'", key),
                            severity: ErrorSeverity::Error,
                            line: node.pos.start.line,
                            col: node.pos.start.col,
                            file: file_path.to_string(),
                            code: Some(error_codes::CW201_UNEXPECTED_FIELD.id.to_string()),
                        });
                    }
                } else {
                    let n = candidates.len();
                    pick_best_candidate(|i, out| {
                        let (rt, opts) = candidates[i];
                        validate_node_against_rule(node, &key, rt, opts, ast, enum_map, table, out, file_path, scope_context, game, ruleset, type_index, modifier_keys);
                    }, errors, n);
                }
            }
            // Item 5: LeafValue validation
            Child::LeafValue(lvidx) => {
                let lv = &ast.arena.leaf_values[*lvidx as usize];
                // Anonymous `{ ... }` block: validate against a ValueClauseRule,
                // recursing into the block's children (e.g. milestones entries).
                if let Value::Clause(clause_children) = &lv.value {
                    let mut matched = false;
                    for (rule_type, _) in rules {
                        if let RuleType::ValueClauseRule { rules: vc_rules } = rule_type {
                            matched = true;
                            validate_children(clause_children, ast, vc_rules, enum_map, table, errors, file_path, scope_context, game, ruleset, type_index, modifier_keys);
                            break;
                        }
                    }
                    if !matched {
                        errors.push(ValidationError {
                            message: "Unexpected value clause '{...}'".to_string(),
                            severity: ErrorSeverity::Warning,
                            line: lv.pos.start.line,
                            col: lv.pos.start.col,
                            file: file_path.to_string(),
                            code: Some(error_codes::CW201_UNEXPECTED_FIELD.id.to_string()),
                        });
                    }
                } else {
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
                            code: Some(error_codes::CW201_UNEXPECTED_FIELD.id.to_string()),
                        });
                    }
                }
            }
            // Item 5: ValueClause validation
            Child::ValueClause(vcidx) => {
                let vc = &ast.arena.value_clauses[*vcidx as usize];
                let mut matched = false;
                for (rule_type, _opts) in rules {
                    if let RuleType::ValueClauseRule { rules: vc_rules } = rule_type {
                        matched = true;
                        validate_children(&vc.children, ast, vc_rules, enum_map, table, errors, file_path, scope_context, game, ruleset, type_index, modifier_keys);
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
                        code: Some(error_codes::CW201_UNEXPECTED_FIELD.id.to_string()),
                    });
                }
            }
            _ => {}
        }
    }

    // Cardinality enforcement. Report at the block's own location (its first
    // child) rather than line 0 — a missing required field belongs to THIS
    // entity (e.g. the specific decision), not the top of the file.
    let (block_line, block_col) = children.iter().find_map(|c| child_start_pos(c, ast)).unwrap_or((0, 0));
    for (rule_idx, (rule_type, opts)) in rules.iter().enumerate() {
        // Both under- and over-count default to a WARNING (config cardinalities are
        // often stricter than the game, and F# emits cardinality-max as a Warning);
        // an explicit `## severity` still wins.
        let card_sev = opts.severity.as_ref()
            .map(|s| severity_to_error(s.clone()))
            .unwrap_or(ErrorSeverity::Warning);
        let missing_sev = card_sev;
        let max_sev = card_sev;

        match rule_type {
            RuleType::LeafRule { .. } | RuleType::NodeRule { .. } => {
                if let Some(key) = get_rule_key(rule_type) {
                    let count = key_counts.get(&key).copied().unwrap_or(0) as i32;
                    if count < opts.min {
                        errors.push(ValidationError {
                            message: format!("Field '{}' appears {} time(s), expected at least {}", key, count, opts.min),
                            severity: missing_sev, line: block_line, col: block_col, file: file_path.to_string(),
                            code: Some(error_codes::CW203_CARDINALITY_MIN.id.to_string()),
                        });
                    }
                    if count > opts.max {
                        errors.push(ValidationError {
                            message: format!("Field '{}' appears {} time(s), expected at most {}", key, count, opts.max),
                            severity: max_sev, line: block_line, col: block_col, file: file_path.to_string(),
                            code: Some(error_codes::CW204_CARDINALITY_MAX.id.to_string()),
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
                        severity: missing_sev, line: block_line, col: block_col, file: file_path.to_string(),
                        code: Some(error_codes::CW203_CARDINALITY_MIN.id.to_string()),
                    });
                }
                if count > opts.max {
                    errors.push(ValidationError {
                        message: format!("LeafValue {:?} appears {} time(s), expected at most {}", right, count, opts.max),
                        severity: max_sev, line: block_line, col: block_col, file: file_path.to_string(),
                        code: Some(error_codes::CW204_CARDINALITY_MAX.id.to_string()),
                    });
                }
            }
            // Item 5: ValueClauseRule cardinality
            RuleType::ValueClauseRule { .. } => {
                let count = valueclause_counts[rule_idx] as i32;
                if count < opts.min {
                    errors.push(ValidationError {
                        message: format!("ValueClause appears {} time(s), expected at least {}", count, opts.min),
                        severity: missing_sev, line: block_line, col: block_col, file: file_path.to_string(),
                        code: Some(error_codes::CW203_CARDINALITY_MIN.id.to_string()),
                    });
                }
                if count > opts.max {
                    errors.push(ValidationError {
                        message: format!("ValueClause appears {} time(s), expected at most {}", count, opts.max),
                        severity: max_sev, line: block_line, col: block_col, file: file_path.to_string(),
                        code: Some(error_codes::CW204_CARDINALITY_MAX.id.to_string()),
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

fn rule_matches_leaf_key(rule_type: &RuleType, key: &str, ruleset: &RuleSet, type_index: Option<&cwtools_info::TypeIndex>) -> bool {
    match rule_type {
        // Cross-kind fallback: a NodeRule can also match a leaf key (e.g. alias blocks)
        RuleType::LeafRule { left, .. } | RuleType::NodeRule { left, .. } => field_matches_key(left, key, ruleset, type_index),
        _ => false,
    }
}

fn rule_matches_node_key(rule_type: &RuleType, key: &str, ruleset: &RuleSet, type_index: Option<&cwtools_info::TypeIndex>) -> bool {
    match rule_type {
        // Cross-kind fallback: a LeafRule can also match a node key
        RuleType::NodeRule { left, .. } | RuleType::LeafRule { left, .. } => field_matches_key(left, key, ruleset, type_index),
        _ => false,
    }
}

/// Whether a key is a scope-switching command — valid wherever an alias category
/// declares `alias[cat:scope_field]` (e.g. `ROOT = { ... }`, `SOV = { ... }`,
/// `FROM.owner = { ... }`, `event_target:x = { ... }`). Deep scope resolution is
/// the scope engine's job; here we just recognise the shape so the nested block
/// still gets validated instead of the whole key reading as unexpected.
fn looks_like_scope_command(key: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "THIS", "ROOT", "PREV", "FROM", "FROMFROM", "FROMFROMFROM", "FROMFROMFROMFROM",
        "PREVPREV", "PREVPREVPREV", "OWNER", "CONTROLLER", "CAPITAL", "OVERLORD",
    ];
    let upper = key.to_ascii_uppercase();
    if KEYWORDS.contains(&upper.as_str()) {
        return true;
    }
    // Scope chains (ROOT.owner) and prefixed refs (event_target:x, var:x).
    if key.contains('.') || key.contains(':') {
        return true;
    }
    // A bare numeric id opens a state/province scope: `642 = { ... }`.
    if !key.is_empty() && key.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    // Country tag: 2-4 chars, all uppercase letters/digits, at least one letter.
    let len = key.len();
    (2..=4).contains(&len)
        && key.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        && key.chars().any(|c| c.is_ascii_uppercase())
}

/// Whether `key` can open a scope in an effect/trigger block: a scope command
/// (ROOT/FROM/tag/id/chain) OR an instance of any type — HOI4 from-data scope
/// links let an instance (character, state, ideology, ...) open its own scope.
fn is_scope_key(key: &str, ruleset: &RuleSet, type_index: Option<&cwtools_info::TypeIndex>) -> bool {
    looks_like_scope_command(key)
        || ruleset.scope_links.contains(key)
        || type_index.is_some_and(|idx| idx.is_any_instance(key))
}

/// If `pattern` embeds a placeholder, test whether `key` matches: a literal
/// prefix and suffix around a member of the placeholder's set. Returns `None`
/// when there is no placeholder (the caller does a literal compare instead).
///
/// Placeholder forms (these appear in dynamic-modifier / scripted-* alias names):
///   `<type>` / `<type.subtype>` — an instance of `type` (subtype ignored)
///   `value[set]` / `value_set[set]` — a member of that value set
///   `enum[name]` — a member of that enum
fn alias_pattern_matches(
    pattern: &str,
    key: &str,
    ruleset: &RuleSet,
    type_index: Option<&cwtools_info::TypeIndex>,
) -> Option<bool> {
    // Locate the placeholder and split into (prefix, kind, name, suffix).
    let (pre, kind, name, suf): (&str, &str, &str, &str) = if let Some(open) = pattern.find('<') {
        let close = open + pattern[open..].find('>')?;
        (&pattern[..open], "type", &pattern[open + 1..close], &pattern[close + 1..])
    } else {
        // Bracketed forms — check the longer markers first so `enum[` doesn't
        // match inside `complex_enum[`, etc. Pick the earliest match in `pattern`.
        let markers = [
            ("value_set[", "value"),
            ("complex_enum[", "enum"),
            ("value[", "value"),
            ("enum[", "enum"),
        ];
        let mut found: Option<(usize, &str, &str, &str, &str)> = None;
        for (marker, kind) in markers {
            if let Some(open) = pattern.find(marker) {
                let inner = open + marker.len();
                let close = inner + pattern[inner..].find(']')?;
                if found.map_or(true, |(o, ..)| open < o) {
                    found = Some((open, &pattern[..open], kind, &pattern[inner..close], &pattern[close + 1..]));
                }
            }
        }
        let (_, p, k, n, s) = found?;
        (p, k, n, s)
    };

    if key.len() < pre.len() + suf.len() || !key.starts_with(pre) || !key.ends_with(suf) {
        return Some(false);
    }
    let middle = &key[pre.len()..key.len() - suf.len()];
    Some(match kind {
        "type" => {
            // `<type.subtype>` → check the base type (subtype is a refinement).
            let base = name.split('.').next().unwrap_or(name);
            type_index.map(|idx| idx.contains(base, middle)).unwrap_or(false)
        }
        "enum" => match ruleset.enums.iter().find(|e| e.key == name) {
            Some(def) if !def.values.is_empty() => def.values.iter().any(|v| v == middle),
            _ => true, // enum absent/empty (game-derived) — permissive
        },
        "value" => match ruleset.values.iter().find(|(n, _)| n == name) {
            Some((_, vs)) if !vs.is_empty() => vs.iter().any(|v| v == middle),
            _ => true, // value set not collected — permissive
        },
        _ => false,
    })
}

fn field_matches_key(field: &NewField, key: &str, ruleset: &RuleSet, type_index: Option<&cwtools_info::TypeIndex>) -> bool {
    match field {
        // Paradox script keys (field and command names) are case-insensitive — the
        // game lowercases them — so `Country_event` matches the `country_event`
        // rule. Values (tags, ids, enum members) stay case-sensitive; those are
        // handled by the value-typed arms below.
        NewField::SpecificField(s) => s.eq_ignore_ascii_case(key),
        NewField::AliasField(category) => {
            // Resolved through the precomputed alias index (ruleset.reindex()) so
            // this is O(1)+O(patterns) instead of a linear scan over every alias.
            // The name part can be a literal (`trigger:original_tag`), a `<type>`
            // reference (`trigger:<scripted_trigger>`, `modifier:..<building>..`),
            // or `scope_field` (any scope-switching key).
            let full = format!("{}:{}", category, key);
            if ruleset.alias_exact.contains_key(&full) {
                return true;
            }
            // Case-insensitive retry: command names like `Country_event` resolve to
            // the lowercase `country_event` alias (config alias names are lowercase).
            let lower = key.to_ascii_lowercase();
            if lower != key && ruleset.alias_exact.contains_key(&format!("{}:{}", category, lower)) {
                return true;
            }
            match ruleset.alias_categories.get(category.as_str()) {
                // Category has no aliases at all — be permissive (avoid floods).
                None => true,
                Some(cat) => {
                    for &idx in &cat.type_pattern_idxs {
                        let name = &ruleset.aliases[idx].0;
                        let rest = &name[category.len() + 1..];
                        if alias_pattern_matches(rest, key, ruleset, type_index) == Some(true) {
                            return true;
                        }
                    }
                    cat.scope_field_idx.is_some() && is_scope_key(key, ruleset, type_index)
                }
            }
        }
        NewField::SingleAliasField(alias_name) => {
            // SingleAliasField matches if the key is exactly this alias name.
            alias_name == key
        }
        // `key = ignore_field` wraps the key in IgnoreField — it matches the inner
        // field's key; the value is then accepted unvalidated (see the IgnoreField
        // short-circuit in validate_{leaf,node}_against_rule).
        NewField::IgnoreField(inner) => field_matches_key(inner, key, ruleset, type_index),
        NewField::IgnoreMarkerField => true,
        NewField::ScalarField => true,
        // A rule keyed by `enum[x] = ...`: the key must be a member of enum x.
        // If the enum isn't loaded (complex/game-derived enums), be permissive
        // rather than flag every key as unexpected.
        NewField::ValueField(ValueType::Enum(enum_name)) => {
            match ruleset.enums.iter().find(|e| &e.key == enum_name) {
                Some(def) => def.values.iter().any(|v| v == key),
                None => true,
            }
        }
        // Numeric-keyed rules: `ordered = { int = { ... } }` uses integer keys.
        NewField::ValueField(ValueType::Int { .. }) => key.parse::<i64>().is_ok(),
        NewField::ValueField(ValueType::Float { .. } | ValueType::Percent) => key.parse::<f64>().is_ok(),
        // `date_field = { ... }` (history dated blocks like `2000.1.1 = { ... }`).
        NewField::ValueField(ValueType::Date) => is_date_shape(key),
        NewField::ValueField(ValueType::DateTime) => is_datetime_shape(key),
        // Keys that reference a type instance (`<focus> = ...`), a scope, a
        // variable, a filepath/loc/icon, etc. CWT allows these on the left-hand
        // side. Existence is verified by other passes (type index, scope engine);
        // here we accept the key so the rule body still gets validated.
        NewField::TypeField(_)
        | NewField::ScopeField(_)
        | NewField::VariableField { .. }
        | NewField::VariableGetField(_)
        | NewField::VariableSetField(_)
        | NewField::ValueScopeField { .. }
        | NewField::ValueScopeMarkerField { .. }
        | NewField::LocalisationField { .. }
        | NewField::FilepathField { .. }
        | NewField::IconField(_)
        | NewField::AliasValueKeysField(_) => true,
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

/// Validate an aliased usage (`alias_name[cat] = ...`) against EVERY overload
/// declared as `alias[cat:key]`.
///
/// CWT lets the same alias name be defined many times (e.g. two
/// `alias[trigger:original_tag]` — one `scope[country]`, one `enum[country_tags]`
/// — or ~40 `alias[ai_strategy_rule:ai_strategy]` blocks keyed by `type`). A usage
/// is valid if it matches ANY overload (F# cwtools semantics). We therefore try
/// each candidate into a throwaway buffer and accept on the first clean match;
/// only when none match do we surface the closest (fewest-errors) candidate's
/// errors, which is also how the `type = ...` discriminator naturally wins.
#[allow(clippy::too_many_arguments)]
fn validate_alias_usage(
    category: &str,
    key: &str,
    leaf: Option<&cwtools_parser::ast::Leaf>,
    clause_children: Option<&[Child]>,
    ast: &ParsedFile,
    enum_map: &HashMap<&str, &EnumDefinition>,
    table: &StringTable,
    errors: &mut Vec<ValidationError>,
    file_path: &str,
    scope_context: &mut Option<ScopeContext>,
    game: Option<Game>,
    ruleset: &RuleSet,
    type_index: Option<&cwtools_info::TypeIndex>,
    modifier_keys: Option<&HashSet<String>>,
) {
    // Gather candidate overloads via the precomputed alias index (O(1) exact +
    // O(patterns)) rather than scanning every alias.
    let alias_key = format!("{}:{}", category, key);
    let mut overloads: Vec<&(RuleType, Options)> = Vec::new();
    if let Some(idxs) = ruleset.alias_exact.get(&alias_key) {
        for &i in idxs {
            overloads.push(&ruleset.aliases[i].1);
        }
    }
    if let Some(cat) = ruleset.alias_categories.get(category) {
        for &idx in &cat.type_pattern_idxs {
            let (name, rule) = &ruleset.aliases[idx];
            let rest = &name[category.len() + 1..];
            if alias_pattern_matches(rest, key, ruleset, type_index) == Some(true) {
                overloads.push(rule);
            }
        }
        if let Some(sf_idx) = cat.scope_field_idx {
            if is_scope_key(key, ruleset, type_index) {
                overloads.push(&ruleset.aliases[sf_idx].1);
            }
        }
    }
    if overloads.is_empty() {
        // Category unloaded or no such alias key — accept silently, matching the
        // permissive key-match in field_matches_key.
        return;
    }

    let mut best: Option<Vec<ValidationError>> = None;
    for (rule_type, opts) in overloads {
        let mut temp: Vec<ValidationError> = Vec::new();
        match rule_type {
            RuleType::LeafRule { .. } => {
                if let Some(leaf) = leaf {
                    validate_leaf(leaf, rule_type, table, enum_map, &mut temp, file_path, type_index);
                } else {
                    // Scalar-valued overload but the usage is a block — not a match.
                    temp.push(alias_mismatch_error(file_path));
                }
            }
            RuleType::NodeRule { rules: alias_inner, .. } => {
                let children = clause_children.or_else(|| match leaf.map(|l| &l.value) {
                    Some(Value::Clause(ch)) => Some(ch.as_slice()),
                    _ => None,
                });
                if let Some(children) = children {
                    let saved = scope_context.as_ref().map(|ctx| ctx.save());
                    if let Some(ctx) = scope_context.as_mut() {
                        if let Some(ref push) = opts.push_scope {
                            ctx.change_scope(push);
                        }
                        if let Some(ref replace) = opts.replace_scopes {
                            apply_replace_scopes(ctx, replace, game);
                        }
                    }
                    validate_children(children, ast, alias_inner, enum_map, table, &mut temp, file_path, scope_context, game, ruleset, type_index, modifier_keys);
                    if let (Some(saved), Some(ctx)) = (saved, scope_context.as_mut()) {
                        ctx.restore(saved);
                    }
                } else {
                    // Block overload but the usage is a scalar — not a match.
                    temp.push(alias_mismatch_error(file_path));
                }
            }
            _ => continue,
        }

        if temp.is_empty() {
            return; // clean match — accept with no errors
        }
        match &best {
            Some(b) if b.len() <= temp.len() => {}
            _ => best = Some(temp),
        }
    }

    if let Some(b) = best {
        errors.extend(b);
    }
}

/// Placeholder error used when an alias overload's shape (scalar vs block) can't
/// match the usage; it only ranks a candidate, it's never surfaced when a better
/// candidate exists.
fn alias_mismatch_error(file_path: &str) -> ValidationError {
    ValidationError {
        message: "value does not match alias".to_string(),
        severity: ErrorSeverity::Error,
        line: 0,
        col: 0,
        file: file_path.to_string(),
        code: Some(error_codes::CW202_INVALID_VALUE.id.to_string()),
    }
}

fn validate_leaf(
    leaf: &cwtools_parser::ast::Leaf,
    rule_type: &RuleType,
    table: &StringTable,
    enum_map: &HashMap<&str, &EnumDefinition>,
    errors: &mut Vec<ValidationError>,
    file_path: &str,
    type_index: Option<&cwtools_info::TypeIndex>,
) {
    if let RuleType::LeafRule { right, .. } = rule_type {
        // TypeField: check type_index when available (Item 1).
        if let NewField::TypeField(type_type) = right {
            // Unquote: `load_oob = "EU_frontex_basic_2017"` references the instance
            // `EU_frontex_basic_2017`; type instances are stored unquoted.
            let raw_value = leaf_value_to_string(&leaf.value, table);
            let value_str = raw_value
                .strip_prefix('"').and_then(|s| s.strip_suffix('"'))
                .unwrap_or(&raw_value)
                .to_string();
            let key = table.get_string(leaf.key.normal).unwrap_or_default();
            let type_name = match type_type {
                TypeType::Simple(n) => n.as_str(),
                TypeType::Complex { name, .. } => name.as_str(),
            };
            // Strip prefix/suffix for Complex TypeField before lookup.
            let lookup_value = match type_type {
                TypeType::Complex { prefix, suffix, .. } => {
                    let mut v = value_str.as_str();
                    if !prefix.is_empty() {
                        v = v.strip_prefix(prefix.as_str()).unwrap_or(v);
                    }
                    if !suffix.is_empty() {
                        v = v.strip_suffix(suffix.as_str()).unwrap_or(v);
                    }
                    v.to_string()
                }
                _ => value_str.clone(),
            };
            if let Some(idx) = type_index {
                // Only flag if we have at least one known instance for this type.
                // If zero instances, vanilla data probably isn't loaded — accept.
                if !idx.instances(type_name).is_empty() && !idx.contains(type_name, &lookup_value) {
                    errors.push(ValidationError {
                        message: format!(
                            "Field '{}' references '{}' which is not a known instance of type '{}'",
                            key, lookup_value, type_name
                        ),
                        severity: ErrorSeverity::Error,
                        line: leaf.pos.start.line,
                        col: leaf.pos.start.col,
                        file: file_path.to_string(),
                        code: Some(error_codes::CW500_TYPE_NOT_FOUND.id.to_string()),
                    });
                }
            }
            // TypeField is otherwise accepted (non-empty check done by field_matches_value).
            return;
        }

        if !field_matches_value(right, &leaf.value, table, enum_map) {
            let expected = field_to_description(right);
            let actual = leaf_value_to_string(&leaf.value, table);
            let key = table.get_string(leaf.key.normal).unwrap_or_default();
            errors.push(ValidationError {
                message: format!("Field '{}' has value '{}', expected {}", key, actual, expected),
                severity: ErrorSeverity::Error,
                line: leaf.pos.start.line, col: leaf.pos.start.col, file: file_path.to_string(),
                code: Some(error_codes::CW202_INVALID_VALUE.id.to_string()),
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

/// Text of a string token with surrounding double-quotes removed. Enum members
/// and rule literals are stored unquoted (the rules converter strips quotes), so
/// a quoted script value like `"MISSION_PATROL"` must be unquoted before matching.
/// Enum membership test. An absent or empty enum (members come from game data
/// that isn't statically loaded — provinces, ship_units, ...) is permissive.
fn enum_contains(enum_map: &HashMap<&str, &EnumDefinition>, enum_name: &str, value: &str) -> bool {
    match enum_map.get(enum_name) {
        Some(def) if !def.values.is_empty() => {
            if def.values.iter().any(|v| v == value) {
                return true;
            }
            // An enum whose members are `@`-prefixed scripted constants (e.g.
            // `enum[command_cap_increase] = { @tier1_cp_cap_increase ... }`) accepts
            // the resolved literal value too (`command_cap_increase = 10`), which we
            // can't resolve statically — be permissive.
            def.values.iter().any(|v| v.starts_with('@'))
        }
        _ => true,
    }
}

fn match_text(table: &StringTable, t: &StringTokens) -> String {
    let s = table.get_string(t.normal).unwrap_or_default();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1].to_string()
    } else {
        s
    }
}

/// Strip a balanced pair of surrounding double-quotes from a child key.
fn unquote_key(s: &str) -> &str {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

fn field_matches_value(field: &NewField, value: &Value, table: &StringTable, enum_map: &HashMap<&str, &EnumDefinition>) -> bool {
    // Item 2: VALUE-VALIDATOR BYPASSES (F# FieldValidators.fs:82-83, 836-839).
    // Before any type-specific checks, accept scripted variables (@...), localisation
    // references ($$), and inline math ([...]).  These are valid CW script idioms that
    // can legitimately appear in place of any typed value.
    match value {
        Value::String(t) | Value::QString(t) => {
            let text = match_text(table, t);
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
            let v = match_text(table, t).to_lowercase();
            v == "yes" || v == "no"
        }

        // --- Int with range enforcement (item 4) ---
        (NewField::ValueField(ValueType::Int { min, max }), Value::Int(v)) => {
            let v_i = *v as i32;
            v_i >= *min && v_i <= *max
        }
        (NewField::ValueField(ValueType::Int { min, max }), Value::String(t)) | (NewField::ValueField(ValueType::Int { min, max }), Value::QString(t)) => {
            let text = match_text(table, t);
            if let Ok(v) = text.parse::<i32>() {
                v >= *min && v <= *max
            } else {
                false
            }
        }

        // --- Float with range enforcement (item 4) ---
        (NewField::ValueField(ValueType::Float { min, max }), Value::Float(v)) => { *v >= *min && *v <= *max }
        // An integer literal is a valid float (the parser emits Int for `1000`).
        (NewField::ValueField(ValueType::Float { min, max }), Value::Int(v)) => { (*v as f64) >= *min && (*v as f64) <= *max }
        (NewField::ValueField(ValueType::Float { min, max }), Value::String(t)) | (NewField::ValueField(ValueType::Float { min, max }), Value::QString(t)) => {
            let text = match_text(table, t);
            if let Ok(v) = text.parse::<f64>() {
                v >= *min && v <= *max
            } else {
                false
            }
        }

        // --- Enum ---
        // An enum that is absent OR loaded-but-empty is one whose members come
        // from game data not statically available (provinces, ship_units, ...).
        // Be permissive there rather than flag every value. Integer members
        // (e.g. province ids) are compared by their string form.
        (NewField::ValueField(ValueType::Enum(enum_name)), Value::String(t))
        | (NewField::ValueField(ValueType::Enum(enum_name)), Value::QString(t)) => {
            let text = match_text(table, t);
            enum_contains(enum_map, enum_name, &text)
        }
        (NewField::ValueField(ValueType::Enum(enum_name)), Value::Int(i)) => {
            enum_contains(enum_map, enum_name, &i.to_string())
        }
        (NewField::ValueField(ValueType::Enum(enum_name)), Value::Float(f)) => {
            enum_contains(enum_map, enum_name, &f.to_string())
        }

        // --- Percent (item 3): value ends with '%' or is a number ---
        (NewField::ValueField(ValueType::Percent), Value::String(t)) | (NewField::ValueField(ValueType::Percent), Value::QString(t)) => {
            let text = match_text(table, t);
            text.ends_with('%') || text.parse::<f64>().is_ok()
        }
        (NewField::ValueField(ValueType::Percent), Value::Float(_) | Value::Int(_)) => true,

        // --- Date / DateTime (item 3): basic YYYY.MM.DD[.HH] shape ---
        (NewField::ValueField(ValueType::Date), Value::String(t)) | (NewField::ValueField(ValueType::Date), Value::QString(t)) => {
            is_date_shape(&match_text(table, t))
        }
        (NewField::ValueField(ValueType::DateTime), Value::String(t)) | (NewField::ValueField(ValueType::DateTime), Value::QString(t)) => {
            is_datetime_shape(&match_text(table, t))
        }

        // --- Ck2Dna (item 3): exactly 32 hex chars (F# FieldValidators.fs:194-204) ---
        (NewField::ValueField(ValueType::Ck2Dna), Value::String(t)) | (NewField::ValueField(ValueType::Ck2Dna), Value::QString(t)) => {
            let text = match_text(table, t);
            text.len() == 32 && text.chars().all(|c| c.is_ascii_hexdigit())
        }

        // --- Ck2DnaProperty (item 3): length 8 or 32, hex chars (F# FieldValidators.fs:205-211) ---
        (NewField::ValueField(ValueType::Ck2DnaProperty), Value::String(t)) | (NewField::ValueField(ValueType::Ck2DnaProperty), Value::QString(t)) => {
            let text = match_text(table, t);
            (text.len() == 8 || text.len() == 32) && text.chars().all(|c| c.is_ascii_hexdigit())
        }

        // --- IrFamilyName / StlNameFormat (item 3): accept any string ---
        (NewField::ValueField(ValueType::IrFamilyName), Value::String(_) | Value::QString(_)) => true,
        (NewField::ValueField(ValueType::StlNameFormat(_)), Value::String(_) | Value::QString(_)) => true,

        // --- Scalar: accept anything ---
        (NewField::ScalarField, _) => true,

        // --- SpecificField: exact string match ---
        (NewField::SpecificField(s), Value::String(t)) | (NewField::SpecificField(s), Value::QString(t)) => {
            match_text(table, t) == *s
        }
        // A `= yes` / `= no` rule literal is a SpecificField, but the parser emits
        // Bool for those values — match them up (affects every boolean rule field).
        (NewField::SpecificField(s), Value::Bool(b)) => {
            (s == "yes" && *b) || (s == "no" && !*b)
        }
        (NewField::SpecificField(s), Value::Int(i)) => s == &i.to_string(),

        // --- TypeField: accept string (cross-file existence is a separate pass) ---
        (NewField::TypeField(TypeType::Simple(type_name)), Value::String(t))
        | (NewField::TypeField(TypeType::Simple(type_name)), Value::QString(t)) => {
            validate_type_reference(&table.get_string(t.normal).unwrap_or_default(), type_name)
        }
        (NewField::TypeField(TypeType::Complex { name, .. }), Value::String(t))
        | (NewField::TypeField(TypeType::Complex { name, .. }), Value::QString(t)) => {
            validate_type_reference(&table.get_string(t.normal).unwrap_or_default(), name)
        }
        // Numeric type instances — state/province ids are written as bare integers
        // (`states = { 599 600 }`, `<state>`). Accept; existence is a separate pass.
        (NewField::TypeField(_), Value::Int(_) | Value::Float(_)) => true,

        // --- ScopeField ---
        // A scope slot (`scope[country]`, `scope[state]`, ...) is satisfied by far
        // more than the literal scope keywords: country tags (USA), state ids (410),
        // event_target/variable references, and scope chains. Deep resolution is the
        // scope engine's job; at the field level accept any non-empty token rather
        // than flag every tag/id as an error.
        (NewField::ScopeField(_), Value::String(t)) | (NewField::ScopeField(_), Value::QString(t)) => {
            !table.get_string(t.normal).unwrap_or_default().is_empty()
        }
        (NewField::ScopeField(_), Value::Int(_)) | (NewField::ScopeField(_), Value::Float(_)) => true,

        // --- VariableField with range enforcement (item 4) ---
        (NewField::VariableField { min, max, .. }, Value::Float(v)) => { *v >= *min && *v <= *max }
        (NewField::VariableField { min, max, .. }, Value::Int(v)) => { (*v as f64) >= *min && (*v as f64) <= *max }
        // yes/no are acceptable in variable contexts.
        (NewField::VariableField { .. }, Value::Bool(_)) => true,
        (NewField::VariableField { min, max, .. }, Value::String(t)) | (NewField::VariableField { min, max, .. }, Value::QString(t)) => {
            let text = match_text(table, t);
            if let Ok(v) = text.parse::<f64>() {
                v >= *min && v <= *max
            } else {
                // non-numeric string: accept (could be a scripted variable not caught by bypass)
                true
            }
        }

        // --- LocalisationField / FilepathField ---
        (NewField::LocalisationField { .. }, Value::String(_) | Value::QString(_)) => true,
        // A localisation slot also accepts the meta-localisation block form
        // `{ localization_key = X PARAM = value ... }` (used in tooltip,
        // custom_override_tooltip, etc.). Accept any clause here.
        (NewField::LocalisationField { .. }, Value::Clause(_)) => true,
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
