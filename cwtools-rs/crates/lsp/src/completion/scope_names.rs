use tower_lsp::lsp_types::*;

use cwtools_info::InfoService;

use super::sort_for_kind;

/// Build best-effort localisation-key completions for .yml files.
///
/// Offers all known loc keys from the InfoService.  Inside a `[...]` data-
/// function block, offers scope/command names instead.  Best-effort only —
/// full CWTools loc completion (F# locComplete:208-243) would need the loc
/// database and scope tracking, which are not yet ported.
pub(crate) fn loc_completions(
    info: &InfoService,
    language: &str,
    registry: Option<&cwtools_game::scope_registry::ScopeRegistry>,
) -> Vec<CompletionItem> {
    // Collect all top-level keys from all files as potential loc keys. Dedup by
    // borrowing &str (not cloning every key into the set) — this walks every
    // workspace file per request, so the per-key String clone was the cost.
    //
    // NOTE: a cross-request cache (#20) is intentionally skipped. The obvious
    // freshness key, `edit_generation`, is not bumped by all the mutations that
    // change `info.files` (the initial scan, `did_close`, and validate's
    // `clear_file` all mutate it without a bump), so keying on it would serve
    // stale completions. The fix would have to live outside completion.rs.
    let mut items: Vec<CompletionItem> = info
        .files
        .values()
        .flat_map(|fi| fi.top_level_keys.iter().map(|(k, _)| k.as_str()))
        .collect::<std::collections::HashSet<&str>>()
        .into_iter()
        .map(|k| CompletionItem {
            label: k.to_string(),
            kind: Some(CompletionItemKind::TEXT),
            detail: Some("loc key".to_string()),
            sort_text: sort_for_kind(Some(CompletionItemKind::TEXT), k),
            ..Default::default()
        })
        .collect();

    // Offer scope names as data-function completions inside [...]
    for name in scope_completion_names(language, registry) {
        let sort_label = name.clone();
        items.push(CompletionItem {
            label: name,
            kind: Some(CompletionItemKind::FUNCTION),
            detail: Some("scope command".to_string()),
            sort_text: sort_for_kind(Some(CompletionItemKind::FUNCTION), &sort_label),
            ..Default::default()
        });
    }

    items
}

/// Chain-keyword prelude for scope completions. These are runtime traversal
/// keywords (`THIS`/`ROOT`/`PREV`/`FROM`) that are not scope types and will
/// not appear in the registry. HOI4 convention is uppercase; other games use
/// lowercase.
fn scope_prelude(language: &str) -> &'static [&'static str] {
    if language == "hoi4" {
        &["THIS", "ROOT", "PREV", "FROM"]
    } else {
        &["this", "root", "prev", "from"]
    }
}

/// Derive scope completion names from the loaded registry when available, with
/// `scope_names_for_game` as the fallback when no registry is loaded.
///
/// The returned list is: chain-keyword prelude + scope type names (from
/// `registry.by_name` keys) + link names (from `registry.links` keys). All
/// registry keys are lowercase; the prelude follows per-game casing.
pub(crate) fn scope_completion_names(
    language: &str,
    registry: Option<&cwtools_game::scope_registry::ScopeRegistry>,
) -> Vec<String> {
    let Some(reg) = registry else {
        return scope_names_for_game(language)
            .iter()
            .map(|s| s.to_string())
            .collect();
    };

    let mut names: Vec<String> = scope_prelude(language)
        .iter()
        .map(|s| s.to_string())
        .collect();

    // Scope type names from the registry (lowercased). Use `by_id` to get the
    // canonical name for each scope (avoids duplicating aliases).
    let mut scope_names: Vec<String> = reg.by_id.values().map(|d| d.name.clone()).collect();
    scope_names.sort_unstable();
    names.extend(scope_names);

    // Named links (owner, capital_scope, every_state, …).
    let mut link_names: Vec<String> = reg.links.keys().cloned().collect();
    link_names.sort_unstable();
    names.extend(link_names);

    names
}

/// Best-effort scope name list for the current game. Used as a fallback when
/// no registry has been loaded.
pub(crate) fn scope_names_for_game(language: &str) -> &'static [&'static str] {
    match language {
        "hoi4" => &[
            "THIS",
            "ROOT",
            "PREV",
            "FROM",
            "OVERLORD",
            "FACTION_LEADER",
            "capital_scope",
            "owner",
        ],
        "stellaris" => &[
            "this",
            "root",
            "prev",
            "from",
            "owner",
            "controller",
            "space_owner",
            "space_controller",
            "solar_system",
        ],
        "eu4" => &[
            "THIS",
            "ROOT",
            "PREV",
            "FROM",
            "EMPEROR",
            "capital_scope",
            "owner",
            "controller",
        ],
        "ck3" => &["this", "root", "prev", "from", "liege", "employer", "host"],
        _ => &["this", "root", "prev", "from"],
    }
}
