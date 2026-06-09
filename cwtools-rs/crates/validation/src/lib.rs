use cwtools_game::constants::Game;
use cwtools_game::scope_engine::{SCOPE_ANY, SCOPE_INVALID, ScopeContext, ScopeId, ScopeLink};
use cwtools_game::scope_registry::{ScopeDefOwned, ScopeRegistry};
use cwtools_localization::LocIndex;
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
    /// CW### error code, e.g. "CW262" for an unexpected property node.
    pub code: Option<String>,
}

pub use cwtools_error_codes::ErrorSeverity;
use cwtools_error_codes::ErrorCode;

impl ValidationError {
    /// Build a diagnostic from a catalog [`ErrorCode`]: pulls severity and id
    /// from the code and formats its template with `args`. Centralizes the
    /// code→severity mapping so call sites don't restate it.
    fn from_code(code: &ErrorCode, file: &str, line: u32, col: u16, args: &[&str]) -> Self {
        ValidationError {
            message: code.format(args),
            severity: code.severity,
            line,
            col,
            file: file.to_string(),
            code: Some(code.id.to_string()),
        }
    }
}

/// Iterate grandchildren of a skip_root_key wrapper and validate each one uniformly.
/// Both the Node-root and Leaf-root shapes delegate here so behaviour is identical.
#[allow(clippy::too_many_arguments)]
fn validate_wrapper_grandchildren(
    grandchildren: &[Child],
    type_def: &TypeDefinition,
    wrapper_root_key: &str,
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
    loc_index: Option<&LocIndex>,
) {
    for grandchild in grandchildren {
        // Pull the grandchild's key and body uniformly for Node and Leaf-clause.
        let (gc_key, gc_children): (String, &[Child]) = match grandchild {
            Child::Node(gc_idx) => {
                let gc_node = &ast.arena.nodes[*gc_idx as usize];
                (
                    table.get_string(gc_node.key.normal).unwrap_or_default(),
                    gc_node.children.as_slice(),
                )
            }
            Child::Leaf(gc_idx) => {
                let gc_leaf = &ast.arena.leaves[*gc_idx as usize];
                match &gc_leaf.value {
                    Value::Clause(gc_children) => (
                        table.get_string(gc_leaf.key.normal).unwrap_or_default(),
                        gc_children.as_slice(),
                    ),
                    // Non-clause scalar leaf inside wrapper: leave as-is (no error).
                    _ => continue,
                }
            }
            Child::LeafValue(idx) => {
                let lv = &ast.arena.leaf_values[*idx as usize];
                let value = leaf_value_to_string(&lv.value, table);
                errors.push(ValidationError::from_code(
                    &error_codes::CW264_UNEXPECTED_PROPERTY_LEAF_VALUE,
                    file_path,
                    lv.pos.start.line,
                    lv.pos.start.col,
                    &[&format!("Unexpected bare value '{}'", value)],
                ));
                continue;
            }
            _ => continue,
        };

        // A wrapper like `objectTypes` can hold instances of several types
        // (pdxmesh, pdxparticle, entity, …) that share a path; pick the type that
        // `## type_key_filter` assigns to THIS grandchild's key rather than
        // validating every grandchild against whichever type won the path lookup.
        let (gc_type_def, gc_rules) =
            match find_grandchild_type(file_path, wrapper_root_key, &gc_key, ruleset) {
                Some(t) => {
                    let r = find_rules_by_name(&t.name, ruleset);
                    let has_content =
                        !r.is_empty() || t.subtypes.iter().any(|st| !st.rules.is_empty());
                    // Resolved to an index-only type (no rule body): its fields
                    // are not content-validated, so don't flag them.
                    if !has_content {
                        continue;
                    }
                    (t, r)
                }
                // No better match. Only fall back to the wrapper's resolved type
                // when that type actually applies to THIS grandchild's key. A type
                // with `## type_key_filter = containerWindowType` must not validate
                // a sibling `scrollbarType`/`guiButtonType` (top-level widgets under
                // `guiTypes`) against the containerWindowType schema — F# excludes
                // them via the filter and leaves them unvalidated. Without this the
                // widgets' own fields (slider/track/priority/...) flag as CW201.
                None => {
                    if let Some((keys, negate)) = &type_def.type_key_filter {
                        let hit = keys.iter().any(|k| k.eq_ignore_ascii_case(&gc_key));
                        if hit == *negate {
                            continue;
                        }
                    }
                    (type_def, inner_rules)
                }
            };

        validate_with_type(
            gc_type_def,
            gc_children,
            ast,
            gc_rules,
            enum_map,
            table,
            errors,
            file_path,
            scope_context,
            game,
            ruleset,
            type_index,
            modifier_keys,
            loc_index,
            Some(&gc_key),
        );
    }
}

/// Validate a parsed file against the ruleset. Localisation-key checks
/// (CW100/CW122) are skipped; use [`validate_ast_with_loc`] to enable them.
pub fn validate_ast(
    ast: &ParsedFile,
    ruleset: &RuleSet,
    table: &StringTable,
    file_path: &str,
    game: Option<Game>,
    type_index: Option<&cwtools_info::TypeIndex>,
    modifier_keys: Option<&HashSet<String>>,
) -> Vec<ValidationError> {
    validate_ast_with_loc(
        ast,
        ruleset,
        table,
        file_path,
        game,
        type_index,
        modifier_keys,
        None,
    )
}

/// As [`validate_ast`], but with a loaded [`LocIndex`] so `LocalisationField`
/// references are checked for existence and scope-correct loc commands.
#[tracing::instrument(skip_all)]
#[allow(clippy::too_many_arguments)]
pub fn validate_ast_with_loc(
    ast: &ParsedFile,
    ruleset: &RuleSet,
    table: &StringTable,
    file_path: &str,
    game: Option<Game>,
    type_index: Option<&cwtools_info::TypeIndex>,
    modifier_keys: Option<&HashSet<String>>,
    loc_index: Option<&LocIndex>,
) -> Vec<ValidationError> {
    // Single-file/test entry point: build the per-run shared state (enum_map +
    // scope registry) here and delegate. Hot multi-file callers should instead
    // build these ONCE outside their loop and call `validate_ast_with_loc_prebuilt`.
    let enum_map = build_enum_map(ruleset);
    let registry = build_scope_registry_arc(ruleset, game);
    validate_ast_with_loc_prebuilt(
        ast,
        ruleset,
        table,
        file_path,
        game,
        type_index,
        modifier_keys,
        loc_index,
        registry.as_ref(),
        &enum_map,
    )
}

/// Build the `enum name -> definition` lookup used throughout validation. It
/// borrows from `ruleset`, so the caller must keep `ruleset` alive for the
/// returned map's lifetime. Cheap to call but pointless to repeat per file, so
/// hot multi-file loops build it once and reuse it.
pub fn build_enum_map(ruleset: &RuleSet) -> HashMap<&str, &EnumDefinition> {
    ruleset.enums.iter().map(|e| (e.key.as_str(), e)).collect()
}

/// Build the config-driven scope/link registry once, wrapped in an `Arc` so it
/// can be shared (cheaply cloned) across every file in a validation run. Returns
/// `None` when no game is set (no scope checks).
pub fn build_scope_registry_arc(
    ruleset: &RuleSet,
    game: Option<Game>,
) -> Option<std::sync::Arc<ScopeRegistry>> {
    game.map(|g| std::sync::Arc::new(build_scope_registry(ruleset, g)))
}

/// As [`validate_ast_with_loc`], but takes the per-run shared state (the scope
/// registry and `enum_map`) prebuilt so multi-file callers can construct them
/// ONCE and reuse them across every file instead of rebuilding per file.
#[tracing::instrument(skip_all)]
#[allow(clippy::too_many_arguments)]
pub fn validate_ast_with_loc_prebuilt(
    ast: &ParsedFile,
    ruleset: &RuleSet,
    table: &StringTable,
    file_path: &str,
    game: Option<Game>,
    type_index: Option<&cwtools_info::TypeIndex>,
    modifier_keys: Option<&HashSet<String>>,
    loc_index: Option<&LocIndex>,
    registry: Option<&std::sync::Arc<ScopeRegistry>>,
    enum_map: &HashMap<&str, &EnumDefinition>,
) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    // Scope-agnostic content is reused from many calling scopes (or operates on a
    // data-dependent element scope), so it can't be pinned to one. Seed ANY so its
    // body isn't scope-checked against an arbitrary default. Everything else starts
    // at the game's primary scope (HOI4 country = 100).
    //   - scripted_effects/triggers/localisation: called from any scope.
    //   - collections: the `limit`/`operators` run in the input element's scope
    //     (`game:all_states` -> state, `game:all_countries` -> country); per the
    //     HOI4 collections docs the element scope is data-dependent.
    //   - dynamic_modifiers: the `enable`/`remove_trigger` run in the scope the
    //     modifier is applied to (country, state, or unit leader; "root is the
    //     effect scope" per the HOI4 docs).
    let clean = file_path.to_ascii_lowercase().replace('\\', "/");
    let scope_agnostic = path_contains_segment(&clean, "scripted_effects")
        || path_contains_segment(&clean, "scripted_triggers")
        || path_contains_segment(&clean, "scripted_localisation")
        || path_contains_segment(&clean, "collections")
        || path_contains_segment(&clean, "dynamic_modifiers");
    let default_root = registry
        .and_then(|r| r.id_of("country"))
        .unwrap_or(ScopeId(100));
    let initial_scope = if scope_agnostic {
        SCOPE_ANY
    } else {
        default_root
    };
    let mut scope_context =
        registry.map(|r| ScopeContext::from_registry(std::sync::Arc::clone(r), initial_scope));

    // Pre-compute path-based type match (most specific wins)
    let path_type = find_type_by_path(file_path, ruleset);

    // type_per_file: the WHOLE file is a single instance of this type (e.g. an
    // OOB file). Its root children ARE the instance body — there is no per-entry
    // wrapper key — so validate them once against the type's rules and stop.
    if let Some(td) = path_type
        && td.type_per_file {
            let inner_rules = find_rules_by_name(&td.name, ruleset);
            let has_content_rules =
                !inner_rules.is_empty() || td.subtypes.iter().any(|st| !st.rules.is_empty());
            if has_content_rules {
                validate_with_type(
                    td,
                    &ast.root_children,
                    ast,
                    inner_rules,
                    enum_map,
                    table,
                    &mut errors,
                    file_path,
                    &mut scope_context,
                    game,
                    ruleset,
                    type_index,
                    modifier_keys,
                    loc_index,
                    None,
                );
            }
            if let Some(g) = game {
                errors.extend(per_game::run_game_validators(
                    ast, ruleset, table, file_path, g,
                ));
            }
            return errors;
        }

    for child in &ast.root_children {
        // 1. Try exact root key match (e.g. ai_strategy_plan = { ... })
        let exact_match = match child {
            Child::Node(node_idx) => {
                let node = &ast.arena.nodes[*node_idx as usize];
                let key = table.get_string(node.key.normal).unwrap_or_default();
                find_type_and_rules_for_file(&key, file_path, ruleset)
                    .map(|(td, rules)| (key.clone(), td, node.children.as_slice(), rules))
            }
            Child::Leaf(leaf_idx) => {
                let leaf = &ast.arena.leaves[*leaf_idx as usize];
                let key = table.get_string(leaf.key.normal).unwrap_or_default();
                if let Value::Clause(children) = &leaf.value {
                    find_type_and_rules_for_file(&key, file_path, ruleset)
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
            let has_content_rules =
                !inner_rules.is_empty() || type_def.subtypes.iter().any(|st| !st.rules.is_empty());
            // A type gated by skip_root_key only applies when the matched key is one
            // of its skip keys (i.e. the key IS the wrapper). If it declares
            // skip_root_key(s) but this key matches none, the name-match is spurious:
            // the type's instances live nested under its wrapper, not at a root key
            // equal to the type name (F# RulesHelpers.fs:98-112 only descends through
            // the skip wrapper, never treats a name-matching root as an instance).
            // Fall through to path matching so another type whose skip_root_key IS
            // this key (e.g. `terrain={}` -> graphical_terrain) can own it.
            let skip_gate_ok =
                type_def.skip_root_key.is_empty() || should_skip_root_key(&type_key, type_def);
            if has_content_rules && skip_gate_ok {
                // When the matched key is itself a skip_root_key wrapper for this
                // type (e.g. `ability = { force_attack = { ... } }` where the type
                // is `ability` AND skip_root_key = ability), the key is a wrapper,
                // not an instance: its children are the instances. Validate them as
                // grandchildren instead of treating them as the type's content.
                if should_skip_root_key(&type_key, type_def) {
                    validate_wrapper_grandchildren(
                        children,
                        type_def,
                        &type_key,
                        ast,
                        inner_rules,
                        enum_map,
                        table,
                        &mut errors,
                        file_path,
                        &mut scope_context,
                        game,
                        ruleset,
                        type_index,
                        modifier_keys,
                        loc_index,
                    );
                } else {
                    validate_with_type(
                        type_def,
                        children,
                        ast,
                        inner_rules,
                        enum_map,
                        table,
                        &mut errors,
                        file_path,
                        &mut scope_context,
                        game,
                        ruleset,
                        type_index,
                        modifier_keys,
                        loc_index,
                        Some(&type_key),
                    );
                }
                continue;
            }
            // matched by name but instance-only: fall through to path matching
        }

        // 2. Fallback: path-based matching.
        // Re-query with the actual root key so that a type with a matching
        // skip_root_key can beat a longer-path type that has no such
        // relationship (e.g. `pdxmesh { skip_root_key = objectTypes }` should
        // win over `light { path = gfx/entities }` for an objectTypes node).
        let child_root_key = match child {
            Child::Node(node_idx) => table
                .get_string(ast.arena.nodes[*node_idx as usize].key.normal)
                .unwrap_or_default(),
            Child::Leaf(leaf_idx) => table
                .get_string(ast.arena.leaves[*leaf_idx as usize].key.normal)
                .unwrap_or_default(),
            _ => String::new(),
        };
        let path_type_for_child =
            find_type_by_path_and_key(file_path, Some(&child_root_key), ruleset);
        if let Some(type_def) = path_type_for_child {
            let inner_rules = find_rules_by_name(&type_def.name, ruleset);

            // A `type[x] = { path = ... name_field = ... }` with no associated rule
            // body exists only to index instances of that type; its instances are
            // not content-validated. Skip when there is nothing to
            // validate against, otherwise every field reads as "unexpected".
            let has_content_rules =
                !inner_rules.is_empty() || type_def.subtypes.iter().any(|st| !st.rules.is_empty());
            if !has_content_rules {
                continue;
            }

            // If skip_root_key = any, the root node is a WRAPPER — validate its children individually.
            // A wrapper is ONLY signalled by skip_root_key. A subtype whose name
            // equals the root key is NOT a wrapper — that's the type_key_filter
            // discriminator pattern (e.g. `country_event = { ... }` selects the
            // `country_event` subtype of `event`); the node is the instance and
            // its children are the content, not a wrapper layer to skip.
            if should_skip_root_key(&child_root_key, type_def) {
                let grandchildren: &[Child] = match child {
                    Child::Node(node_idx) => &ast.arena.nodes[*node_idx as usize].children,
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
                validate_wrapper_grandchildren(
                    grandchildren,
                    type_def,
                    &child_root_key,
                    ast,
                    inner_rules,
                    enum_map,
                    table,
                    &mut errors,
                    file_path,
                    &mut scope_context,
                    game,
                    ruleset,
                    type_index,
                    modifier_keys,
                    loc_index,
                );
                continue;
            }

            // The type declares skip_root_key(s) but this root matches none of them:
            // the type does not apply to this root (skip_root_key gate). Skip it
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
                    validate_with_type(
                        type_def,
                        node.children.as_slice(),
                        ast,
                        inner_rules,
                        enum_map,
                        table,
                        &mut errors,
                        file_path,
                        &mut scope_context,
                        game,
                        ruleset,
                        type_index,
                        modifier_keys,
                        loc_index,
                        Some(&child_root_key),
                    );
                }
                Child::Leaf(leaf_idx) => {
                    let leaf = &ast.arena.leaves[*leaf_idx as usize];
                    if let Value::Clause(children) = &leaf.value {
                        validate_with_type(
                            type_def,
                            children.as_slice(),
                            ast,
                            inner_rules,
                            enum_map,
                            table,
                            &mut errors,
                            file_path,
                            &mut scope_context,
                            game,
                            ruleset,
                            type_index,
                            modifier_keys,
                            loc_index,
                            Some(&child_root_key),
                        );
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
/// Collect the base rules (non-SubtypeRule entries) plus the rules of every matching
/// subtype into a single merged list, then validate the children once against that union.
/// This means:
///   - cardinality is counted over the merged rule set, not per-subtype in isolation
///   - a field that exists in any matching subtype is not "unexpected"
///   - SubtypeRule entries that don't match are silently skipped
// Threads type/ruleset/scope context and output buffers through mutual
// recursion; a context struct here would churn the validation hot path.
#[allow(clippy::too_many_arguments)]
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
    loc_index: Option<&LocIndex>,
    node_key: Option<&str>,
) {
    if type_def.subtypes.is_empty() {
        let pre_count = errors.len();
        let saved = scope_context.as_ref().map(|ctx| ctx.save());
        if let Some(ctx) = scope_context.as_mut() {
            seed_root_scope(ctx, type_def, None, node_key, ruleset, game);
        }
        validate_children(
            children,
            ast,
            inner_rules,
            enum_map,
            table,
            errors,
            file_path,
            scope_context,
            game,
            ruleset,
            type_index,
            modifier_keys,
            loc_index,
            (0, 0),
        );
        if let (Some(saved), Some(ctx)) = (saved, scope_context.as_mut()) {
            ctx.restore(saved);
        }
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

    // Step 1: determine which subtypes match.
    // A subtype matches when:
    //   (a) type_key_field is None, OR the children contain a field whose key equals type_key_field
    //   (b) starts_with is None, OR (no-op here; starts_with filters by the node's OWN key which
    //       we don't have at this point — conservative: treat as matching)
    // Mutual-exclusion via only_if_not is applied after the initial pass.
    let mut matched_subtype_names: Vec<&str> = Vec::new();
    for subtype in &type_def.subtypes {
        if subtype_matches(
            subtype, children, ast, table, enum_map, node_key, type_index,
        ) {
            matched_subtype_names.push(subtype.name.as_str());
        }
    }
    // Apply only_if_not: remove a subtype if any of its only_if_not names are in the matched set.
    let all_names_copy: Vec<&str> = matched_subtype_names.clone();
    matched_subtype_names.retain(|name| {
        let st = type_def.subtypes.iter().find(|s| s.name == *name).unwrap();
        !st.only_if_not
            .iter()
            .any(|excl| all_names_copy.contains(&excl.as_str()))
    });

    // Step 2: collect base rules (non-SubtypeRule entries) + matching SubtypeRule entries.
    // Expand SubtypeRule(key, shouldMatch, cfs) based on whether key is in the
    // active subtypes list.
    //
    // Two sources of rules:
    //   (A) inner_rules — from a separate `type_name = { ... }` TypeRule in the ruleset.
    //       SubtypeRule entries inside it are expanded per the active subtype set.
    //   (B) type_def.subtypes[i].rules — rules stored directly on SubTypeDefinition.
    //       These are populated when the type is defined ONLY via `types = { type[x] = { subtype[y] = { ... } } }`
    //       with no separate `x = { subtype[y] = { ... } }` rule block.
    //
    // If inner_rules has SubtypeRule entries, use path (A).  Otherwise fall back to (B).
    let inner_has_subtype_rules = inner_rules
        .iter()
        .any(|(rt, _)| matches!(rt, RuleType::SubtypeRule { .. }));

    let mut merged: Vec<(RuleType, Options)> = Vec::new();
    if inner_has_subtype_rules {
        // Path A: expand SubtypeRule entries from inner_rules
        for (rule_type, opts) in inner_rules {
            match rule_type {
                RuleType::SubtypeRule {
                    name,
                    positive,
                    rules: st_rules,
                } => {
                    let is_active = matched_subtype_names.contains(&name.as_str());
                    let should_include = if *positive { is_active } else { !is_active };
                    if should_include {
                        // F# never enforces min cardinality for subtype-specific rules:
                        // checkCardinality is called on the parent array of SubtypeRule
                        // entries, which all hit the wildcard case.  Mirror that by
                        // zeroing min so subtype fields are validated when present but
                        // never required when absent.
                        merged.extend(st_rules.iter().map(|(rt, o)| {
                            let mut o2 = o.clone();
                            o2.min = 0;
                            (rt.clone(), o2)
                        }));
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
                // Same min=0 treatment as Path A.
                merged.extend(subtype.rules.iter().map(|(rt, o)| {
                    let mut o2 = o.clone();
                    o2.min = 0;
                    (rt.clone(), o2)
                }));
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
    let push_scope: Option<&str> = type_def
        .subtypes
        .iter()
        .filter(|s| matched_subtype_names.contains(&s.name.as_str()))
        .find_map(|s| s.push_scope.as_deref());

    let saved = scope_context.as_ref().map(|ctx| ctx.save());
    if let Some(ctx) = scope_context.as_mut() {
        seed_root_scope(ctx, type_def, push_scope, node_key, ruleset, game);
    }

    // Step 5: validate children once against the merged rule set.
    let pre_count = errors.len();
    validate_children(
        children,
        ast,
        &merged,
        enum_map,
        table,
        errors,
        file_path,
        scope_context,
        game,
        ruleset,
        type_index,
        modifier_keys,
        loc_index,
        (0, 0),
    );

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
fn find_type_and_rules<'a>(
    name: &str,
    ruleset: &'a RuleSet,
) -> Option<(&'a TypeDefinition, &'a [(RuleType, Options)])> {
    let type_def = ruleset.type_by_name.get(name).map(|&i| &ruleset.types[i])?;
    let rules = find_rules_by_name(name, ruleset);
    Some((type_def, rules))
}

/// True if `t` has no `path_extension` constraint, or `file_path` satisfies it.
fn type_extension_matches(file_path: &str, t: &TypeDefinition) -> bool {
    match &t.path_options.path_extension {
        None => true,
        Some(ext) => {
            let ext = ext.to_lowercase();
            let ext = ext.strip_prefix('.').unwrap_or(&ext);
            if ext.is_empty() {
                return true;
            }
            let path_lower = file_path.to_lowercase();
            let basename = path_lower.rsplit('/').next().unwrap_or(&path_lower);
            basename.rsplit('.').next().is_some_and(|e| e == ext)
        }
    }
}

/// Resolve a top-level entity's type by its root key, honoring `path_extension`.
///
/// The fast path matches the key against type NAMES (`find_type_and_rules`).
/// But several types can share a `## type_key_filter` + path and differ only by
/// `path_extension`: `music` is the `.txt` song lists while `musicasset` is the
/// `.asset` definitions, both keyed `music`. The by-name lookup always returns
/// `music`, so `.asset` bodies (name/file/volume) wrongly flag as unexpected and
/// `song` reads as missing. When the by-name type is gated to an extension the
/// file lacks, defer to the path/extension-aware resolver instead.
fn find_type_and_rules_for_file<'a>(
    name: &str,
    file_path: &str,
    ruleset: &'a RuleSet,
) -> Option<(&'a TypeDefinition, &'a [(RuleType, Options)])> {
    let by_name = find_type_and_rules(name, ruleset);
    if let Some((td, _)) = by_name {
        if type_extension_matches(file_path, td) {
            return by_name;
        }
        if let Some(t) = find_type_by_path_and_key(file_path, Some(name), ruleset) {
            return Some((t, find_rules_by_name(&t.name, ruleset)));
        }
    }
    by_name
}

/// Map a ScopeId to a human-readable name for validation purposes.
/// Build the runtime [`ScopeRegistry`] from a parsed config (`scopes.cwt` +
/// `links.cwt`). When the config carries no scope defs (e.g. a game without a
/// scopes.cwt), fall back to that game's hardcoded table. This is the bridge
/// that makes the scope engine data-driven.
fn build_scope_registry(ruleset: &RuleSet, game: Game) -> ScopeRegistry {
    if ruleset.scope_inputs.is_empty() {
        return ScopeRegistry::from_hardcoded(game);
    }
    let mut reg = ScopeRegistry::default();
    let mut next_id = 100u32;

    // Pass 1: assign ids and names. `any`/`all` -> sentinel ANY, `invalid` -> INVALID.
    for si in &ruleset.scope_inputs {
        let is_invalid = si.name.eq_ignore_ascii_case("invalid")
            || si.aliases.iter().any(|a| a.eq_ignore_ascii_case("invalid"));
        let is_any = si.name.eq_ignore_ascii_case("any")
            || si.aliases.iter().any(|a| a.eq_ignore_ascii_case("any"));
        let id = if is_invalid {
            SCOPE_INVALID
        } else if is_any {
            SCOPE_ANY
        } else {
            let id = ScopeId(next_id);
            next_id += 1;
            id
        };
        reg.by_name.insert(si.name.to_ascii_lowercase(), id);
        for a in &si.aliases {
            reg.by_name.insert(a.to_ascii_lowercase(), id);
        }
        if id != SCOPE_ANY && id != SCOPE_INVALID {
            reg.by_id.insert(
                id,
                ScopeDefOwned {
                    name: si.name.clone(),
                    aliases: si.aliases.clone(),
                    subscope_of: Vec::new(),
                },
            );
        }
    }

    // Pass 2: resolve subscope_of names -> ids (resolve first, then assign).
    for si in &ruleset.scope_inputs {
        let Some(id) = reg.id_of(&si.name) else {
            continue;
        };
        let parents: Vec<ScopeId> = si
            .is_subscope_of
            .iter()
            .filter_map(|n| reg.id_of(n))
            .collect();
        if let Some(def) = reg.by_id.get_mut(&id) {
            def.subscope_of = parents;
        }
    }

    // Links: resolve output/input scope names -> ids; prefix links go to a
    // separate list matched by key prefix.
    for li in &ruleset.link_inputs {
        let target = li.output_scope.as_deref().and_then(|n| reg.id_of(n));
        let valid: Vec<ScopeId> = li
            .input_scopes
            .iter()
            .filter_map(|n| reg.id_of(n))
            .collect();
        let link = ScopeLink {
            valid_scopes: valid,
            target,
            is_scope_change: target.is_some(),
            ignore_keys: Vec::new(),
        };
        match &li.prefix {
            Some(p) => reg.prefix_links.push((p.to_ascii_lowercase(), link)),
            None => {
                reg.links.insert(li.name.to_ascii_lowercase(), link);
            }
        }
    }

    // Synthesize the simple iterators (`every_/random_/any_/all_<scope>`), which
    // links.cwt doesn't list (F# synthesizes them too). Relation iterators
    // (`every_owned_state`, …) are handled by the alias `## push_scope` path.
    let scope_aliases: Vec<(String, ScopeId)> = reg
        .by_id
        .iter()
        .flat_map(|(id, def)| {
            def.aliases
                .iter()
                .filter(|a| a.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
                .map(move |a| (a.to_ascii_lowercase(), *id))
        })
        .collect();
    for (alias, id) in scope_aliases {
        for pre in ["every_", "random_", "any_", "all_"] {
            reg.links
                .entry(format!("{pre}{alias}"))
                .or_insert(ScopeLink {
                    valid_scopes: Vec::new(),
                    target: Some(id),
                    is_scope_change: true,
                    ignore_keys: Vec::new(),
                });
        }
    }

    reg
}

fn get_scope_name(scope: ScopeId, registry: &ScopeRegistry) -> String {
    registry.name_of(scope)
}

/// Number of significant decimal places in a numeric string; trailing zeros do
/// not count (`0.1230` has 3). Used for the CW270 32-bit precision check.
fn decimal_places(s: &str) -> usize {
    match s.split_once('.') {
        Some((_, frac)) => frac.trim_end_matches('0').len(),
        None => 0,
    }
}

/// Whether `key` names a scope (keyword, scope link, or iterator) rather than a
/// variable. A `variable_field` value naming a scope must not be flagged as an
/// unset variable (CW246).
fn resolves_as_scope_key(ctx: &ScopeContext, key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    matches!(
        k.as_str(),
        "this"
            | "root"
            | "prev"
            | "prevprev"
            | "prevprevprev"
            | "from"
            | "fromfrom"
            | "fromfromfrom"
            | "fromfromfromfrom"
    ) || ctx.registry.id_of(&k).is_some()
        || ctx.registry.links.contains_key(&k)
}

/// Whether the trigger/effect/target scope checks (CW104/105/106/243/244/245/248)
/// are on. Now ON by default (the scope engine is config-driven and accurate);
/// set `CWTOOLS_NO_SCOPE_CHECKS=1` as an escape hatch to turn them off.
fn scope_checks_enabled() -> bool {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var("CWTOOLS_NO_SCOPE_CHECKS").is_err());
    *ON
}

/// Whether the project-wide "variable has not been set" check (CW246) is on.
/// OFF by default: it needs a COMPLETE variable index, and a mod that defines
/// variables through dynamic `@`-concatenation or base-game scripts the index
/// hasn't collected would flood. Opt in with `CWTOOLS_VAR_CHECKS=1` once the
/// index is proven complete for a corpus. The local numeric checks (CW270/271)
/// run regardless of this gate.
fn var_checks_enabled() -> bool {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var("CWTOOLS_VAR_CHECKS").is_ok());
    *ON
}

/// True when a leaf value is numerically zero (`0`, `0.0`, `"0"`, …). Used by
/// the CW235 zero-modifier check.
fn value_is_zero(value: &Value) -> bool {
    match value {
        Value::Int(n) => *n == 0,
        Value::Float(f) => *f == 0.0,
        Value::String(_) | Value::QString(_) => false,
        _ => false,
    }
}

fn scope_matches_required(current: ScopeId, registry: &ScopeRegistry, required: &[String]) -> bool {
    // No restriction declared -> valid anywhere.
    if required.is_empty() {
        return true;
    }
    // `## scope = any` / `all` mean unrestricted (very common on triggers).
    if required
        .iter()
        .any(|s| s.eq_ignore_ascii_case("any") || s.eq_ignore_ascii_case("all"))
    {
        return true;
    }
    // Current scope is the open wildcard -> lenient.
    if current == SCOPE_ANY {
        return true;
    }
    // Current scope didn't resolve to a known name (scope tracking gap) -> be
    // lenient rather than emit a false wrong-scope error.
    if registry.name_of(current).starts_with("scope_") {
        return true;
    }
    // A requirement is satisfied if the current scope is that scope or a subscope
    // of it (e.g. `character` satisfies a `country` requirement). Unresolvable
    // requirement names are treated leniently.
    required.iter().any(|r| {
        registry
            .id_of(r)
            .is_none_or(|rid| registry.is_subscope_or_eq(current, rid))
    })
}

/// Validate a scope-target value (`owner`, `root.capital_scope`, …) by resolving
/// the chain from the current scope on a throwaway clone of the context:
/// - CW245 (error in target) if a link in the chain is used in the wrong scope;
/// - CW243 (target wrong scope) if the resolved scope doesn't satisfy the
///   field's expected scopes.
///
/// Lenient: empty, data-ref (tags/ids/`prefix:`), upper-case magic-word and
/// unresolved targets are accepted, so only genuine lower-case link chains are
/// checked. Gated with the other scope checks by the caller.
fn validate_scope_target(
    ctx: &ScopeContext,
    value: &str,
    expected: &[String],
    leaf: &cwtools_parser::ast::Leaf,
    file_path: &str,
    errors: &mut Vec<ValidationError>,
) {
    if value.is_empty() || looks_like_data_ref(value) {
        return;
    }
    let reg = ctx.registry.as_ref();
    // Only validate genuine scope fields: every expected scope name must resolve
    // in the registry. A garbage entry (e.g. `country].value[variable` from a
    // mis-parsed `scope[country].value[variable]`) means this isn't really a
    // scope target slot — skip rather than flag the value as an invalid target.
    if expected.iter().any(|s| {
        !s.eq_ignore_ascii_case("any") && !s.eq_ignore_ascii_case("all") && reg.id_of(s).is_none()
    }) {
        return;
    }
    let mut probe = ctx.clone();
    let (code, message) = match probe.change_scope(value) {
        cwtools_game::scope_engine::ScopeResult::WrongScope {
            command,
            current,
            expected: exp,
        } => {
            let exp_names: Vec<String> = exp.iter().map(|s| reg.name_of(*s)).collect();
            let code = &error_codes::CW245_ERROR_IN_TARGET;
            (
                code,
                code.format(&[&command, &reg.name_of(current), &exp_names.join(" or ")]),
            )
        }
        cwtools_game::scope_engine::ScopeResult::NewScope { scope, .. }
            if !expected.is_empty() && !scope_matches_required(scope, reg, expected) =>
        {
            let code = &error_codes::CW243_TARGET_WRONG_SCOPE;
            (
                code,
                code.format(&[value, &reg.name_of(scope), &expected.join(" or ")]),
            )
        }
        // The token is not a recognised link/var/value target at all. With the
        // full config link map loaded, NotFound is reliable -> CW244.
        cwtools_game::scope_engine::ScopeResult::NotFound => {
            let code = &error_codes::CW244_INVALID_TARGET;
            (code, code.format(&[value, &expected.join(" or ")]))
        }
        // NewScope-in-expected / VarFound / ValueFound / AnyScope -> lenient.
        _ => return,
    };
    errors.push(ValidationError {
        message,
        severity: code.severity,
        line: leaf.pos.start.line,
        col: leaf.pos.start.col,
        file: file_path.to_string(),
        code: Some(code.id.to_string()),
    });
}

/// Find the actual validation rules for a type by looking in root_rules.
fn find_rules_by_name<'a>(name: &str, ruleset: &'a RuleSet) -> &'a [(RuleType, Options)] {
    for rr in &ruleset.root_rules {
        if let RootRule::TypeRule(rule_name, (rule, _opts)) = rr
            && rule_name == name
                && let RuleType::NodeRule { rules, .. } = rule {
                    return rules.as_slice();
                }
    }
    &[]
}

/// The `Options` of a type's root rule (carries `## replace_scope` / `## push_scope`
/// that seed the instance's scope, e.g. the state-history `state` object).
fn find_type_rule_opts<'a>(name: &str, ruleset: &'a RuleSet) -> Option<&'a Options> {
    for rr in &ruleset.root_rules {
        if let RootRule::TypeRule(rule_name, (_rule, opts)) = rr
            && rule_name == name
        {
            return Some(opts);
        }
    }
    None
}

/// Seed the scope context for a type instance's body. Precedence:
/// 1. a matched subtype's `## push_scope`;
/// 2. the type's root rule `## push_scope` or `## replace_scope` (the
///    state-history `state` object uses `replace_scope = { this = state ... }`);
/// 3. the instance's own key when that's a scope link / data ref (`state = {…}`).
///
/// Caller must `save()` first and `restore()` after.
fn seed_root_scope(
    ctx: &mut ScopeContext,
    type_def: &TypeDefinition,
    subtype_push: Option<&str>,
    node_key: Option<&str>,
    ruleset: &RuleSet,
    game: Option<Game>,
) {
    if let Some(ps) = subtype_push {
        push_named_scope(ctx, ps);
        return;
    }
    let root_opts = find_type_rule_opts(&type_def.name, ruleset);
    if let Some(push) = root_opts.and_then(|o| o.push_scope.as_deref()) {
        push_named_scope(ctx, push);
    } else if let Some(replace) = root_opts.and_then(|o| o.replace_scopes.as_ref()) {
        apply_replace_scopes(ctx, replace, game);
    } else if let Some(k) = node_key {
        let before = ctx.scope_depth();
        ctx.change_scope(k);
        if ctx.scope_depth() == before && looks_like_data_ref(k) {
            ctx.push_scope(SCOPE_ANY);
        }
    }
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
    find_type_by_path_and_key(file_path, None, ruleset)
}

/// Like `find_type_by_path` but also considers the root key of the child
/// being validated. Types whose `skip_root_key` matches `root_key` are
/// given a large bonus, so they beat a longer-path type that has no
/// skip_root_key and would otherwise win on path length alone.
///
/// This mirrors F# behaviour where `type[pdxmesh] { skip_root_key = objectTypes }`
/// correctly wins over `type[light] { path = "gfx/entities" }` for
/// a `objectTypes = { pdxmesh = { ... } }` root node in a `.gfx` file.
fn find_type_by_path_and_key<'a>(
    file_path: &str,
    root_key: Option<&str>,
    ruleset: &'a RuleSet,
) -> Option<&'a TypeDefinition> {
    let path_lower = file_path.to_lowercase();
    let basename = path_lower.rsplit('/').next().unwrap_or(&path_lower);
    // The file's directory (no filename, no trailing slash).
    let dir = path_lower
        .strip_suffix(basename)
        .unwrap_or(&path_lower)
        .trim_end_matches('/');
    let mut best: Option<&TypeDefinition> = None;
    let mut best_len = 0usize;

    for t in &ruleset.types {
        // path_file pins the type to one specific filename (e.g. several types
        // share path "map" but only airports.txt is the `airports` type).
        if let Some(pf) = &t.path_options.path_file
            && basename != pf.to_lowercase() {
                continue;
            }
        // path_extension restricts the type to files with a given extension
        // (e.g. sound types require `.asset`, so a `.txt` combat-sounds file must
        // NOT match them). Treat the extension as a hard filter.
        if let Some(ext) = &t.path_options.path_extension {
            let ext = ext.to_lowercase();
            let ext = ext.strip_prefix('.').unwrap_or(&ext);
            if basename.rsplit('.').next().is_none_or(|e| e != ext) {
                continue;
            }
        }
        // `## type_key_filter` gates a NON-wrapper type to nodes whose own key
        // satisfies the filter: a top-level `animation = { ... }` node is only an
        // instance of `type[model_animation] { type_key_filter = animation }`, not
        // of `type[light]` that merely shares the path. A matching filter also
        // earns a bonus so the filtered type beats an unfiltered one on the same
        // path. (For skip_root_key wrappers the filter applies to GRANDCHILDREN,
        // handled in validate_wrapper_grandchildren, so it is not gated here.)
        let tkf_bonus = match (root_key, t.skip_root_key.is_empty(), &t.type_key_filter) {
            (Some(rk), true, Some((keys, negate))) => {
                let hit = keys.iter().any(|k| k.eq_ignore_ascii_case(rk));
                if hit != *negate {
                    5_000
                } else {
                    continue; // filter excludes this key: the type does not apply
                }
            }
            _ => 0,
        };
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
            // A skip_root_key match for the current root key gets a large bonus
            // so that e.g. `type[pdxmesh] { skip_root_key = objectTypes }` beats
            // `type[light] { path = "gfx/entities" }` for an objectTypes node.
            let skip_key_bonus = if let Some(rk) = root_key {
                if should_skip_root_key(rk, t) {
                    10_000
                } else {
                    0
                }
            } else {
                0
            };
            let weight = p_lower.len()
                + skip_key_bonus
                + tkf_bonus
                + if t.path_options.path_file.is_some() {
                    1000
                } else {
                    0
                };
            if matches && weight > best_len {
                best = Some(t);
                best_len = weight;
            }
        }
    }
    best
}

/// True if `t`'s `path_options` select `file_path`. Mirrors the per-path test in
/// [`find_type_by_path_and_key`] without the scoring, for use when several types
/// share a path.
fn type_path_matches(file_path: &str, t: &TypeDefinition) -> bool {
    let path_lower = file_path.to_lowercase();
    let basename = path_lower.rsplit('/').next().unwrap_or(&path_lower);
    let dir = path_lower
        .strip_suffix(basename)
        .unwrap_or(&path_lower)
        .trim_end_matches('/');
    if let Some(pf) = &t.path_options.path_file
        && basename != pf.to_lowercase() {
            return false;
        }
    if let Some(ext) = &t.path_options.path_extension {
        let ext = ext.to_lowercase();
        let ext = ext.strip_prefix('.').unwrap_or(&ext);
        if basename.rsplit('.').next().is_none_or(|e| e != ext) {
            return false;
        }
    }
    t.path_options.paths.iter().any(|p| {
        let p_lower = p.to_lowercase();
        if t.path_options.path_strict {
            dir == p_lower || dir.ends_with(&format!("/{}", p_lower))
        } else {
            path_contains_segment(dir, &p_lower)
        }
    })
}

/// Resolve which type a `skip_root_key` wrapper's grandchild belongs to, by the
/// grandchild's own key. Several types can share a path AND `skip_root_key`
/// (e.g. `pdxmesh`, `pdxparticle`, `entity` all sit under `objectTypes` in `.gfx`
/// files); `## type_key_filter` is what disambiguates them. Prefer a candidate
/// whose filter selects `gc_key`; otherwise fall back to a wrapper type that has
/// no filter. Returns `None` when nothing fits, in which case the caller keeps
/// the type that won the path lookup (so single-type wrappers are unaffected).
fn find_grandchild_type<'a>(
    file_path: &str,
    wrapper_root_key: &str,
    gc_key: &str,
    ruleset: &'a RuleSet,
) -> Option<&'a TypeDefinition> {
    let mut generic: Option<&TypeDefinition> = None;
    for t in &ruleset.types {
        if !should_skip_root_key(wrapper_root_key, t) || !type_path_matches(file_path, t) {
            continue;
        }
        match &t.type_key_filter {
            Some((keys, negative)) => {
                let in_list = keys.iter().any(|k| k.eq_ignore_ascii_case(gc_key));
                // `negative` = `## type_key_filter <> ...` (exclude); otherwise include.
                if in_list != *negative {
                    return Some(t);
                }
            }
            None => {
                if generic.is_none() {
                    generic = Some(t);
                }
            }
        }
    }
    generic
}

/// Test whether a subtype's rules are satisfied by an entity's children.
///
/// A subtype is active unless one of its rules is violated:
///   - a required rule (min >= 1) whose key is absent (or under-count),
///   - a key present more than its max,
///   - a PRESENT field whose value doesn't match the rule.
///
/// Fields the rules don't mention are ignored, so a subtype whose rules are
/// all optional (`## cardinality = 0..1`) and absent matches vacuously.
/// The real discriminators are the un-annotated rules (default `1..1`, required)
/// and any present field whose value contradicts a rule.
fn subtype_rules_match(
    rules: &[(RuleType, Options)],
    children: &[Child],
    ast: &ParsedFile,
    table: &StringTable,
    enum_map: &HashMap<&str, &EnumDefinition>,
    type_index: Option<&cwtools_info::TypeIndex>,
) -> bool {
    // A subtype with discriminators must be *positively activated* by the entity:
    // A subtype matches when its rules apply cleanly, but one whose
    // discriminators are all optional (`0..1`) and absent would otherwise match
    // every entity and wrongly impose its required body fields. So we additionally
    // require some discriminator to be actively met. A present field that fails a
    // discriminator still *blocks* the match (contradiction), and a missing
    // required (`min>=1`) discriminator still fails it.
    //
    // Discriminators are grouped by key. Several rules can share a key as a
    // disjunction — both same-kind (`trait_type = assignable_trait` / `trait_type =
    // assignable_terrain_trait`) and cross-kind (`type = enum[air_units]` as a leaf
    // OR `type = { enum[air_units] }` as a block). Cardinality is counted by key
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
            RuleType::LeafRule {
                left: NewField::SpecificField(k),
                right,
            } => {
                groups
                    .entry(k.as_str())
                    .or_default()
                    .leaf_rights
                    .push((right, opts));
            }
            RuleType::NodeRule {
                left: NewField::SpecificField(k),
                rules: inner,
            } => {
                groups
                    .entry(k.as_str())
                    .or_default()
                    .node_inners
                    .push((inner.as_slice(), opts));
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
            let (matches_key, leaf_value, clause): (bool, Option<&Value>, Option<&[Child]>) =
                match c {
                    Child::Leaf(idx) => {
                        let leaf = &ast.arena.leaves[*idx as usize];
                        if table
                            .with_string(leaf.key.normal, |s| s == *k)
                            .unwrap_or(false)
                        {
                            match &leaf.value {
                                Value::Clause(ch) => (true, None, Some(ch.as_slice())),
                                v => (true, Some(v), None),
                            }
                        } else {
                            (false, None, None)
                        }
                    }
                    Child::Node(idx) => {
                        let node = &ast.arena.nodes[*idx as usize];
                        if table
                            .with_string(node.key.normal, |s| s == *k)
                            .unwrap_or(false)
                        {
                            (true, None, Some(node.children.as_slice()))
                        } else {
                            (false, None, None)
                        }
                    }
                    _ => (false, None, None),
                };
            if !matches_key {
                continue;
            }
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
            if let Some(ic) = clause
                && group.node_inners.iter().any(|(inner, _)| {
                    subtype_rules_match(inner, ic, ast, table, enum_map, type_index)
                }) {
                    any_match = true;
                    activated = true;
                }
        }
        // Present but matching none of the disjuncts (of the applicable kind) → contradiction.
        if count > 0 && !any_match {
            return false;
        }
        // Cardinality is counted by key across both kinds: required if any disjunct
        // demands it, capped by the tightest max.
        let all_opts = group
            .leaf_rights
            .iter()
            .map(|(_, o)| *o)
            .chain(group.node_inners.iter().map(|(_, o)| *o));
        let min_required = all_opts.clone().map(|o| o.min).max().unwrap_or(0);
        let max_allowed = all_opts.map(|o| o.max).min().unwrap_or(i32::MAX);
        if min_required > count || count > max_allowed {
            return false;
        }
        // Absent but a disjunct is the field's default value (`= no`/`false`/`0`).
        if count == 0
            && group
                .leaf_rights
                .iter()
                .any(|(r, _)| is_default_satisfied_literal(r))
        {
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

/// Decide whether a subtype is active for an entity.
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
        return node_key.is_some_and(|k| {
            subtype
                .type_key_filter
                .iter()
                .any(|f| f.eq_ignore_ascii_case(k))
        });
    }
    if let Some(fk) = &subtype.type_key_field {
        return children
            .iter()
            .any(|c| child_key_matches(c, ast, table, fk));
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
        Child::Leaf(i) => {
            let l = &ast.arena.leaves[*i as usize];
            Some((l.pos.start.line, l.pos.start.col))
        }
        Child::Node(i) => {
            let n = &ast.arena.nodes[*i as usize];
            Some((n.pos.start.line, n.pos.start.col))
        }
        Child::LeafValue(i) => {
            let lv = &ast.arena.leaf_values[*i as usize];
            Some((lv.pos.start.line, lv.pos.start.col))
        }
        Child::ValueClause(i) => {
            let vc = &ast.arena.value_clauses[*i as usize];
            Some((vc.pos.start.line, vc.pos.start.col))
        }
        _ => None,
    }
}

fn child_key_matches(
    child: &Child,
    ast: &ParsedFile,
    table: &StringTable,
    filter_key: &str,
) -> bool {
    match child {
        Child::Leaf(idx) => {
            let leaf = &ast.arena.leaves[*idx as usize];
            table
                .with_string(leaf.key.normal, |s| s == filter_key)
                .unwrap_or(false)
        }
        Child::Node(idx) => {
            let node = &ast.arena.nodes[*idx as usize];
            table
                .with_string(node.key.normal, |s| s == filter_key)
                .unwrap_or(false)
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
        RuleType::LeafRule {
            left: NewField::IgnoreField(_),
            ..
        } | RuleType::NodeRule {
            left: NewField::IgnoreField(_),
            ..
        }
    )
}

#[allow(clippy::too_many_arguments)]
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
    loc_index: Option<&LocIndex>,
) {
    // `key = ignore_field`: the field's value is accepted unvalidated.
    if rule_left_is_ignore(rule_type) {
        return;
    }
    if let Some(ctx) = scope_context.as_ref()
        && let Some(current) = ctx.current()
        && !opts.required_scopes.is_empty()
        && !scope_matches_required(current, ctx.registry.as_ref(), &opts.required_scopes)
    {
        // F# `ConfigRulesRuleWrongScope` (CW247): a trigger/effect/modifier rule
        // used in a scope it doesn't support. (Was the Rust-invented CW400.)
        let code = &error_codes::CW247_RULE_WRONG_SCOPE;
        errors.push(ValidationError::from_code(
            code,
            file_path,
            leaf.pos.start.line,
            leaf.pos.start.col,
            &[
                key,
                &get_scope_name(current, ctx.registry.as_ref()),
                &opts.required_scopes.join(" or "),
            ],
        ));
    }
    match rule_type {
        RuleType::LeafRule { left, .. } => {
            if let NewField::AliasField(category) = left {
                validate_alias_usage(
                    category,
                    key,
                    Some(leaf),
                    None,
                    ast,
                    enum_map,
                    table,
                    errors,
                    file_path,
                    scope_context,
                    game,
                    ruleset,
                    type_index,
                    modifier_keys,
                    loc_index,
                );
            } else {
                validate_leaf(
                    leaf,
                    rule_type,
                    table,
                    enum_map,
                    errors,
                    file_path,
                    type_index,
                    scope_context.as_ref(),
                    game,
                    loc_index,
                );
            }
        }
        RuleType::NodeRule {
            left,
            rules: inner_rules,
            ..
        } => {
            if let NewField::AliasField(category) = left {
                validate_alias_usage(
                    category,
                    key,
                    Some(leaf),
                    None,
                    ast,
                    enum_map,
                    table,
                    errors,
                    file_path,
                    scope_context,
                    game,
                    ruleset,
                    type_index,
                    modifier_keys,
                    loc_index,
                );
            } else if let Value::Clause(clause_children) = &leaf.value {
                let saved = scope_context.as_ref().map(|ctx| ctx.save());
                if let Some(ctx) = scope_context.as_mut() {
                    enter_block_scope(ctx, key, opts, game);
                }
                validate_children(
                    clause_children,
                    ast,
                    inner_rules,
                    enum_map,
                    table,
                    errors,
                    file_path,
                    scope_context,
                    game,
                    ruleset,
                    type_index,
                    modifier_keys,
                    loc_index,
                    (leaf.pos.start.line, leaf.pos.start.col),
                );
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
    loc_index: Option<&LocIndex>,
) {
    // `key = ignore_field`: the block is accepted unvalidated.
    if rule_left_is_ignore(rule_type) {
        return;
    }
    if let Some(ctx) = scope_context.as_ref()
        && let Some(current) = ctx.current()
        && !opts.required_scopes.is_empty()
        && !scope_matches_required(current, ctx.registry.as_ref(), &opts.required_scopes)
    {
        // F# `ConfigRulesRuleWrongScope` (CW247); see the leaf site above.
        let code = &error_codes::CW247_RULE_WRONG_SCOPE;
        errors.push(ValidationError::from_code(
            code,
            file_path,
            node.pos.start.line,
            node.pos.start.col,
            &[
                key,
                &get_scope_name(current, ctx.registry.as_ref()),
                &opts.required_scopes.join(" or "),
            ],
        ));
    }
    if let RuleType::NodeRule {
        left,
        rules: inner_rules,
        ..
    } = rule_type
    {
        if let NewField::AliasField(category) = left {
            validate_alias_usage(
                category,
                key,
                None,
                Some(&node.children),
                ast,
                enum_map,
                table,
                errors,
                file_path,
                scope_context,
                game,
                ruleset,
                type_index,
                modifier_keys,
                loc_index,
            );
        } else {
            let saved = scope_context.as_ref().map(|ctx| ctx.save());
            if let Some(ctx) = scope_context.as_mut() {
                enter_block_scope(ctx, key, opts, game);
            }
            validate_children(
                &node.children,
                ast,
                inner_rules,
                enum_map,
                table,
                errors,
                file_path,
                scope_context,
                game,
                ruleset,
                type_index,
                modifier_keys,
                loc_index,
                (node.pos.start.line, node.pos.start.col),
            );
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
    let all: Vec<&(RuleType, Options)> = rules
        .iter()
        .filter(|(rt, _)| matcher(rt, key, ruleset, type_index))
        .collect();
    let specific: Vec<&(RuleType, Options)> = all
        .iter()
        .filter(|(rt, _)| {
            matches!(rt,
            RuleType::LeafRule { left: NewField::SpecificField(s), .. }
            | RuleType::NodeRule { left: NewField::SpecificField(s), .. } if s == key)
        })
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
        if let RuleType::SubtypeRule {
            rules: st_rules, ..
        } = rt
        {
            out.extend(flatten_nested_subtype_rules(st_rules));
        } else {
            out.push((rt.clone(), opts.clone()));
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
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
    loc_index: Option<&LocIndex>,
    // Position of the block that owns `children` (its opening `key = {`). Used to
    // anchor cardinality diagnostics when the block is empty — so a missing
    // required field reports on the block's line, not at the file root (0,0).
    block_pos: (u32, u16),
) {
    // Nested subtype blocks (a `subtype[x] = {...}` not at the entity root) carry
    // their fields inside SubtypeRule entries that the candidate matcher below
    // doesn't see. Flatten them in — but only pay the clone when any are present,
    // since this is a hot path called for every block.
    let flattened;
    let rules: &[(RuleType, Options)] = if rules
        .iter()
        .any(|(rt, _)| matches!(rt, RuleType::SubtypeRule { .. }))
    {
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
                // Paradox keys are case-insensitive; key the counts in lowercase so
                // a field written `texturefile` satisfies a rule keyed `textureFile`.
                let key = unquote_key(&table.get_string(leaf.key.normal).unwrap_or_default())
                    .to_lowercase();
                *key_counts.entry(key).or_insert(0) += 1;
            }
            Child::Node(idx) => {
                let node = &ast.arena.nodes[*idx as usize];
                let key = unquote_key(&table.get_string(node.key.normal).unwrap_or_default())
                    .to_lowercase();
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
                    // independently (checkCardinality is a per-rule sum). Breaking on the first match lets
                    // a permissive earlier alternative (e.g. a `<type>` TypeField,
                    // which accepts any token) starve a later `enum[...]` rule,
                    // producing a spurious "appears 0 time(s)" cardinality error.
                    for (rule_idx, (rule_type, _)) in rules.iter().enumerate() {
                        if let RuleType::LeafValueRule { right } = rule_type
                            && field_matches_value(right, &lv.value, table, enum_map) {
                                leafvalue_counts[rule_idx] += 1;
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
                let key =
                    unquote_key(&table.get_string(leaf.key.normal).unwrap_or_default()).to_string();
                let candidates =
                    matching_candidates(rules, &key, ruleset, type_index, rule_matches_leaf_key);
                if candidates.is_empty() {
                    // Item 5: dynamic modifier keys — if provided and this key is a
                    // known modifier, accept silently (modifier context mechanism).
                    let is_modifier = modifier_keys.map(|mk| mk.contains(&key)).unwrap_or(false);
                    // CW235 (F# `ZeroModifier`): a known modifier set to 0 is a no-op
                    // (modifiers are additive). Only fires on confirmed modifiers.
                    if is_modifier && value_is_zero(&leaf.value) {
                        let code = &error_codes::CW235_ZERO_MODIFIER;
                        errors.push(ValidationError::from_code(
                            code,
                            file_path,
                            leaf.pos.start.line,
                            leaf.pos.start.col,
                            &[&key],
                        ));
                    }
                    // A `@name = value` leaf is a Paradox read-time variable
                    // definition, valid anywhere in a block. F# skips these from the
                    // unexpected-field check (RuleValidationService.fs:266,
                    // `leaf.Key.[0] <> '@'`).
                    let is_define = key.starts_with('@');
                    if !is_modifier && !is_define {
                        // This parser stores `key = { ... }` as a Leaf with a
                        // Clause value, so split the F# way: a clause value is an
                        // unexpected property NODE (CW262), a scalar value an
                        // unexpected property LEAF (CW263).
                        let (msg, code) = if matches!(leaf.value, Value::Clause(_)) {
                            (
                                format!("Unexpected block '{}'", key),
                                &error_codes::CW262_UNEXPECTED_PROPERTY_NODE,
                            )
                        } else {
                            (
                                format!("Unexpected field '{}'", key),
                                &error_codes::CW263_UNEXPECTED_PROPERTY_LEAF,
                            )
                        };
                        errors.push(ValidationError {
                            message: msg,
                            severity: ErrorSeverity::Error,
                            line: leaf.pos.start.line,
                            col: leaf.pos.start.col,
                            file: file_path.to_string(),
                            code: Some(code.id.to_string()),
                        });
                    }
                } else {
                    // An overloaded key (several rules with the same key, e.g. two
                    // `province = { ... }` forms) is a disjunction — accept if any
                    // candidate validates cleanly.
                    let n = candidates.len();
                    pick_best_candidate(
                        |i, out| {
                            let (rt, opts) = candidates[i];
                            validate_leaf_against_rule(
                                leaf,
                                &key,
                                rt,
                                opts,
                                ast,
                                enum_map,
                                table,
                                out,
                                file_path,
                                scope_context,
                                game,
                                ruleset,
                                type_index,
                                modifier_keys,
                                loc_index,
                            );
                        },
                        errors,
                        n,
                    );
                }
            }
            Child::Node(idx) => {
                let node = &ast.arena.nodes[*idx as usize];
                let key =
                    unquote_key(&table.get_string(node.key.normal).unwrap_or_default()).to_string();
                let candidates =
                    matching_candidates(rules, &key, ruleset, type_index, rule_matches_node_key);
                if candidates.is_empty() {
                    // Item 5: dynamic modifier keys — accept known modifier block keys silently.
                    let is_modifier = modifier_keys.map(|mk| mk.contains(&key)).unwrap_or(false);
                    if !is_modifier {
                        errors.push(ValidationError::from_code(
                            &error_codes::CW262_UNEXPECTED_PROPERTY_NODE,
                            file_path,
                            node.pos.start.line,
                            node.pos.start.col,
                            &[&format!("Unexpected block '{}'", key)],
                        ));
                    }
                } else {
                    let n = candidates.len();
                    pick_best_candidate(
                        |i, out| {
                            let (rt, opts) = candidates[i];
                            validate_node_against_rule(
                                node,
                                &key,
                                rt,
                                opts,
                                ast,
                                enum_map,
                                table,
                                out,
                                file_path,
                                scope_context,
                                game,
                                ruleset,
                                type_index,
                                modifier_keys,
                                loc_index,
                            );
                        },
                        errors,
                        n,
                    );
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
                            validate_children(
                                clause_children,
                                ast,
                                vc_rules,
                                enum_map,
                                table,
                                errors,
                                file_path,
                                scope_context,
                                game,
                                ruleset,
                                type_index,
                                modifier_keys,
                                loc_index,
                                (lv.pos.start.line, lv.pos.start.col),
                            );
                            break;
                        }
                    }
                    if !matched {
                        errors.push(ValidationError::from_code(
                            &error_codes::CW265_UNEXPECTED_PROPERTY_VALUE_CLAUSE,
                            file_path,
                            lv.pos.start.line,
                            lv.pos.start.col,
                            &["Unexpected value clause '{...}'"],
                        ));
                    }
                } else {
                    let mut matched = false;
                    for (rule_type, _opts) in rules {
                        if let RuleType::LeafValueRule { right } = rule_type
                            && field_matches_value(right, &lv.value, table, enum_map) {
                                matched = true;
                                break;
                            }
                    }
                    if !matched {
                        let val_str = leaf_value_to_string(&lv.value, table);
                        errors.push(ValidationError::from_code(
                            &error_codes::CW264_UNEXPECTED_PROPERTY_LEAF_VALUE,
                            file_path,
                            lv.pos.start.line,
                            lv.pos.start.col,
                            &[&format!("Unexpected bare value '{}'", val_str)],
                        ));
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
                        validate_children(
                            &vc.children,
                            ast,
                            vc_rules,
                            enum_map,
                            table,
                            errors,
                            file_path,
                            scope_context,
                            game,
                            ruleset,
                            type_index,
                            modifier_keys,
                            loc_index,
                            (vc.pos.start.line, vc.pos.start.col),
                        );
                        break;
                    }
                }
                if !matched {
                    errors.push(ValidationError::from_code(
                        &error_codes::CW265_UNEXPECTED_PROPERTY_VALUE_CLAUSE,
                        file_path,
                        vc.pos.start.line,
                        vc.pos.start.col,
                        &["Unexpected value clause '{...}'"],
                    ));
                }
            }
            _ => {}
        }
    }

    // Cardinality enforcement. Report at the block's own location (its first
    // child) rather than line 0 — a missing required field belongs to THIS
    // entity (e.g. the specific decision), not the top of the file.
    let (block_line, block_col) = children
        .iter()
        .find_map(|c| child_start_pos(c, ast))
        .unwrap_or(block_pos);

    // Aggregate keyed-rule cardinality per (lowercased) key. Duplicate keys are
    // overloads/alternatives (e.g. two `clicksound =` rules in one subtype), so
    // the key is checked once against the most permissive bounds rather than
    // once per overload — otherwise a present-once field reads as missing N-1
    // times, or an absent optional alternative double-reports.
    // Third field tracks strictness: a `~` (soft) minimum on ANY overload of a
    // key makes the whole key's minimum soft, so an under-count is not flagged.
    let mut key_card: HashMap<String, (i32, i32, bool)> = HashMap::new();
    for (rule_type, opts) in rules.iter() {
        if matches!(
            rule_type,
            RuleType::LeafRule { .. } | RuleType::NodeRule { .. }
        )
            && let Some(k) = get_rule_key(rule_type) {
                let e = key_card.entry(k.to_lowercase()).or_insert((
                    opts.min,
                    opts.max,
                    opts.strict_min,
                ));
                e.0 = e.0.min(opts.min);
                e.1 = e.1.max(opts.max);
                e.2 = e.2 && opts.strict_min;
            }
    }
    let mut reported_keys: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (rule_idx, (rule_type, opts)) in rules.iter().enumerate() {
        // Both under- and over-count default to a WARNING (config cardinalities are
        // often stricter than the game, and cardinality-max is emitted as a Warning);
        // an explicit `## severity` still wins.
        let card_sev = opts
            .severity
            .as_ref()
            .map(|s| severity_to_error(s.clone()))
            .unwrap_or(ErrorSeverity::Warning);
        let missing_sev = card_sev;
        let max_sev = card_sev;

        match rule_type {
            RuleType::LeafRule { .. } | RuleType::NodeRule { .. } => {
                if let Some(key) = get_rule_key(rule_type) {
                    let lkey = key.to_lowercase();
                    // Each distinct key is reported at most once (see key_card above).
                    if reported_keys.insert(lkey.clone()) {
                        let (kmin, kmax, kstrict) = key_card.get(&lkey).copied().unwrap_or((
                            opts.min,
                            opts.max,
                            opts.strict_min,
                        ));
                        let count = key_counts.get(&lkey).copied().unwrap_or(0) as i32;
                        if count < kmin && kstrict {
                            errors.push(ValidationError {
                                message: format!(
                                    "Field '{}' appears {} time(s), expected at least {}",
                                    key, count, kmin
                                ),
                                severity: missing_sev,
                                line: block_line,
                                col: block_col,
                                file: file_path.to_string(),
                                code: Some(error_codes::CW242_WRONG_NUMBER.id.to_string()),
                            });
                        }
                        if count > kmax {
                            errors.push(ValidationError {
                                message: format!(
                                    "Field '{}' appears {} time(s), expected at most {}",
                                    key, count, kmax
                                ),
                                severity: max_sev,
                                line: block_line,
                                col: block_col,
                                file: file_path.to_string(),
                                code: Some(error_codes::CW242_WRONG_NUMBER.id.to_string()),
                            });
                        }
                    }
                }
            }
            // Item 5: LeafValueRule cardinality
            RuleType::LeafValueRule { right } => {
                let count = leafvalue_counts[rule_idx] as i32;
                // `~` (soft) minimum: don't flag an under-count. These rules are
                // typically a disjunction of overlapping leafvalue kinds (e.g.
                // `ship_types` accepts <naval_equip> OR <ship_unit> OR
                // enum[ship_units], each `~1..inf`); a value matching one leaves
                // the others at 0, which is not an error. Genuinely invalid values
                // are still caught by the per-value "Unexpected bare value" check.
                if count < opts.min && opts.strict_min {
                    errors.push(ValidationError {
                        message: format!(
                            "LeafValue {:?} appears {} time(s), expected at least {}",
                            right, count, opts.min
                        ),
                        severity: missing_sev,
                        line: block_line,
                        col: block_col,
                        file: file_path.to_string(),
                        code: Some(error_codes::CW242_WRONG_NUMBER.id.to_string()),
                    });
                }
                if count > opts.max {
                    errors.push(ValidationError {
                        message: format!(
                            "LeafValue {:?} appears {} time(s), expected at most {}",
                            right, count, opts.max
                        ),
                        severity: max_sev,
                        line: block_line,
                        col: block_col,
                        file: file_path.to_string(),
                        code: Some(error_codes::CW242_WRONG_NUMBER.id.to_string()),
                    });
                }
            }
            // Item 5: ValueClauseRule cardinality
            RuleType::ValueClauseRule { .. } => {
                let count = valueclause_counts[rule_idx] as i32;
                if count < opts.min && opts.strict_min {
                    errors.push(ValidationError {
                        message: format!(
                            "ValueClause appears {} time(s), expected at least {}",
                            count, opts.min
                        ),
                        severity: missing_sev,
                        line: block_line,
                        col: block_col,
                        file: file_path.to_string(),
                        code: Some(error_codes::CW242_WRONG_NUMBER.id.to_string()),
                    });
                }
                if count > opts.max {
                    errors.push(ValidationError {
                        message: format!(
                            "ValueClause appears {} time(s), expected at most {}",
                            count, opts.max
                        ),
                        severity: max_sev,
                        line: block_line,
                        col: block_col,
                        file: file_path.to_string(),
                        code: Some(error_codes::CW242_WRONG_NUMBER.id.to_string()),
                    });
                }
            }
            _ => {}
        }
    }
}

/// A block key that isn't a known scope command but resolves to a scope via the
/// game data: a numeric state/province id, an upper-case country/state tag, or a
/// `prefix:data` reference. Plain lowercase effect/trigger names are excluded.
fn looks_like_data_ref(key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    key.contains(':')
        || key.bytes().all(|b| b.is_ascii_digit())
        || key.chars().any(|c| c.is_ascii_uppercase())
}

/// Apply the scope change for entering a `key = { ... }` block. The caller must
/// have `save()`d the context first and `restore()` after.
///
/// Order of precedence:
/// 1. an explicit `## push_scope` on the rule (alias effects like `every_state`);
/// 2. otherwise the scope produced by the key itself when it's a scope-change
///    link/iterator/data-ref (`owner`, `random_owned_state`, `root`, `prev`, …);
/// 3. otherwise, if the key looks like a data reference we don't model
///    (`sp:foo`, `state:5`, any `prefix:data`), enter ANY scope so inner
///    effects aren't falsely scope-checked. Plain effect blocks keep the
///    current scope unchanged.
///
/// Apply a `## push_scope = <scope>` value (or a `{ a b }` list): resolve the
/// scope NAME through the registry and push that scope id. `any`/`all` push the
/// wildcard, which is the correct lenient behaviour for `for_each_scope_loop`
/// etc. Falls back to `change_scope` for command-like values (`prev`, `root`).
fn push_named_scope(ctx: &mut ScopeContext, push: &str) {
    let first = push
        .trim_matches(|c: char| c == '{' || c == '}' || c.is_whitespace())
        .split_whitespace()
        .next()
        .unwrap_or(push);
    match ctx.registry.id_of(first) {
        Some(id) => ctx.push_scope(id),
        None => {
            ctx.change_scope(push);
        }
    }
}

fn enter_block_scope(ctx: &mut ScopeContext, key: &str, opts: &Options, game: Option<Game>) {
    if key.contains(':') {
        // A `prefix:data` key (`var:x`, `event_target:y`, `sp:z`) is resolved by
        // the registry prefix link, NOT by a matched rule's `## push_scope` — that
        // would be an unreliable guess (a `var:` ref holds a data-dependent scope).
        // change_scope pushes ANY for value prefixes (var:/event_target:) or the
        // target scope for scope-change prefixes (sp: -> project). Unknown prefix
        // -> ANY (lenient).
        let before = ctx.scope_depth();
        ctx.change_scope(key);
        if ctx.scope_depth() == before {
            ctx.push_scope(SCOPE_ANY);
        }
    } else if let Some(ref push) = opts.push_scope {
        push_named_scope(ctx, push);
    } else {
        let before = ctx.scope_depth();
        ctx.change_scope(key);
        // change_scope didn't recognise the key, but a from-data reference used
        // as a block key DOES change scope to whatever it resolves to: a numeric
        // state/province id (`857 = {...}`) or a country tag (`GER = {...}`). We
        // can't pin the exact scope here without the full data index, so enter
        // ANY — lenient, so the block's body isn't validated against the (wrong)
        // outer scope.
        if ctx.scope_depth() == before && looks_like_data_ref(key) {
            ctx.push_scope(SCOPE_ANY);
        }
    }
    if let Some(ref replace) = opts.replace_scopes {
        apply_replace_scopes(ctx, replace, game);
    }
}

fn apply_replace_scopes(ctx: &mut ScopeContext, replace: &ReplaceScopes, game: Option<Game>) {
    if game.is_some() {
        ctx.apply_replace_scope(
            replace.root.as_deref(),
            replace.this.as_deref(),
            &replace.froms,
            &replace.prevs,
        );
    }
}

fn rule_matches_leaf_key(
    rule_type: &RuleType,
    key: &str,
    ruleset: &RuleSet,
    type_index: Option<&cwtools_info::TypeIndex>,
) -> bool {
    match rule_type {
        // Cross-kind fallback: a NodeRule can also match a leaf key (e.g. alias blocks)
        RuleType::LeafRule { left, .. } | RuleType::NodeRule { left, .. } => {
            field_matches_key(left, key, ruleset, type_index)
        }
        _ => false,
    }
}

fn rule_matches_node_key(
    rule_type: &RuleType,
    key: &str,
    ruleset: &RuleSet,
    type_index: Option<&cwtools_info::TypeIndex>,
) -> bool {
    match rule_type {
        // Cross-kind fallback: a LeafRule can also match a node key
        RuleType::NodeRule { left, .. } | RuleType::LeafRule { left, .. } => {
            field_matches_key(left, key, ruleset, type_index)
        }
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
        "THIS",
        "ROOT",
        "PREV",
        "FROM",
        "FROMFROM",
        "FROMFROMFROM",
        "FROMFROMFROMFROM",
        "PREVPREV",
        "PREVPREVPREV",
        "OWNER",
        "CONTROLLER",
        "CAPITAL",
        "OVERLORD",
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
        && key
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        && key.chars().any(|c| c.is_ascii_uppercase())
}

/// Whether `key` can open a scope in an effect/trigger block: a scope command
/// (ROOT/FROM/tag/id/chain) OR an instance of any type — HOI4 from-data scope
/// links let an instance (character, state, ideology, ...) open its own scope.
fn is_scope_key(
    key: &str,
    ruleset: &RuleSet,
    type_index: Option<&cwtools_info::TypeIndex>,
) -> bool {
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
        (
            &pattern[..open],
            "type",
            &pattern[open + 1..close],
            &pattern[close + 1..],
        )
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
                if found.is_none_or(|(o, ..)| open < o) {
                    found = Some((
                        open,
                        &pattern[..open],
                        kind,
                        &pattern[inner..close],
                        &pattern[close + 1..],
                    ));
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
            type_index
                .map(|idx| idx.contains(base, middle))
                .unwrap_or(false)
        }
        "enum" => match ruleset.enum_by_name.get(name) {
            Some(&idx) if !ruleset.enums[idx].values.is_empty() => {
                ruleset.enums[idx].values.iter().any(|v| v == middle)
            }
            _ => true, // enum absent/empty (game-derived) — permissive
        },
        "value" => match ruleset.values.iter().find(|(n, _)| n == name) {
            Some((_, vs)) if !vs.is_empty() => vs.iter().any(|v| v == middle),
            _ => true, // value set not collected — permissive
        },
        _ => false,
    })
}

fn field_matches_key(
    field: &NewField,
    key: &str,
    ruleset: &RuleSet,
    type_index: Option<&cwtools_info::TypeIndex>,
) -> bool {
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
            if lower != key
                && ruleset
                    .alias_exact
                    .contains_key(&format!("{}:{}", category, lower))
            {
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
            match ruleset.enum_by_name.get(enum_name.as_str()) {
                Some(&idx) => ruleset.enums[idx].values.iter().any(|v| v == key),
                None => true,
            }
        }
        // Numeric-keyed rules: `ordered = { int = { ... } }` uses integer keys.
        NewField::ValueField(ValueType::Int { .. }) => key.parse::<i64>().is_ok(),
        NewField::ValueField(ValueType::Float { .. } | ValueType::Percent) => {
            key.parse::<f64>().is_ok()
        }
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
    loc_index: Option<&LocIndex>,
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
    // Case-insensitive retry: usages like `IF`, `Country_event` resolve to the
    // lowercase alias (config alias names are lowercase). Mirrors the fallback in
    // field_matches_key, which matches the key so the body must validate too.
    let lower = key.to_ascii_lowercase();
    if overloads.is_empty()
        && lower != key
        && let Some(idxs) = ruleset.alias_exact.get(&format!("{}:{}", category, lower))
    {
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
        if let Some(sf_idx) = cat.scope_field_idx
            && is_scope_key(key, ruleset, type_index) {
                overloads.push(&ruleset.aliases[sf_idx].1);
            }
    }
    if overloads.is_empty() {
        // Category unloaded or no such alias key — accept silently, matching the
        // permissive key-match in field_matches_key.
        return;
    }

    // CW248: an invalid scope command in a chain. Restricted to dotted lower-case
    // chains (`owner.capital`): a bare command that's missing from this config's
    // links.cwt (e.g. `overlord`) is valid-but-unlisted, not invalid, so only
    // chains — where a segment is genuinely unresolvable — are flagged.
    if scope_checks_enabled()
        && key.contains('.')
        && looks_like_scope_command(key)
        && !looks_like_data_ref(key)
        && let Some(ctx) = scope_context.as_ref()
    {
        let mut probe = ctx.clone();
        if matches!(
            probe.change_scope(key),
            cwtools_game::scope_engine::ScopeResult::NotFound
        ) {
            let code = &error_codes::CW248_INVALID_SCOPE_COMMAND;
            let (line, col) = leaf
                .map(|l| (l.pos.start.line, l.pos.start.col))
                .unwrap_or((0, 0));
            errors.push(ValidationError::from_code(code, file_path, line, col, &[key]));
        }
    }

    // CW104/105/106: scope check. A trigger/effect (alias) carries a `## scope`
    // restriction in the config; if NONE of its overloads is valid in the current
    // scope, it's used in the wrong place. `scope_matches_required` treats
    // unrestricted / `any` / unresolved scopes leniently, so this only fires when
    // the current scope is known and every overload demands a different one.
    //
    // ON by default (escape hatch CWTOOLS_NO_SCOPE_CHECKS=1). Accurate firing
    // needs scope-change tracking: the engine seeds the right root scope per file
    // type (e.g. state-history files are state-scoped, not country) and pushes
    // scope through every scope-change effect/trigger link (`random_owned_state`,
    // leader abilities, iterators). With the config-driven scope/link registry
    // that tracking is now in place, so this runs by default.
    if scope_checks_enabled()
        && let Some(ctx) = scope_context.as_ref()
        && let Some(current) = ctx.current()
    {
        let reg = ctx.registry.as_ref();
        let any_ok = overloads
            .iter()
            .any(|(_, opts)| scope_matches_required(current, reg, &opts.required_scopes));
        if !any_ok {
            let mut expected: Vec<String> = overloads
                .iter()
                .flat_map(|(_, o)| o.required_scopes.iter().cloned())
                .collect();
            expected.dedup();
            let code = match category {
                "trigger" => &error_codes::CW104_INCORRECT_TRIGGER_SCOPE,
                "effect" => &error_codes::CW105_INCORRECT_EFFECT_SCOPE,
                _ => &error_codes::CW106_INCORRECT_SCOPE_SCOPE,
            };
            let (line, col) = leaf
                .map(|l| (l.pos.start.line, l.pos.start.col))
                .unwrap_or((0, 0));
            errors.push(ValidationError::from_code(
                code,
                file_path,
                line,
                col,
                &[key, &reg.name_of(current), &expected.join(" or ")],
            ));
        }
    }

    let mut best: Option<Vec<ValidationError>> = None;
    for (rule_type, opts) in overloads {
        let mut temp: Vec<ValidationError> = Vec::new();
        match rule_type {
            RuleType::LeafRule { .. } => {
                if let Some(leaf) = leaf {
                    validate_leaf(
                        leaf,
                        rule_type,
                        table,
                        enum_map,
                        &mut temp,
                        file_path,
                        type_index,
                        scope_context.as_ref(),
                        game,
                        loc_index,
                    );
                } else {
                    // Scalar-valued overload but the usage is a block — not a match.
                    let (line, col) = leaf
                        .map(|l| (l.pos.start.line, l.pos.start.col))
                        .unwrap_or((0, 0));
                    temp.push(alias_mismatch_error(file_path, category, "{...}", line, col));
                }
            }
            RuleType::NodeRule {
                rules: alias_inner, ..
            } => {
                let children = clause_children.or_else(|| match leaf.map(|l| &l.value) {
                    Some(Value::Clause(ch)) => Some(ch.as_slice()),
                    _ => None,
                });
                if let Some(children) = children {
                    let saved = scope_context.as_ref().map(|ctx| ctx.save());
                    if let Some(ctx) = scope_context.as_mut() {
                        enter_block_scope(ctx, key, opts, game);
                    }
                    validate_children(
                        children,
                        ast,
                        alias_inner,
                        enum_map,
                        table,
                        &mut temp,
                        file_path,
                        scope_context,
                        game,
                        ruleset,
                        type_index,
                        modifier_keys,
                        loc_index,
                        leaf.map(|l| (l.pos.start.line, l.pos.start.col))
                            .unwrap_or((0, 0)),
                    );
                    if let (Some(saved), Some(ctx)) = (saved, scope_context.as_mut()) {
                        ctx.restore(saved);
                    }
                } else {
                    // Block overload but the usage is a scalar — not a match.
                    let (value, line, col) = leaf
                        .map(|l| {
                            (
                                leaf_value_to_string(&l.value, table),
                                l.pos.start.line,
                                l.pos.start.col,
                            )
                        })
                        .unwrap_or_else(|| (String::new(), 0, 0));
                    temp.push(alias_mismatch_error(file_path, category, &value, line, col));
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

/// Error used when an alias overload's shape (scalar vs block) can't match the
/// usage; it ranks a candidate and, when no better candidate exists, is surfaced
/// at the offending leaf's position. F# `ConfigRulesUnexpectedAliasKeyValue`.
fn alias_mismatch_error(
    file_path: &str,
    category: &str,
    value: &str,
    line: u32,
    col: u16,
) -> ValidationError {
    let code = &error_codes::CW267_UNEXPECTED_ALIAS_KEY_VALUE;
    ValidationError {
        message: code.format(&[category, value]),
        severity: code.severity,
        line,
        col,
        file: file_path.to_string(),
        code: Some(code.id.to_string()),
    }
}

/// Build the set of valid modifier names for `alias_name[modifier]` slots from
/// the ruleset's `modifiers = { ... }` block. Templated entries like
/// `production_speed_<building>_factor` / `<ideology>_drift` are expanded against
/// the type index, one instance each. Single source of truth so the CLI and LSP
/// agree on what counts as a modifier.
pub fn build_modifier_keys(
    ruleset: &RuleSet,
    type_index: &cwtools_info::TypeIndex,
) -> std::collections::HashSet<String> {
    let mut mk = std::collections::HashSet::new();
    for m in &ruleset.modifiers {
        match (m.find('<'), m.find('>')) {
            (Some(open), Some(close)) if open < close => {
                let tn = &m[open + 1..close];
                let pre = &m[..open];
                let suf = &m[close + 1..];
                for (_uri, inst) in type_index.instances(tn) {
                    mk.insert(format!("{}{}{}", pre, inst.name, suf));
                }
            }
            _ => {
                mk.insert(m.clone());
            }
        }
    }
    mk
}

/// Validate a `LocalisationField` leaf: that the referenced loc key exists
/// (CW100 / CW122) and, when the scope is known, that the loc string's commands
/// are valid in that scope (CW260 / CW262). Mirrors F# `checkLocKey*` plus the
/// scope-aware loc-command checks.
#[allow(clippy::too_many_arguments)]
fn validate_localisation_field(
    leaf: &cwtools_parser::ast::Leaf,
    synced: bool,
    is_inline: bool,
    table: &StringTable,
    errors: &mut Vec<ValidationError>,
    file_path: &str,
    scope_context: Option<&ScopeContext>,
    game: Option<Game>,
    loc_index: Option<&LocIndex>,
) {
    // The meta-localisation block form `{ localization_key = X PARAM = ... }` is
    // accepted unconditionally (its inner key is validated as its own leaf).
    if let Value::Clause(_) = &leaf.value {
        return;
    }

    let was_quoted = matches!(leaf.value, Value::QString(_));
    let raw = leaf_value_to_string(&leaf.value, table);
    let key_raw = raw.trim_matches('"');

    // F# skip rules: empty keys, keys with spaces (prose / compound), `[...]`
    // inline command blocks, `$VAR$` scripted references, and `@`-vars are not
    // plain key references and are accepted.
    if key_raw.is_empty()
        || key_raw.contains(' ')
        || (key_raw.starts_with('[') && key_raw.ends_with(']'))
        || key_raw.contains('$')
        || key_raw.starts_with('@')
    {
        return;
    }

    // No loc data loaded → accept leniently (e.g. vanilla loc absent).
    let Some(idx) = loc_index else {
        return;
    };
    let key_lower = key_raw.to_lowercase();
    let exists = idx.exists_any(&key_lower);

    let push_missing = |errors: &mut Vec<ValidationError>, lang: &str| {
        let code = &error_codes::CW100_MISSING_LOCALISATION;
        errors.push(ValidationError::from_code(
            code,
            file_path,
            leaf.pos.start.line,
            leaf.pos.start.col,
            &[key_raw, lang],
        ));
    };

    if is_inline {
        // F# four-way logic for inline loc keys.
        match (was_quoted, exists) {
            (true, true) => {
                let code = &error_codes::CW122_LOC_KEY_IN_INLINE;
                errors.push(ValidationError::from_code(
                    code,
                    file_path,
                    leaf.pos.start.line,
                    leaf.pos.start.col,
                    &[key_raw],
                ));
            }
            (true, false) => {} // quoted + missing → skip (lenient, matches F#)
            (false, true) => {} // unquoted + exists → ok
            (false, false) => push_missing(errors, "any language"),
        }
    } else if synced {
        // Must exist in every language the project ships loc data for.
        for lang in idx.missing_synced_languages(&key_lower) {
            push_missing(errors, &lang.to_string());
        }
    } else if !exists {
        push_missing(errors, "any language");
    }

    // Scope-aware loc-command validation at the reference site: validate the
    // referenced loc string's `[command]` chains against the scope of THIS field.
    if exists
        && let Some(entry) = idx.entry(&key_lower) {
            let initial = scope_context
                .and_then(|c| c.current())
                .unwrap_or(cwtools_game::scope_engine::SCOPE_ANY);
            let data = cwtools_localization::LocScopeData {
                game: cwtools_localization::Game::from_engine(game),
                registry: scope_context.map(|c| c.registry.clone()),
                ..Default::default()
            };
            for diag in cwtools_localization::validate_loc_commands(entry, initial, &data) {
                push_loc_command_diagnostic(
                    &diag,
                    leaf,
                    file_path,
                    scope_context.map(|c| c.registry.as_ref()),
                    errors,
                );
            }
        }
}

/// Convert a `LocCommandDiagnostic` (from the loc scope engine) into a
/// `ValidationError` with the matching F# numeric code.
fn push_loc_command_diagnostic(
    diag: &cwtools_localization::LocCommandDiagnostic,
    leaf: &cwtools_parser::ast::Leaf,
    file_path: &str,
    registry: Option<&ScopeRegistry>,
    errors: &mut Vec<ValidationError>,
) {
    use cwtools_localization::LocCommandDiagnostic as D;
    let scope_name = |id: u32| -> String {
        match registry {
            Some(reg) => reg.name_of(ScopeId(id)),
            None => id.to_string(),
        }
    };
    let (code, message) = match diag {
        D::WrongScope {
            command,
            current_scope,
            expected_scopes,
        } => {
            let expected = expected_scopes
                .iter()
                .map(|s| scope_name(*s))
                .collect::<Vec<_>>()
                .join(", ");
            let code = &error_codes::CW260_LOC_COMMAND_WRONG_SCOPE;
            (
                code,
                code.format(&[command, &scope_name(*current_scope), &expected]),
            )
        }
        D::ChainEndsInScope { command } => {
            let code = &error_codes::CW266_LOC_COMMAND_NOT_IN_DATA_TYPE;
            (
                code,
                code.format(&[command.as_str(), command.as_str(), "scope"]),
            )
        }
    };
    errors.push(ValidationError {
        message,
        severity: code.severity,
        line: leaf.pos.start.line,
        col: leaf.pos.start.col,
        file: file_path.to_string(),
        code: Some(code.id.to_string()),
    });
}

#[allow(clippy::too_many_arguments)]
fn validate_leaf(
    leaf: &cwtools_parser::ast::Leaf,
    rule_type: &RuleType,
    table: &StringTable,
    enum_map: &HashMap<&str, &EnumDefinition>,
    errors: &mut Vec<ValidationError>,
    file_path: &str,
    type_index: Option<&cwtools_info::TypeIndex>,
    scope_context: Option<&ScopeContext>,
    game: Option<Game>,
    loc_index: Option<&LocIndex>,
) {
    if let RuleType::LeafRule { right, .. } = rule_type {
        // LocalisationField: check the referenced loc key exists (CW100/CW122)
        // and, when we know the scope, validate the loc string's commands
        // (CW260/CW262). See `validate_localisation_field`.
        if let NewField::LocalisationField { synced, is_inline } = right {
            validate_localisation_field(
                leaf,
                *synced,
                *is_inline,
                table,
                errors,
                file_path,
                scope_context,
                game,
                loc_index,
            );
            return;
        }
        // TypeField: check type_index when available (Item 1).
        if let NewField::TypeField(type_type) = right {
            // Unquote: `load_oob = "EU_frontex_basic_2017"` references the instance
            // `EU_frontex_basic_2017`; type instances are stored unquoted.
            let raw_value = leaf_value_to_string(&leaf.value, table);
            let value_str = raw_value
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .unwrap_or(&raw_value)
                .to_string();
            let key = table.get_string(leaf.key.normal).unwrap_or_default();
            let type_name = match type_type {
                TypeType::Simple(n) => n.as_str(),
                TypeType::Complex { name, .. } => name.as_str(),
            };
            // Complex TypeField (`prefix<type>suffix`) maps a value to an instance
            // and the game accepts any of these forms, so we try them all:
            //   (a) strip: the value carries the affixes and the instance is
            //       stored without them (`GFX_event_x` -> `x`).
            //   (b) raw: the value IS already the full instance name
            //       (HOI4 ideas may write `picture = GFX_idea_x` directly).
            //   (c) prepend: the value is bare and the affixed form is the real
            //       instance (HOI4 ideas: `picture = x` -> `GFX_idea_x`).
            // The reference resolves if ANY candidate is a known instance, so this
            // branch can only ever REMOVE false positives, never add them.
            let (lookup_value, alt_candidates) = match type_type {
                TypeType::Complex { prefix, suffix, .. } => {
                    let mut v = value_str.as_str();
                    if !prefix.is_empty() {
                        v = v.strip_prefix(prefix.as_str()).unwrap_or(v);
                    }
                    if !suffix.is_empty() {
                        v = v.strip_suffix(suffix.as_str()).unwrap_or(v);
                    }
                    let prepended = format!("{}{}{}", prefix, value_str, suffix);
                    (v.to_string(), vec![value_str.clone(), prepended])
                }
                _ => (value_str.clone(), Vec::new()),
            };
            if let Some(idx) = type_index {
                // Only flag if we have at least one known instance for this type.
                // If zero instances, vanilla data probably isn't loaded — accept.
                let resolved = idx.contains(type_name, &lookup_value)
                    || alt_candidates.iter().any(|c| idx.contains(type_name, c));
                if !idx.instances(type_name).is_empty() && !resolved {
                    // An unknown `<event>` / `<event.country_event>` reference is
                    // F#'s CW222 (UndefinedEvent, Warning); other unknown type
                    // refs keep the Rust-only CW500.
                    let is_event = type_name == "event" || type_name.starts_with("event.");
                    let (code, message) = if is_event {
                        let c = &error_codes::CW222_UNDEFINED_EVENT;
                        (c, c.format(&[&lookup_value]))
                    } else {
                        (
                            &error_codes::CW500_TYPE_NOT_FOUND,
                            format!(
                                "Field '{}' references '{}' which is not a known instance of type '{}'",
                                key, lookup_value, type_name
                            ),
                        )
                    };
                    errors.push(ValidationError {
                        message,
                        severity: code.severity,
                        line: leaf.pos.start.line,
                        col: leaf.pos.start.col,
                        file: file_path.to_string(),
                        code: Some(code.id.to_string()),
                    });
                }
            }
            // TypeField is otherwise accepted (non-empty check done by field_matches_value).
            return;
        }
        // FilepathField: check the referenced file exists (CW113). Only when the
        // file index is populated (vanilla loaded); otherwise stay silent.
        if let NewField::FilepathField { prefix, extension } = right {
            if let Some(idx) = type_index
                && !idx.file_index.is_empty() {
                    let raw = leaf_value_to_string(&leaf.value, table);
                    let value = raw.trim_matches('"').trim();
                    // Skip dynamic / templated paths we can't resolve statically.
                    let dynamic = value.is_empty()
                        || value.contains('$')
                        || value.contains('[')
                        || value.contains('<');
                    if !dynamic {
                        let mut candidate = match prefix {
                            Some(p)
                                if !value
                                    .to_ascii_lowercase()
                                    .starts_with(&p.to_ascii_lowercase()) =>
                            {
                                format!("{}{}", p, value)
                            }
                            _ => value.to_string(),
                        };
                        if let Some(ext) = extension
                            && !ext.is_empty()
                                && !candidate
                                    .to_ascii_lowercase()
                                    .ends_with(&ext.to_ascii_lowercase())
                            {
                                candidate.push_str(ext);
                            }
                        if !idx.file_index.contains(&candidate) {
                            let code = &error_codes::CW113_MISSING_FILE;
                            errors.push(ValidationError::from_code(
                                code,
                                file_path,
                                leaf.pos.start.line,
                                leaf.pos.start.col,
                                &[&candidate],
                            ));
                        }
                    }
                }
            return;
        }

        // VariableField: a value that must be a number-in-range or a defined
        // variable reference (`add = 5`, `value = my_var`). Mirrors F#
        // `checkVariableField`. Two parts:
        //   - numeric checks (CW271 int-only / CW270 3-decimal precision) run
        //     always — they only fire on a value that parses as a number and
        //     violates the field's int/precision constraint, so they cannot
        //     flood valid config.
        //   - the "variable has not been set" check (CW246) is gated behind
        //     `var_checks_enabled()` because it needs a complete variable index.
        if let NewField::VariableField {
            is_int, is_32bit, ..
        } = right
        {
            let raw = leaf_value_to_string(&leaf.value, table);
            let v = raw.trim_matches('"').trim();
            // Accept at-vars (@x), inline math ([...]), loc refs ($$) and boolean
            // literals (`yes`/`no`, used by boolean modifiers) — all valid in a
            // value slot (F# FieldValidators bypasses).
            let is_bool = matches!(leaf.value, Value::Bool(_))
                || matches!(v.to_ascii_lowercase().as_str(), "yes" | "no");
            let bypass = v.is_empty()
                || v.starts_with('@')
                || v.starts_with('[')
                || v.contains("$$")
                || is_bool;
            if !bypass {
                // Strip a `?`/`^` default-value selector before parsing.
                let core = v.split(['?', '^']).next().unwrap_or(v).trim();
                if let Ok(f) = core.parse::<f64>() {
                    // Numeric value: enforce int-ness / decimal precision.
                    if *is_int && f.fract() != 0.0 {
                        let code = &error_codes::CW271_VARIABLE_INT_ONLY;
                        errors.push(ValidationError::from_code(
                            code,
                            file_path,
                            leaf.pos.start.line,
                            leaf.pos.start.col,
                            &[],
                        ));
                    } else if *is_32bit && decimal_places(core) > 3 {
                        let code = &error_codes::CW270_VARIABLE_TOO_SMALL;
                        errors.push(ValidationError::from_code(
                            code,
                            file_path,
                            leaf.pos.start.line,
                            leaf.pos.start.col,
                            &[],
                        ));
                    }
                } else if var_checks_enabled() {
                    // Non-numeric value: it must name a defined variable. Stay
                    // lenient: only flag a single bare token (a `.`-chain is a
                    // scope/target, handled elsewhere) that isn't a scope
                    // keyword/link and isn't in the project variable index.
                    let single_token = !core.contains('.') && !core.contains(':');
                    let is_scopeish = scope_context
                        .map(|ctx| resolves_as_scope_key(ctx, core))
                        .unwrap_or(false);
                    if single_token
                        && !is_scopeish
                        && let Some(idx) = type_index
                        && !idx.var_index.is_empty()
                        && !idx.var_index.contains(core)
                    {
                        let code = &error_codes::CW246_UNSET_VARIABLE;
                        errors.push(ValidationError::from_code(
                            code,
                            file_path,
                            leaf.pos.start.line,
                            leaf.pos.start.col,
                            &[core],
                        ));
                    }
                }
            }
            return;
        }

        // Scope-target validation (CW243 target-wrong-scope / CW245 error-in-target):
        // resolve the chain from the current scope. Gated with the other scope checks.
        if let NewField::ScopeField(expected) = right
            && scope_checks_enabled()
            && let Some(ctx) = scope_context
        {
            let value = leaf_value_to_string(&leaf.value, table);
            validate_scope_target(ctx, &value, expected, leaf, file_path, errors);
        }

        if !field_matches_value(right, &leaf.value, table, enum_map) {
            let expected = field_to_description(right);
            let actual = leaf_value_to_string(&leaf.value, table);
            let key = table.get_string(leaf.key.normal).unwrap_or_default();
            errors.push(ValidationError::from_code(
                &error_codes::CW240_UNEXPECTED_VALUE,
                file_path,
                leaf.pos.start.line,
                leaf.pos.start.col,
                &[&format!(
                    "Field '{}' has value '{}', expected {}",
                    key, actual, expected
                )],
            ));
        }
    }
}

/// Check that a string has the YYYY.MM.DD shape for a CW date field.
fn is_date_shape(s: &str) -> bool {
    // Accept YYYY.MM.DD or YYYY.M.D — split by '.' and check 3 numeric parts
    let parts: Vec<&str> = s.splitn(4, '.').collect();
    parts.len() >= 3
        && parts[0].parse::<i32>().is_ok()
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
            // Enum membership is case-insensitive (F# lowercases both the enum
            // values and the checked key — FieldValidators.fs `getLowerKey` +
            // RuleValidationService.fs `.lower`). e.g. `containerOrientations`
            // is authored UPPER_LEFT/CENTER but files use upper_left/center.
            if def.values.iter().any(|v| v.eq_ignore_ascii_case(value)) {
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

fn field_matches_value(
    field: &NewField,
    value: &Value,
    table: &StringTable,
    enum_map: &HashMap<&str, &EnumDefinition>,
) -> bool {
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
        (NewField::ValueField(ValueType::Bool), Value::String(t))
        | (NewField::ValueField(ValueType::Bool), Value::QString(t)) => {
            let v = match_text(table, t).to_lowercase();
            v == "yes" || v == "no"
        }

        // --- Int with range enforcement (item 4) ---
        (NewField::ValueField(ValueType::Int { min, max }), Value::Int(v)) => {
            let v_i = *v as i32;
            v_i >= *min && v_i <= *max
        }
        (NewField::ValueField(ValueType::Int { min, max }), Value::String(t))
        | (NewField::ValueField(ValueType::Int { min, max }), Value::QString(t)) => {
            let text = match_text(table, t);
            if let Ok(v) = text.parse::<i32>() {
                v >= *min && v <= *max
            } else {
                false
            }
        }

        // --- Float with range enforcement (item 4) ---
        (NewField::ValueField(ValueType::Float { min, max }), Value::Float(v)) => {
            *v >= *min && *v <= *max
        }
        // An integer literal is a valid float (the parser emits Int for `1000`).
        (NewField::ValueField(ValueType::Float { min, max }), Value::Int(v)) => {
            (*v as f64) >= *min && (*v as f64) <= *max
        }
        (NewField::ValueField(ValueType::Float { min, max }), Value::String(t))
        | (NewField::ValueField(ValueType::Float { min, max }), Value::QString(t)) => {
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
        (NewField::ValueField(ValueType::Percent), Value::String(t))
        | (NewField::ValueField(ValueType::Percent), Value::QString(t)) => {
            let text = match_text(table, t);
            text.ends_with('%') || text.parse::<f64>().is_ok()
        }
        (NewField::ValueField(ValueType::Percent), Value::Float(_) | Value::Int(_)) => true,

        // --- Date / DateTime (item 3): basic YYYY.MM.DD[.HH] shape ---
        (NewField::ValueField(ValueType::Date), Value::String(t))
        | (NewField::ValueField(ValueType::Date), Value::QString(t)) => {
            is_date_shape(&match_text(table, t))
        }
        (NewField::ValueField(ValueType::DateTime), Value::String(t))
        | (NewField::ValueField(ValueType::DateTime), Value::QString(t)) => {
            is_datetime_shape(&match_text(table, t))
        }

        // --- Ck2Dna (item 3): exactly 32 hex chars (F# FieldValidators.fs:194-204) ---
        (NewField::ValueField(ValueType::Ck2Dna), Value::String(t))
        | (NewField::ValueField(ValueType::Ck2Dna), Value::QString(t)) => {
            let text = match_text(table, t);
            text.len() == 32 && text.chars().all(|c| c.is_ascii_hexdigit())
        }

        // --- Ck2DnaProperty (item 3): length 8 or 32, hex chars (F# FieldValidators.fs:205-211) ---
        (NewField::ValueField(ValueType::Ck2DnaProperty), Value::String(t))
        | (NewField::ValueField(ValueType::Ck2DnaProperty), Value::QString(t)) => {
            let text = match_text(table, t);
            (text.len() == 8 || text.len() == 32) && text.chars().all(|c| c.is_ascii_hexdigit())
        }

        // --- IrFamilyName / StlNameFormat (item 3): accept any string ---
        (NewField::ValueField(ValueType::IrFamilyName), Value::String(_) | Value::QString(_)) => {
            true
        }
        (
            NewField::ValueField(ValueType::StlNameFormat(_)),
            Value::String(_) | Value::QString(_),
        ) => true,

        // --- Scalar: accept anything ---
        (NewField::ScalarField, _) => true,

        // --- SpecificField: exact string match ---
        (NewField::SpecificField(s), Value::String(t))
        | (NewField::SpecificField(s), Value::QString(t)) => table
            .with_string(t.normal, |text| unquote_key(text) == *s)
            .unwrap_or(false),
        // A `= yes` / `= no` rule literal is a SpecificField, but the parser emits
        // Bool for those values — match them up (affects every boolean rule field).
        (NewField::SpecificField(s), Value::Bool(b)) => (s == "yes" && *b) || (s == "no" && !*b),
        (NewField::SpecificField(s), Value::Int(i)) => s == &i.to_string(),

        // --- TypeField: accept string (cross-file existence is a separate pass) ---
        (NewField::TypeField(TypeType::Simple(type_name)), Value::String(t))
        | (NewField::TypeField(TypeType::Simple(type_name)), Value::QString(t)) => table
            .with_string(t.normal, |s| validate_type_reference(s, type_name))
            .unwrap_or(false),
        (NewField::TypeField(TypeType::Complex { name, .. }), Value::String(t))
        | (NewField::TypeField(TypeType::Complex { name, .. }), Value::QString(t)) => table
            .with_string(t.normal, |s| validate_type_reference(s, name))
            .unwrap_or(false),
        // Numeric type instances — state/province ids are written as bare integers
        // (`states = { 599 600 }`, `<state>`). Accept; existence is a separate pass.
        (NewField::TypeField(_), Value::Int(_) | Value::Float(_)) => true,

        // --- ScopeField ---
        // A scope slot (`scope[country]`, `scope[state]`, ...) is satisfied by far
        // more than the literal scope keywords: country tags (USA), state ids (410),
        // event_target/variable references, and scope chains. Deep resolution is the
        // scope engine's job; at the field level accept any non-empty token rather
        // than flag every tag/id as an error.
        (NewField::ScopeField(_), Value::String(t))
        | (NewField::ScopeField(_), Value::QString(t)) => table
            .with_string(t.normal, |s| !s.is_empty())
            .unwrap_or(false),
        (NewField::ScopeField(_), Value::Int(_)) | (NewField::ScopeField(_), Value::Float(_)) => {
            true
        }

        // --- VariableField with range enforcement (item 4) ---
        (NewField::VariableField { min, max, .. }, Value::Float(v)) => *v >= *min && *v <= *max,
        (NewField::VariableField { min, max, .. }, Value::Int(v)) => {
            (*v as f64) >= *min && (*v as f64) <= *max
        }
        // yes/no are acceptable in variable contexts.
        (NewField::VariableField { .. }, Value::Bool(_)) => true,
        (NewField::VariableField { min, max, .. }, Value::String(t))
        | (NewField::VariableField { min, max, .. }, Value::QString(t)) => {
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

        // --- AliasField / SingleAliasField: shape check only (accept clause or
        // string). Deep validation of alias bodies happens in validate_alias_usage,
        // not here — this path is the secondary value-matching fallback. ---
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
    format!(
        "{}|{}|{}|{}",
        sev_str, error.file, error.line, error.message
    )
}
