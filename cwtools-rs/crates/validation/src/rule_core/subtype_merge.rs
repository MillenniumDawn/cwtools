//! Subtype resolution and rule merging: which subtypes match an entity and the
//! merged rule set that validation runs against.

use cwtools_game::scope_engine::ScopeContext;
use cwtools_parser::ast::Child;
use cwtools_rules::rules_types::*;
use rustc_hash::FxHashSet;
use std::borrow::Cow;

use crate::common::*;
use crate::ctx::ValidationCtx;
use crate::scope::seed_root_scope;
use crate::subtype::subtype_matches;

use super::children::validate_children;

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
