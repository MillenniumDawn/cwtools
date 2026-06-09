use cwtools_game::constants::Game;
use cwtools_game::scope_engine::{SCOPE_ANY, ScopeContext, ScopeId};
use cwtools_game::scope_registry::ScopeRegistry;
use cwtools_localization::LocIndex;
use cwtools_parser::ast::{Child, ParsedFile, Value};
use cwtools_rules::rules_types::*;
use cwtools_string_table::string_table::StringTable;
use std::collections::{HashMap, HashSet};

pub mod error_codes;
pub mod per_game;

mod common;
mod ctx;
mod loc_field;
mod resolve;
mod rule_core;
mod scope;
mod subtype;

pub use common::{ErrorSeverity, ValidationError, error_hash};
pub use loc_field::build_modifier_keys;

use common::{leaf_value_to_string, path_contains_segment};
use ctx::ValidationCtx;
use resolve::{
    find_grandchild_type, find_rules_by_name, find_type_and_rules_for_file, find_type_by_path,
    find_type_by_path_and_key, should_skip_root_key,
};
use rule_core::validate_with_type;
use scope::build_scope_registry;

/// Iterate grandchildren of a skip_root_key wrapper and validate each one uniformly.
/// Both the Node-root and Leaf-root shapes delegate here so behaviour is identical.
fn validate_wrapper_grandchildren(
    ctx: &ValidationCtx,
    grandchildren: &[Child],
    type_def: &TypeDefinition,
    wrapper_root_key: &str,
    inner_rules: &[(RuleType, Options)],
    scope_context: &mut Option<ScopeContext>,
    errors: &mut Vec<ValidationError>,
) {
    let ast = ctx.ast;
    let table = ctx.table;
    let file_path = ctx.file_path;
    let ruleset = ctx.ruleset;
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
            ctx,
            gc_type_def,
            gc_children,
            gc_rules,
            scope_context,
            Some(&gc_key),
            errors,
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
    type_index: Option<&cwtools_index::TypeIndex>,
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
    type_index: Option<&cwtools_index::TypeIndex>,
    modifier_keys: Option<&HashSet<String>>,
    loc_index: Option<&LocIndex>,
) -> Vec<ValidationError> {
    // Single-file/test entry point: build the per-run shared state (enum_map +
    // scope registry) here and delegate. Hot multi-file callers should instead
    // build a `Prepared` ONCE outside their loop and call `validate_prepared`.
    let enum_map = build_enum_map(ruleset);
    let registry = build_scope_registry_arc(ruleset, game);
    validate_prepared(
        ast,
        file_path,
        &Prepared {
            ruleset,
            table,
            game,
            type_index,
            modifier_keys,
            loc_index,
            registry: registry.as_ref(),
            enum_map: &enum_map,
        },
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

/// The per-run shared validation state, built once and reused across every file
/// in a run. Bundles everything [`validate_prepared`] needs beyond the per-file
/// `ast` and `file_path`, so callers pass one value instead of a ten-argument
/// call. All fields are borrows, so it is cheap to copy.
#[derive(Clone, Copy)]
pub struct Prepared<'a> {
    pub ruleset: &'a RuleSet,
    pub table: &'a StringTable,
    pub game: Option<Game>,
    pub type_index: Option<&'a cwtools_index::TypeIndex>,
    pub modifier_keys: Option<&'a HashSet<String>>,
    pub loc_index: Option<&'a LocIndex>,
    pub registry: Option<&'a std::sync::Arc<ScopeRegistry>>,
    pub enum_map: &'a HashMap<&'a str, &'a EnumDefinition>,
}

/// Validate one parsed file against prebuilt per-run state. The hot path: build
/// [`Prepared`] once (scope registry + enum map + indexes) and call this per file
/// instead of rebuilding that state for every file.
#[tracing::instrument(skip_all)]
pub fn validate_prepared(
    ast: &ParsedFile,
    file_path: &str,
    prepared: &Prepared,
) -> Vec<ValidationError> {
    let Prepared {
        ruleset,
        table,
        game,
        type_index,
        modifier_keys,
        loc_index,
        registry,
        enum_map,
    } = *prepared;
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

    let ctx = ValidationCtx {
        ast,
        ruleset,
        table,
        enum_map,
        file_path,
        game,
        type_index,
        modifier_keys,
        loc_index,
    };

    // Pre-compute path-based type match (most specific wins)
    let path_type = find_type_by_path(file_path, ruleset);

    // type_per_file: the WHOLE file is a single instance of this type (e.g. an
    // OOB file). Its root children ARE the instance body — there is no per-entry
    // wrapper key — so validate them once against the type's rules and stop.
    if let Some(td) = path_type
        && td.type_per_file
    {
        let inner_rules = find_rules_by_name(&td.name, ruleset);
        let has_content_rules =
            !inner_rules.is_empty() || td.subtypes.iter().any(|st| !st.rules.is_empty());
        if has_content_rules {
            validate_with_type(
                &ctx,
                td,
                &ast.root_children,
                inner_rules,
                &mut scope_context,
                None,
                &mut errors,
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
                        &ctx,
                        children,
                        type_def,
                        &type_key,
                        inner_rules,
                        &mut scope_context,
                        &mut errors,
                    );
                } else {
                    validate_with_type(
                        &ctx,
                        type_def,
                        children,
                        inner_rules,
                        &mut scope_context,
                        Some(&type_key),
                        &mut errors,
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
                    &ctx,
                    grandchildren,
                    type_def,
                    &child_root_key,
                    inner_rules,
                    &mut scope_context,
                    &mut errors,
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
                        &ctx,
                        type_def,
                        node.children.as_slice(),
                        inner_rules,
                        &mut scope_context,
                        Some(&child_root_key),
                        &mut errors,
                    );
                }
                Child::Leaf(leaf_idx) => {
                    let leaf = &ast.arena.leaves[*leaf_idx as usize];
                    if let Value::Clause(children) = &leaf.value {
                        validate_with_type(
                            &ctx,
                            type_def,
                            children.as_slice(),
                            inner_rules,
                            &mut scope_context,
                            Some(&child_root_key),
                            &mut errors,
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
