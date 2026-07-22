use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

use cwtools_parser::ast::ParsedFile;
use cwtools_rules::rules_types::RuleSet;
use cwtools_validation::position::{rules_at_pos, value_rules_for_key};

use crate::paths::{
    current_token_range_with_encoding, current_token_text_with_encoding,
    line_value_key_with_encoding, logical_path_from_uri, lsp_pos_to_source_in_text,
};
use crate::{AstSource, Backend, CompletionCacheEntry};

mod builders;
mod cwt;
mod resolve;
mod scope_names;
mod snippets;

pub(crate) use builders::{
    ValueCompletionSets, completions_from_rules, expanded_modifier_scopes, root_type_snippets,
    value_completions, value_rules_need_loc_keys,
};
pub(crate) use scope_names::loc_completions;
pub(crate) use snippets::generate_node_snippet;

/// Build a `sortText` so the most relevant items surface first as the user
/// iterates. The LSP spec is clear the SERVER must return all valid items and
/// let the client filter by the typed prefix, so the natural label sort is
/// what the user sees if every item has the same prefix. The kind buckets
/// below keep the order useful even when a half-typed word matches many
/// items: specific leaf fields ahead of node blocks ahead of alias-driven
/// keys ahead of type instances ahead of enum values ahead of scope names
/// ahead of generic text. The bucket prefix (`0_` ... `9_`) is fixed-width
/// so a later secondary sort by label stays stable.
fn sort_for_kind(kind: Option<CompletionItemKind>, label: &str) -> Option<String> {
    let bucket = match kind? {
        CompletionItemKind::FIELD => "1",   // specific leaf key (concrete)
        CompletionItemKind::STRUCT => "2",  // specific node key + type def
        CompletionItemKind::KEYWORD => "3", // alias, bool yes/no
        CompletionItemKind::ENUM_MEMBER => "4", // enum value
        CompletionItemKind::VALUE => "5",   // scope name (value side)
        CompletionItemKind::CONSTANT => "6", // variable, value set member
        CompletionItemKind::REFERENCE => "7", // type instance reference
        CompletionItemKind::FUNCTION => "8", // scope command ([GetName])
        CompletionItemKind::TEXT => "9",    // loc key, generic text
        _ => "9",
    };
    Some(format!("{}_{}", bucket, label))
}

/// Stamp an explicit replace-range on every item so the client filters and
/// inserts against exactly the identifier token under the cursor. The LSP spec
/// lets the client guess the replaced word when an item carries no `textEdit`,
/// and that guess is wrong right after a backspace across a `=` / `<` / `>`:
/// the client filters the whole list against the operator (or empty string)
/// and the ranking collapses to noise — the "matching is off / irrelevant
/// context after backspace" symptom. An explicit range pins the filter input
/// to the typed text. `insert_text` (snippets) moves into `text_edit.new_text`
/// so `insert_text_format` still applies; `filter_text` is pinned to the label
/// so the client never filters against a snippet body.
fn anchor_items(items: &mut [CompletionItem], range: Range) {
    for it in items.iter_mut() {
        if it.text_edit.is_some() {
            continue;
        }
        let new_text = it.insert_text.take().unwrap_or_else(|| it.label.clone());
        if it.filter_text.is_none() {
            it.filter_text = Some(it.label.clone());
        }
        it.text_edit = Some(CompletionTextEdit::Edit(TextEdit { range, new_text }));
    }
}

/// A resolved-context list at or under this size is returned unfiltered with
/// `is_incomplete: false`: small enough that VS Code filters and re-filters it
/// client-side for free as the user keeps typing, with zero further requests
/// until a word boundary or trigger char forces a re-query.
const CONTEXT_COMPLETE_THRESHOLD: usize = 750;
/// Above the threshold, a resolved-context list is subsequence-filtered by the
/// typed token and truncated to this many items (see [`filter_and_cap`])
/// before it's marked `is_incomplete: true` — the response stays cheap to
/// serialize and the client re-queries on the next keystroke anyway.
const CONTEXT_CAP: usize = 1000;

/// Case-insensitive subsequence match: every character of `needle` appears in
/// `haystack` in the same order, not necessarily contiguously. This is a
/// superset of VS Code's own fuzzy matcher, so filtering by it never hides an
/// item the client would otherwise show — it only trims candidates the client
/// would filter out anyway, shrinking the payload. An empty `needle` matches
/// everything.
fn subsequence_match(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let mut needle_it = needle.chars().flat_map(char::to_lowercase).peekable();
    for c in haystack.chars().flat_map(char::to_lowercase) {
        if needle_it.peek() == Some(&c) {
            needle_it.next();
        }
    }
    needle_it.peek().is_none()
}

/// How well `hay` matches the typed token: exact (0) ahead of prefix (1)
/// ahead of mere subsequence (2), case-insensitively.
fn token_match_rank(hay: &str, token: &str) -> u8 {
    if hay.eq_ignore_ascii_case(token) {
        0
    } else if hay
        .get(..token.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(token))
    {
        1
    } else {
        2
    }
}

/// Drop every item whose `filter_text` (or `label`) doesn't subsequence-match
/// `token`, then sort by match quality ([`token_match_rank`]) and `sort_text`
/// (falling back to `label`, same as the client would) so a later truncation
/// keeps the most relevant items — in particular an exact match for the typed
/// token can never be truncated away behind better-bucketed items (#94). An
/// empty `token` matches everything, so only the `sort_text` order applies.
fn filter_by_token(items: Vec<CompletionItem>, token: &str) -> Vec<CompletionItem> {
    let mut items = items;
    fn hay(it: &CompletionItem) -> &str {
        it.filter_text.as_deref().unwrap_or(it.label.as_str())
    }
    if !token.is_empty() {
        items.retain(|it| subsequence_match(hay(it), token));
    }
    items.sort_by(|a, b| {
        if !token.is_empty() {
            let ra = token_match_rank(hay(a), token);
            let rb = token_match_rank(hay(b), token);
            if ra != rb {
                return ra.cmp(&rb);
            }
        }
        let ka = a.sort_text.as_deref().unwrap_or(a.label.as_str());
        let kb = b.sort_text.as_deref().unwrap_or(b.label.as_str());
        ka.cmp(kb)
    });
    items
}

/// [`filter_by_token`] then truncate to `cap`. Returns the (possibly shrunk)
/// list plus whether anything was dropped — by the filter, the cap, or both —
/// so the caller can decide whether the result is safe to mark complete.
fn filter_and_cap(
    items: Vec<CompletionItem>,
    token: &str,
    cap: usize,
) -> (Vec<CompletionItem>, bool) {
    let total = items.len();
    let mut filtered = filter_by_token(items, token);
    let dropped = filtered.len() < total || filtered.len() > cap;
    filtered.truncate(cap);
    (filtered, dropped)
}

fn prepare_context_items(
    items: Vec<CompletionItem>,
    built_dropped: usize,
    token: &str,
    ast_clean: bool,
    ast_current: bool,
    complete_threshold: usize,
    cap: usize,
) -> (Vec<CompletionItem>, bool, &'static str) {
    // A list is only "complete" (client filters it locally with no further
    // requests) if the builders dropped nothing: any build-time prefiltered
    // candidate could match a different token after a backspace.
    if built_dropped == 0 && ast_clean && ast_current && items.len() <= complete_threshold {
        return (items, false, "complete");
    }
    let (items, _) = filter_and_cap(items, token, cap);
    (items, true, "filtered")
}

/// Snapshotted ruleset-derived state for one completion request. The `Arc`s
/// carry the lifetime across the request so the helpers can take borrows
/// without holding the rules read guard.
type RulesSnapshot = (
    Option<Arc<RuleSet>>,
    Arc<HashSet<String>>,
    Arc<HashMap<String, Vec<String>>>,
    Option<Arc<cwtools_game::scope_registry::ScopeRegistry>>,
);

/// One line per completion request on `target: "cwtools_completion"`, emitted
/// before every return path. Cheap when the level is disabled — tracing
/// checks that before formatting the fields — so it stays on unconditionally.
/// `strategy` is one of `"complete"` (small unfiltered list), `"filtered"`
/// (subsequence-prefiltered and capped), or `"none"` (nothing to offer);
/// `incomplete` — whether the response is flagged `is_incomplete` — follows
/// directly from it, so it isn't a separate parameter.
#[allow(clippy::too_many_arguments)]
fn log_completion_summary(
    total: Duration,
    ast: Duration,
    rules: Duration,
    build: Duration,
    items: usize,
    strategy: &str,
    path: &str,
    ast_source: &str,
) {
    let incomplete = strategy == "filtered";
    tracing::info!(
        target: "cwtools_completion",
        total_us = total.as_micros() as u64,
        ast_us = ast.as_micros() as u64,
        rules_us = rules.as_micros() as u64,
        build_us = build.as_micros() as u64,
        items,
        incomplete,
        strategy,
        path,
        ast_source,
    );
}

impl Backend {
    fn completion_loc_keys(&self, token: &str) -> HashSet<String> {
        let overlay_keys = self.loc_overlay_keys();
        let index_guard = self.state.loc_index.read();
        let keys = index_guard
            .as_ref()
            .map(|index| index.union())
            .into_iter()
            .flat_map(|keys| keys.iter())
            .chain(overlay_keys.iter());
        let mut selected = BTreeSet::new();
        for key in keys.filter(|key| subsequence_match(key, token)) {
            let key = key.as_str();
            let ranked = (
                !key.get(..token.len())
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(token)),
                key,
            );
            if selected.len() < CONTEXT_CAP {
                selected.insert(ranked);
            } else if selected.last().is_some_and(|largest| ranked < *largest)
                && selected.insert(ranked)
            {
                selected.pop_last();
            }
        }
        selected
            .into_iter()
            .map(|(_, key)| key.to_owned())
            .collect()
    }

    #[tracing::instrument(
        skip_all,
        fields(
            uri = %params.text_document_position.text_document.uri,
            line = params.text_document_position.position.line,
            col = params.text_document_position.position.character,
        )
    )]
    pub(crate) async fn completion_impl(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        self.mark_activity();
        let t_start = Instant::now();
        let mut ast_dur = Duration::ZERO;
        let mut rules_dur = Duration::ZERO;
        let mut build_dur = Duration::ZERO;

        let uri = params.text_document_position.text_document.uri.to_string();
        let pos = params.text_document_position.position;
        let position_encoding = self.state.config.read().position_encoding.clone();

        if crate::paths::is_cwt_file(&uri) {
            let text = self
                .state
                .documents
                .lock()
                .get(&uri)
                .map(|doc| Arc::clone(&doc.text))
                .unwrap_or_default();
            let range = cwt::cwt_completion_range(&text, pos, &position_encoding);
            let token = current_token_text_with_encoding(
                &text,
                pos.line,
                pos.character,
                range.start.character,
                &position_encoding,
            );
            let filter_token = token
                .split_once('[')
                .map_or(token.as_str(), |(head, _)| head);
            let mut items = filter_by_token(
                cwt::cwt_completions(&text, pos, &position_encoding),
                filter_token,
            );
            anchor_items(&mut items, range);
            let strategy = if items.is_empty() { "none" } else { "complete" };
            log_completion_summary(
                t_start.elapsed(),
                ast_dur,
                rules_dur,
                build_dur,
                items.len(),
                strategy,
                "cwt",
                AstSource::None.as_str(),
            );
            return Ok(
                (!items.is_empty()).then_some(CompletionResponse::List(CompletionList {
                    is_incomplete: true,
                    items,
                })),
            );
        }

        // Resource and unsupported files never enter the game-script fallback.
        // Localisation has a separate path below.
        if !crate::paths::is_loc_file(&uri) && !crate::paths::is_script_file(&uri) {
            log_completion_summary(
                t_start.elapsed(),
                ast_dur,
                rules_dur,
                build_dur,
                0,
                "none",
                "unsupported",
                AstSource::None.as_str(),
            );
            return Ok(None);
        }

        // Fast-typing cancel: each new request for the same URI bumps the
        // per-URI generation. The request captures the value at entry and
        // re-checks it before doing any heavy work; if a newer request has
        // already started, this one returns `None` so the runtime can drop
        // the work. Stops a burst of N keystrokes from stacking N parallel
        // AST walks when only the latest one matters.
        let my_generation = {
            let mut gens = self.state.completion_generation.lock();
            let g = gens.entry(uri.clone()).or_insert(0);
            *g += 1;
            *g
        };
        let is_stale =
            || self.state.completion_generation.lock().get(&uri).copied() > Some(my_generation);
        if is_stale() {
            log_completion_summary(
                t_start.elapsed(),
                ast_dur,
                rules_dur,
                build_dur,
                0,
                "none",
                "none",
                AstSource::None.as_str(),
            );
            return Ok(None);
        }

        // Try context-aware completions first: resolve the rules at the cursor
        // with the validation engine's own descent (aliases, typed keys,
        // subtypes, skip_root_key — see cwtools_validation::position).
        let (ws_prefix, language, scope_checks, var_checks) = {
            let cfg = self.state.config.read();
            (
                cfg.workspace_prefix.clone(),
                cfg.language.clone(),
                cfg.scope_checks,
                cfg.var_checks,
            )
        };
        let logical_path = logical_path_from_uri(&uri, &ws_prefix);

        // Snapshot the doc text + AST into owned data, then drop the
        // `documents` guard before any heavy work. `documents.lock()` is the
        // only exclusive lock in the LSP state, so holding it across the whole
        // completion blocks `did_open`/`did_change`/`did_close` and the
        // debounced validate's AST update for the duration — the worst case
        // being the user typing into a file whose previous validate is still
        // running. The same pattern for the rules guard: clone the Arcs and
        // drop the guard. The helpers below take borrows, so the Arcs carry
        // the lifetime across the work without holding the lock. `text` is an
        // `Arc<str>`, so this clone is a refcount bump, not a copy of the
        // whole document.
        let doc_text: Arc<str> = {
            let docs = self.state.documents.lock();
            docs.get(&uri).map(|d| d.text.clone()).unwrap_or_default()
        };
        let (lsp_line, lsp_col) = lsp_pos_to_source_in_text(&doc_text, pos, &position_encoding);
        // Replace-range for every item the script paths return: the identifier
        // token under the cursor. Loc completion keeps its own behavior (the
        // cached items are shared and the token shape differs), so it is not
        // anchored here.
        let replace_range = current_token_range_with_encoding(
            &doc_text,
            pos.line,
            pos.character,
            &position_encoding,
        );
        // The typed token so far (up to the cursor, not the whole replace
        // range): the subsequence prefilter below matches candidates against
        // this, not the range end, since the range may extend past the cursor
        // mid-word.
        let token = current_token_text_with_encoding(
            &doc_text,
            pos.line,
            pos.character,
            replace_range.start.character,
            &position_encoding,
        );
        let (ruleset_arc, modifier_keys_arc, modifier_scopes_arc, scope_registry_arc): RulesSnapshot = {
            let rules_guard = self.state.rules.read();
            (
                rules_guard.ruleset.clone(),
                rules_guard.modifier_keys.clone(),
                rules_guard.modifier_scopes.clone(),
                rules_guard.scope_registry.clone(),
            )
        };
        // Drop the read guard before the heavy work. Bump the generation
        // check here too — the rule-snapshot block is cheap, but we want the
        // staleness gate to cover the full body of the function from here.
        if is_stale() {
            log_completion_summary(
                t_start.elapsed(),
                ast_dur,
                rules_dur,
                build_dur,
                0,
                "none",
                "none",
                AstSource::None.as_str(),
            );
            return Ok(None);
        }

        // Localisation completion is syntax-sensitive: ordinary text and `$...$`
        // references offer loc keys, while an open `[...]` offers data functions.
        if crate::paths::is_loc_file(&uri) {
            let context = scope_names::loc_completion_context(&doc_text, pos, &position_encoding);
            let loc_range =
                scope_names::loc_completion_range(&doc_text, pos, context, &position_encoding);
            let loc_token = current_token_text_with_encoding(
                &doc_text,
                pos.line,
                pos.character,
                loc_range.start.character,
                &position_encoding,
            );
            let t_build = Instant::now();
            let loc_keys = if context == scope_names::LocCompletionContext::DataFunction {
                HashSet::new()
            } else {
                self.completion_loc_keys(&loc_token)
            };
            let items =
                loc_completions(&loc_keys, &language, scope_registry_arc.as_deref(), context);
            build_dur = t_build.elapsed();
            let mut items = filter_by_token(items, &loc_token);
            anchor_items(&mut items, loc_range);
            log_completion_summary(
                t_start.elapsed(),
                ast_dur,
                rules_dur,
                build_dur,
                items.len(),
                if items.is_empty() { "none" } else { "filtered" },
                "loc",
                AstSource::None.as_str(),
            );
            return Ok(
                (!items.is_empty()).then_some(CompletionResponse::List(CompletionList {
                    is_incomplete: true,
                    items,
                })),
            );
        }

        // `Some(items)` = the rule context resolved (items may still be empty:
        // an unknown block where suggestions from any other level would be
        // wrong). `None` = no doc/ruleset/AST — fall through to the flat list.
        //
        // `ast_for` returns the last good parse, or (when the last parse failed)
        // a fresh parse of the live text for this request only, so a half-typed
        // buffer still resolves a context.
        //
        // The bool paired with the item list is `resolved_value_pos`: true when
        // the cursor sat at a leaf VALUE position whose rule set was concretely
        // matched (non-empty value rules). When such a position yields no items,
        // the flat variable dump below must NOT stand in for it — that dump is
        // the #74/#75/#79 bug. Key positions and unresolved contexts keep the
        // bool false so the fallback still fires for them.
        let t_ast = Instant::now();
        let mut ast_source = AstSource::None;
        let effective_ast: Option<Arc<ParsedFile>> = self.ast_snapshot_for(&uri).map(|snapshot| {
            ast_source = snapshot.source;
            snapshot.ast
        });
        ast_dur = t_ast.elapsed();
        // Whether the AST the context resolved against parsed with no errors.
        // A buffer with an unclosed clause elsewhere is still in flux — the
        // resolved context can flip on the very next keystroke — so a small
        // list from a dirty parse must stay `is_incomplete: true` even though
        // its size alone would otherwise qualify it as `"complete"` below.
        let mut context_is_clean = false;
        let context_items: Option<(Vec<CompletionItem>, usize, bool)> =
            match (effective_ast, ruleset_arc.as_ref()) {
                (Some(ast), Some(rs)) => {
                    if is_stale() {
                        log_completion_summary(
                            t_start.elapsed(),
                            ast_dur,
                            rules_dur,
                            build_dur,
                            0,
                            "none",
                            "none",
                            ast_source.as_str(),
                        );
                        return Ok(None);
                    }
                    context_is_clean = ast.errors.is_empty();
                    let info_guard = self.state.info_service.read();
                    let game = cwtools_game::constants::Game::from_str(&language);
                    let prepared = crate::validate::make_prepared(
                        rs,
                        &self.state.string_table,
                        game,
                        &info_guard.type_index,
                        &modifier_keys_arc,
                        None,
                        None,
                        scope_registry_arc.as_ref(),
                        scope_checks,
                        var_checks,
                    );
                    let t_rules = Instant::now();
                    let rctx_opt =
                        rules_at_pos(&ast, &logical_path, &prepared, lsp_line, lsp_col, true);
                    rules_dur = t_rules.elapsed();
                    let t_build = Instant::now();
                    let items = match rctx_opt {
                        // Outside any known entity — offer root-type snippets.
                        None => Some((root_type_snippets(rs, &logical_path), 0, false)),
                        Some(rctx) => {
                            let ((items, built_dropped), resolved_value_pos) =
                                if rctx.leaf.as_ref().is_some_and(|l| l.in_value) {
                                    let is_bare_key = rctx.leaf.as_ref().is_some_and(|l| {
                                        l.key.is_empty() && rctx.value_rules.is_empty()
                                    });
                                    if is_bare_key {
                                        // Bare token (no `=`): treat as a key being typed, not a value.
                                        (
                                            completions_from_rules(
                                                &rctx.child_rules,
                                                rs,
                                                &info_guard,
                                                &language,
                                                &modifier_keys_arc,
                                                &modifier_scopes_arc,
                                                scope_registry_arc.as_deref(),
                                                rctx.scope.as_ref().map(|s| s.current()),
                                                &token,
                                            ),
                                            false,
                                        )
                                    } else {
                                        let loc_keys =
                                            if value_rules_need_loc_keys(&rctx.value_rules) {
                                                self.completion_loc_keys(&token)
                                            } else {
                                                Default::default()
                                            };
                                        (
                                            value_completions(
                                                &rctx.value_rules,
                                                rs,
                                                &info_guard,
                                                scope_registry_arc.as_deref(),
                                                &language,
                                                ValueCompletionSets {
                                                    modifier_keys: &modifier_keys_arc,
                                                    modifier_scopes: &modifier_scopes_arc,
                                                    loc_keys: &loc_keys,
                                                },
                                                rctx.scope.as_ref().map(|s| s.current()),
                                                &token,
                                            ),
                                            !rctx.value_rules.is_empty(),
                                        )
                                    }
                                } else if let Some(key) = line_value_key_with_encoding(
                                    &doc_text,
                                    pos.line,
                                    pos.character,
                                    &position_encoding,
                                ) {
                                    // Mid-edit `key = |`: the last good parse has no such
                                    // leaf yet; resolve the value rules from the live line.
                                    let vr = value_rules_for_key(
                                        rs,
                                        Some(&info_guard.type_index),
                                        &rctx.child_rules,
                                        &key,
                                    );
                                    let resolved = !vr.is_empty();
                                    let loc_keys = if value_rules_need_loc_keys(&vr) {
                                        self.completion_loc_keys(&token)
                                    } else {
                                        Default::default()
                                    };
                                    (
                                        value_completions(
                                            &vr,
                                            rs,
                                            &info_guard,
                                            scope_registry_arc.as_deref(),
                                            &language,
                                            ValueCompletionSets {
                                                modifier_keys: &modifier_keys_arc,
                                                modifier_scopes: &modifier_scopes_arc,
                                                loc_keys: &loc_keys,
                                            },
                                            rctx.scope.as_ref().map(|s| s.current()),
                                            &token,
                                        ),
                                        resolved,
                                    )
                                } else {
                                    (
                                        completions_from_rules(
                                            &rctx.child_rules,
                                            rs,
                                            &info_guard,
                                            &language,
                                            &modifier_keys_arc,
                                            &modifier_scopes_arc,
                                            scope_registry_arc.as_deref(),
                                            rctx.scope.as_ref().map(|s| s.current()),
                                            &token,
                                        ),
                                        false,
                                    )
                                };
                            Some((items, built_dropped, resolved_value_pos))
                        }
                    };
                    build_dur = t_build.elapsed();
                    items
                }
                _ => None,
            };

        if let Some((items, built_dropped, resolved_value_pos)) = context_items {
            // `built_dropped > 0` with an empty list still means the context
            // resolved (every candidate was prefiltered out) — return the empty
            // incomplete list rather than falling into the flat fallback dump.
            if !items.is_empty() || built_dropped > 0 {
                let (mut items, is_incomplete, strategy) = prepare_context_items(
                    items,
                    built_dropped,
                    &token,
                    context_is_clean,
                    ast_source.is_current(),
                    CONTEXT_COMPLETE_THRESHOLD,
                    CONTEXT_CAP,
                );
                anchor_items(&mut items, replace_range);
                log_completion_summary(
                    t_start.elapsed(),
                    ast_dur,
                    rules_dur,
                    build_dur,
                    items.len(),
                    strategy,
                    "context",
                    ast_source.as_str(),
                );
                return Ok(Some(CompletionResponse::List(CompletionList {
                    is_incomplete,
                    items,
                })));
            }
            // Value position matched a concrete rule but had nothing to offer
            // (empty dynamic set, or a value type with no enumerable members):
            // return no completions rather than the flat variable dump.
            if resolved_value_pos {
                log_completion_summary(
                    t_start.elapsed(),
                    ast_dur,
                    rules_dur,
                    build_dur,
                    0,
                    "none",
                    "none",
                    ast_source.as_str(),
                );
                return Ok(None);
            }
        }

        // Fallback: flat global list (original behavior) when context-aware
        // matching produced nothing (no rules loaded, unrecognised path, or a
        // context `descend_rules` can't reach, e.g. inside an alias-driven block
        // like `check_variable = { … }`). On a large mod the workspace has tens
        // of thousands of variables/targets/keys, so cap the dump and flag the
        // result `is_incomplete` — the client re-requests as the user narrows.
        //
        // Cached by info revision: a hit skips the `info.files` walk that
        // dominates the build time on a 7k-file mod. This is the case that
        // fires on every keystroke in the half-typed state — the user is in a
        // position the AST doesn't know about, context-aware returns None,
        // and the fallback is the only thing returned. Without the cache,
        // every keystroke re-walks every file's top-level keys.
        // Only the dynamic value sets — variables and event targets — are
        // capped this low; see the comment below on why the old fallback's
        // full type/enum/key dump was cut down to just these.
        const FALLBACK_CAP: usize = 2000;
        let revision = self
            .state
            .info_revision
            .load(std::sync::atomic::Ordering::Relaxed);
        if let Some(cached) = self.state.fallback_cache.lock().as_ref()
            && cached.revision == revision
        {
            let items = cached.items.clone();
            if !items.is_empty() {
                // Filter the retrieved copy, never the cache (see below): the
                // cached list is shared across every request regardless of
                // typed token.
                let (mut items, _truncated) = filter_and_cap(items, &token, FALLBACK_CAP);
                anchor_items(&mut items, replace_range);
                log_completion_summary(
                    t_start.elapsed(),
                    ast_dur,
                    rules_dur,
                    build_dur,
                    items.len(),
                    "filtered",
                    "fallback",
                    ast_source.as_str(),
                );
                return Ok(Some(CompletionResponse::List(CompletionList {
                    is_incomplete: true,
                    items,
                })));
            }
        }
        // Narrowed fallback: only the dynamic value sets — variables and event
        // targets. The old fallback also dumped every type, enum, and top-level
        // key in the workspace; that flood is exactly the "irrelevant context"
        // that appears the moment a backspace drops the cursor into a position
        // the AST can't resolve (most often a math / `check_variable` block,
        // where variables and event targets are the only things you'd type
        // anyway). Types/enums/keys are still offered wherever the context-aware
        // path resolves a real rule. The `text_edit` anchor below filters this
        // set to the typed token client-side.
        let mut items = Vec::new();

        let t_fallback_build = Instant::now();
        let info = self.state.info_service.read();
        for var in info.variable_counts.keys() {
            if items.len() >= FALLBACK_CAP {
                break;
            }
            items.push(CompletionItem {
                label: var.clone(),
                kind: Some(CompletionItemKind::CONSTANT),
                detail: Some("Variable".to_string()),
                sort_text: sort_for_kind(Some(CompletionItemKind::CONSTANT), var),
                ..Default::default()
            });
        }
        for et in info.event_target_counts.keys() {
            if items.len() >= FALLBACK_CAP {
                break;
            }
            let label = format!("event_target:{}", et);
            items.push(CompletionItem {
                label: label.clone(),
                kind: Some(CompletionItemKind::VARIABLE),
                detail: Some("Event target".to_string()),
                sort_text: sort_for_kind(Some(CompletionItemKind::VARIABLE), &label),
                ..Default::default()
            });
        }
        build_dur += t_fallback_build.elapsed();

        if items.is_empty() {
            log_completion_summary(
                t_start.elapsed(),
                ast_dur,
                rules_dur,
                build_dur,
                0,
                "none",
                "none",
                ast_source.as_str(),
            );
            Ok(None)
        } else {
            // Always flag the fallback `is_incomplete` so the client re-requests
            // on each keystroke. Otherwise VS Code caches this generic dump and
            // keeps filtering it client-side even after the parse recovers and a
            // real rule context becomes available — the "stuck on abc suggestions"
            // symptom. With is_incomplete, the next keystroke re-queries and the
            // context-aware list replaces it. (#41)
            // Cache the un-anchored, un-filtered items: the replace-range is
            // per-request and the token narrows on every keystroke, so filter
            // and anchor only the clone that is returned, not the cached copy.
            *self.state.fallback_cache.lock() = Some(CompletionCacheEntry {
                revision,
                items: items.clone(),
            });
            let (mut items, _truncated) = filter_and_cap(items, &token, FALLBACK_CAP);
            anchor_items(&mut items, replace_range);
            log_completion_summary(
                t_start.elapsed(),
                ast_dur,
                rules_dur,
                build_dur,
                items.len(),
                "filtered",
                "fallback",
                ast_source.as_str(),
            );
            Ok(Some(CompletionResponse::List(CompletionList {
                is_incomplete: true,
                items,
            })))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use cwtools_rules::rules_types::{
        EnumDefinition, NewField, NewRule, Options, PathOptions, RootRule, RuleType,
        TypeDefinition, ValueType,
    };

    use super::*;

    // ── helpers ──────────────────────────────────────────────────────────────

    fn make_leaf_rule(key: &str, right: NewField) -> NewRule {
        (
            RuleType::LeafRule {
                left: NewField::SpecificField(key.to_string()),
                right,
            },
            Options::default(),
        )
    }

    fn make_node_rule(key: &str, children: Vec<NewRule>) -> NewRule {
        (
            RuleType::NodeRule {
                left: NewField::SpecificField(key.to_string()),
                rules: children,
            },
            Options::default(),
        )
    }

    fn bool_enum_ruleset() -> RuleSet {
        let mut rs = RuleSet::new();

        // enum: my_enum = { alpha beta gamma }
        rs.enums.push(EnumDefinition {
            key: "my_enum".to_string(),
            description: String::new(),
            values: vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()],
        });

        // type: my_type paths = { events }
        rs.types.push(TypeDefinition {
            name: "my_type".to_string(),
            name_field: Some("id".to_string()),
            path_options: PathOptions {
                paths: vec!["events".to_string()],
                path_strict: false,
                path_file: None,
                path_extension: None,
                paths_lower: Vec::new(),
                ..Default::default()
            },
            subtypes: Vec::new(),
            type_key_filter: None,
            skip_root_key: Vec::new(),
            starts_with: None,
            type_per_file: false,
            key_prefix: None,
            warning_only: false,
            unique: false,
            should_be_referenced: false,
            localisation: Vec::new(),
            graph_related_types: Vec::new(),
            modifiers: Vec::new(),
        });

        // TypeRule for my_type with child fields
        rs.root_rules.push(RootRule::TypeRule(
            "my_type".to_string(),
            make_node_rule(
                "my_type",
                vec![
                    make_leaf_rule(
                        "kind",
                        NewField::ValueField(ValueType::Enum("my_enum".to_string())),
                    ),
                    make_leaf_rule("active", NewField::ValueField(ValueType::Bool)),
                    make_leaf_rule("name", NewField::ScalarField),
                ],
            ),
        ));

        rs.reindex();
        rs
    }

    // ── completion context tests ─────────────────────────────────────────────

    #[test]
    fn test_completions_from_rules_enum() {
        let rs = bool_enum_ruleset();
        let info = cwtools_info::InfoService::new();

        // Grab the inner rules from the TypeRule
        let rules = if let Some(RootRule::TypeRule(_, (RuleType::NodeRule { rules, .. }, _))) =
            rs.root_rules.first()
        {
            rules.as_slice()
        } else {
            panic!("expected TypeRule");
        };

        let items = completions_from_rules(
            rules,
            &rs,
            &info,
            "stellaris",
            &HashSet::new(),
            &Default::default(),
            None,
            None,
            "",
        )
        .0;

        // "kind" should appear with a snippet containing enum values
        let kind_item = items.iter().find(|i| i.label == "kind");
        assert!(
            kind_item.is_some(),
            "expected 'kind' completion, got: {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
        let kind = kind_item.unwrap();
        assert_eq!(kind.insert_text_format, Some(InsertTextFormat::SNIPPET));
        let snippet = kind.insert_text.as_deref().unwrap_or("");
        assert!(snippet.contains("alpha"), "snippet: {}", snippet);

        // "active" should have yes/no snippet
        let active_item = items.iter().find(|i| i.label == "active");
        assert!(active_item.is_some(), "expected 'active' completion");
        let active = active_item.unwrap();
        let asnip = active.insert_text.as_deref().unwrap_or("");
        assert!(asnip.contains("yes"), "active snippet: {}", asnip);
    }

    #[test]
    fn test_completion_scalar_key_inserts_equals() {
        // A plain field (scalar/int/type value) must autocomplete to `name = `,
        // not a bare `name` (cwtools-vscode#16).
        let rs = bool_enum_ruleset();
        let info = cwtools_info::InfoService::new();
        let first_root = rs.root_rules.first().expect("expected root rule");
        let rules: &[(RuleType, cwtools_rules::rules_types::Options)] = match first_root {
            RootRule::TypeRule(_, (RuleType::NodeRule { rules, .. }, _)) => rules.as_slice(),
            _ => panic!("expected TypeRule"),
        };
        let items = completions_from_rules(
            rules,
            &rs,
            &info,
            "stellaris",
            &HashSet::new(),
            &Default::default(),
            None,
            None,
            "",
        )
        .0;
        let name = items
            .iter()
            .find(|i| i.label == "name")
            .expect("name completion");
        let snip = name.insert_text.as_deref().unwrap_or("");
        assert!(
            snip.starts_with("name = "),
            "scalar key should insert 'name = ', got: {:?}",
            name.insert_text
        );
    }

    #[test]
    fn test_completion_items_have_kind_aware_sort_text() {
        // Every item in a key-context list must carry a sortText so VS Code
        // orders them by usefulness as the user types. Concrete leaf fields
        // sort ahead of node blocks, which sort ahead of aliases, which sort
        // ahead of type instances, which sort ahead of enum values, which sort
        // ahead of scope names. The user-visible "iteration" feel depends on
        // this — without it, the popup sorts purely alphabetically and a
        // common prefix keeps many similarly-named items in the same row.
        let rs = bool_enum_ruleset();
        let info = cwtools_info::InfoService::new();
        let first_root = rs.root_rules.first().expect("expected root rule");
        let rules: &[(RuleType, cwtools_rules::rules_types::Options)] =
            if let RootRule::TypeRule(_, (RuleType::NodeRule { rules, .. }, _)) = first_root {
                rules.as_slice()
            } else {
                panic!("expected TypeRule");
            };
        let items = completions_from_rules(
            rules,
            &rs,
            &info,
            "stellaris",
            &HashSet::new(),
            &Default::default(),
            None,
            None,
            "",
        )
        .0;
        assert!(!items.is_empty(), "expected some completions");
        for item in &items {
            assert!(
                item.sort_text.is_some(),
                "completion {:?} has no sortText, will sort alphabetically",
                item.label
            );
        }
        // The first item by sortText should be a concrete leaf field (the
        // bool `active` from the fixture), not an enum value or alias.
        let mut sorted = items.clone();
        sorted.sort_by(|a, b| {
            a.sort_text
                .as_deref()
                .unwrap()
                .cmp(b.sort_text.as_deref().unwrap())
        });
        let first = sorted.first().unwrap();
        assert_eq!(
            first.kind,
            Some(CompletionItemKind::FIELD),
            "first item by sort should be a concrete field, got {:?}",
            first.label
        );
    }

    #[test]
    fn test_completion_sort_key_buckets() {
        // The bucket prefix is fixed-width (single digit 0-9) so the secondary
        // sort by label stays stable when the same item kind appears in two
        // different rule lists. The scope-aware bucket for `required_scopes`
        // is `0_` and must always lead the list.
        assert_eq!(
            sort_for_kind(Some(CompletionItemKind::FIELD), "x"),
            Some("1_x".to_string())
        );
        assert_eq!(
            sort_for_kind(Some(CompletionItemKind::STRUCT), "x"),
            Some("2_x".to_string())
        );
        assert_eq!(
            sort_for_kind(Some(CompletionItemKind::KEYWORD), "x"),
            Some("3_x".to_string())
        );
        assert_eq!(
            sort_for_kind(Some(CompletionItemKind::ENUM_MEMBER), "x"),
            Some("4_x".to_string())
        );
        assert_eq!(
            sort_for_kind(Some(CompletionItemKind::VALUE), "x"),
            Some("5_x".to_string())
        );
        assert_eq!(
            sort_for_kind(Some(CompletionItemKind::CONSTANT), "x"),
            Some("6_x".to_string())
        );
        assert_eq!(
            sort_for_kind(Some(CompletionItemKind::REFERENCE), "x"),
            Some("7_x".to_string())
        );
        assert_eq!(
            sort_for_kind(Some(CompletionItemKind::FUNCTION), "x"),
            Some("8_x".to_string())
        );
        assert_eq!(
            sort_for_kind(Some(CompletionItemKind::TEXT), "x"),
            Some("9_x".to_string())
        );
        assert_eq!(sort_for_kind(None, "x"), None);
    }

    // ── snippet generation tests ─────────────────────────────────────────────

    #[test]
    fn test_generate_node_snippet_no_required_fields() {
        let rs = bool_enum_ruleset();
        // Build a rule with no required children (min=0)
        let snippet = generate_node_snippet("my_block", &[], &rs);
        assert!(snippet.contains("my_block = {"), "got: {}", snippet);
        assert!(
            snippet.contains("$0"),
            "expected cursor $0, got: {}",
            snippet
        );
    }

    #[test]
    fn test_generate_node_snippet_with_required_bool() {
        let rs = bool_enum_ruleset();
        // Build rules with min=1
        let required_rules = vec![(
            RuleType::LeafRule {
                left: NewField::SpecificField("active".to_string()),
                right: NewField::ValueField(ValueType::Bool),
            },
            Options {
                min: 1,
                ..Options::default()
            },
        )];
        let snippet = generate_node_snippet("my_type", &required_rules, &rs);
        assert!(snippet.contains("my_type = {"), "got: {}", snippet);
        assert!(
            snippet.contains("active"),
            "expected 'active' in snippet: {}",
            snippet
        );
        assert!(
            snippet.contains("yes") || snippet.contains("${1"),
            "expected bool placeholder: {}",
            snippet
        );
    }

    #[test]
    fn test_generate_node_snippet_with_required_enum() {
        let rs = bool_enum_ruleset();
        let required_rules = vec![(
            RuleType::LeafRule {
                left: NewField::SpecificField("kind".to_string()),
                right: NewField::ValueField(ValueType::Enum("my_enum".to_string())),
            },
            Options {
                min: 1,
                ..Options::default()
            },
        )];
        let snippet = generate_node_snippet("my_type", &required_rules, &rs);
        // The enum values alpha, beta, gamma should appear as choices
        assert!(
            snippet.contains("alpha"),
            "expected enum choices in snippet: {}",
            snippet
        );
    }

    #[test]
    fn test_generate_node_snippet_ignores_optional_fields() {
        let rs = bool_enum_ruleset();
        // Only the min=1 field should appear; min=0 should not.
        let rules = vec![
            (
                RuleType::LeafRule {
                    left: NewField::SpecificField("required_field".to_string()),
                    right: NewField::ValueField(ValueType::Bool),
                },
                Options {
                    min: 1,
                    ..Options::default()
                },
            ),
            (
                RuleType::LeafRule {
                    left: NewField::SpecificField("optional_field".to_string()),
                    right: NewField::ValueField(ValueType::Bool),
                },
                Options {
                    min: 0,
                    ..Options::default()
                },
            ),
        ];
        let snippet = generate_node_snippet("my_type", &rules, &rs);
        assert!(
            snippet.contains("required_field"),
            "should have required: {}",
            snippet
        );
        assert!(
            !snippet.contains("optional_field"),
            "should not have optional: {}",
            snippet
        );
    }

    // ── alias (effect/trigger) snippet tests ─────────────────────────────────

    /// A ruleset with two effect aliases: `if` (a block effect with a required
    /// `limit` child) and `add_political_power` (a value effect).
    fn alias_effect_ruleset() -> RuleSet {
        let mut rs = RuleSet::new();
        // alias[effect:if] = { limit = { } alias_name[effect] = ... }
        rs.aliases.push((
            "effect:if".to_string(),
            (
                RuleType::NodeRule {
                    left: NewField::SpecificField("alias[effect:if]".to_string()),
                    rules: vec![
                        // `limit` has no ## cardinality -> required (1..1).
                        (
                            RuleType::NodeRule {
                                left: NewField::SpecificField("limit".to_string()),
                                rules: vec![],
                            },
                            Options {
                                min: 1,
                                ..Options::default()
                            },
                        ),
                        // The effect-recursion alias child is not a SpecificField,
                        // so it must not appear in the snippet.
                        (
                            RuleType::LeafRule {
                                left: NewField::AliasField("effect".to_string()),
                                right: NewField::AliasField("effect".to_string()),
                            },
                            Options::default(),
                        ),
                    ],
                },
                Options::default(),
            ),
        ));
        // alias[effect:add_political_power] = variable_field
        rs.aliases.push((
            "effect:add_political_power".to_string(),
            (
                RuleType::LeafRule {
                    left: NewField::SpecificField("alias[effect:add_political_power]".to_string()),
                    right: NewField::ScalarField,
                },
                Options::default(),
            ),
        ));
        rs.reindex();
        rs
    }

    /// The rule context inside an effect block: a single `alias_name[effect]`
    /// usage, which drives the alias-expansion arm for category `effect`.
    fn effect_alias_usage() -> Vec<NewRule> {
        vec![(
            RuleType::LeafRule {
                left: NewField::AliasField("effect".to_string()),
                right: NewField::AliasField("effect".to_string()),
            },
            Options::default(),
        )]
    }

    #[test]
    fn alias_block_effect_completes_to_block_with_required_child() {
        // `if` should tab-complete to a block that pre-fills its required
        // `limit = { }` with proper tab stops (cwtools-vscode autocomplete ask).
        let rs = alias_effect_ruleset();
        let info = cwtools_info::InfoService::new();
        let rules = effect_alias_usage();
        let items = completions_from_rules(
            &rules,
            &rs,
            &info,
            "hoi4",
            &HashSet::new(),
            &Default::default(),
            None,
            None,
            "",
        )
        .0;

        let if_item = items
            .iter()
            .find(|i| i.label == "if")
            .expect("'if' completion");
        assert_eq!(if_item.insert_text_format, Some(InsertTextFormat::SNIPPET));
        let snip = if_item.insert_text.as_deref().unwrap_or("");
        assert!(snip.starts_with("if = {"), "if snippet: {}", snip);
        assert!(
            snip.contains("limit ="),
            "if snippet missing limit: {}",
            snip
        );
    }

    #[test]
    fn alias_value_effect_completes_with_equals() {
        // `add_political_power` should tab-complete to `add_political_power = `
        // with the cursor after the `=`, ready for the value.
        let rs = alias_effect_ruleset();
        let info = cwtools_info::InfoService::new();
        let rules = effect_alias_usage();
        let items = completions_from_rules(
            &rules,
            &rs,
            &info,
            "hoi4",
            &HashSet::new(),
            &Default::default(),
            None,
            None,
            "",
        )
        .0;

        let appp = items
            .iter()
            .find(|i| i.label == "add_political_power")
            .expect("'add_political_power' completion");
        assert_eq!(appp.insert_text_format, Some(InsertTextFormat::SNIPPET));
        let snip = appp.insert_text.as_deref().unwrap_or("");
        assert!(
            snip.starts_with("add_political_power = "),
            "value-effect snippet: {}",
            snip
        );
        // A value effect is a single line, not a `{ … }` block.
        assert!(!snip.contains('\n'), "should not be a block: {}", snip);
        assert!(!snip.contains("= {"), "should not open a clause: {}", snip);
    }

    // ── #94: control-flow keys must not sink below scope-matched effects ─────

    /// Effect ruleset mirroring the real hoi4 config shape: a plain effect
    /// carrying `## scope = country`, `if` carrying `## scope = any`, and
    /// `else` with no scope annotation at all. Both `if` and `else` recurse
    /// into `alias_name[effect]`.
    fn scoped_effect_ruleset() -> RuleSet {
        let mut rs = RuleSet::new();
        let recursive_body = || {
            vec![(
                RuleType::LeafRule {
                    left: NewField::AliasField("effect".to_string()),
                    right: NewField::AliasField("effect".to_string()),
                },
                Options::default(),
            )]
        };
        rs.aliases.push((
            "effect:if".to_string(),
            (
                RuleType::NodeRule {
                    left: NewField::SpecificField("alias[effect:if]".to_string()),
                    rules: recursive_body(),
                },
                Options {
                    required_scopes: vec!["any".to_string()],
                    ..Options::default()
                },
            ),
        ));
        rs.aliases.push((
            "effect:else".to_string(),
            (
                RuleType::NodeRule {
                    left: NewField::SpecificField("alias[effect:else]".to_string()),
                    rules: recursive_body(),
                },
                Options::default(),
            ),
        ));
        rs.aliases.push((
            "effect:add_political_power".to_string(),
            (
                RuleType::LeafRule {
                    left: NewField::SpecificField("alias[effect:add_political_power]".to_string()),
                    right: NewField::ScalarField,
                },
                Options {
                    required_scopes: vec!["country".to_string()],
                    ..Options::default()
                },
            ),
        ));
        rs.reindex();
        rs
    }

    #[test]
    fn control_flow_effects_rank_with_scope_matched_effects() {
        // In a country-scope effect block every `## scope = country` effect
        // ranks in the top bucket, and `if`/`else` (valid in ANY scope) must
        // not sink below them (#94).
        let rs = scoped_effect_ruleset();
        let info = cwtools_info::InfoService::new();
        // Hoi4's registry is config-driven (empty here); Stellaris has the same
        // country scope hardcoded, which is all this test needs.
        let reg = cwtools_game::scope_registry::ScopeRegistry::from_hardcoded(
            cwtools_game::constants::Game::Stellaris,
        );
        let country = reg.id_of("country").expect("country scope");
        let items = completions_from_rules(
            &effect_alias_usage(),
            &rs,
            &info,
            "stellaris",
            &HashSet::new(),
            &Default::default(),
            Some(&reg),
            Some(country),
            "",
        )
        .0;
        let sort = |label: &str| {
            items
                .iter()
                .find(|i| i.label == label)
                .unwrap_or_else(|| panic!("no '{}' item", label))
                .sort_text
                .clone()
                .expect("sort_text")
        };
        let plain = sort("add_political_power");
        assert!(plain.starts_with("0_"), "scope match bucket: {}", plain);
        for label in ["if", "else"] {
            let s = sort(label);
            assert!(
                s.starts_with("0_"),
                "'{}' must rank with scope-matched effects, got sort_text {:?}",
                label,
                s
            );
        }
    }

    #[test]
    fn typed_token_ranks_exact_match_first_and_survives_cap() {
        // A capped list must keep and lead with the exact match for the typed
        // token, even when >cap better-bucketed items subsequence-match it —
        // otherwise typing `if` in a big effect block buries or drops `if` (#94).
        let mut items: Vec<CompletionItem> = (0..1500)
            .map(|i| {
                let label = format!("ai_f_{:04}", i);
                CompletionItem {
                    sort_text: Some(format!("0_{}", label)),
                    label,
                    ..Default::default()
                }
            })
            .collect();
        items.push(CompletionItem {
            label: "if".to_string(),
            sort_text: Some("3_if".to_string()),
            ..Default::default()
        });
        items.push(CompletionItem {
            label: "iff_prefixed".to_string(),
            sort_text: Some("3_iff_prefixed".to_string()),
            ..Default::default()
        });
        let (filtered, dropped) = filter_and_cap(items, "if", 1000);
        assert!(dropped);
        assert_eq!(filtered.len(), 1000);
        assert_eq!(filtered[0].label, "if", "exact match must lead");
        assert_eq!(
            filtered[1].label, "iff_prefixed",
            "prefix match ranks ahead of subsequence matches"
        );
    }

    // ── root-type snippets tests ─────────────────────────────────────────────

    #[test]
    fn test_root_type_snippets_path_match() {
        let rs = bool_enum_ruleset();
        // The type "my_type" is in path "events"
        let items = root_type_snippets(&rs, "events/test.txt");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            labels.contains(&"my_type") || !labels.is_empty(),
            "expected type items: {:?}",
            labels
        );
    }

    #[test]
    fn test_root_type_snippets_path_mismatch() {
        let rs = bool_enum_ruleset();
        // The type "my_type" is in path "events", not "common"
        let items = root_type_snippets(&rs, "common/foo.txt");
        assert!(
            items.is_empty(),
            "should not offer types for wrong path, got: {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
    }

    // ── #67: bool trigger alias must insert `key = ${yes/no}` ────────────────

    fn bool_trigger_ruleset() -> RuleSet {
        let mut rs = RuleSet::new();
        rs.aliases.push((
            "trigger:always".to_string(),
            (
                RuleType::LeafRule {
                    left: NewField::SpecificField("alias[trigger:always]".to_string()),
                    right: NewField::ValueField(ValueType::Bool),
                },
                Options::default(),
            ),
        ));
        rs.reindex();
        rs
    }

    #[test]
    fn alias_bool_trigger_completes_with_equals_and_yesno() {
        // #67: `alias[trigger:always] = bool` must complete to
        // `always = ${1|yes,no|}$0`, not a bare `${1|yes,no|}` with no `=`.
        let rs = bool_trigger_ruleset();
        let info = cwtools_info::InfoService::new();
        let rules = vec![(
            RuleType::LeafRule {
                left: NewField::AliasField("trigger".to_string()),
                right: NewField::AliasField("trigger".to_string()),
            },
            Options::default(),
        )];
        let items = completions_from_rules(
            &rules,
            &rs,
            &info,
            "hoi4",
            &HashSet::new(),
            &Default::default(),
            None,
            None,
            "",
        )
        .0;

        let always = items
            .iter()
            .find(|i| i.label == "always")
            .expect("'always' completion missing");
        let snip = always.insert_text.as_deref().unwrap_or("");
        assert!(
            snip.starts_with("always = "),
            "bool trigger must insert 'always = ', got: {:?}",
            always.insert_text
        );
        // #77: pin the corrected shape `always = ${1|yes,no|}$0`. A choice on the
        // final `$0` tab stop (`${0|…|}`) is inserted literally by VS Code, so the
        // choice must sit on tab stop 1 with a trailing `$0`.
        assert!(
            snip.contains("${1|") && snip.ends_with("$0") && !snip.contains("${0|"),
            "bool trigger must use a non-zero choice tab stop ending in $0, got: {:?}",
            always.insert_text
        );
        assert!(
            snip.contains("yes") && snip.contains("no"),
            "bool trigger must offer yes/no choices, got: {:?}",
            always.insert_text
        );
    }

    // ── #77: has_dlc enum snippet — tab stops, escaping, quoting ──────────────

    /// A ruleset with `alias[trigger:has_dlc] = enum[dlc]` whose enum mixes a
    /// multi-word value, a value with an embedded comma, a colon value, and a
    /// bare identifier — the shapes that exercise all three snippet defects.
    fn dlc_enum_ruleset() -> RuleSet {
        let mut rs = RuleSet::new();
        rs.enums.push(EnumDefinition {
            key: "dlc".to_string(),
            description: String::new(),
            values: vec![
                "Together for Victory".to_string(),
                "No Compromise, No Surrender".to_string(),
                "expansion:foo".to_string(),
                "base_game".to_string(),
            ],
        });
        rs.aliases.push((
            "trigger:has_dlc".to_string(),
            (
                RuleType::LeafRule {
                    left: NewField::SpecificField("alias[trigger:has_dlc]".to_string()),
                    right: NewField::ValueField(ValueType::Enum("dlc".to_string())),
                },
                Options::default(),
            ),
        ));
        rs.reindex();
        rs
    }

    #[test]
    fn alias_dlc_enum_snippet_escapes_and_quotes_choices() {
        // #77: an enum alias must complete to `has_dlc = ${1|...|}$0` — a choice
        // on tab stop 1 (not the unsupported `$0`), with each choice value quoted
        // when it has whitespace and its delimiters escaped.
        let rs = dlc_enum_ruleset();
        let info = cwtools_info::InfoService::new();
        let rules = vec![(
            RuleType::LeafRule {
                left: NewField::AliasField("trigger".to_string()),
                right: NewField::AliasField("trigger".to_string()),
            },
            Options::default(),
        )];
        let items = completions_from_rules(
            &rules,
            &rs,
            &info,
            "hoi4",
            &HashSet::new(),
            &Default::default(),
            None,
            None,
            "",
        )
        .0;

        let has_dlc = items
            .iter()
            .find(|i| i.label == "has_dlc")
            .expect("'has_dlc' completion missing");
        assert_eq!(has_dlc.insert_text_format, Some(InsertTextFormat::SNIPPET));
        let snip = has_dlc.insert_text.as_deref().unwrap_or("");
        assert!(
            snip.starts_with("has_dlc = ${1|"),
            "must be a choice on tab stop 1, got: {:?}",
            has_dlc.insert_text
        );
        assert!(
            snip.ends_with("|}$0"),
            "must end with a trailing $0, got: {:?}",
            has_dlc.insert_text
        );
        // Multi-word values are quoted.
        assert!(
            snip.contains("\"Together for Victory\""),
            "multi-word value must be quoted, got: {:?}",
            has_dlc.insert_text
        );
        // The comma inside a value is escaped so it can't split the choice, and
        // the quotes are kept around the whitespace-bearing value.
        assert!(
            snip.contains("\"No Compromise\\, No Surrender\""),
            "embedded comma must be escaped and value quoted, got: {:?}",
            has_dlc.insert_text
        );
        // A bare identifier stays unquoted.
        assert!(
            snip.contains("base_game") && !snip.contains("\"base_game\""),
            "bare identifier must stay unquoted, got: {:?}",
            has_dlc.insert_text
        );
    }

    #[test]
    fn value_completions_enum_quotes_spaced_values() {
        // #77: at a value position, an enum member with whitespace inserts quoted
        // (so it parses as one token); a bare identifier inserts as its label.
        let rs = dlc_enum_ruleset();
        let info = cwtools_info::InfoService::new();
        let value_rules = vec![(
            RuleType::LeafValueRule {
                right: NewField::ValueField(ValueType::Enum("dlc".to_string())),
            },
            Options::default(),
        )];
        let items = value_completions(
            &value_rules,
            &rs,
            &info,
            None,
            "hoi4",
            ValueCompletionSets {
                modifier_keys: &HashSet::new(),
                modifier_scopes: &Default::default(),
                loc_keys: &HashSet::new(),
            },
            None,
            "",
        )
        .0;

        let spaced = items
            .iter()
            .find(|i| i.label == "Together for Victory")
            .expect("spaced enum value missing");
        assert_eq!(
            spaced.insert_text.as_deref(),
            Some("\"Together for Victory\""),
            "spaced value must insert quoted, got: {:?}",
            spaced.insert_text
        );
        let bare = items
            .iter()
            .find(|i| i.label == "base_game")
            .expect("bare enum value missing");
        assert_eq!(
            bare.insert_text, None,
            "bare identifier must not carry a quoted insert_text, got: {:?}",
            bare.insert_text
        );
    }

    // ── #64: type-pattern alias expands to type instances ────────────────────

    fn scripted_effect_alias_ruleset() -> RuleSet {
        let mut rs = RuleSet::new();
        rs.types.push(TypeDefinition {
            name: "scripted_effect".to_string(),
            name_field: None,
            path_options: PathOptions {
                paths: vec!["common/scripted_effects".to_string()],
                path_strict: false,
                path_file: None,
                path_extension: None,
                paths_lower: Vec::new(),
                ..Default::default()
            },
            subtypes: Vec::new(),
            type_key_filter: None,
            skip_root_key: Vec::new(),
            starts_with: None,
            type_per_file: false,
            key_prefix: None,
            warning_only: false,
            unique: false,
            should_be_referenced: false,
            localisation: Vec::new(),
            graph_related_types: Vec::new(),
            modifiers: Vec::new(),
        });
        // alias[effect:<scripted_effect>] = yes
        rs.aliases.push((
            "effect:<scripted_effect>".to_string(),
            (
                RuleType::LeafRule {
                    left: NewField::SpecificField("alias[effect:<scripted_effect>]".to_string()),
                    right: NewField::SpecificField("yes".to_string()),
                },
                Options::default(),
            ),
        ));
        rs.reindex();
        rs
    }

    #[test]
    fn alias_type_pattern_expands_to_instances() {
        // #64: type-pattern aliases like `alias[effect:<scripted_effect>] = yes`
        // must emit one KEYWORD item per known instance, NOT the raw placeholder
        // label `<scripted_effect>`.
        let rs = scripted_effect_alias_ruleset();
        let mut info = cwtools_info::InfoService::new();
        let mut per_type: std::collections::HashMap<String, Vec<cwtools_info::TypeInstance>> =
            std::collections::HashMap::new();
        per_type.insert(
            "scripted_effect".to_string(),
            vec![cwtools_info::TypeInstance {
                name: "my_special_effect".to_string(),
                location: cwtools_info::SourceLocation {
                    line: 1,
                    col: 0,
                    end: (1, 0),
                },
                primary_loc_key: None,
            }],
        );
        info.type_index
            .merge("file:///scripted_effects/se.txt", per_type);

        let rules = vec![(
            RuleType::LeafRule {
                left: NewField::AliasField("effect".to_string()),
                right: NewField::AliasField("effect".to_string()),
            },
            Options::default(),
        )];
        let items = completions_from_rules(
            &rules,
            &rs,
            &info,
            "hoi4",
            &HashSet::new(),
            &Default::default(),
            None,
            None,
            "",
        )
        .0;

        assert!(
            items.iter().any(|i| i.label == "my_special_effect"),
            "type-pattern alias must expand to type instances, got: {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
        assert!(
            !items.iter().any(|i| i.label == "<scripted_effect>"),
            "raw pattern placeholder must not appear in labels, got: {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
        // The instance's snippet should be `my_special_effect = yes` because the
        // alias rule has `right = SpecificField("yes")`.
        let item = items
            .iter()
            .find(|i| i.label == "my_special_effect")
            .unwrap();
        let snip = item.insert_text.as_deref().unwrap_or("");
        assert!(
            snip.contains("= yes"),
            "scripted_effect snippet should contain '= yes', got: {:?}",
            item.insert_text
        );
    }

    // ── #65: alias_keys_field[modifier] must emit modifier keys ──────────────

    #[test]
    fn alias_keys_field_emits_modifier_keys() {
        // #65: a rule with `alias_keys_field[modifier]` on its left side (as in
        // `dynamic_modifier` blocks) must offer modifier keys as completions.
        let rs = bool_enum_ruleset(); // arbitrary ruleset with reindex() called
        let info = cwtools_info::InfoService::new();
        let modifier_keys: HashSet<String> = ["my_modifier".to_string(), "other_mod".to_string()]
            .into_iter()
            .collect();
        let rules = vec![(
            RuleType::LeafRule {
                left: NewField::AliasValueKeysField("modifier".to_string()),
                right: NewField::ValueField(ValueType::Float {
                    min: -1e8,
                    max: 1e8,
                }),
            },
            Options::default(),
        )];
        let items = completions_from_rules(
            &rules,
            &rs,
            &info,
            "hoi4",
            &modifier_keys,
            &Default::default(),
            None,
            None,
            "",
        )
        .0;

        assert!(
            items.iter().any(|i| i.label == "my_modifier"),
            "alias_keys_field[modifier] must offer modifier keys, got: {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
        assert!(
            items.iter().any(|i| i.label == "other_mod"),
            "alias_keys_field[modifier] must offer all modifier keys, got: {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
    }

    // ── #66: duplicate labels are removed from the completion list ───────────

    #[test]
    fn completions_from_rules_deduplicates() {
        // #66: when the same concrete field appears in multiple rule entries
        // (e.g. from subtype-flattening), the label must appear only once.
        let rs = bool_enum_ruleset();
        let info = cwtools_info::InfoService::new();
        // Two identical `active = bool` rules.
        let rules = vec![
            make_leaf_rule("active", NewField::ValueField(ValueType::Bool)),
            make_leaf_rule("active", NewField::ValueField(ValueType::Bool)),
        ];
        let items = completions_from_rules(
            &rules,
            &rs,
            &info,
            "hoi4",
            &HashSet::new(),
            &Default::default(),
            None,
            None,
            "",
        )
        .0;
        let count = items.iter().filter(|i| i.label == "active").count();
        assert_eq!(
            count, 1,
            "duplicate label 'active' must appear exactly once, got {} copies",
            count
        );
    }

    // ── A1a: subsequence prefilter + cap (perf/completion-responsiveness) ────

    fn item(label: &str) -> CompletionItem {
        CompletionItem {
            label: label.to_string(),
            sort_text: Some(label.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn subsequence_match_matches_in_order_non_contiguous() {
        assert!(subsequence_match("has_completed_focus", "hcf"));
        assert!(!subsequence_match("has_completed_focus", "xyz"));
    }

    #[test]
    fn subsequence_match_is_case_insensitive() {
        assert!(subsequence_match("HAS_COMPLETED_FOCUS", "hcf"));
        assert!(subsequence_match("has_completed_focus", "HCF"));
    }

    #[test]
    fn subsequence_match_empty_needle_matches_everything() {
        assert!(subsequence_match("anything", ""));
    }

    #[test]
    fn filter_and_cap_empty_token_is_passthrough_but_sorted() {
        let items = vec![item("zebra"), item("apple"), item("mango")];
        let (out, truncated) = filter_and_cap(items, "", 10);
        assert_eq!(
            out.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(),
            vec!["apple", "mango", "zebra"]
        );
        assert!(!truncated, "nothing dropped, nothing capped");
    }

    #[test]
    fn filter_and_cap_drops_non_matching_items_and_flags_truncated() {
        let items = vec![item("has_completed_focus"), item("xyz_unrelated")];
        let (out, truncated) = filter_and_cap(items, "hcf", 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "has_completed_focus");
        assert!(
            truncated,
            "dropping a non-matching item must flag truncated"
        );
    }

    #[test]
    fn filter_and_cap_enforces_cap_and_flags_truncated() {
        let items: Vec<CompletionItem> = (0..10).map(|i| item(&format!("item_{i}"))).collect();
        let (out, truncated) = filter_and_cap(items, "", 3);
        assert_eq!(out.len(), 3);
        assert!(truncated, "truncating to the cap must flag truncated");
    }

    #[test]
    fn filter_and_cap_no_drop_no_truncate() {
        let items = vec![item("alpha"), item("beta")];
        let (out, truncated) = filter_and_cap(items, "a", 10);
        assert_eq!(out.len(), 2, "both labels subsequence-match 'a'");
        assert!(!truncated);
    }

    #[test]
    fn prepare_context_items_marks_small_current_clean_list_complete() {
        let items = vec![item("allowed"), item("cost")];
        let (out, incomplete, strategy) = prepare_context_items(items, 0, "", true, true, 10, 10);
        assert_eq!(out.len(), 2);
        assert!(!incomplete);
        assert_eq!(strategy, "complete");
    }

    #[test]
    fn prepare_context_items_marks_small_stale_list_incomplete() {
        let items = vec![item("allowed"), item("cost")];
        let (out, incomplete, strategy) = prepare_context_items(items, 0, "", true, false, 10, 10);
        assert_eq!(out.len(), 2);
        assert!(incomplete);
        assert_eq!(strategy, "filtered");
    }

    #[test]
    fn prepare_context_items_marks_dirty_list_incomplete() {
        let items = vec![item("allowed"), item("cost")];
        let (out, incomplete, strategy) = prepare_context_items(items, 0, "", false, true, 10, 10);
        assert_eq!(out.len(), 2);
        assert!(incomplete);
        assert_eq!(strategy, "filtered");
    }

    #[test]
    fn prepare_context_items_filters_and_caps_large_list() {
        let items: Vec<CompletionItem> = (0..10).map(|i| item(&format!("item_{i}"))).collect();
        let (out, incomplete, strategy) = prepare_context_items(items, 0, "", true, true, 3, 3);
        assert_eq!(out.len(), 3);
        assert!(incomplete);
        assert_eq!(strategy, "filtered");
    }

    // ── snippet grammar validity (cwtools-vscode#89 snippet hardening) ───────

    /// A focused check mirroring VS Code's `snippetParser.ts`: rejects constructs
    /// the editor inserts literally or mishandles. Stricter than the (lenient)
    /// real parser about a literal `{`/`}` inside a placeholder default, which is
    /// where the node-required prefill used to leak an unescaped `}`.
    fn snippet_defect(s: &str) -> std::result::Result<(), String> {
        let c: Vec<char> = s.chars().collect();
        let mut i = 0;
        while i < c.len() {
            match c[i] {
                '\\' => i += 2,
                '$' => i = scan_dollar(&c, i)?,
                _ => i += 1,
            }
        }
        Ok(())
    }

    /// Consume a `$` construct at `i` (`c[i] == '$'`), returning the next index.
    /// A bare `$` (or a `$name` variable) is literal text to the parser.
    fn scan_dollar(c: &[char], i: usize) -> std::result::Result<usize, String> {
        match c.get(i + 1) {
            Some(d) if d.is_ascii_digit() => {
                let mut j = i + 1;
                while j < c.len() && c[j].is_ascii_digit() {
                    j += 1;
                }
                Ok(j)
            }
            Some('{') => scan_brace(c, i),
            _ => Ok(i + 1),
        }
    }

    /// Consume a `${ … }` construct starting at `i` (`c[i..i+2] == "${"`).
    fn scan_brace(c: &[char], i: usize) -> std::result::Result<usize, String> {
        let digits_start = i + 2;
        let mut j = digits_start;
        while j < c.len() && c[j].is_ascii_digit() {
            j += 1;
        }
        if j == digits_start {
            return Err("`${` without a tab-stop number".into());
        }
        let is_zero = c[digits_start..j].iter().all(|d| *d == '0');
        match c.get(j) {
            Some('}') => Ok(j + 1),
            Some('|') => scan_choice(c, j, is_zero),
            Some(':') => scan_default(c, j + 1),
            _ => Err("malformed `${…}`".into()),
        }
    }

    /// Consume a choice body from its opening `|` (`c[j] == '|'`) to the `|}` close.
    fn scan_choice(c: &[char], j: usize, is_zero: bool) -> std::result::Result<usize, String> {
        if is_zero {
            return Err("choice on tab stop $0 is inserted literally".into());
        }
        let mut k = j + 1;
        while k < c.len() {
            match c[k] {
                '\\' => k += 2,
                '|' if c.get(k + 1) == Some(&'}') => return Ok(k + 2),
                '|' => return Err("unescaped `|` in a choice value".into()),
                _ => k += 1,
            }
        }
        Err("unterminated choice".into())
    }

    /// Consume a placeholder default from the first default char to the matching
    /// unescaped `}`. A bare `{` here is the `${1:{ }}` defect (the `}` closes early).
    fn scan_default(c: &[char], mut k: usize) -> std::result::Result<usize, String> {
        while k < c.len() {
            match c[k] {
                '\\' => k += 2,
                '}' => return Ok(k + 1),
                '$' => k = scan_dollar(c, k)?,
                '{' => return Err("bare `{` in a placeholder default".into()),
                _ => k += 1,
            }
        }
        Err("unterminated placeholder default".into())
    }

    #[test]
    fn snippet_checker_accepts_valid_and_rejects_defects() {
        for good in [
            "k = { $1 }",
            "k = {\n\t$0\n}",
            "add = ${1}$0",
            "always = ${1|yes,no|}$0",
            "has_dlc = ${1|\"a b\",c|}",
            "lit = a\\$b\\}c$0",
            "plain = yes$0",
        ] {
            assert!(
                snippet_defect(good).is_ok(),
                "should accept {:?}: {:?}",
                good,
                snippet_defect(good)
            );
        }
        for bad in [
            "k = ${1:{ }}",  // bare `{` default — the old node-required bug
            "c = ${0|a,b|}", // choice on $0
            "u = ${1:foo",   // unterminated default
            "u = ${1|a,b|",  // unterminated choice
            "u = ${}",       // no tab-stop number
        ] {
            assert!(snippet_defect(bad).is_err(), "should reject {:?}", bad);
        }
    }

    /// The SNIPPET-format `insert_text` of every item, for the sweep below.
    fn snippet_texts(items: &[CompletionItem]) -> Vec<String> {
        items
            .iter()
            .filter(|it| it.insert_text_format == Some(InsertTextFormat::SNIPPET))
            .filter_map(|it| it.insert_text.clone())
            .collect()
    }

    #[test]
    fn all_generated_snippets_are_grammar_valid() {
        let info = cwtools_info::InfoService::new();
        let empty = HashSet::new();
        let mut snips: Vec<String> = Vec::new();

        // Key-context snippets across the leaf/node/enum builder arms.
        let rs = bool_enum_ruleset();
        let rules = match rs.root_rules.first() {
            Some(RootRule::TypeRule(_, (RuleType::NodeRule { rules, .. }, _))) => rules.as_slice(),
            _ => panic!("expected TypeRule"),
        };
        snips.extend(snippet_texts(
            &completions_from_rules(
                rules,
                &rs,
                &info,
                "hoi4",
                &empty,
                &Default::default(),
                None,
                None,
                "",
            )
            .0,
        ));
        snips.extend(snippet_texts(&root_type_snippets(&rs, "events/x.txt")));

        // Alias block (required child prefill) + value alias.
        let rs = alias_effect_ruleset();
        snips.extend(snippet_texts(
            &completions_from_rules(
                &effect_alias_usage(),
                &rs,
                &info,
                "hoi4",
                &empty,
                &Default::default(),
                None,
                None,
                "",
            )
            .0,
        ));

        // Enum choice with spaced / comma / colon values.
        let rs = dlc_enum_ruleset();
        let trigger_usage = vec![(
            RuleType::LeafRule {
                left: NewField::AliasField("trigger".to_string()),
                right: NewField::AliasField("trigger".to_string()),
            },
            Options::default(),
        )];
        snips.extend(snippet_texts(
            &completions_from_rules(
                &trigger_usage,
                &rs,
                &info,
                "hoi4",
                &empty,
                &Default::default(),
                None,
                None,
                "",
            )
            .0,
        ));

        // A required NODE child — the case that used to emit `${1:{ }}`.
        let node_required = vec![(
            RuleType::NodeRule {
                left: NewField::SpecificField("child".to_string()),
                rules: vec![],
            },
            Options {
                min: 1,
                ..Options::default()
            },
        )];
        let node_snip = generate_node_snippet("outer", &node_required, &rs);
        assert!(
            node_snip.contains("child = { $1 }"),
            "required node child must use an interior tab stop, got: {}",
            node_snip
        );
        assert!(
            !node_snip.contains("${1:{"),
            "required node child must not use a brace default, got: {}",
            node_snip
        );
        snips.push(node_snip);

        assert!(!snips.is_empty(), "sweep produced no snippets");
        for s in &snips {
            assert!(
                snippet_defect(s).is_ok(),
                "generated snippet {:?} is invalid: {}",
                s,
                snippet_defect(s).unwrap_err()
            );
        }
    }

    #[test]
    fn specific_field_literal_is_snippet_escaped() {
        // A concrete alias value carrying `$`/`}` must be escaped so VS Code
        // doesn't read it as a variable/tab stop or truncate the snippet.
        let mut rs = RuleSet::new();
        rs.aliases.push((
            "effect:danger".to_string(),
            (
                RuleType::LeafRule {
                    left: NewField::SpecificField("alias[effect:danger]".to_string()),
                    right: NewField::SpecificField("a$b}c".to_string()),
                },
                Options::default(),
            ),
        ));
        rs.reindex();
        let info = cwtools_info::InfoService::new();
        let usage = vec![(
            RuleType::LeafRule {
                left: NewField::AliasField("effect".to_string()),
                right: NewField::AliasField("effect".to_string()),
            },
            Options::default(),
        )];
        let items = completions_from_rules(
            &usage,
            &rs,
            &info,
            "hoi4",
            &HashSet::new(),
            &Default::default(),
            None,
            None,
            "",
        )
        .0;
        let danger = items
            .iter()
            .find(|i| i.label == "danger")
            .expect("danger completion");
        let snip = danger.insert_text.as_deref().unwrap_or("");
        assert!(
            snip.contains("a\\$b\\}c"),
            "literal must be snippet-escaped, got: {:?}",
            snip
        );
        assert!(
            snippet_defect(snip).is_ok(),
            "escaped snippet must be valid, got: {:?}",
            snip
        );
    }

    // keep Arc in scope to avoid unused-import warning when no test uses it
    const _: fn() = || {
        let _ = Arc::new(());
    };
}

// ── MD-scale completion micro-benchmark (ignored, manual) ────────────────────
//
// Synthetic ruleset + type index sized like Millennium Dawn (thousands of
// pattern-expanded scripted effects, thousands of modifiers, high-cardinality
// type refs). Run with:
//
//   cargo test --release -p cwtools_lsp --bin cwtools-server -- \
//     --ignored --nocapture perf_completion_synthetic
#[cfg(test)]
mod perf_bench {
    use std::collections::{HashMap, HashSet};

    use cwtools_rules::rules_types::{NewField, NewRule, Options, RuleSet, RuleType};

    use super::*;

    const EXACT_EFFECTS: usize = 600;
    const SCRIPTED_EFFECTS: usize = 8_000;
    const PLAIN_MODIFIERS: usize = 5_000;
    const TEMPLATED_BUILDINGS: usize = 3_000;
    const STATES: usize = 2_000;

    fn alias_usage(cat: &str) -> Vec<NewRule> {
        vec![(
            RuleType::LeafRule {
                left: NewField::AliasField(cat.to_string()),
                right: NewField::AliasField(cat.to_string()),
            },
            Options::default(),
        )]
    }

    fn synthetic_ruleset() -> RuleSet {
        let mut rs = RuleSet::new();
        for i in 0..EXACT_EFFECTS {
            let scopes = if i % 2 == 0 {
                vec!["country".to_string()]
            } else {
                Vec::new()
            };
            rs.aliases.push((
                format!("effect:eff_{:04}", i),
                (
                    RuleType::LeafRule {
                        left: NewField::SpecificField(format!("alias[effect:eff_{:04}]", i)),
                        right: NewField::ScalarField,
                    },
                    Options {
                        required_scopes: scopes,
                        ..Options::default()
                    },
                ),
            ));
        }
        for name in ["if", "else_if", "else"] {
            rs.aliases.push((
                format!("effect:{}", name),
                (
                    RuleType::NodeRule {
                        left: NewField::SpecificField(format!("alias[effect:{}]", name)),
                        rules: alias_usage("effect"),
                    },
                    Options::default(),
                ),
            ));
        }
        // Pattern alias expanded against the type index (scripted effects).
        rs.aliases.push((
            "effect:<scripted_effect>".to_string(),
            (
                RuleType::LeafRule {
                    left: NewField::SpecificField("alias[effect:<scripted_effect>]".to_string()),
                    right: NewField::ValueField(cwtools_rules::rules_types::ValueType::Bool),
                },
                Options::default(),
            ),
        ));
        for i in 0..PLAIN_MODIFIERS {
            rs.modifiers
                .push((format!("mod_{:04}", i), "country".to_string()));
        }
        rs.modifiers.push((
            "production_speed_<building>_factor".to_string(),
            "state".to_string(),
        ));
        rs.modifier_categories
            .insert("country".to_string(), vec!["country".to_string()]);
        rs.modifier_categories
            .insert("state".to_string(), vec!["state".to_string()]);
        rs.reindex();
        rs
    }

    fn synthetic_info() -> cwtools_info::InfoService {
        let mut info = cwtools_info::InfoService::new();
        let inst = |name: String| cwtools_info::TypeInstance {
            name,
            location: cwtools_info::SourceLocation {
                line: 1,
                col: 0,
                end: (1, 0),
            },
            primary_loc_key: None,
        };
        let mut per_type: HashMap<String, Vec<cwtools_info::TypeInstance>> = HashMap::new();
        per_type.insert(
            "scripted_effect".to_string(),
            (0..SCRIPTED_EFFECTS)
                .map(|i| inst(format!("se_do_things_{:05}", i)))
                .collect(),
        );
        per_type.insert(
            "building".to_string(),
            (0..TEMPLATED_BUILDINGS)
                .map(|i| inst(format!("building_{:04}", i)))
                .collect(),
        );
        per_type.insert(
            "state".to_string(),
            (0..STATES).map(|i| inst(format!("{}", i + 1))).collect(),
        );
        info.type_index.merge("file:///bench/defs.txt", per_type);
        info
    }

    fn bench<F: FnMut() -> usize>(label: &str, mut f: F) {
        const WARMUP: usize = 3;
        const ITERS: usize = 30;
        for _ in 0..WARMUP {
            f();
        }
        let mut times = Vec::with_capacity(ITERS);
        let mut items = 0;
        for _ in 0..ITERS {
            let t = std::time::Instant::now();
            items = f();
            times.push(t.elapsed());
        }
        times.sort();
        let mean = times.iter().sum::<std::time::Duration>() / ITERS as u32;
        eprintln!(
            "{:>28}: mean {:>10.1?}  min {:>10.1?}  max {:>10.1?}  ({} items, n={})",
            label,
            mean,
            times[0],
            times[ITERS - 1],
            items,
            ITERS
        );
    }

    #[test]
    #[ignore]
    fn perf_completion_synthetic() {
        let rs = synthetic_ruleset();
        let info = synthetic_info();
        let reg = cwtools_game::scope_registry::ScopeRegistry::from_hardcoded(
            cwtools_game::constants::Game::Stellaris,
        );
        let country = reg.id_of("country").expect("country scope");
        let modifier_keys: HashSet<String> =
            cwtools_validation::build_modifier_keys(&rs, &info.type_index);
        eprintln!(
            "fixture: {} aliases, {} modifier keys, {} scripted effects, {} states",
            rs.aliases.len(),
            modifier_keys.len(),
            SCRIPTED_EFFECTS,
            STATES
        );

        let modifier_scopes = expanded_modifier_scopes(&rs, &info.type_index);
        let effect_rules = alias_usage("effect");
        let modifier_rules = alias_usage("modifier");

        for token in ["", "if", "add_p"] {
            let label = if token.is_empty() {
                "effect key (no token)".to_string()
            } else {
                format!("effect key (token {:?})", token)
            };
            bench(&label, || {
                let (items, dropped) = completions_from_rules(
                    &effect_rules,
                    &rs,
                    &info,
                    "stellaris",
                    &modifier_keys,
                    &modifier_scopes,
                    Some(&reg),
                    Some(country),
                    token,
                );
                let (items, _, _) = prepare_context_items(
                    items,
                    dropped,
                    token,
                    true,
                    true,
                    CONTEXT_COMPLETE_THRESHOLD,
                    CONTEXT_CAP,
                );
                items.len()
            });
        }

        // Duplicated alias rule (subtype flattening can repeat one): the
        // seen-categories guard should make the repeat free.
        let effect_rules_dup: Vec<NewRule> = effect_rules
            .iter()
            .cloned()
            .chain(effect_rules.iter().cloned())
            .collect();
        bench("effect key (dup arm)", || {
            let (items, dropped) = completions_from_rules(
                &effect_rules_dup,
                &rs,
                &info,
                "stellaris",
                &modifier_keys,
                &modifier_scopes,
                Some(&reg),
                Some(country),
                "",
            );
            let (items, _, _) = prepare_context_items(
                items,
                dropped,
                "",
                true,
                true,
                CONTEXT_COMPLETE_THRESHOLD,
                CONTEXT_CAP,
            );
            items.len()
        });

        bench("modifier key (scoped)", || {
            let (items, dropped) = completions_from_rules(
                &modifier_rules,
                &rs,
                &info,
                "stellaris",
                &modifier_keys,
                &modifier_scopes,
                Some(&reg),
                Some(country),
                "",
            );
            let (items, _, _) = prepare_context_items(
                items,
                dropped,
                "",
                true,
                true,
                CONTEXT_COMPLETE_THRESHOLD,
                CONTEXT_CAP,
            );
            items.len()
        });

        let state_value_rules: Vec<NewRule> = vec![(
            RuleType::LeafRule {
                left: NewField::SpecificField("add_state_core".to_string()),
                right: NewField::TypeField(cwtools_rules::rules_types::TypeType::Simple(
                    "state".to_string(),
                )),
            },
            Options::default(),
        )];
        bench("state value (token 28)", || {
            let (items, dropped) = value_completions(
                &state_value_rules,
                &rs,
                &info,
                Some(&reg),
                "stellaris",
                ValueCompletionSets {
                    modifier_keys: &modifier_keys,
                    modifier_scopes: &modifier_scopes,
                    loc_keys: &HashSet::new(),
                },
                Some(country),
                "28",
            );
            let (items, _, _) = prepare_context_items(
                items,
                dropped,
                "28",
                true,
                true,
                CONTEXT_COMPLETE_THRESHOLD,
                CONTEXT_CAP,
            );
            items.len()
        });
    }
}
