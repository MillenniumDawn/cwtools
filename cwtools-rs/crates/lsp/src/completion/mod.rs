use std::collections::HashSet;
use std::sync::Arc;

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

use cwtools_parser::ast::ParsedFile;
use cwtools_rules::rules_types::RuleSet;
use cwtools_validation::position::{rules_at_pos, value_rules_for_key};

use crate::Backend;
use crate::CompletionCacheEntry;
use crate::paths::{current_token_range, line_value_key, logical_path_from_uri};

mod builders;
mod scope_names;
mod snippets;

pub(crate) use builders::{completions_from_rules, root_type_snippets, value_completions};
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

        let (lsp_line, lsp_col) = crate::paths::lsp_pos_to_source(pos);

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
        let doc_text: String = {
            let docs = self.state.documents.lock();
            docs.get(&uri).map(|d| d.text.clone()).unwrap_or_default()
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
        let effective_ast: Option<Arc<ParsedFile>> = self.ast_for(&uri);
        let context_items: Option<(Vec<CompletionItem>, bool)> =
            match (effective_ast, ruleset_arc.as_ref()) {
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
                        None => Some((root_type_snippets(rs, &logical_path), false)),
                        Some(rctx) => {
                            let (items, resolved_value_pos) =
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
                                                scope_registry_arc.as_deref(),
                                                rctx.scope.as_ref().map(|s| s.current()),
                                            ),
                                            false,
                                        )
                                    } else {
                                        (
                                            value_completions(
                                                &rctx.value_rules,
                                                rs,
                                                &info_guard,
                                                scope_registry_arc.as_deref(),
                                                &language,
                                            ),
                                            !rctx.value_rules.is_empty(),
                                        )
                                    }
                                } else if let Some(key) =
                                    line_value_key(&doc_text, pos.line, pos.character)
                                {
                                    // Mid-edit `key = |`: the last good parse has no such
                                    // leaf yet; resolve the value rules from the live line.
                                    let vr = value_rules_for_key(
                                        rs,
                                        Some(&info_guard.type_index),
                                        &rctx.child_rules,
                                        &key,
                                    );
                                    let resolved = !vr.is_empty();
                                    (
                                        value_completions(
                                            &vr,
                                            rs,
                                            &info_guard,
                                            scope_registry_arc.as_deref(),
                                            &language,
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
                                            scope_registry_arc.as_deref(),
                                            rctx.scope.as_ref().map(|s| s.current()),
                                        ),
                                        false,
                                    )
                                };
                            Some((items, resolved_value_pos))
                        }
                    }
                }
                _ => None,
            };

        if let Some((mut items, resolved_value_pos)) = context_items {
            if !items.is_empty() {
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
            // Value position matched a concrete rule but had nothing to offer
            // (empty dynamic set, or a value type with no enumerable members):
            // return no completions rather than the flat variable dump.
            if resolved_value_pos {
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

        let items =
            completions_from_rules(rules, &rs, &info, "stellaris", &HashSet::new(), None, None);

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
        let items =
            completions_from_rules(rules, &rs, &info, "stellaris", &HashSet::new(), None, None);
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
        let items =
            completions_from_rules(rules, &rs, &info, "stellaris", &HashSet::new(), None, None);
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
        let items = completions_from_rules(&rules, &rs, &info, "hoi4", &HashSet::new(), None, None);

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
        let items = completions_from_rules(&rules, &rs, &info, "hoi4", &HashSet::new(), None, None);

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
        let items = completions_from_rules(&rules, &rs, &info, "hoi4", &HashSet::new(), None, None);

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
        let items = completions_from_rules(&rules, &rs, &info, "hoi4", &HashSet::new(), None, None);

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
        let items = value_completions(&value_rules, &rs, &info, None, "hoi4");

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
        let items = completions_from_rules(&rules, &rs, &info, "hoi4", &HashSet::new(), None, None);

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
        let items = completions_from_rules(&rules, &rs, &info, "hoi4", &modifier_keys, None, None);

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
        let items = completions_from_rules(&rules, &rs, &info, "hoi4", &HashSet::new(), None, None);
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
