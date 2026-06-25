//! The rule-vs-AST core: matching children against rules, cardinality,
//! alias-usage resolution, and per-field value checks.

use cwtools_game::scope_engine::ScopeContext;
use cwtools_parser::ast::{Child, Value};
use cwtools_rules::rules_types::*;
use cwtools_string_table::string_table::StringTable;
use rustc_hash::{FxHashMap, FxHashSet};
use std::borrow::Cow;

use crate::common::*;
use crate::ctx::ValidationCtx;
use crate::error_codes;
use crate::loc_field::validate_localisation_field;
use crate::scope::{
    enter_block_scope, scope_matches_required, seed_root_scope, validate_scope_target,
};
use crate::subtype::subtype_matches;

/// Validate a set of children against a type's rules, handling subtypes.
///
/// Collect the base rules (non-SubtypeRule entries) plus the rules of every matching
/// subtype into a single merged list, then validate the children once against that union.
/// This means:
///   - cardinality is counted over the merged rule set, not per-subtype in isolation
///   - a field that exists in any matching subtype is not "unexpected"
///   - SubtypeRule entries that don't match are silently skipped
#[allow(clippy::too_many_arguments)]
pub(crate) fn validate_with_type(
    ctx: &ValidationCtx,
    type_def: &TypeDefinition,
    children: &[Child],
    inner_rules: &[(RuleType, Options)],
    scope_context: &mut Option<ScopeContext>,
    node_key: Option<&str>,
    // Position of the entity node; used as the anchor for required-field errors.
    node_pos: (u32, u16),
    errors: &mut Vec<ValidationError>,
) {
    let game = ctx.game;
    let ruleset = ctx.ruleset;
    if type_def.subtypes.is_empty() {
        let pre_count = errors.len();
        let saved = scope_context.as_ref().map(|sc| sc.save());
        if let Some(sc) = scope_context.as_mut() {
            seed_root_scope(sc, type_def, None, node_key, ruleset, game);
        }
        validate_children(ctx, children, inner_rules, scope_context, node_pos, errors);
        if let (Some(saved), Some(sc)) = (saved, scope_context.as_mut()) {
            sc.restore(saved);
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

    let (merged, matched_subtype_names, push_scope) =
        merged_rules_for_type(ctx, type_def, children, inner_rules, node_key);

    // Step 3: if no subtypes matched and there are no base rules, there's nothing to validate.
    // This handles the case where a type is defined purely via subtypes: a script object that
    // doesn't match any subtype discriminator is silently accepted.
    if matched_subtype_names.is_empty() && merged.is_empty() {
        return;
    }

    let saved = scope_context.as_ref().map(|sc| sc.save());
    if let Some(sc) = scope_context.as_mut() {
        seed_root_scope(sc, type_def, push_scope, node_key, ruleset, game);
    }

    // Step 5: validate children once against the merged rule set.
    let pre_count = errors.len();
    validate_children(
        ctx,
        children,
        merged.as_ref(),
        scope_context,
        node_pos,
        errors,
    );

    // Item 9: warning_only — downgrade all newly-added errors to warnings (F# RuleValidationService.fs:916).
    if type_def.warning_only {
        for err in errors[pre_count..].iter_mut() {
            if err.severity == ErrorSeverity::Error {
                err.severity = ErrorSeverity::Warning;
            }
        }
    }

    if let (Some(saved), Some(sc)) = (saved, scope_context.as_mut()) {
        sc.restore(saved);
    }
}

/// `(merged rules, matched subtype names, push_scope)` — see [`merged_rules_for_type`].
pub(crate) type MergedTypeRules<'a> = (
    Cow<'a, [(RuleType, Options)]>,
    Vec<&'a str>,
    Option<&'a str>,
);

/// Resolve the effective rule set for an entity of `type_def`: determine which
/// subtypes match the children, merge their rules with the base rules, and pick
/// the subtype push_scope. Shared between validation (above) and the
/// position resolver (`crate::position`) so the two can't drift.
///
/// Returns `(merged rules, matched subtype names, push_scope)`. With no
/// subtypes this is just `(inner_rules, [], None)`.
pub(crate) fn merged_rules_for_type<'a>(
    ctx: &ValidationCtx,
    type_def: &'a TypeDefinition,
    children: &[Child],
    inner_rules: &'a [(RuleType, Options)],
    node_key: Option<&str>,
) -> MergedTypeRules<'a> {
    if type_def.subtypes.is_empty() {
        return (Cow::Borrowed(inner_rules), Vec::new(), None);
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
            subtype,
            children,
            ctx.ast,
            ctx.table,
            ctx.ruleset,
            node_key,
            ctx.type_index,
        ) {
            matched_subtype_names.push(subtype.name.as_str());
        }
    }
    // Apply only_if_not: remove a subtype if any of its only_if_not names are in the matched set.
    let all_names_copy: FxHashSet<&str> = matched_subtype_names.iter().copied().collect();
    matched_subtype_names.retain(|name| {
        let st = type_def.subtypes.iter().find(|s| s.name == *name).unwrap();
        !st.only_if_not
            .iter()
            .any(|excl| all_names_copy.contains(excl.as_str()))
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

    // Use Cow to avoid cloning inner_rules when no expansion is needed.
    let merged: Cow<'_, [(RuleType, Options)]>;
    if inner_has_subtype_rules {
        // Path A: expand SubtypeRule entries from inner_rules — must build owned Vec.
        let mut v: Vec<(RuleType, Options)> = Vec::new();
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
                        v.extend(st_rules.iter().map(|(rt, o)| {
                            let mut o2 = o.clone();
                            o2.min = 0;
                            (rt.clone(), o2)
                        }));
                    }
                }
                _ => {
                    v.push((rule_type.clone(), opts.clone()));
                }
            }
        }
        merged = Cow::Owned(v);
    } else {
        // Path B: pull rules directly from the matching SubTypeDefinition entries.
        // When no subtypes add extra rules, borrow inner_rules directly.
        let extra_rules_needed = type_def
            .subtypes
            .iter()
            .any(|s| matched_subtype_names.contains(&s.name.as_str()) && !s.rules.is_empty());
        if extra_rules_needed {
            let mut v: Vec<(RuleType, Options)> = inner_rules.to_vec();
            for subtype in &type_def.subtypes {
                if matched_subtype_names.contains(&subtype.name.as_str()) {
                    // Same min=0 treatment as Path A.
                    v.extend(subtype.rules.iter().map(|(rt, o)| {
                        let mut o2 = o.clone();
                        o2.min = 0;
                        (rt.clone(), o2)
                    }));
                }
            }
            merged = Cow::Owned(v);
        } else {
            // Borrow inner_rules directly — no allocation needed.
            merged = Cow::Borrowed(inner_rules);
        }
    }

    // Step 4: pick push_scope from the first matching subtype that has one.
    let push_scope: Option<&str> = type_def
        .subtypes
        .iter()
        .filter(|s| matched_subtype_names.contains(&s.name.as_str()))
        .find_map(|s| s.push_scope.as_deref());

    (merged, matched_subtype_names, push_scope)
}

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

fn validate_leaf_against_rule(
    ctx: &ValidationCtx,
    leaf: &cwtools_parser::ast::Leaf,
    key: &str,
    rule_type: &RuleType,
    opts: &Options,
    scope_context: &mut Option<ScopeContext>,
    errors: &mut Vec<ValidationError>,
) {
    // `key = ignore_field`: the field's value is accepted unvalidated.
    if rule_left_is_ignore(rule_type) {
        return;
    }
    if let Some(sc) = scope_context.as_ref()
        && !opts.required_scopes.is_empty()
        && !scope_matches_required(sc.current(), sc.registry.as_ref(), &opts.required_scopes)
    {
        let current = sc.current();
        // F# `ConfigRulesRuleWrongScope` (CW247): a trigger/effect/modifier rule
        // used in a scope it doesn't support. (Was the Rust-invented CW400.)
        let code = &error_codes::CW247_RULE_WRONG_SCOPE;
        errors.push(ValidationError::from_code(
            code,
            ctx.file_path,
            leaf.pos.start.line,
            leaf.pos.start.col,
            &[
                key,
                &sc.registry.name_of(current),
                &opts.required_scopes.join(" or "),
            ],
        ));
    }
    match rule_type {
        RuleType::LeafRule { left, .. } => {
            if let NewField::AliasField(category) = left {
                let leaf_pos = (leaf.pos.start.line, leaf.pos.start.col);
                validate_alias_usage(
                    ctx,
                    category,
                    key,
                    Some(leaf),
                    None,
                    leaf_pos,
                    scope_context,
                    errors,
                );
            } else {
                validate_leaf(ctx, leaf, rule_type, scope_context.as_ref(), errors);
                // CW282: a bool field explicitly set to the default declared by
                // `## default_bool = yes|no` is redundant and can be omitted.
                if let Some(default) = opts.default_bool {
                    let raw = leaf_value_to_string(&leaf.value, ctx.table);
                    let v = raw.trim_matches('"').trim();
                    let is_default = match v.to_ascii_lowercase().as_str() {
                        "yes" | "true" => default,
                        "no" | "false" => !default,
                        _ => false,
                    };
                    if is_default {
                        let code = &error_codes::CW282_REDUNDANT_DEFAULT_BOOL;
                        errors.push(ValidationError::from_code(
                            code,
                            ctx.file_path,
                            leaf.pos.start.line,
                            leaf.pos.start.col,
                            &[v],
                        ));
                    }
                }
            }
        }
        RuleType::NodeRule {
            left,
            rules: inner_rules,
            ..
        } => {
            if let NewField::AliasField(category) = left {
                let leaf_pos = (leaf.pos.start.line, leaf.pos.start.col);
                validate_alias_usage(
                    ctx,
                    category,
                    key,
                    Some(leaf),
                    None,
                    leaf_pos,
                    scope_context,
                    errors,
                );
            } else if let Value::Clause(clause_children) = &leaf.value {
                let saved = scope_context.as_ref().map(|sc| sc.save());
                if let Some(sc) = scope_context.as_mut() {
                    // Explicit field rule (e.g. `int = {}` random_list weight): a
                    // numeric key here is NOT a state scope, so `numeric_state_ok=false`.
                    enter_block_scope(sc, key, opts, ctx.game, false);
                }
                validate_children(
                    ctx,
                    clause_children,
                    inner_rules,
                    scope_context,
                    (leaf.pos.start.line, leaf.pos.start.col),
                    errors,
                );
                if let (Some(saved), Some(ref mut sc)) = (saved, scope_context.as_mut()) {
                    sc.restore(saved);
                }
            } else {
                // NodeRule (block expected) but the value is a scalar — kind mismatch.
                let val_str = leaf_value_to_string(&leaf.value, ctx.table);
                errors.push(ValidationError::from_code(
                    &error_codes::CW267_UNEXPECTED_ALIAS_KEY_VALUE,
                    ctx.file_path,
                    leaf.pos.start.line,
                    leaf.pos.start.col,
                    &[key, &val_str],
                ));
            }
        }
        _ => {}
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
    let mut temp: Vec<ValidationError> = Vec::new();
    for i in 0..n {
        temp.clear();
        validate_one(i, &mut temp);
        if temp.is_empty() {
            return; // clean match
        }
        match &best {
            Some(b) if b.len() <= temp.len() => {}
            // New best — take `temp`'s contents, leaving a reusable empty buffer.
            _ => best = Some(std::mem::take(&mut temp)),
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
pub(crate) fn matching_candidates<'a, F>(
    rules: &'a [(RuleType, Options)],
    key: &str,
    ruleset: &RuleSet,
    type_index: Option<&cwtools_index::TypeIndex>,
    matcher: F,
) -> Vec<&'a (RuleType, Options)>
where
    F: Fn(&RuleType, &str, &RuleSet, Option<&cwtools_index::TypeIndex>) -> bool,
{
    let is_specific = |rt: &RuleType| {
        matches!(rt,
        RuleType::LeafRule { left: NewField::SpecificField(s), .. }
        | RuleType::NodeRule { left: NewField::SpecificField(s), .. } if s.eq_ignore_ascii_case(key))
    };
    // A literal `SpecificField` rule wins over catch-all matches; scan once to see
    // if any exists, then collect only the relevant subset (one heap alloc, not two).
    let has_specific = rules
        .iter()
        .any(|(rt, _)| is_specific(rt) && matcher(rt, key, ruleset, type_index));
    rules
        .iter()
        .filter(|(rt, _)| {
            matcher(rt, key, ruleset, type_index) && (!has_specific || is_specific(rt))
        })
        .collect()
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
pub(crate) fn flatten_nested_subtype_rules(
    rules: &[(RuleType, Options)],
) -> Vec<(RuleType, Options)> {
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

pub(crate) fn validate_children(
    ctx: &ValidationCtx,
    children: &[Child],
    rules: &[(RuleType, Options)],
    scope_context: &mut Option<ScopeContext>,
    // Position of the block that owns `children` (its opening `key = {`). Used to
    // anchor cardinality diagnostics when the block is empty — so a missing
    // required field reports on the block's line, not at the file root (0,0).
    block_pos: (u32, u16),
    errors: &mut Vec<ValidationError>,
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

    // Phase 1: count occurrences of all children kinds (for cardinality).
    let (key_counts, leafvalue_counts, valueclause_counts) = count_children(ctx, children, rules);

    // Phase 2: validate each child.
    validate_each_child(ctx, children, rules, scope_context, errors);

    // Phase 3: cardinality enforcement against the phase-1 counts.
    enforce_cardinality(
        ctx,
        children,
        rules,
        block_pos,
        &key_counts,
        &leafvalue_counts,
        &valueclause_counts,
        errors,
    );
}

/// Phase 1 of [`validate_children`]: count occurrences of every child kind so
/// the cardinality pass can check min/max. Returns the three count maps that
/// phase 3 ([`enforce_cardinality`]) consumes:
/// - `key_counts`: lowercased key string -> count (Leaf/Node children),
/// - `leafvalue_counts`: per-rule count of matching `LeafValueRule`s,
/// - `valueclause_counts`: per-rule count of anonymous `{ ... }` clauses.
fn count_children(
    ctx: &ValidationCtx,
    children: &[Child],
    rules: &[(RuleType, Options)],
) -> (FxHashMap<String, usize>, Vec<usize>, Vec<usize>) {
    let ast = ctx.ast;
    let table = ctx.table;
    let ruleset = ctx.ruleset;

    // Keyed children (Leaf/Node): key string -> count.
    let mut key_counts: FxHashMap<String, usize> =
        FxHashMap::with_capacity_and_hasher(children.len(), Default::default());
    // Item 5: LeafValues — count per LeafValueRule index.
    let mut leafvalue_counts: Vec<usize> = vec![0usize; rules.len()];
    // Item 5: ValueClause — count per ValueClauseRule index.
    let mut valueclause_counts: Vec<usize> = vec![0usize; rules.len()];

    for child in children {
        match child {
            Child::Leaf(idx) => {
                let leaf = &ast.arena.leaves[*idx as usize];
                // Paradox keys are case-insensitive; key the counts in lowercase so
                // a field written `texturefile` satisfies a rule keyed `textureFile`.
                let key = table
                    .with_string(leaf.key.normal, |s| unquote_key(s).to_ascii_lowercase())
                    .unwrap_or_default();
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
                            && field_matches_value(right, &lv.value, table, ruleset)
                        {
                            leafvalue_counts[rule_idx] += 1;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    (key_counts, leafvalue_counts, valueclause_counts)
}

/// Phase 2 of [`validate_children`]: validate each child against the matching
/// rules, emitting unexpected-property and per-rule diagnostics. Recurses into
/// nested blocks via [`validate_children`].
fn validate_each_child(
    ctx: &ValidationCtx,
    children: &[Child],
    rules: &[(RuleType, Options)],
    scope_context: &mut Option<ScopeContext>,
    errors: &mut Vec<ValidationError>,
) {
    let ast = ctx.ast;
    let table = ctx.table;
    let file_path = ctx.file_path;
    let ruleset = ctx.ruleset;
    let type_index = ctx.type_index;
    let modifier_keys = ctx.modifier_keys;

    for child in children {
        match child {
            Child::Leaf(idx) => {
                let leaf = &ast.arena.leaves[*idx as usize];
                let key = table
                    .with_string(leaf.key.normal, |s| unquote_key(s).to_string())
                    .unwrap_or_default();
                let candidates =
                    matching_candidates(rules, &key, ruleset, type_index, rule_matches_leaf_key);
                if candidates.is_empty() {
                    // Item 5: dynamic modifier keys — if provided and this key is a
                    // known modifier, accept silently (modifier context mechanism).
                    // The modifier set is built lowercase; compare lowercase. Compute
                    // the lowercase form lazily here — only the no-candidate branch
                    // needs it, and most leaves match a candidate.
                    let key_lower = key.to_lowercase();
                    let is_modifier = modifier_keys
                        .map(|mk| mk.contains(key_lower.as_str()))
                        .unwrap_or(false);
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
                            code: Some(code.id),
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
                                ctx,
                                leaf,
                                &key,
                                rt,
                                opts,
                                scope_context,
                                out,
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
                                ctx,
                                clause_children,
                                vc_rules,
                                scope_context,
                                (lv.pos.start.line, lv.pos.start.col),
                                errors,
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
                            && field_matches_value(right, &lv.value, table, ruleset)
                        {
                            // VariableGetField bare read: validate against the
                            // project-wide variable index (CW246), mirroring the
                            // Leaf path and F# checkVariableGetFieldNE.
                            if let NewField::VariableGetField(_) = right {
                                let raw = leaf_value_to_string(&lv.value, table);
                                check_variable_get(
                                    ctx,
                                    &raw,
                                    lv.pos.start.line,
                                    lv.pos.start.col,
                                    errors,
                                );
                            }
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
            _ => {}
        }
    }
}

/// Phase 3 of [`validate_children`]: enforce cardinality (min/max occurrence)
/// against the counts gathered by [`count_children`]. Reads `key_counts`,
/// `leafvalue_counts`, and `valueclause_counts`; emits CW242 diagnostics.
#[allow(clippy::too_many_arguments)]
fn enforce_cardinality(
    ctx: &ValidationCtx,
    children: &[Child],
    rules: &[(RuleType, Options)],
    block_pos: (u32, u16),
    key_counts: &FxHashMap<String, usize>,
    leafvalue_counts: &[usize],
    valueclause_counts: &[usize],
    errors: &mut Vec<ValidationError>,
) {
    let ast = ctx.ast;
    let table = ctx.table;
    let file_path = ctx.file_path;

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
    let mut key_card: FxHashMap<String, (i32, i32, bool)> =
        FxHashMap::with_capacity_and_hasher(rules.len(), Default::default());
    for (rule_type, opts) in rules.iter() {
        if matches!(
            rule_type,
            RuleType::LeafRule { .. } | RuleType::NodeRule { .. }
        ) && let Some(k) = get_rule_key(rule_type)
        {
            let e = key_card.entry(k.to_ascii_lowercase()).or_insert((
                opts.min,
                opts.max,
                opts.strict_min,
            ));
            e.0 = e.0.min(opts.min);
            e.1 = e.1.max(opts.max);
            e.2 = e.2 && opts.strict_min;
        }
    }
    let mut reported_keys: FxHashSet<String> =
        FxHashSet::with_capacity_and_hasher(key_card.len(), Default::default());

    for (rule_idx, (rule_type, opts)) in rules.iter().enumerate() {
        // Both under- and over-count default to a WARNING (config cardinalities are
        // often stricter than the game, and cardinality-max is emitted as a Warning);
        // an explicit `## severity` still wins.
        let card_sev = opts
            .severity
            .as_ref()
            .map(severity_to_error)
            .unwrap_or(ErrorSeverity::Warning);
        let missing_sev = card_sev;
        let max_sev = card_sev;

        match rule_type {
            RuleType::LeafRule { .. } | RuleType::NodeRule { .. } => {
                if let Some(key) = get_rule_key(rule_type) {
                    let lkey = key.to_ascii_lowercase();
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
                                code: Some(error_codes::CW242_WRONG_NUMBER.id),
                            });
                        }
                        if count > kmax {
                            // Anchor the over-count on the first actual
                            // occurrence of this key rather than the block's
                            // first child — the squiggle belongs on the field
                            // being flagged, not on whatever happens to sit at
                            // the top of the block. (The under-count case has no
                            // occurrence to point at, so it stays on the block.)
                            let (line, col) = children
                                .iter()
                                .find(|c| child_key_matches(c, ast, table, &lkey))
                                .and_then(|c| child_start_pos(c, ast))
                                .unwrap_or((block_line, block_col));
                            errors.push(ValidationError {
                                message: format!(
                                    "Field '{}' appears {} time(s), expected at most {}",
                                    key, count, kmax
                                ),
                                severity: max_sev,
                                line,
                                col,
                                file: file_path.to_string(),
                                code: Some(error_codes::CW242_WRONG_NUMBER.id),
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
                        code: Some(error_codes::CW242_WRONG_NUMBER.id),
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
                        code: Some(error_codes::CW242_WRONG_NUMBER.id),
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
                        code: Some(error_codes::CW242_WRONG_NUMBER.id),
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
                        code: Some(error_codes::CW242_WRONG_NUMBER.id),
                    });
                }
            }
            _ => {}
        }
    }
}

pub(crate) fn rule_matches_leaf_key(
    rule_type: &RuleType,
    key: &str,
    ruleset: &RuleSet,
    type_index: Option<&cwtools_index::TypeIndex>,
) -> bool {
    match rule_type {
        // Cross-kind fallback: a NodeRule can also match a leaf key (e.g. alias blocks)
        RuleType::LeafRule { left, .. } | RuleType::NodeRule { left, .. } => {
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
    if KEYWORDS.iter().any(|kw| key.eq_ignore_ascii_case(kw)) {
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
/// (ROOT/FROM/tag/id/chain), an instance of any type, or a member of a value-set
/// that a `from_data` scope link draws from. HOI4 from-data scope links let an
/// instance (character, state, ideology, ...) — or a dynamically-defined token
/// (`generate_character`'s `token_base`) — open its own scope.
fn is_scope_key(
    key: &str,
    ruleset: &RuleSet,
    type_index: Option<&cwtools_index::TypeIndex>,
) -> bool {
    looks_like_scope_command(key)
        || scope_links_contains(ruleset, key)
        || type_index.is_some_and(|idx| {
            idx.is_any_instance(key) || is_from_data_value_set_member(key, ruleset, idx)
        })
}

/// Whether `key` is a member of a value-set that a `from_data` scope link draws
/// from (`links.cwt`: `data_source = value[<set>]`). Such a link makes any of that
/// set's members a valid scope-opening key (e.g. the `character_token` link's
/// `data_source = value[character_token]` lets a `generate_character` token open a
/// `character` scope). Checked last because it scans `link_inputs`; only reached
/// when the cheaper scope-command / link-name / type-instance checks all miss.
fn is_from_data_value_set_member(
    key: &str,
    ruleset: &RuleSet,
    type_index: &cwtools_index::TypeIndex,
) -> bool {
    if type_index.value_set_values.is_empty() {
        return false;
    }
    ruleset
        .link_inputs
        .iter()
        .filter(|li| li.from_data)
        .flat_map(|li| li.data_source.iter())
        .filter_map(|src| value_set_name(src))
        .any(|set| type_index.value_set_values.values(set).any(|m| m == key))
}

/// Extract `<set>` from a `data_source = value[<set>]` entry; `None` for any other
/// data-source shape (`<type>`, `enum[..]`, a bare scalar).
fn value_set_name(data_source: &str) -> Option<&str> {
    data_source
        .strip_prefix("value[")
        .and_then(|rest| rest.strip_suffix(']'))
        .map(str::trim)
}

/// Case-insensitive membership in `ruleset.scope_links` (a lowercase `String`
/// set), allocating a lowercased key only when `key` actually has uppercase
/// bytes — the common all-lowercase case probes the set directly.
fn scope_links_contains(ruleset: &RuleSet, key: &str) -> bool {
    if key.bytes().any(|b| b.is_ascii_uppercase()) {
        ruleset
            .scope_links
            .contains(&key.to_ascii_lowercase() as &str)
    } else {
        ruleset.scope_links.contains(key)
    }
}

/// Test whether `key` matches the pre-parsed alias pattern.
///
/// The pattern was already split into (prefix, kind, placeholder_name, suffix)
/// at ruleset build time by `ParsedAliasPattern::parse`; this function only
/// does the per-call key-matching work (prefix/suffix strip + membership test).
/// `permissive` controls the fallback when a pattern's backing data is absent or
/// empty (a game-derived `enum[..]`/`value[..]` that wasn't populated, e.g. when
/// vanilla isn't indexed). `true` accepts the key (the key-existence checks want
/// this, to avoid flooding "unknown key" errors); `false` rejects it, so the
/// scope check only trusts a match it can actually verify and never inherits an
/// unrelated alias's `## scope` from a coincidental empty-enum match.
fn parsed_pattern_matches(
    pat: &ParsedAliasPattern,
    key: &str,
    ruleset: &RuleSet,
    type_index: Option<&cwtools_index::TypeIndex>,
    permissive: bool,
) -> bool {
    let pre = pat.prefix.as_str();
    let suf = pat.suffix.as_str();
    if key.len() < pre.len() + suf.len() || !key.starts_with(pre) || !key.ends_with(suf) {
        return false;
    }
    let middle = &key[pre.len()..key.len() - suf.len()];
    let name = pat.placeholder_name.as_str();
    match pat.kind {
        PatternKind::Type => {
            // `<type.subtype>` → check the base type (subtype is a refinement).
            let base = name.split('.').next().unwrap_or(name);
            type_index
                .map(|idx| idx.contains(base, middle))
                .unwrap_or(false)
        }
        PatternKind::Enum => match ruleset.enum_by_name.get(name) {
            Some(&idx) if !ruleset.enums[idx].values.is_empty() => {
                let def = &ruleset.enums[idx];
                def.values.iter().any(|v| v.eq_ignore_ascii_case(middle))
                    || def.values.iter().any(|v| v.starts_with('@'))
                    || enum_is_authoritative(def)
            }
            _ => permissive, // enum absent/empty (game-derived)
        },
        PatternKind::Value => match ruleset.values.get(name) {
            Some(vs) if !vs.is_empty() => vs.iter().any(|v| v == middle),
            _ => permissive, // value set not collected
        },
    }
}

pub(crate) fn field_matches_key(
    field: &NewField,
    key: &str,
    ruleset: &RuleSet,
    type_index: Option<&cwtools_index::TypeIndex>,
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
            if ruleset
                .alias_exact
                .get(category.as_str())
                .is_some_and(|m| m.contains_key(key))
            {
                return true;
            }
            // Case-insensitive retry: command names like `Country_event` resolve to
            // the lowercase `country_event` alias (config alias names are lowercase).
            // Only allocate the lowercased form when `key` actually has uppercase.
            if key.bytes().any(|b| b.is_ascii_uppercase()) {
                let lower = key.to_ascii_lowercase();
                if ruleset
                    .alias_exact
                    .get(category.as_str())
                    .is_some_and(|m| m.contains_key(lower.as_str()))
                {
                    return true;
                }
            }
            match ruleset.alias_categories.get(category.as_str()) {
                // Category has no aliases at all — be permissive (avoid floods).
                None => true,
                Some(cat) => {
                    for pat in &cat.parsed_patterns {
                        if parsed_pattern_matches(pat, key, ruleset, type_index, true) {
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
        // Mirrors `enum_contains` from common.rs: case-insensitive, permissive on
        // absent/empty enums, permissive when any member is an @-constant.
        NewField::ValueField(ValueType::Enum(enum_name)) => {
            match ruleset.enum_by_name.get(enum_name.as_str()) {
                Some(&idx) => {
                    let def = &ruleset.enums[idx];
                    if def.values.is_empty() {
                        return true;
                    }
                    if def.values.iter().any(|v| v.eq_ignore_ascii_case(key)) {
                        return true;
                    }
                    if def.values.iter().any(|v| v.starts_with('@')) {
                        return true;
                    }
                    enum_is_authoritative(def)
                }
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

fn get_rule_key(rule_type: &RuleType) -> Option<&str> {
    match rule_type {
        RuleType::LeafRule { left, .. } | RuleType::NodeRule { left, .. } => field_to_key(left),
        _ => None,
    }
}

fn field_to_key(field: &NewField) -> Option<&str> {
    match field {
        NewField::SpecificField(s) => Some(s.as_str()),
        _ => None,
    }
}

/// Gather every alias overload `alias[cat:key]` that the usage `key` resolves
/// to: exact name, lowercase retry, `<type>`/`value[..]`/`enum[..]` patterns,
/// and the category's `scope_field` entry for scope-switching keys.
///
/// Shared between alias validation (below) and the position resolver
/// (`crate::position`) so completion/hover resolve aliases exactly like the
/// validator does.
pub(crate) fn alias_overloads<'a>(
    ruleset: &'a RuleSet,
    type_index: Option<&cwtools_index::TypeIndex>,
    category: &str,
    key: &str,
) -> Vec<&'a (RuleType, Options)> {
    alias_overloads_impl(ruleset, type_index, category, key, true)
}

/// As [`alias_overloads`], but pattern matches `confident`ly: an `enum[..]` /
/// `value[..]` / `<type>` pattern matches only when its backing data is actually
/// populated, never via the empty/absent permissive fallback. Used by the scope
/// check so a coincidental match against an unpopulated game-derived enum (e.g.
/// `oil` against an empty `enum[equipment_category]` when vanilla isn't indexed)
/// doesn't drag in that alias's unrelated `## scope` and flag a false CW104.
pub(crate) fn confident_alias_overloads<'a>(
    ruleset: &'a RuleSet,
    type_index: Option<&cwtools_index::TypeIndex>,
    category: &str,
    key: &str,
) -> Vec<&'a (RuleType, Options)> {
    alias_overloads_impl(ruleset, type_index, category, key, false)
}

fn alias_overloads_impl<'a>(
    ruleset: &'a RuleSet,
    type_index: Option<&cwtools_index::TypeIndex>,
    category: &str,
    key: &str,
    permissive: bool,
) -> Vec<&'a (RuleType, Options)> {
    // Gather candidate overloads via the precomputed alias index (O(1) exact +
    // O(patterns)) rather than scanning every alias.
    let mut overloads: Vec<&(RuleType, Options)> = Vec::new();
    if let Some(idxs) = ruleset.alias_exact.get(category).and_then(|m| m.get(key)) {
        for &i in idxs {
            overloads.push(&ruleset.aliases[i].1);
        }
    }
    // Case-insensitive retry: usages like `IF`, `Country_event` resolve to the
    // lowercase alias (config alias names are lowercase). Mirrors the fallback in
    // field_matches_key, which matches the key so the body must validate too.
    // Only allocate the lowercased form when `key` actually has uppercase.
    if overloads.is_empty() && key.bytes().any(|b| b.is_ascii_uppercase()) {
        let lower = key.to_ascii_lowercase();
        if let Some(idxs) = ruleset
            .alias_exact
            .get(category)
            .and_then(|m| m.get(lower.as_str()))
        {
            for &i in idxs {
                overloads.push(&ruleset.aliases[i].1);
            }
        }
    }
    if let Some(cat) = ruleset.alias_categories.get(category) {
        for pat in &cat.parsed_patterns {
            if parsed_pattern_matches(pat, key, ruleset, type_index, permissive) {
                overloads.push(&ruleset.aliases[pat.alias_idx].1);
            }
        }
        if let Some(sf_idx) = cat.scope_field_idx
            && is_scope_key(key, ruleset, type_index)
        {
            overloads.push(&ruleset.aliases[sf_idx].1);
        }
    }
    overloads
}

/// The implicit default temp-variable name a loop-effect field declares when it
/// is omitted: `value` → `v`, `index` → `i`, `break` → `break` (HOI4
/// `for_each_loop` & friends, documented in effects.cwt). Any other key is not a
/// loop-variable binding.
fn loop_var_default(key: &str) -> Option<&'static str> {
    match key {
        "value" => Some("v"),
        "index" => Some("i"),
        "break" => Some("break"),
        _ => None,
    }
}

/// Collect the loop-local variable names a loop-effect block exposes to its body,
/// normalized for the variable index.
///
/// A loop effect (`for_each_loop`, `while_loop`, `every_country`, …) is detected
/// purely from its rule shape: a `value`/`index`/`break` field bound to
/// `value_set[variable]`. For each such field we seed its implicit default name
/// (`v`/`i`/`break`) and, when the block explicitly rebinds it (`value = my_elem`),
/// the explicit name too. Seeding both is the lenient choice and matches the
/// `var:NAME` form already accepted. Returns empty for any non-loop block.
fn collect_loop_vars(
    alias_inner: &[(RuleType, Options)],
    children: &[Child],
    ast: &cwtools_parser::ast::ParsedFile,
    table: &StringTable,
) -> Vec<String> {
    // Which keys this alias declares as `<key> = value_set[variable]`.
    let mut seeded: Vec<String> = Vec::new();
    for (rule, _) in alias_inner {
        let RuleType::LeafRule {
            left: NewField::SpecificField(key),
            right: NewField::VariableSetField(_),
        } = rule
        else {
            continue;
        };
        let Some(default) = loop_var_default(key.as_str()) else {
            continue;
        };
        // Default name (used when the key is omitted).
        seeded.push(cwtools_index::VarIndex::normalize(default));
        // Explicit rebinding, if the block provides `<key> = NAME`.
        for child in children {
            let Child::Leaf(idx) = child else { continue };
            let leaf = &ast.arena.leaves[*idx as usize];
            let matches_key = table
                .with_string(leaf.key.normal, |s| {
                    unquote_key(s).eq_ignore_ascii_case(key)
                })
                .unwrap_or(false);
            if matches_key {
                let name = leaf_value_to_string(&leaf.value, table);
                let norm = cwtools_index::VarIndex::normalize(&name);
                if !norm.is_empty() {
                    seeded.push(norm);
                }
            }
        }
    }
    seeded
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
    ctx: &ValidationCtx,
    category: &str,
    key: &str,
    leaf: Option<&cwtools_parser::ast::Leaf>,
    clause_children: Option<&[Child]>,
    // Position to anchor diagnostics when `leaf` is None (node-form usage).
    fallback_pos: (u32, u16),
    scope_context: &mut Option<ScopeContext>,
    errors: &mut Vec<ValidationError>,
) {
    let table = ctx.table;
    let file_path = ctx.file_path;
    let ruleset = ctx.ruleset;
    let overloads = alias_overloads(ruleset, ctx.type_index, category, key);
    if overloads.is_empty() {
        // Category unloaded or no such alias key — accept silently, matching the
        // permissive key-match in field_matches_key.
        return;
    }

    // CW248: an invalid scope command in a chain. Restricted to dotted lower-case
    // chains (`owner.capital`): a bare command that's missing from this config's
    // links.cwt (e.g. `overlord`) is valid-but-unlisted, not invalid, so only
    // chains — where a segment is genuinely unresolvable — are flagged.
    if ctx.scope_checks
        && key.contains('.')
        && !looks_like_data_ref(key)
        && let Some(sc) = scope_context.as_ref()
    {
        let mut probe = sc.clone();
        if matches!(
            probe.change_scope(key),
            cwtools_game::scope_engine::ScopeResult::NotFound
        ) {
            let code = &error_codes::CW248_INVALID_SCOPE_COMMAND;
            let (line, col) = leaf
                .map(|l| (l.pos.start.line, l.pos.start.col))
                .unwrap_or(fallback_pos);
            errors.push(ValidationError::from_code(
                code,
                file_path,
                line,
                col,
                &[key],
            ));
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
    //
    // Modifiers are exempt: a modifier's `## scope` denotes its CATEGORY (where it
    // takes effect), not where it may be written. A country idea/national-spirit
    // `modifier = {}` block legitimately carries state-category modifiers
    // (`state_resource_cost_<resource>`) that cascade to the country's owned
    // states. Scope-checking them like a trigger/effect is a false positive.
    if ctx.scope_checks
        && category != "modifier"
        && let Some(sc) = scope_context.as_ref()
    {
        let reg = sc.registry.as_ref();
        let current = sc.current();
        // Only fire on overloads we matched confidently: a permissive match
        // against an unpopulated game-derived enum/value (or an unindexed type,
        // e.g. `oil` when vanilla resources aren't indexed) must not contribute
        // its unrelated `## scope`. With no confident overload the key's real
        // alias is unverifiable here, so stay lenient and skip the check.
        let confident = confident_alias_overloads(ctx.ruleset, ctx.type_index, category, key);
        let any_ok = confident.is_empty()
            || confident
                .iter()
                .any(|(_, opts)| scope_matches_required(current, reg, &opts.required_scopes));
        if !any_ok {
            let mut expected: Vec<String> = confident
                .iter()
                .flat_map(|(_, o)| o.required_scopes.iter().cloned())
                .collect();
            expected.sort_unstable();
            expected.dedup();
            let code = match category {
                "trigger" => &error_codes::CW104_INCORRECT_TRIGGER_SCOPE,
                "effect" => &error_codes::CW105_INCORRECT_EFFECT_SCOPE,
                _ => &error_codes::CW106_INCORRECT_SCOPE_SCOPE,
            };
            let (line, col) = leaf
                .map(|l| (l.pos.start.line, l.pos.start.col))
                .unwrap_or(fallback_pos);
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
    let mut temp: Vec<ValidationError> = Vec::new();
    for (rule_type, opts) in overloads {
        temp.clear();
        match rule_type {
            RuleType::LeafRule { .. } => {
                if let Some(leaf) = leaf {
                    validate_leaf(ctx, leaf, rule_type, scope_context.as_ref(), &mut temp);
                } else {
                    // Scalar-valued overload but the usage is a block — not a match.
                    let (line, col) = fallback_pos;
                    temp.push(alias_mismatch_error(
                        file_path, category, "{...}", line, col,
                    ));
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
                    let saved = scope_context.as_ref().map(|sc| sc.save());
                    if let Some(sc) = scope_context.as_mut() {
                        // Effect/trigger alias usage: a bare integer block key here
                        // is a HOI4 state-id scope (`129 = {}`), so allow numeric→state.
                        enter_block_scope(sc, key, opts, ctx.game, true);
                    }
                    // Seed loop-local variables: a `for_each_loop`-style block
                    // exposes `value`/`index`/`break` temp variables its body can
                    // read bare. Push them for the body only and truncate back
                    // after, so they don't leak to siblings/parents.
                    let loop_var_base = ctx.loop_vars.borrow().len();
                    let seeded = collect_loop_vars(alias_inner, children, ctx.ast, table);
                    if !seeded.is_empty() {
                        ctx.loop_vars.borrow_mut().extend(seeded);
                    }
                    validate_children(
                        ctx,
                        children,
                        alias_inner,
                        scope_context,
                        leaf.map(|l| (l.pos.start.line, l.pos.start.col))
                            .unwrap_or(fallback_pos),
                        &mut temp,
                    );
                    ctx.loop_vars.borrow_mut().truncate(loop_var_base);
                    if let (Some(saved), Some(sc)) = (saved, scope_context.as_mut()) {
                        sc.restore(saved);
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
                        .unwrap_or_else(|| (String::new(), fallback_pos.0, fallback_pos.1));
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
            // New best — take `temp`'s contents, leaving a reusable empty buffer.
            _ => best = Some(std::mem::take(&mut temp)),
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
        code: Some(code.id),
    }
}

/// Check a `value[variable]` (VariableGetField) read against the project-wide
/// variable index. Emits CW246 when the value names a variable that was never
/// set. Mirrors F# `checkVariableGetField`: bypasses @-vars, inline math, and
/// loc embeds (those resolve dynamically), and only fires when the index is
/// populated AND the variable checks are enabled.
/// Whether `token` names a config-declared built-in variable: a member of the
/// `value[variable]` set (variables.cwt lists engine-provided reads like
/// `faction_leader`, `num_days`, `threat`). These are valid variable references
/// even without the `var:` prefix and are never dynamically "set", so they must
/// not flag CW246. Members may carry a scope suffix (`name@<type>` /
/// `name@enum[...]`); match the base name before the `@`.
fn is_builtin_variable(ruleset: &RuleSet, token: &str) -> bool {
    ruleset.values.get("variable").is_some_and(|members| {
        members.iter().any(|m| {
            let base = m.split('@').next().unwrap_or(m);
            base.eq_ignore_ascii_case(token)
        })
    })
}

fn check_variable_get(
    ctx: &ValidationCtx,
    raw: &str,
    line: u32,
    col: u16,
    errors: &mut Vec<ValidationError>,
) {
    if !ctx.var_checks {
        return;
    }
    let v = raw.trim_matches('"').trim();
    // Dynamic / non-variable forms that resolve at runtime are accepted.
    if v.is_empty()
        || v.starts_with('@')
        || v.starts_with('[')
        || v.contains("$$")
        || v.contains(':')
    {
        return;
    }
    // Strip a `?`/`^` default-value selector before the lookup.
    let core = v.split(['?', '^']).next().unwrap_or(v).trim();
    if core.is_empty() {
        return;
    }
    if !is_builtin_variable(ctx.ruleset, core)
        && !ctx.is_loop_var(core)
        && let Some(idx) = ctx.type_index
        && !idx.var_index.is_empty()
        && !idx.var_index.contains(core)
    {
        errors.push(ValidationError::from_code(
            &error_codes::CW246_UNSET_VARIABLE,
            ctx.file_path,
            line,
            col,
            &[core],
        ));
    }
}

/// The engine resolves textures by stem: a `.dds` reference is satisfied by a
/// shipped `.tga` and vice versa (e.g. vanilla `core.gfx` points at
/// `sort_button_83x29.tga` while only the `.dds` ships). Returns true when the
/// candidate is a texture whose sibling-extension file exists in the index, so
/// CW113 only fires when neither extension is present.
fn texture_sibling_exists(candidate: &str, file_index: &cwtools_index::FileIndex) -> bool {
    let lower = candidate.to_ascii_lowercase();
    let sibling = if let Some(stem) = lower.strip_suffix(".dds") {
        format!("{stem}.tga")
    } else if let Some(stem) = lower.strip_suffix(".tga") {
        format!("{stem}.dds")
    } else {
        return false;
    };
    file_index.contains(&sibling)
}

fn validate_leaf(
    ctx: &ValidationCtx,
    leaf: &cwtools_parser::ast::Leaf,
    rule_type: &RuleType,
    scope_context: Option<&ScopeContext>,
    errors: &mut Vec<ValidationError>,
) {
    let table = ctx.table;
    let file_path = ctx.file_path;
    let type_index = ctx.type_index;
    if let RuleType::LeafRule { right, .. } = rule_type {
        // LocalisationField: check the referenced loc key exists (CW100/CW122)
        // and, when we know the scope, validate the loc string's commands
        // (CW260/CW262). See `validate_localisation_field`.
        if let NewField::LocalisationField { synced, is_inline } = right {
            validate_localisation_field(ctx, leaf, *synced, *is_inline, scope_context, errors);
            return;
        }
        // TypeField: check type_index when available and the index is complete
        // (includes vanilla). When validating a mod without vanilla data the type
        // index only contains mod-defined instances; vanilla instances are absent,
        // so every valid cross-reference would be a false positive.
        if let NewField::TypeField(type_type) = right {
            let raw_value = leaf_value_to_string(&leaf.value, table);
            let value_str = raw_value
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .unwrap_or(&raw_value)
                .to_string();
            // An empty value (`soundeffect = ""`, `textureFile = ""`) is the
            // engine's "none" — there's nothing to resolve, so don't flag it.
            if value_str.is_empty() {
                return;
            }
            // A `[...]` value is inline scripted localisation / a defined_text
            // reference (e.g. `picture = "[GetCivilWarVictorPicture]"`) that the
            // engine resolves at runtime, so it can't be checked against a literal
            // type instance.
            if value_str.starts_with('[') {
                return;
            }
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
            // Subtype-qualified references (`<type.subtype>`, e.g.
            // `<event.country_event>` / `<equipment.naval_equip>`) resolve
            // permissively. The index's `type.subtype` membership is derived from
            // each instance's own discriminators for subtype *activation* and is
            // intentionally incomplete for *references*: a variant that inherits a
            // subtype through `archetype = <type.subtype>` isn't listed, so a
            // strict check would false-flag valid references to it. (Precise
            // subtype-reference validation would need full membership, as F#'s
            // invertedTypeMap has.)
            if let Some(idx) = type_index
                && !cwtools_index::is_subtype_key(type_name)
            {
                // Only flag when the index is complete (vanilla loaded) AND we have
                // known instances for this type AND the reference doesn't resolve.
                let resolved = idx.contains(type_name, &lookup_value)
                    || alt_candidates.iter().any(|c| idx.contains(type_name, c));
                if idx.complete && !idx.instances(type_name).is_empty() && !resolved {
                    let is_event = type_name == "event" || type_name.starts_with("event.");
                    let (code, message) = if is_event {
                        let c = &error_codes::CW222_UNDEFINED_EVENT;
                        (c, c.format(&[&lookup_value]))
                    } else {
                        let key = table
                            .with_string(leaf.key.normal, |s| s.to_string())
                            .unwrap_or_default();
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
                        code: Some(code.id),
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
                && !idx.file_index.is_empty()
            {
                let raw = leaf_value_to_string(&leaf.value, table);
                let value = raw.trim_matches('"').trim();
                // Skip dynamic / templated paths we can't resolve statically.
                let dynamic = value.is_empty()
                    || value.contains('$')
                    || value.contains('[')
                    || value.contains('<');
                if !dynamic {
                    // The reference with the field's configured extension applied
                    // (if any), without the root prefix. Used for the root-prefixed
                    // lookup and the `.asset`-relative fallback below.
                    let mut rel_value = value.to_string();
                    if let Some(ext) = extension
                        && !ext.is_empty()
                        && !rel_value
                            .to_ascii_lowercase()
                            .ends_with(&ext.to_ascii_lowercase())
                    {
                        rel_value.push_str(ext);
                    }
                    let candidate = match prefix {
                        Some(p)
                            if !value
                                .to_ascii_lowercase()
                                .starts_with(&p.to_ascii_lowercase()) =>
                        {
                            format!("{}{}", p, rel_value)
                        }
                        _ => rel_value.clone(),
                    };
                    // A `.asset` `file =` (sound/entity assets) resolves relative
                    // to the .asset's own directory, not the field's root prefix
                    // (e.g. `sound/zom/zom_vo.asset` -> `zom_idle_001.wav` beside
                    // it). Genuinely-missing siblings still fail to resolve.
                    let asset_relative = file_path.to_ascii_lowercase().ends_with(".asset")
                        && idx.file_index.resolve_relative(file_path, &rel_value);
                    if !idx.file_index.contains(&candidate)
                        && !texture_sibling_exists(&candidate, &idx.file_index)
                        && !asset_relative
                    {
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
        //     `ctx.var_checks` because it needs a complete variable index.
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
                } else if ctx.var_checks {
                    // Non-numeric value: it must name a defined variable. Stay
                    // lenient: only flag a single bare token (a `.`-chain is a
                    // scope/target, handled elsewhere) that isn't a scope
                    // keyword/link and isn't in the project variable index.
                    let single_token = !core.contains('.') && !core.contains(':');
                    let is_scopeish = scope_context
                        .map(|sc| resolves_as_scope_key(sc, core))
                        .unwrap_or(false);
                    if single_token
                        && !is_scopeish
                        && !is_builtin_variable(ctx.ruleset, core)
                        && !ctx.is_loop_var(core)
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

        // VariableGetField (rules `value[variable]`): a bare read of a defined
        // variable. Mirrors F# `checkVariableGetField` — the value must name a
        // variable that was set somewhere. Gated like CW246 (needs a complete
        // variable index) so empty-index setups don't false-positive.
        if let NewField::VariableGetField(_) = right {
            let raw = leaf_value_to_string(&leaf.value, table);
            check_variable_get(ctx, &raw, leaf.pos.start.line, leaf.pos.start.col, errors);
            return;
        }

        // Scope-target validation (CW243 target-wrong-scope / CW245 error-in-target):
        // resolve the chain from the current scope. Gated with the other scope checks.
        if let NewField::ScopeField(expected) = right
            && ctx.scope_checks
            && let Some(ctx) = scope_context
        {
            let value = leaf_value_to_string(&leaf.value, table);
            validate_scope_target(ctx, &value, expected, leaf, file_path, errors);
        }

        if !field_matches_value(right, &leaf.value, table, ctx.ruleset) {
            let expected = field_to_description(right);
            let actual = leaf_value_to_string(&leaf.value, table);
            let key = table
                .with_string(leaf.key.normal, |s| s.to_string())
                .unwrap_or_default();
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

pub(crate) fn field_matches_value(
    field: &NewField,
    value: &Value,
    table: &StringTable,
    ruleset: &RuleSet,
) -> bool {
    // Item 2: VALUE-VALIDATOR BYPASSES (F# FieldValidators.fs:82-83, 836-839).
    // Before any type-specific checks, accept scripted variables (@...), localisation
    // references ($$), and inline math ([...]).  These are valid CW script idioms that
    // can legitimately appear in place of any typed value.
    match value {
        Value::String(t) | Value::QString(t)
            if with_match_text(table, t, |text| {
                text.starts_with('@') || text.contains("$$") || text.starts_with('[')
            }) =>
        {
            return true;
        }
        _ => {}
    }

    match (field, value) {
        // --- Boolean ---
        (NewField::ValueField(ValueType::Bool), Value::Bool(_)) => true,
        (NewField::ValueField(ValueType::Bool), Value::String(t))
        | (NewField::ValueField(ValueType::Bool), Value::QString(t)) => {
            with_match_text(table, t, |text| {
                text.eq_ignore_ascii_case("yes") || text.eq_ignore_ascii_case("no")
            })
        }

        // --- Int with range enforcement (item 4) ---
        (NewField::ValueField(ValueType::Int { min, max }), Value::Int(v)) => {
            let v_i64 = *v;
            v_i64 >= i64::from(*min) && v_i64 <= i64::from(*max)
        }
        (NewField::ValueField(ValueType::Int { min, max }), Value::String(t))
        | (NewField::ValueField(ValueType::Int { min, max }), Value::QString(t)) => {
            with_match_text(table, t, |text| {
                if let Ok(v) = text.parse::<i64>() {
                    v >= i64::from(*min) && v <= i64::from(*max)
                } else {
                    false
                }
            })
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
            with_match_text(table, t, |text| {
                if let Ok(v) = text.parse::<f64>() {
                    v >= *min && v <= *max
                } else {
                    false
                }
            })
        }

        // --- Enum ---
        // An enum that is absent OR loaded-but-empty is one whose members come
        // from game data not statically available (provinces, ship_units, ...).
        // Be permissive there rather than flag every value. Integer members
        // (e.g. province ids) are compared by their string form.
        (NewField::ValueField(ValueType::Enum(enum_name)), Value::String(t))
        | (NewField::ValueField(ValueType::Enum(enum_name)), Value::QString(t)) => {
            with_match_text(table, t, |text| enum_contains(ruleset, enum_name, text))
        }
        (NewField::ValueField(ValueType::Enum(enum_name)), Value::Int(i)) => {
            enum_contains(ruleset, enum_name, &i.to_string())
        }
        (NewField::ValueField(ValueType::Enum(enum_name)), Value::Float(f)) => {
            enum_contains(ruleset, enum_name, &f.to_string())
        }

        // --- Percent (item 3): value ends with '%' or is a number ---
        (NewField::ValueField(ValueType::Percent), Value::String(t))
        | (NewField::ValueField(ValueType::Percent), Value::QString(t)) => {
            with_match_text(table, t, |text| {
                text.ends_with('%') || text.parse::<f64>().is_ok()
            })
        }
        (NewField::ValueField(ValueType::Percent), Value::Float(_) | Value::Int(_)) => true,

        // --- Date / DateTime (item 3): basic YYYY.MM.DD[.HH] shape ---
        (NewField::ValueField(ValueType::Date), Value::String(t))
        | (NewField::ValueField(ValueType::Date), Value::QString(t)) => {
            with_match_text(table, t, is_date_shape)
        }
        (NewField::ValueField(ValueType::DateTime), Value::String(t))
        | (NewField::ValueField(ValueType::DateTime), Value::QString(t)) => {
            with_match_text(table, t, is_datetime_shape)
        }

        // --- Ck2Dna (item 3): exactly 32 hex chars (F# FieldValidators.fs:194-204) ---
        (NewField::ValueField(ValueType::Ck2Dna), Value::String(t))
        | (NewField::ValueField(ValueType::Ck2Dna), Value::QString(t)) => {
            with_match_text(table, t, |text| {
                text.len() == 32 && text.chars().all(|c| c.is_ascii_hexdigit())
            })
        }

        // --- Ck2DnaProperty (item 3): length 8 or 32, hex chars (F# FieldValidators.fs:205-211) ---
        (NewField::ValueField(ValueType::Ck2DnaProperty), Value::String(t))
        | (NewField::ValueField(ValueType::Ck2DnaProperty), Value::QString(t)) => {
            with_match_text(table, t, |text| {
                (text.len() == 8 || text.len() == 32) && text.chars().all(|c| c.is_ascii_hexdigit())
            })
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

        // --- SpecificField: case-insensitive string match ---
        (NewField::SpecificField(s), Value::String(t))
        | (NewField::SpecificField(s), Value::QString(t)) => table
            .with_string(t.normal, |text| unquote_key(text).eq_ignore_ascii_case(s))
            .unwrap_or(false),
        // A `= yes` / `= no` rule literal is a SpecificField, but the parser emits
        // Bool for those values — match them up (affects every boolean rule field).
        (NewField::SpecificField(s), Value::Bool(b)) => (s == "yes" && *b) || (s == "no" && !*b),
        (NewField::SpecificField(s), Value::Int(i)) => s == &i.to_string(),
        (NewField::SpecificField(s), Value::Float(f)) => s == &f.to_string(),
        // In Paradox script, `key = yes` and `key = { ... }` are often
        // interchangeable (e.g. `create_intelligence_agency = { ... }`).
        // The parser stores blocks as Value::Clause on a Leaf — accept them
        // when the rule expects a specific scalar.
        (NewField::SpecificField(_), Value::Clause(_)) => true,

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
            with_match_text(table, t, |text| {
                if let Ok(v) = text.parse::<f64>() {
                    v >= *min && v <= *max
                } else {
                    // non-numeric string: accept (could be a scripted variable not caught by bypass)
                    true
                }
            })
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
