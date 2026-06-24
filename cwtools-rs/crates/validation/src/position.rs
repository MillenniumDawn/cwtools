//! Position-targeted rule resolution for editor features (completion, hover,
//! goto-definition).
//!
//! [`rules_at_pos`] mirrors the validator's descent (`validate_prepared` →
//! `validate_with_type` → `validate_children`) but follows only the branch that
//! contains the cursor and returns the applicable rules instead of emitting
//! errors. It shares the validator's matching machinery (`matching_candidates`,
//! `alias_overloads`, `merged_rules_for_type`, `flatten_nested_subtype_rules`)
//! so the two can't disagree about what a key resolves to. The entry walk over
//! root children intentionally mirrors `validate_prepared` (lib.rs) — keep the
//! two in step when changing either.

use cwtools_game::scope_engine::ScopeContext;
use cwtools_parser::ast::{Child, ParsedFile, SourcePos, SourceRange, Value};
use cwtools_rules::rules_types::*;

use crate::common::{leaf_value_to_string, unquote_key};
use crate::ctx::ValidationCtx;
use crate::resolve::{
    DispatchInput, ResolvedType, find_grandchild_type, find_rules_by_name, find_type_by_path,
    path_candidates_for_file, resolve_root_child, type_has_content,
};
use crate::rule_core::{
    alias_overloads, flatten_nested_subtype_rules, matching_candidates, merged_rules_for_type,
    rule_matches_leaf_key,
};
use crate::scope::{enter_block_scope, seed_root_scope};
use crate::{Prepared, initial_scope_context};

/// The leaf under the cursor, when the cursor sits on a `key = value` line
/// rather than at a block insert position.
#[derive(Debug, Clone)]
pub struct LeafAtPos {
    pub key: String,
    /// Raw value text; empty for clause values.
    pub value: String,
    /// True when the cursor is on the value side of the `=`.
    pub in_value: bool,
    pub line: u32,
    pub col: u16,
}

/// The rules applicable at a cursor position.
#[derive(Debug, Clone, Default)]
pub struct RuleContext {
    /// Rules for NEW keys in the innermost block containing the cursor
    /// (subtype-merged and nested-subtype-flattened; `AliasField` lefts are NOT
    /// pre-expanded — completion enumerates the category's aliases itself).
    pub child_rules: Vec<(RuleType, Options)>,
    /// When the cursor is on a leaf: every matched rule for that leaf.
    /// `AliasField` matches are expanded to their alias-body overloads, so
    /// `has_completed_focus = X` yields the `LeafRule` whose right side is
    /// `TypeField("focus")`.
    pub value_rules: Vec<(RuleType, Options)>,
    pub leaf: Option<LeafAtPos>,
    /// Scope context at the cursor (None when no game/registry).
    pub scope: Option<ScopeContext>,
}

fn pos_in_range(line: u32, col: u16, range: &SourceRange) -> bool {
    let target = SourcePos { line, col };
    let (s, e) = (&range.start, &range.end);
    if target.line < s.line || target.line > e.line {
        return false;
    }
    if target.line == s.line && target.col < s.col {
        return false;
    }
    if target.line == e.line && target.col > e.col {
        return false;
    }
    true
}

/// Resolve the rules applicable at `(line, col)` (parser coordinates: `line` is
/// 1-based, `col` is 0-based).
///
/// Returns `None` when the position is outside any known entity — at the file
/// top level, in a file no type covers, or under an index-only type with no
/// rule body. Callers fall back to their generic behavior (e.g. root-type
/// snippets) in that case.
pub fn rules_at_pos(
    ast: &ParsedFile,
    file_path: &str,
    prepared: &Prepared,
    line: u32,
    col: u16,
) -> Option<RuleContext> {
    let ruleset = prepared.ruleset;
    let table = prepared.table;
    let mut scope_context = initial_scope_context(file_path, prepared.registry);
    let ctx = ValidationCtx {
        ast,
        ruleset,
        table,
        file_path,
        game: prepared.game,
        type_index: prepared.type_index,
        modifier_keys: prepared.modifier_keys,
        loc_index: prepared.loc_index,
        extra_loc_keys: prepared.extra_loc_keys,
        scope_checks: prepared.scope_checks,
        var_checks: prepared.var_checks,
        loop_vars: std::cell::RefCell::new(Vec::new()),
    };

    // type_per_file: the whole file is one instance; root children are its body.
    let path_type = find_type_by_path(file_path, ruleset);
    if let Some(td) = path_type
        && td.type_per_file
    {
        let inner_rules = find_rules_by_name(&td.name, ruleset);
        if !type_has_content(td, inner_rules) {
            return None;
        }
        return Some(enter_entity(
            &ctx,
            td,
            &ast.root_children,
            inner_rules,
            None,
            &mut scope_context,
            line,
            col,
        ));
    }

    // Find the root child containing the position.
    let child = ast.root_children.iter().find(|c| match c {
        Child::Leaf(idx) => pos_in_range(line, col, &ast.arena.leaves[*idx as usize].pos),
        Child::LeafValue(idx) => pos_in_range(line, col, &ast.arena.leaf_values[*idx as usize].pos),
        _ => false,
    })?;

    let Child::Leaf(leaf_idx) = child else {
        return None;
    };
    let leaf = &ast.arena.leaves[*leaf_idx as usize];
    let Value::Clause(children) = &leaf.value else {
        return None;
    };
    let root_key = table.get_string(leaf.key.normal).unwrap_or_default();
    // Cursor on the root key itself (`my_focus| = { ... }`): top-level context,
    // not inside the entity. Columns are char counts (see parser), so measure the
    // key in chars, not bytes.
    if line == leaf.pos.start.line
        && (col as usize) <= leaf.pos.start.col as usize + root_key.chars().count()
    {
        return None;
    }

    // Resolve which type owns this root node (exact root-key match, then path
    // fallback) via the shared dispatch, then descend toward the cursor.
    // Navigation opts into the content-bearing fallback (`allow_content_fallback`)
    // so the cursor can still descend through a rule-less skip wrapper whose body
    // lives in a sibling base type (e.g. `on_actions` -> `on_action`).
    let file_path_lower = file_path.to_lowercase();
    let path_candidates = path_candidates_for_file(&file_path_lower, ruleset);
    let dispatch = DispatchInput {
        ruleset,
        file_path,
        path_candidates: &path_candidates,
        allow_content_fallback: true,
    };
    match resolve_root_child(&dispatch, &root_key) {
        ResolvedType::Entity {
            type_def,
            inner_rules,
        } => Some(enter_entity(
            &ctx,
            type_def,
            children,
            inner_rules,
            Some(&root_key),
            &mut scope_context,
            line,
            col,
        )),
        ResolvedType::Wrapper {
            type_def,
            inner_rules,
            skip_tail,
        } => descend_wrapper(
            &ctx,
            children,
            type_def,
            &root_key,
            inner_rules,
            skip_tail,
            &mut scope_context,
            line,
            col,
        ),
        ResolvedType::None => None,
    }
}

/// Descend through a skip_root_key wrapper to the grandchild containing the
/// position — mirrors `validate_wrapper_grandchildren`.
#[allow(clippy::too_many_arguments)]
fn descend_wrapper(
    ctx: &ValidationCtx,
    grandchildren: &[Child],
    type_def: &TypeDefinition,
    wrapper_root_key: &str,
    inner_rules: &[(RuleType, Options)],
    skip_tail: &[SkipRootKey],
    scope_context: &mut Option<ScopeContext>,
    line: u32,
    col: u16,
) -> Option<RuleContext> {
    for grandchild in grandchildren {
        let Child::Leaf(gc_idx) = grandchild else {
            continue;
        };
        let gc_leaf = &ctx.ast.arena.leaves[*gc_idx as usize];
        if !pos_in_range(line, col, &gc_leaf.pos) {
            continue;
        }
        let Value::Clause(gc_children) = &gc_leaf.value else {
            return None;
        };
        let gc_key = ctx.table.get_string(gc_leaf.key.normal).unwrap_or_default();
        // Cursor on the instance key itself: treat as outside the entity.
        if line == gc_leaf.pos.start.line
            && (col as usize) <= gc_leaf.pos.start.col as usize + gc_key.chars().count()
        {
            return None;
        }

        if let [next_level, deeper_tail @ ..] = skip_tail {
            if cwtools_index::skip_root_key_matches(next_level, &gc_key) {
                return descend_wrapper(
                    ctx,
                    gc_children,
                    type_def,
                    &gc_key,
                    inner_rules,
                    deeper_tail,
                    scope_context,
                    line,
                    col,
                );
            }
            return None;
        }

        // At the instance level: refine the type per grandchild key, as the
        // validator does.
        let (gc_type_def, gc_rules) =
            match find_grandchild_type(ctx.file_path, wrapper_root_key, &gc_key, ctx.ruleset) {
                Some(t) => {
                    let r = find_rules_by_name(&t.name, ctx.ruleset);
                    if !type_has_content(t, r) {
                        return None;
                    }
                    (t, r)
                }
                None => {
                    if let Some((keys, negate)) = &type_def.type_key_filter {
                        let hit = keys.iter().any(|k| k.eq_ignore_ascii_case(&gc_key));
                        if hit == *negate {
                            return None;
                        }
                    }
                    (type_def, inner_rules)
                }
            };
        return Some(enter_entity(
            ctx,
            gc_type_def,
            gc_children,
            gc_rules,
            Some(&gc_key),
            scope_context,
            line,
            col,
        ));
    }
    None
}

/// Resolve subtypes + seed the root scope for an entity, then descend to the
/// innermost block containing the position — mirrors `validate_with_type`.
#[allow(clippy::too_many_arguments)]
fn enter_entity(
    ctx: &ValidationCtx,
    type_def: &TypeDefinition,
    children: &[Child],
    inner_rules: &[(RuleType, Options)],
    node_key: Option<&str>,
    scope_context: &mut Option<ScopeContext>,
    line: u32,
    col: u16,
) -> RuleContext {
    let (merged, _matched, push_scope) =
        merged_rules_for_type(ctx, type_def, children, inner_rules, node_key);
    if let Some(sc) = scope_context.as_mut() {
        seed_root_scope(sc, type_def, push_scope, node_key, ctx.ruleset, ctx.game);
    }
    descend(ctx, children, merged.as_ref(), scope_context, line, col)
}

/// Walk one block level: find the child containing the position and either
/// recurse into the matched rule bodies or report the leaf/insert context.
/// Mirrors `validate_children`'s matching (without cardinality or errors).
fn descend(
    ctx: &ValidationCtx,
    children: &[Child],
    rules: &[(RuleType, Options)],
    scope_context: &mut Option<ScopeContext>,
    line: u32,
    col: u16,
) -> RuleContext {
    // Nested `subtype[x] = { ... }` blocks carry their fields inside SubtypeRule
    // entries; union all branches like the validator does at depth.
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

    for child in children {
        match child {
            Child::Leaf(idx) => {
                let leaf = &ctx.ast.arena.leaves[*idx as usize];
                if !pos_in_range(line, col, &leaf.pos) {
                    continue;
                }
                let raw_key = ctx.table.get_string(leaf.key.normal).unwrap_or_default();
                let key = unquote_key(&raw_key).to_string();
                // on_key spans the source key (quotes included), measured in chars
                // since columns are char counts; `key` is unquoted and may be shorter.
                let on_key = line == leaf.pos.start.line
                    && (col as usize) <= leaf.pos.start.col as usize + raw_key.chars().count();

                if let Value::Clause(clause_children) = &leaf.value {
                    if on_key {
                        // Editing the key of an existing block: sibling context.
                        return leaf_context(
                            ctx,
                            rules,
                            scope_context,
                            leaf,
                            &key,
                            String::new(),
                            false,
                        );
                    }
                    // Descend into every matching rule body (disjunction → union).
                    let candidates = matching_candidates(
                        rules,
                        &key,
                        ctx.ruleset,
                        ctx.type_index,
                        rule_matches_leaf_key,
                    );
                    let mut next: Vec<(RuleType, Options)> = Vec::new();
                    let mut entered: Option<&Options> = None;
                    // Whether `entered` was first set via an effect/trigger alias
                    // (a real scope block) vs an explicit field rule (`int = {}`
                    // weight). Mirrors the validator's two enter_block_scope sites
                    // so a numeric key resolves to state only for genuine scope
                    // blocks (`129 = {}`), not random_list weights.
                    let mut entered_via_alias = false;
                    for (rule_type, opts) in &candidates {
                        match rule_type {
                            RuleType::NodeRule {
                                left: NewField::AliasField(cat),
                                ..
                            }
                            | RuleType::LeafRule {
                                left: NewField::AliasField(cat),
                                ..
                            } => {
                                for (ort, oopts) in
                                    alias_overloads(ctx.ruleset, ctx.type_index, cat, &key)
                                {
                                    if let RuleType::NodeRule { rules: body, .. } = ort {
                                        next.extend(body.iter().cloned());
                                        if entered.is_none() {
                                            entered_via_alias = true;
                                        }
                                        entered.get_or_insert(oopts);
                                    }
                                }
                            }
                            RuleType::NodeRule { rules: body, .. } => {
                                next.extend(body.iter().cloned());
                                entered.get_or_insert(opts);
                            }
                            _ => {}
                        }
                    }
                    if next.is_empty() {
                        // Unknown block or leaf-only matches: no rule context below
                        // here. Empty child_rules (not the parent's) — suggestions
                        // from the parent level would be wrong inside this block.
                        return RuleContext {
                            scope: scope_context.clone(),
                            ..Default::default()
                        };
                    }
                    if let (Some(sc), Some(opts)) = (scope_context.as_mut(), entered) {
                        enter_block_scope(sc, &key, opts, ctx.game, entered_via_alias);
                    }
                    return descend(ctx, clause_children, &next, scope_context, line, col);
                }

                // A scalar `key = value` is single-line, but the parser's leaf
                // range absorbs trailing whitespace up to the next token (see
                // parse_value). So a cursor on a later, blank line falls inside
                // this leaf's range while actually being a new-field insert
                // position — fall through to the block's child rules instead of
                // offering this leaf's (usually empty) value completions.
                if line != leaf.pos.start.line {
                    continue;
                }
                // Scalar leaf: cursor on a `key = value` line.
                let value = leaf_value_to_string(&leaf.value, ctx.table);
                return leaf_context(ctx, rules, scope_context, leaf, &key, value, !on_key);
            }
            Child::LeafValue(idx) => {
                let lv = &ctx.ast.arena.leaf_values[*idx as usize];
                if !pos_in_range(line, col, &lv.pos) {
                    continue;
                }
                if let Value::Clause(ch) = &lv.value {
                    // Anonymous `{ ... }` block → ValueClauseRule bodies.
                    let next = valueclause_bodies(rules);
                    if next.is_empty() {
                        return RuleContext {
                            scope: scope_context.clone(),
                            ..Default::default()
                        };
                    }
                    return descend(ctx, ch, &next, scope_context, line, col);
                }
                // Bare value: complete against the block's LeafValueRules.
                let value = leaf_value_to_string(&lv.value, ctx.table);
                let value_rules: Vec<(RuleType, Options)> = rules
                    .iter()
                    .filter(|(rt, _)| matches!(rt, RuleType::LeafValueRule { .. }))
                    .cloned()
                    .collect();
                return RuleContext {
                    child_rules: rules.to_vec(),
                    value_rules,
                    leaf: Some(LeafAtPos {
                        key: String::new(),
                        value,
                        in_value: true,
                        line: lv.pos.start.line,
                        col: lv.pos.start.col,
                    }),
                    scope: scope_context.clone(),
                };
            }
            _ => {}
        }
    }

    // No child contains the position: the cursor is at an insert position in
    // this block.
    RuleContext {
        child_rules: rules.to_vec(),
        value_rules: Vec::new(),
        leaf: None,
        scope: scope_context.clone(),
    }
}

fn valueclause_bodies(rules: &[(RuleType, Options)]) -> Vec<(RuleType, Options)> {
    let mut next = Vec::new();
    for (rt, _) in rules {
        if let RuleType::ValueClauseRule { rules: body } = rt {
            next.extend(body.iter().cloned());
        }
    }
    next
}

/// Build the context for a cursor on a leaf: the matched rules become
/// `value_rules` (alias matches expanded to their leaf overloads), the current
/// block's rules stay available as `child_rules` for key edits.
fn leaf_context(
    ctx: &ValidationCtx,
    rules: &[(RuleType, Options)],
    scope_context: &Option<ScopeContext>,
    leaf: &cwtools_parser::ast::Leaf,
    key: &str,
    value: String,
    in_value: bool,
) -> RuleContext {
    RuleContext {
        child_rules: rules.to_vec(),
        value_rules: value_rules_for_key(ctx.ruleset, ctx.type_index, rules, key),
        leaf: Some(LeafAtPos {
            key: key.to_string(),
            value,
            in_value,
            line: leaf.pos.start.line,
            col: leaf.pos.start.col,
        }),
        scope: scope_context.clone(),
    }
}

/// The alias category (`trigger`, `effect`, `modifier`, …) that `key` resolves
/// through within `child_rules`, if any. Editor hovers use it as the header
/// ("trigger" vs "effect") for a usage like `has_completed_focus`.
pub fn alias_category_for_key(
    ruleset: &RuleSet,
    type_index: Option<&cwtools_index::TypeIndex>,
    child_rules: &[(RuleType, Options)],
    key: &str,
) -> Option<String> {
    let candidates =
        matching_candidates(child_rules, key, ruleset, type_index, rule_matches_leaf_key);
    candidates.iter().find_map(|(rt, _)| match rt {
        RuleType::LeafRule {
            left: NewField::AliasField(cat),
            ..
        }
        | RuleType::NodeRule {
            left: NewField::AliasField(cat),
            ..
        } => Some(cat.clone()),
        _ => None,
    })
}

/// The matched rules for `key` within a block whose rules are `child_rules`.
/// Alias-keyed matches are expanded to their alias overloads (so
/// `has_completed_focus` resolves through `alias[trigger:...]` to its
/// `<focus>` right side). Includes matched NodeRules too — completion only
/// reads LeafRule/LeafValueRule rights, while hover wants any matched rule's
/// description. Public so the LSP can resolve a mid-edit `key = |` line where
/// no leaf exists in the last good parse yet.
pub fn value_rules_for_key(
    ruleset: &RuleSet,
    type_index: Option<&cwtools_index::TypeIndex>,
    child_rules: &[(RuleType, Options)],
    key: &str,
) -> Vec<(RuleType, Options)> {
    let candidates =
        matching_candidates(child_rules, key, ruleset, type_index, rule_matches_leaf_key);
    let mut out: Vec<(RuleType, Options)> = Vec::new();
    for (rule_type, opts) in candidates {
        match rule_type {
            RuleType::LeafRule {
                left: NewField::AliasField(cat),
                ..
            }
            | RuleType::NodeRule {
                left: NewField::AliasField(cat),
                ..
            } => {
                for (ort, oopts) in alias_overloads(ruleset, type_index, cat, key) {
                    out.push((ort.clone(), oopts.clone()));
                }
            }
            RuleType::LeafRule { .. } | RuleType::NodeRule { .. } => {
                out.push((rule_type.clone(), opts.clone()))
            }
            _ => {}
        }
    }
    out
}
