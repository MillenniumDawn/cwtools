//! Scope-registry construction and per-block scope seeding/tracking, plus the
//! scope-target / wrong-scope diagnostics (CW243/244/245).

use cwtools_game::constants::Game;
use cwtools_game::scope_engine::{SCOPE_ANY, SCOPE_INVALID, ScopeContext, ScopeId, ScopeLink};
use cwtools_game::scope_registry::{ScopeDefOwned, ScopeRegistry};
use cwtools_rules::rules_types::*;

use crate::common::{ValidationError, looks_like_data_ref};
use crate::error_codes;
use crate::resolve::find_type_rule_opts;

/// Build the runtime [`ScopeRegistry`] from a parsed config (`scopes.cwt` +
/// `links.cwt`). When the config carries no scope defs (e.g. a game without a
/// scopes.cwt), fall back to that game's hardcoded table. This is the bridge
/// that makes the scope engine data-driven.
pub(crate) fn build_scope_registry(ruleset: &RuleSet, game: Game) -> ScopeRegistry {
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

pub(crate) fn scope_matches_required(
    current: ScopeId,
    registry: &ScopeRegistry,
    required: &[String],
) -> bool {
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
    // A requirement is satisfied if the current scope is that scope or a subscope
    // of it (e.g. `character` satisfies a `country` requirement). An unresolvable
    // requirement name does NOT auto-satisfy.
    required.iter().any(|r| {
        registry
            .id_of(r)
            .is_some_and(|rid| registry.is_subscope_or_eq(current, rid))
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
pub(crate) fn validate_scope_target(
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

/// Seed the scope context for a type instance's body. Precedence:
/// 1. a matched subtype's `## push_scope`;
/// 2. the type's root rule `## push_scope` or `## replace_scope` (the
///    state-history `state` object uses `replace_scope = { this = state ... }`);
/// 3. the instance's own key when that's a scope link / data ref (`state = {…}`).
///
/// Caller must `save()` first and `restore()` after.
pub(crate) fn seed_root_scope(
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

/// Apply a `## push_scope = <scope>` value (or a `{ a b }` list): resolve the
/// scope NAME through the registry and push that scope id. `any`/`all` push the
/// wildcard, which is the correct lenient behaviour for `for_each_scope_loop`
/// etc. Falls back to `change_scope` for command-like values (`prev`, `root`).
pub(crate) fn push_named_scope(ctx: &mut ScopeContext, push: &str) {
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
pub(crate) fn enter_block_scope(
    ctx: &mut ScopeContext,
    key: &str,
    opts: &Options,
    game: Option<Game>,
) {
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

pub(crate) fn apply_replace_scopes(
    ctx: &mut ScopeContext,
    replace: &ReplaceScopes,
    game: Option<Game>,
) {
    if game.is_some() {
        ctx.apply_replace_scope(
            replace.root.as_deref(),
            replace.this.as_deref(),
            &replace.froms,
            &replace.prevs,
        );
    }
}
