//! Block validation: matching each child against candidate rules, cardinality
//! counting/enforcement, and the per-leaf dispatch.

use cwtools_game::scope_engine::ScopeContext;
use cwtools_parser::ast::{Child, Value};
use cwtools_rules::rules_types::*;
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;
use std::sync::LazyLock;

use crate::common::*;
use crate::ctx::ValidationCtx;
use crate::error_codes;
use crate::scope::{enter_block_scope, scope_matches_required};

use super::alias::validate_alias_usage;
use super::leaf::{check_variable_get, field_matches_value, validate_leaf};
use super::matching::{get_rule_key, matching_candidates, rule_matches_leaf_key};
use super::subtype_merge::flatten_nested_subtype_rules;

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
                    with_leaf_value_str(&leaf.value, ctx.table, |raw| {
                        let v = raw.trim_matches('"').trim();
                        let is_default = if v.eq_ignore_ascii_case("yes")
                            || v.eq_ignore_ascii_case("true")
                        {
                            default
                        } else if v.eq_ignore_ascii_case("no") || v.eq_ignore_ascii_case("false") {
                            !default
                        } else {
                            false
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
                    });
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

    // Only the rule kinds actually present in `rules` need a count structure.
    // The dominant block kind (effect/trigger bodies whose sole rule is an
    // `alias_name[...]` wildcard) has no keyed / LeafValue / ValueClause rule at
    // all, so every structure below stays unallocated and the scan is skipped.
    let any_keyed = rules.iter().any(|(rt, _)| get_rule_key(rt).is_some());
    let any_leafvalue = rules
        .iter()
        .any(|(rt, _)| matches!(rt, RuleType::LeafValueRule { .. }));
    let any_valueclause = rules
        .iter()
        .any(|(rt, _)| matches!(rt, RuleType::ValueClauseRule { .. }));

    // Keyed children (Leaf/Node): key string -> count.
    let mut key_counts: FxHashMap<String, usize> = if any_keyed {
        FxHashMap::with_capacity_and_hasher(children.len(), Default::default())
    } else {
        FxHashMap::default()
    };
    // Item 5: LeafValues — count per LeafValueRule index.
    let mut leafvalue_counts: Vec<usize> = if any_leafvalue {
        vec![0usize; rules.len()]
    } else {
        Vec::new()
    };
    // Item 5: ValueClause — count per ValueClauseRule index.
    let mut valueclause_counts: Vec<usize> = if any_valueclause {
        vec![0usize; rules.len()]
    } else {
        Vec::new()
    };

    if !any_keyed && !any_leafvalue && !any_valueclause {
        return (key_counts, leafvalue_counts, valueclause_counts);
    }

    let mut keybuf: SmallVec<[u8; 24]> = SmallVec::new();
    for child in children {
        match child {
            Child::Leaf(idx) if any_keyed => {
                let leaf = &ast.arena.leaves[*idx as usize];
                // Paradox keys are case-insensitive; key the counts in lowercase so
                // a field written `texturefile` satisfies a rule keyed `textureFile`.
                // Lowercase into a reused stack buffer so the owned String is only
                // allocated on the first occurrence of each distinct key.
                keybuf.clear();
                table.with_string(leaf.key.normal, |s| {
                    keybuf.extend_from_slice(unquote_key(s).as_bytes())
                });
                keybuf.make_ascii_lowercase();
                let key: &str = std::str::from_utf8(&keybuf).unwrap_or_default();
                match key_counts.get_mut(key) {
                    Some(c) => *c += 1,
                    None => {
                        key_counts.insert(key.to_owned(), 1);
                    }
                }
            }
            Child::LeafValue(lvidx) => {
                let lv = &ast.arena.leaf_values[*lvidx as usize];
                // An anonymous `{ ... }` block parses as a clause-valued LeafValue;
                // count it toward a ValueClauseRule, not a LeafValueRule.
                if matches!(lv.value, Value::Clause(_)) {
                    if any_valueclause {
                        for (rule_idx, (rule_type, _)) in rules.iter().enumerate() {
                            if matches!(rule_type, RuleType::ValueClauseRule { .. }) {
                                valueclause_counts[rule_idx] += 1;
                            }
                        }
                    }
                } else if any_leafvalue {
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

/// Whether a rule's right-hand side is the `math_expr` value type.
pub(crate) fn rule_right_is_math_expr(rule_type: &RuleType) -> bool {
    matches!(
        rule_type,
        RuleType::LeafRule {
            right: NewField::ValueField(ValueType::MathExpr),
            ..
        }
    )
}

/// The rule set a `math_expr` `{block}` is validated against: a `value` base
/// (itself a math operand), an optional `tooltip`, and any registered
/// `mathexpr` operator key (`add`/`subtract`/…). No `= scalar` catch-all — that
/// is the whole point: an unrecognised key is an unexpected field, not a
/// silently-accepted variable assignment. Operator argument shapes are owned by
/// the config's `alias[mathexpr:*]` definitions, which `validate_children`
/// expands; an operator whose argument is itself `math_expr` recurses here.
static MATH_CLAUSE_RULES: LazyLock<Vec<(RuleType, Options)>> = LazyLock::new(|| {
    let many = Options {
        min: 0,
        max: i32::MAX,
        ..Default::default()
    };
    vec![
        (
            RuleType::LeafRule {
                left: NewField::SpecificField("value".to_string()),
                right: NewField::ValueField(ValueType::MathExpr),
            },
            many.clone(),
        ),
        (
            RuleType::LeafRule {
                left: NewField::SpecificField("tooltip".to_string()),
                right: NewField::ScalarField,
            },
            many.clone(),
        ),
        (
            RuleType::LeafRule {
                left: NewField::AliasField("mathexpr".to_string()),
                right: NewField::AliasField("mathexpr".to_string()),
            },
            many,
        ),
    ]
});

pub(crate) fn math_clause_rules() -> &'static [(RuleType, Options)] {
    &MATH_CLAUSE_RULES
}

/// Validate a `{block}` math expression strictly against [`math_clause_rules`].
pub(super) fn validate_math_clause(
    ctx: &ValidationCtx,
    children: &[Child],
    scope_context: &mut Option<ScopeContext>,
    pos: (u32, u16),
    errors: &mut Vec<ValidationError>,
) {
    validate_children(
        ctx,
        children,
        math_clause_rules(),
        scope_context,
        pos,
        errors,
    );
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
                // Script keys are short; buffer the unquoted key on the stack to
                // avoid a per-leaf heap String. Borrowing across `with_string` is
                // unsafe (the closure holds the table's read guard and validation
                // recurses), so copy the bytes out into an owned stack buffer.
                let mut keybuf: SmallVec<[u8; 24]> = SmallVec::new();
                table.with_string(leaf.key.normal, |s| {
                    keybuf.extend_from_slice(unquote_key(s).as_bytes())
                });
                let key: &str = std::str::from_utf8(&keybuf).unwrap_or_default();
                let candidates =
                    matching_candidates(rules, key, ruleset, type_index, rule_matches_leaf_key);
                // `math_expr` is authoritative: a `{block}` math expression is
                // validated strictly here, BEFORE the candidate disjunction
                // below. A permissive sibling overload (`value_set[variable] =
                // scalar`, present on every variable-math effect) would
                // otherwise accept the block with zero errors and `pick_best_candidate`
                // would discard the strict unexpected-key diagnostic. Bypassing
                // the disjunction keeps the strict check.
                if let Value::Clause(math_children) = &leaf.value
                    && candidates.iter().any(|(rt, _)| rule_right_is_math_expr(rt))
                {
                    let pos = (leaf.pos.start.line, leaf.pos.start.col);
                    validate_math_clause(ctx, math_children, scope_context, pos, errors);
                    continue;
                }
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
                            &[key],
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
                        errors.push(ValidationError::from_code_with(
                            code,
                            ErrorSeverity::Error,
                            file_path,
                            leaf.pos.start.line,
                            leaf.pos.start.col,
                            msg,
                        ));
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
                                key,
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

    // Under-count: anchor on the block's key position (block_pos) so the
    // squiggle lands on the `key = { ... }` opener, not on whatever child
    // happens to be first. When block_pos is (0,0) — the type_per_file
    // sentinel where the whole file is one entity and there is no block key
    // — fall back to the first child to avoid a line-0 diagnostic.
    let (block_line, block_col) = if block_pos != (0, 0) {
        block_pos
    } else {
        children
            .iter()
            .find_map(|c| child_start_pos(c, ast))
            .unwrap_or(block_pos)
    };

    // Aggregate keyed-rule cardinality per (lowercased) key. Duplicate keys are
    // overloads/alternatives (e.g. two `clicksound =` rules in one subtype), so
    // the key is checked once against the most permissive bounds rather than
    // once per overload — otherwise a present-once field reads as missing N-1
    // times, or an absent optional alternative double-reports.
    // Third field tracks strictness: a `~` (soft) minimum on ANY overload of a
    // key makes the whole key's minimum soft, so an under-count is not flagged.
    // Lowercase each keyed rule's key once and reuse it below (both the key_card
    // aggregation and the per-rule report loop need it), instead of re-lowercasing
    // per rule per phase. `None` for non-keyed rules.
    let rule_keys_lower: Vec<Option<String>> = rules
        .iter()
        .map(|(rt, _)| get_rule_key(rt).map(|k| k.to_ascii_lowercase()))
        .collect();

    let mut key_card: FxHashMap<&str, (i32, i32, bool)> =
        FxHashMap::with_capacity_and_hasher(rules.len(), Default::default());
    for (i, (_, opts)) in rules.iter().enumerate() {
        if let Some(lkey) = &rule_keys_lower[i] {
            let e = key_card
                .entry(lkey.as_str())
                .or_insert((opts.min, opts.max, opts.strict_min));
            e.0 = e.0.min(opts.min);
            e.1 = e.1.max(opts.max);
            e.2 = e.2 && opts.strict_min;
        }
    }
    let mut reported_keys: FxHashSet<&str> =
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
                if let (Some(key), Some(lkey)) =
                    (get_rule_key(rule_type), &rule_keys_lower[rule_idx])
                {
                    // Each distinct key is reported at most once (see key_card above).
                    if reported_keys.insert(lkey.as_str()) {
                        let (kmin, kmax, kstrict) = key_card
                            .get(lkey.as_str())
                            .copied()
                            .unwrap_or((opts.min, opts.max, opts.strict_min));
                        let count = key_counts.get(lkey.as_str()).copied().unwrap_or(0) as i32;
                        if count < kmin && kstrict {
                            errors.push(ValidationError::from_code_with(
                                &error_codes::CW242_WRONG_NUMBER,
                                missing_sev,
                                file_path,
                                block_line,
                                block_col,
                                format!(
                                    "Field '{}' appears {} time(s), expected at least {}",
                                    key, count, kmin
                                ),
                            ));
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
                                .find(|c| child_key_matches(c, ast, table, lkey))
                                .and_then(|c| child_start_pos(c, ast))
                                .unwrap_or((block_line, block_col));
                            errors.push(ValidationError::from_code_with(
                                &error_codes::CW242_WRONG_NUMBER,
                                max_sev,
                                file_path,
                                line,
                                col,
                                format!(
                                    "Field '{}' appears {} time(s), expected at most {}",
                                    key, count, kmax
                                ),
                            ));
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
                    errors.push(ValidationError::from_code_with(
                        &error_codes::CW242_WRONG_NUMBER,
                        missing_sev,
                        file_path,
                        block_line,
                        block_col,
                        format!(
                            "LeafValue {:?} appears {} time(s), expected at least {}",
                            right, count, opts.min
                        ),
                    ));
                }
                if count > opts.max {
                    errors.push(ValidationError::from_code_with(
                        &error_codes::CW242_WRONG_NUMBER,
                        max_sev,
                        file_path,
                        block_line,
                        block_col,
                        format!(
                            "LeafValue {:?} appears {} time(s), expected at most {}",
                            right, count, opts.max
                        ),
                    ));
                }
            }
            // Item 5: ValueClauseRule cardinality
            RuleType::ValueClauseRule { .. } => {
                let count = valueclause_counts[rule_idx] as i32;
                if count < opts.min && opts.strict_min {
                    errors.push(ValidationError::from_code_with(
                        &error_codes::CW242_WRONG_NUMBER,
                        missing_sev,
                        file_path,
                        block_line,
                        block_col,
                        format!(
                            "ValueClause appears {} time(s), expected at least {}",
                            count, opts.min
                        ),
                    ));
                }
                if count > opts.max {
                    errors.push(ValidationError::from_code_with(
                        &error_codes::CW242_WRONG_NUMBER,
                        max_sev,
                        file_path,
                        block_line,
                        block_col,
                        format!(
                            "ValueClause appears {} time(s), expected at most {}",
                            count, opts.max
                        ),
                    ));
                }
            }
            _ => {}
        }
    }
}
