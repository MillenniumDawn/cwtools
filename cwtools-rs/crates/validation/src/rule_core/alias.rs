//! Alias-usage validation: resolving `alias_name[cat]` overloads and validating a
//! usage against every overload as a disjunction.

use cwtools_game::scope_engine::ScopeContext;
use cwtools_parser::ast::{Child, Value};
use cwtools_rules::rules_types::*;
use cwtools_string_table::string_table::StringTable;

use crate::common::*;
use crate::ctx::ValidationCtx;
use crate::error_codes;
use crate::scope::{enter_block_scope, scope_matches_required};

use super::children::{rule_right_is_math_expr, validate_children};
use super::leaf::validate_leaf;
use super::matching::{PatternMatch, classify_pattern_match, is_scope_key};

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
    alias_overloads_with_confidence(ruleset, type_index, category, key)
        .into_iter()
        .map(|(rule, _)| rule)
        .collect()
}

/// Gather every alias overload for `key` in a single pass, tagging each with
/// whether the match is `confident` (verified against populated backing data)
/// vs a permissive-only pattern match (backing enum/value set absent/empty).
///
/// A confident pattern match (`enum[..]` / `value[..]` / `<type>` matched against
/// populated data, never the empty/absent permissive fallback) is what the scope
/// check trusts, so a coincidental match against an unpopulated game-derived enum
/// (e.g. `oil` against an empty `enum[equipment_category]` when vanilla isn't
/// indexed) doesn't drag in that alias's unrelated `## scope` and flag a false
/// CW104. Exact, lowercase-retry and `scope_field` overloads are always confident.
///
/// Push order is exact → lowercase → patterns → scope_field; [`alias_overloads`]
/// keeps all, the scope check filters to the confident subset, preserving that
/// order. Order feeds `pick_best`'s tie-break.
fn alias_overloads_with_confidence<'a>(
    ruleset: &'a RuleSet,
    type_index: Option<&cwtools_index::TypeIndex>,
    category: &str,
    key: &str,
) -> Vec<(&'a (RuleType, Options), bool)> {
    // Gather candidate overloads via the precomputed alias index (O(1) exact +
    // O(patterns)) rather than scanning every alias.
    let mut overloads: Vec<(&(RuleType, Options), bool)> = Vec::new();
    if let Some(idxs) = ruleset.alias_exact.get(category).and_then(|m| m.get(key)) {
        for &i in idxs {
            overloads.push((&ruleset.aliases[i].1, true));
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
                overloads.push((&ruleset.aliases[i].1, true));
            }
        }
    }
    if let Some(cat) = ruleset.alias_categories.get(category) {
        for pat in &cat.parsed_patterns {
            // Classify once: a `Confident` match is included in both sets, a
            // `PermissiveOnly` match only in the permissive (all) set.
            match classify_pattern_match(pat, key, ruleset, type_index) {
                PatternMatch::Confident => {
                    overloads.push((&ruleset.aliases[pat.alias_idx].1, true))
                }
                PatternMatch::PermissiveOnly => {
                    overloads.push((&ruleset.aliases[pat.alias_idx].1, false))
                }
                PatternMatch::No => {}
            }
        }
        if let Some(sf_idx) = cat.scope_field_idx
            && is_scope_key(key, ruleset, type_index)
        {
            overloads.push((&ruleset.aliases[sf_idx].1, true));
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
pub(super) fn validate_alias_usage(
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
    // Compute the overload set (with per-overload confidence) ONCE; the scope
    // check below reuses the confident subset instead of re-walking the aliases.
    let overloads_conf = alias_overloads_with_confidence(ruleset, ctx.type_index, category, key);
    if overloads_conf.is_empty() {
        // Category unloaded or no such alias key — accept silently, matching the
        // permissive key-match in field_matches_key.
        return;
    }
    let overloads: Vec<&(RuleType, Options)> =
        overloads_conf.iter().map(|(rule, _)| *rule).collect();

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
        let confident: Vec<&(RuleType, Options)> = overloads_conf
            .iter()
            .filter(|(_, c)| *c)
            .map(|(rule, _)| *rule)
            .collect();
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

    // math_expr is authoritative: when the usage is a `{block}` and any
    // overload types it as math_expr (e.g. `check_expr = math_expr`), validate
    // it strictly and skip the overload disjunction below. Otherwise a
    // permissive sibling overload — typically a pattern alias whose backing enum
    // is unpopulated, so it matches any key with a `variable_field` that accepts
    // the block cleanly — would win clean and discard the strict math
    // diagnostic. Mirrors the same authoritative bypass in `validate_each_child`.
    if let Some(leaf) = leaf
        && matches!(&leaf.value, Value::Clause(_))
        && let Some((mrt, _)) = overloads.iter().find(|(rt, _)| rule_right_is_math_expr(rt))
    {
        validate_leaf(ctx, leaf, mrt, scope_context.as_ref(), errors);
        return;
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
    ValidationError::from_code(code, file_path, line, col, &[category, value])
}
