use std::collections::HashSet;
use std::sync::Arc;

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

use cwtools_info::InfoService;
use cwtools_parser::ast::ParsedFile;
use cwtools_rules::rules_types::{
    NewField, ParsedAliasPattern, PatternKind, RootRule, RuleSet, RuleType, TypeType, ValueType,
};
use cwtools_validation::position::{rules_at_pos, value_rules_for_key};

use crate::Backend;
use crate::CompletionCacheEntry;
use crate::paths::{current_token_range, line_value_key, logical_path_from_uri};

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

/// Snapshotted ruleset-derived state for one completion request. The `Arc`s
/// carry the lifetime across the request so the helpers can take borrows
/// without holding the rules read guard.
type RulesSnapshot = (
    Option<Arc<RuleSet>>,
    Arc<HashSet<String>>,
    Option<Arc<cwtools_game::scope_registry::ScopeRegistry>>,
);

impl Backend {
    pub(crate) async fn completion_impl(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri.to_string();
        let pos = params.text_document_position.position;

        // `.cwt` rule files aren't game content — no rule-driven completion. (#43)
        if crate::paths::is_cwt_file(&uri) {
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
            return Ok(None);
        }

        let lsp_line = pos.line + 1;
        let lsp_col = pos.character as u16;

        // Try context-aware completions first: resolve the rules at the cursor
        // with the validation engine's own descent (aliases, typed keys,
        // subtypes, skip_root_key — see cwtools_validation::position).
        let (ws_uri, language, scope_checks, var_checks) = {
            let cfg = self.state.config.read();
            (
                cfg.workspace_uri.clone(),
                cfg.language.clone(),
                cfg.scope_checks,
                cfg.var_checks,
            )
        };
        let logical_path = logical_path_from_uri(&uri, &ws_uri);

        // Snapshot the doc text + AST into owned data, then drop the
        // `documents` guard before any heavy work. `documents.lock()` is the
        // only exclusive lock in the LSP state, so holding it across the whole
        // completion blocks `did_open`/`did_change`/`did_close` and the
        // debounced validate's AST update for the duration — the worst case
        // being the user typing into a file whose previous validate is still
        // running. The same pattern for the rules guard: clone the Arcs and
        // drop the guard. The helpers below take borrows, so the Arcs carry
        // the lifetime across the work without holding the lock.
        let (doc_text, doc_ast): (String, Option<Arc<ParsedFile>>) = {
            let docs = self.state.documents.lock();
            docs.get(&uri)
                .map(|d| (d.text.clone(), d.ast.clone()))
                .unwrap_or_default()
        };
        // Replace-range for every item the script paths return: the identifier
        // token under the cursor. Loc completion keeps its own behavior (the
        // cached items are shared and the token shape differs), so it is not
        // anchored here.
        let replace_range = current_token_range(&doc_text, pos.line, pos.character);
        let (ruleset_arc, modifier_keys_arc, scope_registry_arc): RulesSnapshot = {
            let rules_guard = self.state.rules.read();
            (
                rules_guard.ruleset.clone(),
                rules_guard.modifier_keys.clone(),
                rules_guard.scope_registry.clone(),
            )
        };
        // Drop the read guard before the heavy work. Bump the generation
        // check here too — the rule-snapshot block is cheap, but we want the
        // staleness gate to cover the full body of the function from here.
        if is_stale() {
            return Ok(None);
        }

        // Localisation file — offer loc-key / data-function completions. The
        // list is workspace-wide (every open `.yml` shares the same set), so
        // cache it keyed by (info_revision, language) — a hit skips the
        // `info.files` walk and the per-request scope-name build, both of
        // which are expensive on a large mod and fire on every keystroke in
        // the half-typed state.
        if crate::paths::is_loc_file(&uri) {
            let revision = self
                .state
                .info_revision
                .load(std::sync::atomic::Ordering::Relaxed);
            if let Some(cached) = self.state.loc_cache.lock().as_ref()
                && cached.revision == revision
                && cached.language == language
            {
                let items = (*cached.items).clone();
                if !items.is_empty() {
                    return Ok(Some(CompletionResponse::List(CompletionList {
                        is_incomplete: true,
                        items,
                    })));
                }
            } else {
                let info_guard = self.state.info_service.read();
                let items = loc_completions(&info_guard, &language, scope_registry_arc.as_deref());
                if !items.is_empty() {
                    *self.state.loc_cache.lock() = Some(CompletionCacheEntry {
                        revision,
                        language: language.clone(),
                        items: std::sync::Arc::new(items.clone()),
                    });
                    return Ok(Some(CompletionResponse::List(CompletionList {
                        is_incomplete: true,
                        items,
                    })));
                }
            }
        }

        // `Some(items)` = the rule context resolved (items may still be empty:
        // an unknown block where suggestions from any other level would be
        // wrong). `None` = no doc/ruleset/AST — fall through to the flat list.
        //
        // Re-parse on demand when the AST is missing: the last parse failed
        // (the user typed something unparseable), so `doc.ast` is None. Try
        // a fresh parse for THIS request only — if the user has since added a
        // `}` or fixed the syntax, the new AST is closer to the live text
        // and `rules_at_pos` finds a useful context. The fresh AST is not
        // written back; the debounced validate still owns the long-term one.
        // Wrapped in `block_in_place` so the parse doesn't stall the async
        // executor for slow large files.
        let effective_ast: Option<Arc<ParsedFile>> = match doc_ast {
            Some(ast) => Some(ast),
            None => {
                let text = doc_text.clone();
                let table = self.state.string_table.clone();
                tokio::task::block_in_place(|| {
                    cwtools_parser::parser::parse_string(&text, &table)
                        .ok()
                        .map(Arc::new)
                })
            }
        };
        let context_items: Option<Vec<CompletionItem>> = match (effective_ast, ruleset_arc.as_ref())
        {
            (Some(ast), Some(rs)) => {
                if is_stale() {
                    return Ok(None);
                }
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
                match rules_at_pos(&ast, &logical_path, &prepared, lsp_line, lsp_col) {
                    // Outside any known entity — offer root-type snippets.
                    None => Some(root_type_snippets(rs, &logical_path)),
                    Some(rctx) => {
                        let items = if rctx.leaf.as_ref().is_some_and(|l| l.in_value) {
                            let is_bare_key = rctx
                                .leaf
                                .as_ref()
                                .is_some_and(|l| l.key.is_empty() && rctx.value_rules.is_empty());
                            if is_bare_key {
                                // Bare token (no `=`): treat as a key being typed, not a value.
                                completions_from_rules(
                                    &rctx.child_rules,
                                    rs,
                                    &info_guard,
                                    &language,
                                    &modifier_keys_arc,
                                    scope_registry_arc.as_deref(),
                                )
                            } else {
                                value_completions(
                                    &rctx.value_rules,
                                    rs,
                                    &info_guard,
                                    scope_registry_arc.as_deref(),
                                    &language,
                                )
                            }
                        } else if let Some(key) = line_value_key(&doc_text, pos.line, pos.character)
                        {
                            // Mid-edit `key = |`: the last good parse has no such
                            // leaf yet; resolve the value rules from the live line.
                            let vr = value_rules_for_key(
                                rs,
                                Some(&info_guard.type_index),
                                &rctx.child_rules,
                                &key,
                            );
                            value_completions(
                                &vr,
                                rs,
                                &info_guard,
                                scope_registry_arc.as_deref(),
                                &language,
                            )
                        } else {
                            completions_from_rules(
                                &rctx.child_rules,
                                rs,
                                &info_guard,
                                &language,
                                &modifier_keys_arc,
                                scope_registry_arc.as_deref(),
                            )
                        };
                        Some(items)
                    }
                }
            }
            _ => None,
        };

        if let Some(mut items) = context_items
            && !items.is_empty()
        {
            anchor_items(&mut items, replace_range);
            // `is_incomplete` so the client re-queries on every keystroke.
            // Without it, VS Code caches the list and filters client-side —
            // which feels right until the half-typed state recovers (a new
            // block, a recovered parse) and the cached list stays stuck on
            // the wrong context. The re-query is cheap: the server returns
            // the same items for a stable cursor.
            return Ok(Some(CompletionResponse::List(CompletionList {
                is_incomplete: true,
                items,
            })));
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
        let revision = self
            .state
            .info_revision
            .load(std::sync::atomic::Ordering::Relaxed);
        if let Some(cached) = self.state.fallback_cache.lock().as_ref()
            && cached.revision == revision
        {
            let mut items = (*cached.items).clone();
            if !items.is_empty() {
                anchor_items(&mut items, replace_range);
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
        const FALLBACK_CAP: usize = 2000;
        let mut items = Vec::new();

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

        if items.is_empty() {
            Ok(None)
        } else {
            // Always flag the fallback `is_incomplete` so the client re-requests
            // on each keystroke. Otherwise VS Code caches this generic dump and
            // keeps filtering it client-side even after the parse recovers and a
            // real rule context becomes available — the "stuck on abc suggestions"
            // symptom. With is_incomplete, the next keystroke re-queries and the
            // context-aware list replaces it. (#41)
            // Cache the un-anchored items: the replace-range is per-request
            // (it moves with the cursor), so anchor the clone that is returned,
            // not the cached copy.
            *self.state.fallback_cache.lock() = Some(CompletionCacheEntry {
                revision,
                language: String::new(),
                items: std::sync::Arc::new(items.clone()),
            });
            anchor_items(&mut items, replace_range);
            Ok(Some(CompletionResponse::List(CompletionList {
                is_incomplete: true,
                items,
            })))
        }
    }
}

/// Build context-aware completion items from the child rules at the cursor's
/// position (the rules come from `position::rules_at_pos`, which resolves
/// aliases, typed keys, and subtypes the same way validation does).
pub(crate) fn completions_from_rules(
    rules: &[(RuleType, cwtools_rules::rules_types::Options)],
    ruleset: &RuleSet,
    info: &InfoService,
    language: &str,
    modifier_keys: &HashSet<String>,
    registry: Option<&cwtools_game::scope_registry::ScopeRegistry>,
) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = Vec::new();
    // Per-request memo so a repeated enum is only collected/sorted once (#46).
    let mut enum_cache: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    // Built (sort + clone) at most once per call even if several scope rules
    // appear in this block (#44).
    let mut scope_names: Option<Vec<String>> = None;

    for (rule_type, opts) in rules {
        match rule_type {
            // A concrete key in the block
            RuleType::LeafRule {
                left: NewField::SpecificField(k),
                right,
            } => push_specific_leaf_key(&mut items, k, right, opts, ruleset),
            // A node block key — generate snippet with required child fields pre-populated
            RuleType::NodeRule {
                left: NewField::SpecificField(k),
                rules: inner,
            } => push_specific_node_key(&mut items, k, inner, opts, ruleset),
            // An enum-keyed field: every member of the enum is a valid key here
            // (e.g. MIO `equipment_bonus = { enum[equipment_stat] = variable_field }`).
            RuleType::LeafRule {
                left: NewField::ValueField(ValueType::Enum(e)),
                right,
            } => push_enum_keyed_leaf(&mut items, &mut enum_cache, ruleset, info, e, right),
            RuleType::NodeRule {
                left: NewField::ValueField(ValueType::Enum(e)),
                ..
            } => push_enum_keyed_node(&mut items, &mut enum_cache, ruleset, info, e),
            // A typed key: every instance of the type is a valid key here
            // (e.g. `equipment_type = { <equipment_group> }` blocks, or
            // `<equipment> = { ... }` entries).
            RuleType::LeafRule {
                left: NewField::TypeField(TypeType::Simple(t)),
                right: NewField::AliasField(_) | NewField::ValueField(_) | NewField::ScalarField,
            }
            | RuleType::NodeRule {
                left: NewField::TypeField(TypeType::Simple(t)),
                ..
            } => {
                let style = if matches!(rule_type, RuleType::NodeRule { .. }) {
                    TypeInstanceStyle::NodeKey
                } else {
                    TypeInstanceStyle::LeafKey
                };
                push_type_instances(&mut items, info, t, style);
            }
            // An enum value at the leaf level
            RuleType::LeafValueRule {
                right: NewField::ValueField(ValueType::Enum(e)),
            } => push_enum_leaf_values(&mut items, &mut enum_cache, ruleset, info, e),
            // A bare type reference value
            RuleType::LeafValueRule {
                right: NewField::TypeField(TypeType::Simple(t)),
            }
            | RuleType::LeafRule {
                right: NewField::TypeField(TypeType::Simple(t)),
                ..
            } => push_type_instances(&mut items, info, t, TypeInstanceStyle::Reference),
            // An alias expansion
            RuleType::LeafRule {
                right: NewField::AliasField(cat),
                ..
            }
            | RuleType::LeafValueRule {
                right: NewField::AliasField(cat),
            }
            | RuleType::NodeRule {
                left: NewField::AliasField(cat),
                ..
            } => push_alias_keys(&mut items, ruleset, info, modifier_keys, cat),
            // alias_keys_field[cat]: the KEY of this leaf must be one of the alias
            // names in category `cat` (e.g. `alias_keys_field[modifier]` in a
            // dynamic_modifier block). Offer the same set as alias_name[cat] would
            // (cwtools-vscode#65).
            RuleType::LeafRule {
                left: NewField::AliasValueKeysField(cat),
                ..
            } => push_alias_keys(&mut items, ruleset, info, modifier_keys, cat),
            // Scope names
            RuleType::LeafRule {
                right: NewField::ScopeField(_),
                ..
            }
            | RuleType::LeafValueRule {
                right: NewField::ScopeField(_),
            } => {
                let names =
                    scope_names.get_or_insert_with(|| scope_completion_names(language, registry));
                push_scope_names(&mut items, names);
            }
            // Boolean field at leaf value level
            RuleType::LeafValueRule {
                right: NewField::ValueField(ValueType::Bool),
            } => push_bool_leaf_values(&mut items),
            _ => {}
        }
    }

    // Dedup by label: subtype-flattening can produce duplicate rules when the same
    // field appears in multiple subtypes. Keep the first occurrence (which carries
    // the most specific snippet) (cwtools-vscode#66).
    let mut seen_labels: HashSet<String> = HashSet::new();
    items.retain(|item| seen_labels.insert(item.label.clone()));

    items
}

/// A concrete leaf key (`key = <value>`): one `FIELD` item completing to
/// `key = ` with a value-shaped placeholder (yes/no for bools, inline choices
/// for short enums, a bare tab stop otherwise — cwtools-vscode#16).
fn push_specific_leaf_key(
    items: &mut Vec<CompletionItem>,
    k: &str,
    right: &NewField,
    opts: &cwtools_rules::rules_types::Options,
    ruleset: &RuleSet,
) {
    let snippet = match right {
        NewField::ValueField(ValueType::Bool) => {
            // Insert a yes/no placeholder
            Some(format!("{} = ${{1|yes,no|}}", k))
        }
        NewField::ValueField(ValueType::Enum(e)) => {
            // Inline enum values if the list is short enough
            let vals = enum_values_for(ruleset, e);
            if !vals.is_empty() && vals.len() <= 20 {
                let choices = vals.join(",");
                Some(format!("{} = ${{1|{}|}}", k, choices))
            } else {
                // Long enum: still complete the `key = ` and let the
                // value be typed/triggered.
                Some(format!("{} = $0", k))
            }
        }
        // Any other value kind (scalar, int, float, type ref, …):
        // complete `key = ` with the cursor after the `=`, rather than
        // a bare `key` with no operator (cwtools-vscode#16).
        _ => Some(format!("{} = $0", k)),
    };
    items.push(CompletionItem {
        label: k.to_string(),
        kind: Some(CompletionItemKind::FIELD),
        detail: opts.description.clone(),
        insert_text: snippet.clone(),
        insert_text_format: if snippet.is_some() {
            Some(InsertTextFormat::SNIPPET)
        } else {
            None
        },
        sort_text: sort_for_kind(Some(CompletionItemKind::FIELD), k),
        ..Default::default()
    });
}

/// A node block key (`key = { … }`): one `STRUCT` item whose snippet pre-fills
/// the block's required child fields. Rules carrying `required_scopes` sort
/// ahead of the rest.
fn push_specific_node_key(
    items: &mut Vec<CompletionItem>,
    k: &str,
    inner: &[(RuleType, cwtools_rules::rules_types::Options)],
    opts: &cwtools_rules::rules_types::Options,
    ruleset: &RuleSet,
) {
    let snippet = generate_node_snippet(k, inner, ruleset);
    // Scope-aware sortText: if rule has required_scopes push it earlier (lower sort key).
    let sort = if !opts.required_scopes.is_empty() {
        format!("0_{}", k)
    } else {
        format!("1_{}", k)
    };
    items.push(CompletionItem {
        label: k.to_string(),
        kind: Some(CompletionItemKind::STRUCT),
        detail: opts.description.clone(),
        insert_text: Some(snippet),
        insert_text_format: Some(InsertTextFormat::SNIPPET),
        sort_text: Some(sort),
        ..Default::default()
    });
}

/// Enum members as leaf keys (`enum[stat] = variable_field`): each member is a
/// valid key completing to `member = <value>`, the value placeholder shaped by
/// the rule's right-hand side.
fn push_enum_keyed_leaf(
    items: &mut Vec<CompletionItem>,
    enum_cache: &mut std::collections::HashMap<String, Vec<String>>,
    ruleset: &RuleSet,
    info: &InfoService,
    e: &str,
    right: &NewField,
) {
    let snippet_value = match right {
        NewField::ValueField(ValueType::Bool) => "${1|yes,no|}".to_string(),
        _ => "${1}".to_string(),
    };
    for v in all_enum_values_cached(enum_cache, ruleset, info, e) {
        items.push(CompletionItem {
            label: v.clone(),
            kind: Some(CompletionItemKind::FIELD),
            detail: Some(format!("enum {}", e)),
            insert_text: Some(format!("{} = {}", v, snippet_value)),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            sort_text: sort_for_kind(Some(CompletionItemKind::FIELD), v),
            ..Default::default()
        });
    }
}

/// Enum members as node-block keys (`enum[x] = { … }`): each member completes to
/// `member = { $0 }`.
fn push_enum_keyed_node(
    items: &mut Vec<CompletionItem>,
    enum_cache: &mut std::collections::HashMap<String, Vec<String>>,
    ruleset: &RuleSet,
    info: &InfoService,
    e: &str,
) {
    for v in all_enum_values_cached(enum_cache, ruleset, info, e) {
        items.push(CompletionItem {
            label: v.clone(),
            kind: Some(CompletionItemKind::STRUCT),
            detail: Some(format!("enum {}", e)),
            insert_text: Some(format!("{} = {{\n\t$0\n}}", v)),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            sort_text: sort_for_kind(Some(CompletionItemKind::STRUCT), v),
            ..Default::default()
        });
    }
}

/// Enum members as bare leaf values: one `ENUM_MEMBER` item per member, no
/// insert-text (the value is the label itself).
fn push_enum_leaf_values(
    items: &mut Vec<CompletionItem>,
    enum_cache: &mut std::collections::HashMap<String, Vec<String>>,
    ruleset: &RuleSet,
    info: &InfoService,
    e: &str,
) {
    for v in all_enum_values_cached(enum_cache, ruleset, info, e) {
        items.push(CompletionItem {
            label: v.clone(),
            kind: Some(CompletionItemKind::ENUM_MEMBER),
            detail: Some(format!("enum {}", e)),
            sort_text: sort_for_kind(Some(CompletionItemKind::ENUM_MEMBER), v),
            ..Default::default()
        });
    }
}

/// How a `<type>` reference is offered: as a key (with a `key = …` snippet) or
/// as a bare value reference (just the label).
enum TypeInstanceStyle {
    /// `<type> = { … }` key — completes to `name = { $0 }`.
    NodeKey,
    /// `<type> = value` key — completes to `name = ${1}`.
    LeafKey,
    /// A bare `<type>` value reference — the label is the instance name.
    Reference,
}

/// Emit one completion item per known instance of `t`. Shared by the typed-key
/// arms (which complete to `name = …` snippets) and the bare type-reference
/// value arm (which offers the instance name directly).
fn push_type_instances(
    items: &mut Vec<CompletionItem>,
    info: &InfoService,
    t: &str,
    style: TypeInstanceStyle,
) {
    for (_, inst) in info.type_index.instances(t) {
        let (kind, insert_text) = match style {
            TypeInstanceStyle::NodeKey => (
                CompletionItemKind::STRUCT,
                Some(format!("{} = {{\n\t$0\n}}", inst.name)),
            ),
            TypeInstanceStyle::LeafKey => (
                CompletionItemKind::STRUCT,
                Some(format!("{} = ${{1}}", inst.name)),
            ),
            TypeInstanceStyle::Reference => (CompletionItemKind::REFERENCE, None),
        };
        items.push(CompletionItem {
            label: inst.name.clone(),
            kind: Some(kind),
            detail: Some(format!("{} instance", t)),
            insert_text_format: insert_text.as_ref().map(|_| InsertTextFormat::SNIPPET),
            insert_text,
            sort_text: sort_for_kind(Some(kind), &inst.name),
            ..Default::default()
        });
    }
}

/// Emit the keys of all `alias:<cat>` entries, labelled with the category
/// (trigger/effect/…) and carrying the alias's ### docs. Overloads collapse
/// onto one item (first description and first resolvable snippet win). The
/// `modifier` category has no alias entries, so its keys come from the expanded
/// modifier-key set instead.
///
/// Type-pattern aliases like `alias[effect:<scripted_effect>] = yes` are expanded
/// here: instead of emitting the raw `<scripted_effect>` placeholder, we look up
/// every instance of the `scripted_effect` type in the index and offer each as a
/// KEYWORD item (cwtools-vscode#64).
fn push_alias_keys(
    items: &mut Vec<CompletionItem>,
    ruleset: &RuleSet,
    info: &InfoService,
    modifier_keys: &HashSet<String>,
    cat: &str,
) {
    let prefix = format!("{}:", cat);
    // Own the keys so that instance names (borrowed from the type index, not from
    // `ruleset.aliases`) can also participate in the seen-check below.
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (alias_name, (rule, opts)) in &ruleset.aliases {
        let Some(k) = alias_name.strip_prefix(&prefix) else {
            continue;
        };
        if k == "scope_field" {
            continue;
        }
        // Skip type/enum/value pattern aliases (e.g. `<scripted_effect>`,
        // `enum[idea_name]`). They are expanded from the type index below.
        if ParsedAliasPattern::parse(k, 0).is_some() {
            continue;
        }
        if let Some(&idx) = seen.get(k) {
            let item: &mut CompletionItem = &mut items[idx];
            if item.documentation.is_none()
                && let Some(d) = &opts.description
            {
                item.documentation = Some(Documentation::String(d.clone()));
            }
            // First overload wins the snippet; adopt a later one only
            // if the first had no resolvable shape.
            if item.insert_text.is_none()
                && let Some(snip) = alias_completion_snippet(k, rule, ruleset)
            {
                item.insert_text = Some(snip);
                item.insert_text_format = Some(InsertTextFormat::SNIPPET);
            }
            continue;
        }
        seen.insert(k.to_string(), items.len());
        // A block effect/trigger (`if`, `random`, …) completes to
        // `key = { …required fields… }`; a value one
        // (`add_political_power`) to `key = <placeholder>` so the
        // cursor lands after the `=`, ready for the value.
        let snippet = alias_completion_snippet(k, rule, ruleset);
        items.push(CompletionItem {
            label: k.to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            detail: Some(cat.to_string()),
            documentation: opts.description.clone().map(Documentation::String),
            insert_text_format: snippet.as_ref().map(|_| InsertTextFormat::SNIPPET),
            insert_text: snippet,
            sort_text: sort_for_kind(Some(CompletionItemKind::KEYWORD), k),
            ..Default::default()
        });
    }

    // Expand pure type-pattern aliases (e.g. `alias[effect:<scripted_effect>] = yes`):
    // emit one KEYWORD item per instance of the referenced type. Composite patterns
    // like `production_speed_<building>_factor` (non-empty prefix/suffix) are too
    // complex to expand here and are skipped.
    if let Some(cat_idx) = ruleset.alias_categories.get(cat) {
        for pattern in &cat_idx.parsed_patterns {
            if !pattern.prefix.is_empty() || !pattern.suffix.is_empty() {
                continue;
            }
            let (_, (rule, _)) = &ruleset.aliases[pattern.alias_idx];
            match pattern.kind {
                PatternKind::Type => {
                    for (_, inst) in info.type_index.instances(&pattern.placeholder_name) {
                        if seen.contains_key(&inst.name) {
                            continue;
                        }
                        let snippet = alias_completion_snippet(&inst.name, rule, ruleset);
                        items.push(CompletionItem {
                            label: inst.name.clone(),
                            kind: Some(CompletionItemKind::KEYWORD),
                            detail: Some(cat.to_string()),
                            insert_text_format: snippet.as_ref().map(|_| InsertTextFormat::SNIPPET),
                            insert_text: snippet,
                            sort_text: sort_for_kind(Some(CompletionItemKind::KEYWORD), &inst.name),
                            ..Default::default()
                        });
                    }
                }
                PatternKind::Enum => {
                    for v in all_enum_values(ruleset, info, &pattern.placeholder_name) {
                        if seen.contains_key(&v) {
                            continue;
                        }
                        let snippet = alias_completion_snippet(&v, rule, ruleset);
                        items.push(CompletionItem {
                            label: v.clone(),
                            kind: Some(CompletionItemKind::KEYWORD),
                            detail: Some(cat.to_string()),
                            insert_text_format: snippet.as_ref().map(|_| InsertTextFormat::SNIPPET),
                            insert_text: snippet,
                            sort_text: sort_for_kind(Some(CompletionItemKind::KEYWORD), &v),
                            ..Default::default()
                        });
                    }
                }
                PatternKind::Value => {
                    for v in info
                        .type_index
                        .value_set_values
                        .values(&pattern.placeholder_name)
                    {
                        if seen.contains_key(v) {
                            continue;
                        }
                        let snippet = alias_completion_snippet(v, rule, ruleset);
                        items.push(CompletionItem {
                            label: v.to_string(),
                            kind: Some(CompletionItemKind::KEYWORD),
                            detail: Some(cat.to_string()),
                            insert_text_format: snippet.as_ref().map(|_| InsertTextFormat::SNIPPET),
                            insert_text: snippet,
                            sort_text: sort_for_kind(Some(CompletionItemKind::KEYWORD), v),
                            ..Default::default()
                        });
                    }
                }
            }
        }
    }

    // The `modifier` category has no alias entries — modifiers live
    // in the expanded modifier-key set (modifiers.cwt + templated
    // names like production_speed_<building>_factor). This is the
    // MIO `equipment_bonus` / idea `modifier` block case.
    if cat == "modifier" {
        for m in modifier_keys {
            items.push(CompletionItem {
                label: m.clone(),
                kind: Some(CompletionItemKind::FIELD),
                detail: Some("modifier".to_string()),
                insert_text: Some(format!("{} = $0", m)),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                sort_text: sort_for_kind(Some(CompletionItemKind::FIELD), m),
                ..Default::default()
            });
        }
    }
}

/// Scope names (`scope[country]` positions): one `VALUE` item per name.
fn push_scope_names(items: &mut Vec<CompletionItem>, names: &[String]) {
    for name in names {
        items.push(CompletionItem {
            label: name.clone(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some("scope".to_string()),
            sort_text: sort_for_kind(Some(CompletionItemKind::VALUE), name),
            ..Default::default()
        });
    }
}

/// Boolean leaf value: the `yes`/`no` keywords.
fn push_bool_leaf_values(items: &mut Vec<CompletionItem>) {
    for v in &["yes", "no"] {
        items.push(CompletionItem {
            label: v.to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            detail: Some("bool".to_string()),
            sort_text: sort_for_kind(Some(CompletionItemKind::KEYWORD), v),
            ..Default::default()
        });
    }
}

pub(crate) fn enum_values_for<'a>(ruleset: &'a RuleSet, enum_name: &str) -> &'a [String] {
    if let Some(&idx) = ruleset.enum_by_name.get(enum_name) {
        return &ruleset.enums[idx].values;
    }
    &[]
}

/// Enum members from the static definition AND the collected complex-enum
/// values (equipment_stat, country_tags, idea_name, … extracted from game
/// files). The completion-item paths use this; snippet placeholders stick to
/// the static list (dynamic enums are too large for inline choices).
pub(crate) fn all_enum_values(
    ruleset: &RuleSet,
    info: &InfoService,
    enum_name: &str,
) -> Vec<String> {
    let mut vals = enum_values_for(ruleset, enum_name).to_vec();
    vals.extend(
        info.type_index
            .complex_enum_values
            .values(enum_name)
            .map(str::to_string),
    );
    vals.sort_unstable();
    vals.dedup();
    vals
}

/// Per-request memo for [`all_enum_values`]: one completion request can hit the
/// same enum across several match arms (e.g. multiple `LeafValueRule`s sharing
/// `equipment_stat`), and `all_enum_values` re-collects + sorts + dedups each
/// time. Cache by enum name within a single call so it only happens once.
fn all_enum_values_cached<'c>(
    cache: &'c mut std::collections::HashMap<String, Vec<String>>,
    ruleset: &RuleSet,
    info: &InfoService,
    enum_name: &str,
) -> &'c [String] {
    cache
        .entry(enum_name.to_string())
        .or_insert_with(|| all_enum_values(ruleset, info, enum_name))
}

/// Completion items for a leaf VALUE position: enumerate what the matched
/// rules' right-hand sides accept. `value_rules` comes from
/// `position::rules_at_pos` (alias usages already expanded to their overloads,
/// so `has_completed_focus = |` arrives here as a `TypeField("focus")` rule).
pub(crate) fn value_completions(
    value_rules: &[(RuleType, cwtools_rules::rules_types::Options)],
    ruleset: &RuleSet,
    info: &InfoService,
    registry: Option<&cwtools_game::scope_registry::ScopeRegistry>,
    language: &str,
) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    // Per-request memo so a repeated enum is only collected/sorted once (#46).
    let mut enum_cache: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    // Built (sort + clone) at most once per call even if several scope-typed
    // value rules arrive here (#44).
    let mut scope_names: Option<Vec<String>> = None;
    let mut push = |label: String,
                    kind: CompletionItemKind,
                    detail: String,
                    items: &mut Vec<CompletionItem>| {
        if seen.insert(label.clone()) {
            let sort_label = label.clone();
            items.push(CompletionItem {
                label,
                kind: Some(kind),
                detail: Some(detail),
                sort_text: sort_for_kind(Some(kind), &sort_label),
                ..Default::default()
            });
        }
    };

    for (rule_type, _opts) in value_rules {
        let right = match rule_type {
            RuleType::LeafRule { right, .. } => right,
            RuleType::LeafValueRule { right } => right,
            _ => continue,
        };
        match right {
            NewField::TypeField(TypeType::Simple(t)) => {
                for (_, inst) in info.type_index.instances(t) {
                    push(
                        inst.name.clone(),
                        CompletionItemKind::REFERENCE,
                        format!("{} instance", t),
                        &mut items,
                    );
                }
            }
            NewField::TypeField(TypeType::Complex {
                prefix,
                name,
                suffix,
            }) => {
                for (_, inst) in info.type_index.instances(name) {
                    push(
                        format!("{}{}{}", prefix, inst.name, suffix),
                        CompletionItemKind::REFERENCE,
                        format!("{} instance", name),
                        &mut items,
                    );
                }
            }
            NewField::ValueField(ValueType::Enum(e)) => {
                for v in all_enum_values_cached(&mut enum_cache, ruleset, info, e) {
                    push(
                        v.clone(),
                        CompletionItemKind::ENUM_MEMBER,
                        format!("enum {}", e),
                        &mut items,
                    );
                }
            }
            NewField::ValueField(ValueType::Bool) => {
                for v in ["yes", "no"] {
                    push(
                        v.to_string(),
                        CompletionItemKind::KEYWORD,
                        "bool".to_string(),
                        &mut items,
                    );
                }
            }
            NewField::ScopeField(_)
            | NewField::ValueScopeField { .. }
            | NewField::ValueScopeMarkerField { .. } => {
                let names =
                    scope_names.get_or_insert_with(|| scope_completion_names(language, registry));
                for name in names.iter() {
                    push(
                        name.clone(),
                        CompletionItemKind::VALUE,
                        "scope".to_string(),
                        &mut items,
                    );
                }
            }
            // `value[x]` reads and writes both offer the already-collected set
            // members. A write (`set_country_flag = |`) names a new member, so the
            // list is a "did you mean an existing one" hint rather than a closed
            // set; reads (`value[x]`) want exactly these. Same source either way.
            NewField::VariableGetField(ns) | NewField::VariableSetField(ns) => {
                let source: Vec<String> = match ns.as_str() {
                    "event_target" => info.event_target_counts.keys().cloned().collect(),
                    "variable" => info.variable_counts.keys().cloned().collect(),
                    // Flags/tokens/…: config-declared values plus the members
                    // collected from mod+vanilla effects (set_country_flag etc.).
                    other => {
                        let mut vals: Vec<String> =
                            ruleset.values.get(other).cloned().unwrap_or_default();
                        vals.extend(
                            info.type_index
                                .value_set_values
                                .values(other)
                                .map(str::to_string),
                        );
                        vals
                    }
                };
                for v in source {
                    push(
                        v,
                        CompletionItemKind::CONSTANT,
                        format!("value[{}]", ns),
                        &mut items,
                    );
                }
            }
            NewField::VariableField { .. } => {
                for v in info.variable_counts.keys() {
                    push(
                        v.clone(),
                        CompletionItemKind::CONSTANT,
                        "variable".to_string(),
                        &mut items,
                    );
                }
            }
            _ => {}
        }
    }

    items
}

/// Build an LSP snippet body for a NodeRule, pre-populating required child fields
/// (those with cardinality min >= 1 and a SpecificField left-side).
///
/// Mirrors F# createSnippetForClause:346-390. Tab-stop numbering starts at 1.
pub(crate) fn generate_node_snippet(
    key: &str,
    child_rules: &[(RuleType, cwtools_rules::rules_types::Options)],
    ruleset: &RuleSet,
) -> String {
    // Collect required SpecificField leaves/nodes (min >= 1).
    let mut required_parts: Vec<String> = Vec::new();
    let mut tab_stop = 1u32;

    // Use a seen-set so duplicate keys (e.g. from subtype rules) don't repeat.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (rule_type, opts) in child_rules {
        if opts.min < 1 {
            continue;
        }
        match rule_type {
            RuleType::LeafRule {
                left: NewField::SpecificField(k),
                right,
            } => {
                if seen.contains(k) {
                    continue;
                }
                seen.insert(k.clone());
                let placeholder = leaf_right_placeholder(right, tab_stop, ruleset);
                required_parts.push(format!("\t{} = {}", k, placeholder));
                tab_stop += 1;
            }
            RuleType::NodeRule {
                left: NewField::SpecificField(k),
                ..
            } => {
                if seen.contains(k) {
                    continue;
                }
                seen.insert(k.clone());
                required_parts.push(format!("\t{} = ${{{}:{{ }}}}", k, tab_stop));
                tab_stop += 1;
            }
            _ => {}
        }
    }

    if required_parts.is_empty() {
        // No required fields — just a block with cursor inside.
        format!("{} = {{\n\t$0\n}}", key)
    } else {
        let body = required_parts.join("\n");
        format!("{} = {{\n{}\n}}", key, body)
    }
}

/// Build the snippet body for an alias (effect/trigger) completion item from the
/// alias's rule shape. A block alias (`if`, `random`, `every_state`) expands to
/// `key = { …required fields… }` via [`generate_node_snippet`] (so e.g. `if`
/// pre-fills its required `limit = { }`); a value alias (`add_political_power`,
/// `set_country_flag`) expands to `key = <placeholder>` so the cursor lands after
/// the `=`, ready for the value. Returns `None` for shapes that have no snippet.
fn alias_completion_snippet(key: &str, rule: &RuleType, ruleset: &RuleSet) -> Option<String> {
    match rule {
        RuleType::NodeRule { rules, .. } => Some(generate_node_snippet(key, rules, ruleset)),
        RuleType::LeafRule { right, .. } => Some(format!(
            "{} = {}",
            key,
            leaf_right_placeholder(right, 0, ruleset)
        )),
        _ => None,
    }
}

/// Produce a snippet placeholder string for the right-hand side of a leaf rule.
pub(crate) fn leaf_right_placeholder(right: &NewField, tab_stop: u32, ruleset: &RuleSet) -> String {
    match right {
        NewField::ValueField(ValueType::Bool) => {
            format!("${{{}|yes,no|}}", tab_stop)
        }
        NewField::ValueField(ValueType::Enum(e)) => {
            let vals = enum_values_for(ruleset, e);
            if !vals.is_empty() && vals.len() <= 20 {
                format!("${{{}|{}|}}", tab_stop, vals.join(","))
            } else {
                format!("${{{}}}", tab_stop)
            }
        }
        // A concrete literal value (e.g. `alias[effect:<se>] = yes`): insert it
        // directly so the snippet reads `my_se = yes` rather than `my_se = ${0}`.
        NewField::SpecificField(s) if !s.is_empty() => s.clone(),
        _ => format!("${{{}}}", tab_stop),
    }
}

/// Build root-level type snippets for types whose path matches `logical_path`.
///
/// When the cursor is at the top level of a file, offer a snippet for each
/// matching type.  Mirrors F# rootTypeItems:1077-1097: uses typeKeyFilter keys
/// as the block opener if set, otherwise the type name itself; also adds
/// subtype.typeKeyField alternatives.
pub(crate) fn root_type_snippets(ruleset: &RuleSet, logical_path: &str) -> Vec<CompletionItem> {
    let mut items = Vec::new();

    for td in &ruleset.types {
        if !cwtools_info::check_path_dir(&td.path_options, logical_path) {
            continue;
        }

        // Determine which keys to offer as block openers.
        let mut openers: Vec<String> = match &td.type_key_filter {
            Some((keys, false)) if !keys.is_empty() => keys.clone(),
            _ => vec![td.name.clone()],
        };

        // Add subtype typeKeyField alternatives.
        for st in &td.subtypes {
            if let Some(tkf) = &st.type_key_field
                && !openers.contains(tkf)
            {
                openers.push(tkf.clone());
            }
        }

        // Find the TypeRule for this type to get child rules for snippet body.
        let child_rules: Option<&[(RuleType, cwtools_rules::rules_types::Options)]> =
            ruleset.root_rules.iter().find_map(|r| {
                if let RootRule::TypeRule(name, (RuleType::NodeRule { rules, .. }, _)) = r {
                    if name == &td.name {
                        Some(rules.as_slice())
                    } else {
                        None
                    }
                } else {
                    None
                }
            });

        for opener in openers {
            let snippet = if let Some(cr) = child_rules {
                generate_node_snippet(&opener, cr, ruleset)
            } else {
                format!("{} = {{\n\t$0\n}}", opener)
            };
            items.push(CompletionItem {
                label: opener.clone(),
                kind: Some(CompletionItemKind::STRUCT),
                detail: Some(format!("type {} instance", td.name)),
                insert_text: Some(snippet),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                sort_text: Some(format!("0_{}", opener)),
                ..Default::default()
            });
        }
    }

    items
}

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

        let items = completions_from_rules(rules, &rs, &info, "stellaris", &HashSet::new(), None);

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
        let items = completions_from_rules(rules, &rs, &info, "stellaris", &HashSet::new(), None);
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
        let items = completions_from_rules(rules, &rs, &info, "stellaris", &HashSet::new(), None);
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
        let items = completions_from_rules(&rules, &rs, &info, "hoi4", &HashSet::new(), None);

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
        let items = completions_from_rules(&rules, &rs, &info, "hoi4", &HashSet::new(), None);

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
        // `always = ${0|yes,no|}`, not a bare `${0|yes,no|}` with no `=`.
        let rs = bool_trigger_ruleset();
        let info = cwtools_info::InfoService::new();
        let rules = vec![(
            RuleType::LeafRule {
                left: NewField::AliasField("trigger".to_string()),
                right: NewField::AliasField("trigger".to_string()),
            },
            Options::default(),
        )];
        let items = completions_from_rules(&rules, &rs, &info, "hoi4", &HashSet::new(), None);

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
        assert!(
            snip.contains("yes") && snip.contains("no"),
            "bool trigger must offer yes/no choices, got: {:?}",
            always.insert_text
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
                location: cwtools_info::SourceLocation { line: 1, col: 0 },
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
        let items = completions_from_rules(&rules, &rs, &info, "hoi4", &HashSet::new(), None);

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
        let items = completions_from_rules(&rules, &rs, &info, "hoi4", &modifier_keys, None);

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
        let items = completions_from_rules(&rules, &rs, &info, "hoi4", &HashSet::new(), None);
        let count = items.iter().filter(|i| i.label == "active").count();
        assert_eq!(
            count, 1,
            "duplicate label 'active' must appear exactly once, got {} copies",
            count
        );
    }

    // keep Arc in scope to avoid unused-import warning when no test uses it
    const _: fn() = || {
        let _ = Arc::new(());
    };
}
