use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde_json::Value;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

use cwtools_info::check_path_dir;
use cwtools_rules::rules_types::RuleSet;
use cwtools_rules::ruleset_loader::load_ruleset_from_dir;
use cwtools_validation::build_scope_registry_arc;

use crate::Backend;
use crate::paths::default_cache_dir;

/// Pull `ignoreFilePatterns` and `ignoreDirectories` arrays out of a
/// `serde_json::Value` (the `initializationOptions` payload and the
/// `workspace/didChangeConfiguration` payload share the same shape).
/// Returns the two lists. Filters non-string and empty entries.
pub(crate) fn extract_ignore_patterns(opts: &Value) -> (Vec<String>, Vec<String>) {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    if let Some(arr) = opts.get("ignoreFilePatterns").and_then(|v| v.as_array()) {
        for v in arr {
            if let Some(s) = v.as_str()
                && !s.is_empty()
            {
                files.push(s.to_string());
            }
        }
    }
    if let Some(arr) = opts.get("ignoreDirectories").and_then(|v| v.as_array()) {
        for v in arr {
            if let Some(s) = v.as_str()
                && !s.is_empty()
            {
                dirs.push(s.to_string());
            }
        }
    }
    (files, dirs)
}

/// Pull `ignoredErrorCodes` (diagnostic codes the user suppressed via
/// `errors.ignore`) out of the shared init/didChange payload. Lowercased so the
/// publish-time filter compares case-insensitively; non-string and empty
/// entries are dropped.
pub(crate) fn extract_ignored_error_codes(opts: &Value) -> Vec<String> {
    let mut codes = Vec::new();
    if let Some(arr) = opts.get("ignoredErrorCodes").and_then(|v| v.as_array()) {
        for v in arr {
            if let Some(s) = v.as_str()
                && !s.is_empty()
            {
                codes.push(s.to_ascii_lowercase());
            }
        }
    }
    codes
}

/// Read an optional non-negative integer setting from the shared
/// init/didChange payload. Absent → `None` silently; present but not a u64
/// (string, float, negative) → `None` with a warning naming the key and the
/// received value, so a mistyped setting doesn't just vanish.
pub(crate) fn extract_u64_setting(opts: &Value, key: &str) -> Option<u64> {
    let v = opts.get(key)?;
    let parsed = v.as_u64();
    if parsed.is_none() {
        tracing::warn!(key, value = %v, "ignoring setting: expected a non-negative integer");
    }
    parsed
}

/// Render one localisation stub file for `lang` covering every `missing` key,
/// as `{language, filename_suggestion, content}`. Standard Paradox loc shape:
/// an `l_<lang>:` header then ` KEY:0 "TODO"` entries. The file needs a UTF-8
/// BOM on save — the client prepends it — so the suggested name is the only
/// server-side hint the caller writes it as a `_l_<lang>.yml`.
fn render_loc_stub(lang: cwtools_localization::Lang, missing: &BTreeSet<String>) -> Value {
    let mut content = format!("l_{}:\n", lang);
    for key in missing {
        content.push_str(&format!(" {}:0 \"TODO\"\n", key));
    }
    serde_json::json!({
        "language": lang.to_string(),
        "filename_suggestion": format!("generated_l_{}.yml", lang),
        "content": content,
    })
}

impl Backend {
    /// Install a freshly-loaded ruleset and rebuild the cached scope registry to
    /// match it. The registry depends only on `(ruleset, game)`; building it here
    /// (once per load) keeps it out of the per-file validation hot path. The
    /// ruleset + registry live in one `rules` guard so they never disagree.
    pub(crate) fn set_ruleset(&self, ruleset: RuleSet) {
        let game = self.state.config.read().game();
        // Build the registry and the cached var-effects before taking any of the
        // ruleset-family locks, so the write section is short.
        let registry = build_scope_registry_arc(&ruleset, game);
        // Cache the variable-defining effects so per-file indexing can collect
        // value_set[variable] names (and values) for the CW246 / VariableGetField
        // checks and for hover/goto.
        let var_effects = cwtools_info::variable_defining_effects(&ruleset);
        // Lock order: rules -> info_service.
        let mut rules = self.state.rules.write();
        rules.ruleset = Some(Arc::new(ruleset));
        rules.scope_registry = registry;
        self.state
            .info_service
            .write()
            .update_ruleset_data(var_effects);
        drop(rules);
        self.bump_info_revision();
        // Bump the quiet-pass fingerprint generation: a new ruleset changes
        // validation output, even though reloadrulesconfig also rescans right away.
        self.state
            .settings_generation
            .fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) async fn initialize_impl(
        &self,
        params: InitializeParams,
    ) -> Result<InitializeResult> {
        // Distinctive banner so it's unmistakable in the Output panel WHICH server
        // is running. If you don't see this line, you're on an old/F# binary.
        self.client
            .log_message(
                MessageType::INFO,
                format!("★ CWTools Rust LSP server v{}", env!("CARGO_PKG_VERSION")),
            )
            .await;
        // Store language from init options
        if let Some(opts) = &params.initialization_options {
            if let Some(lang) = opts.get("language").and_then(|v| v.as_str()) {
                self.state.config.write().language = lang.to_string();
                self.client
                    .log_message(MessageType::INFO, format!("language: {}", lang))
                    .await;
            }

            // Optional list of loc languages to validate (e.g. ["english"]).
            // Unknown/empty entries are ignored; an empty resulting list leaves
            // scoping off (validate all languages). See `loc_languages`.
            if let Some(arr) = opts.get("localisationLanguages").and_then(|v| v.as_array()) {
                let langs: Vec<cwtools_localization::Lang> = arr
                    .iter()
                    .filter_map(|v| v.as_str())
                    .filter_map(cwtools_localization::Lang::from_name)
                    .collect();
                if !langs.is_empty() {
                    self.client
                        .log_message(
                            MessageType::INFO,
                            format!("localisation languages scoped to: {:?}", langs),
                        )
                        .await;
                    self.state.config.write().loc_languages = Some(langs);
                }
            }

            // Whether hover shows all loc languages or just the primary one.
            if let Some(all) = opts.get("hoverShowAllLanguages").and_then(|v| v.as_bool()) {
                self.state
                    .hover_show_all_languages
                    .store(all, std::sync::atomic::Ordering::Relaxed);
            }

            // Developer hover: when on, include the raw rule classification
            // (field / type / scope) lines. Off by default — most users only
            // want the localisation, description, and required scopes.
            if let Some(dbg) = opts.get("hoverDebug").and_then(|v| v.as_bool()) {
                self.state
                    .hover_debug
                    .store(dbg, std::sync::atomic::Ordering::Relaxed);
            }

            // Scope display: "resolved" adds a `Resolves to` line (the scope the
            // hovered link/keyword evaluates to); "context" (default) shows only
            // the ambient current scope. (#37)
            if let Some(mode) = opts.get("hoverScopeDisplay").and_then(|v| v.as_str()) {
                self.state
                    .hover_resolved_scope
                    .store(mode == "resolved", std::sync::atomic::Ordering::Relaxed);
            }

            // Inlay hints. Loc-title hints (`cwtools.inlayHints.locTitles`) default
            // ON; resolved-scope hints (`cwtools.inlayHints.scopes`) default OFF.
            // Absent leaves the constructor defaults untouched. Read once at init,
            // matching the hover toggles above.
            if let Some(on) = opts.get("inlayHintsLocTitles").and_then(|v| v.as_bool()) {
                self.state
                    .inlay_hints_loc_titles
                    .store(on, std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(on) = opts.get("inlayHintsScopes").and_then(|v| v.as_bool()) {
                self.state
                    .inlay_hints_scopes
                    .store(on, std::sync::atomic::Ordering::Relaxed);
            }

            // Persistent cache directory for the base-game index (so it isn't
            // re-parsed every startup). The client should pass its global
            // storage path; we fall back to an OS cache dir otherwise.
            if let Some(cd) = opts.get("cacheDir").and_then(|v| v.as_str()) {
                self.state.config.write().cache_dir = Some(std::path::PathBuf::from(cd));
            }

            // Minutes between quiet background re-index passes (0 disables).
            // A live change comes through `did_change_configuration_impl`.
            if let Some(mins) = extract_u64_setting(opts, "backgroundReindexIntervalMinutes") {
                self.state
                    .config
                    .write()
                    .background_reindex_interval_minutes = mins;
            }

            // Seconds of user inactivity a background pass waits for (default
            // 15). A live change comes through `did_change_configuration_impl`
            // and applies on the next reindex cycle.
            if let Some(secs) = extract_u64_setting(opts, "backgroundReindexIdleSeconds") {
                self.state.config.write().background_reindex_idle_seconds = secs;
            }
            self.client
                .log_message(MessageType::INFO, format!("init options: {:?}", opts))
                .await;

            // Load a pre-generated vanilla cache if provided, so the editor
            // resolves base-game references (sprites, operation_tokens, …)
            // without re-parsing the install. Merged into the index in
            // validate_entire_workspace.
            if let Some(vc) = opts.get("vanillaCache").and_then(|v| v.as_str()) {
                match cwtools_info::vanilla_cache::load(std::path::Path::new(vc)) {
                    Ok((game, _fingerprint, data)) => {
                        let total: usize = data.per_type.values().map(|v| v.len()).sum();
                        *self.state.vanilla_index.lock() = Some(data.per_type);
                        if !data.loc_keys.is_empty() {
                            *self.state.vanilla_loc_keys.lock() = Some(data.loc_keys);
                        }
                        self.merge_vanilla_dynamic_values(
                            data.complex_enum_values,
                            data.value_set_values,
                        );
                        self.client
                            .log_message(
                                MessageType::INFO,
                                format!(
                                    "Loaded {} base-game instances from vanilla cache {} (game {})",
                                    total, vc, game
                                ),
                            )
                            .await;
                    }
                    Err(e) => {
                        self.client
                            .log_message(
                                MessageType::WARNING,
                                format!("Could not load vanilla cache {}: {}", vc, e),
                            )
                            .await;
                    }
                }
            }

            // A raw base-game install dir (like the CLI's `--vanilla`). Stored
            // here and indexed lazily on the first full-workspace scan, so the
            // editor resolves base-game references without a pre-built cache.
            if let Some(vd) = opts.get("vanilla").and_then(|v| v.as_str()) {
                let p = std::path::PathBuf::from(vd);
                if p.is_dir() {
                    self.state.config.write().vanilla_dir = Some(p);
                    self.client
                        .log_message(MessageType::INFO, format!("Base-game dir set: {}", vd))
                        .await;
                } else {
                    self.client
                        .log_message(
                            MessageType::WARNING,
                            format!("`vanilla` dir does not exist: {}", vd),
                        )
                        .await;
                }
            }

            // Load .cwt rules from rulesCache if provided. Retain the dir so the
            // `reloadrulesconfig` command can re-read it later without a restart.
            if let Some(cache) = opts.get("rulesCache").and_then(|v| v.as_str()) {
                let cache_path = std::path::PathBuf::from(cache);
                self.state.config.write().rules_dir = Some(cache_path.clone());
                self.load_rules_config(&cache_path).await;
            }
        }

        // Store workspace URI: prefer workspace_folders (multi-root aware), fall
        // back to the legacy root_uri field for clients that only send that.
        let root = if let Some(folders) = &params.workspace_folders
            && let Some(first) = folders.first()
        {
            Some(first.uri.to_string())
        } else {
            params.root_uri.as_ref().map(|u| u.to_string())
        };
        if let Some(root) = root {
            let mut cfg = self.state.config.write();
            cfg.workspace_prefix = Some(crate::paths::workspace_prefix_of(&root));
            cfg.workspace_uri = Some(root.into());
        }

        // Per-workspace ignore globs from the extension. The extension
        // forwards `cwtools.ignore.filePatterns` and `cwtools.ignore.directories`
        // into initializationOptions on first launch; runtime updates come
        // through `workspace/didChangeConfiguration` and re-apply the same
        // helper. We layer these on top of the engine's hard-coded baseline
        // (Changelog.txt, README.*, LICENSE.*, *.md) — user patterns extend,
        // they don't replace.
        if let Some(opts) = &params.initialization_options {
            let (files, dirs) = extract_ignore_patterns(opts);
            let codes = extract_ignored_error_codes(opts);
            if !files.is_empty() || !dirs.is_empty() || !codes.is_empty() {
                let (n_files, n_dirs, n_codes) = (files.len(), dirs.len(), codes.len());
                {
                    let mut cfg = self.state.config.write();
                    cfg.ignore_file_patterns = files;
                    cfg.ignore_dir_patterns = dirs;
                    cfg.ignored_error_codes = codes;
                }
                self.client
                    .log_message(
                        MessageType::INFO,
                        format!(
                            "ignore patterns: {} files, {} dirs, {} suppressed codes (engine defaults still apply)",
                            n_files, n_dirs, n_codes,
                        ),
                    )
                    .await;
            }
        }

        // Negotiate position encoding. The parser counts Unicode scalar values
        // (chars), which equal UTF-32 code units, so advertise utf-32 when the
        // client lists it — that client then gets exact columns on non-BMP
        // lines for free. Clients that don't advertise utf-32 (VS Code) stay on
        // the LSP default (utf-16), so their behavior is unchanged.
        let position_encoding = params
            .capabilities
            .general
            .as_ref()
            .and_then(|g| g.position_encodings.as_ref())
            .filter(|encs| encs.contains(&PositionEncodingKind::UTF32))
            .map(|_| PositionEncodingKind::UTF32);
        self.state.config.write().position_encoding = position_encoding
            .clone()
            .unwrap_or(PositionEncodingKind::UTF16);

        // documentSymbol: return a nested tree only when the client advertises
        // support; otherwise the flat SymbolInformation list is served.
        let hierarchical = params
            .capabilities
            .text_document
            .as_ref()
            .and_then(|td| td.document_symbol.as_ref())
            .and_then(|ds| ds.hierarchical_document_symbol_support)
            .unwrap_or(false);
        self.state
            .hierarchical_symbols
            .store(hierarchical, Ordering::Relaxed);

        // completion: origin labels next to deferred type/enum/alias items,
        // only when the client can render them.
        let label_details = params
            .capabilities
            .text_document
            .as_ref()
            .and_then(|td| td.completion.as_ref())
            .and_then(|c| c.completion_item.as_ref())
            .and_then(|ci| ci.label_details_support)
            .unwrap_or(false);
        self.state
            .completion_label_details
            .store(label_details, Ordering::Relaxed);

        // rename: versioned documentChanges only when the client advertises
        // support; otherwise the legacy `changes` map is served.
        let document_changes = params
            .capabilities
            .workspace
            .as_ref()
            .and_then(|w| w.workspace_edit.as_ref())
            .and_then(|we| we.document_changes)
            .unwrap_or(false);
        self.state
            .workspace_edit_document_changes
            .store(document_changes, Ordering::Relaxed);

        // `$/progress`: only usable when the client says it will answer
        // `window/workDoneProgress/create`. See `scan::send_work_done_progress`.
        let work_done_progress = params
            .capabilities
            .window
            .as_ref()
            .and_then(|w| w.work_done_progress)
            .unwrap_or(false);
        self.state
            .client_work_done_progress
            .store(work_done_progress, Ordering::Relaxed);

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                position_encoding,
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        will_save: None,
                        will_save_wait_until: None,
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(false),
                        })),
                    },
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions {
                    // `completionItem/resolve` fills in `documentation`/`detail`
                    // for the one item the client focuses, deferred out of the
                    // initial list to shrink every response (perf/completion-
                    // responsiveness) — see `completion::resolve`.
                    resolve_provider: Some(true),
                    trigger_characters: Some(vec![
                        "=".to_string(),
                        "<".to_string(),
                        "[".to_string(),
                        "$".to_string(),
                        "#".to_string(),
                    ]),
                    work_done_progress_options: Default::default(),
                    all_commit_characters: None,
                    completion_item: None,
                }),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![
                        "getFileTypes".to_string(),
                        "exportProfilingLog".to_string(),
                        "cacheVanilla".to_string(),
                        "clearAllCaches".to_string(),
                        "reloadrulesconfig".to_string(),
                        "genlocall".to_string(),
                        "reindexWorkspace".to_string(),
                        // The extension greys out its graph commands unless it
                        // finds this name here (`graphAvailability.ts`).
                        "getGraphData".to_string(),
                    ],
                    work_done_progress_options: Default::default(),
                }),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
                // Filepath/icon leaves as clickable links; targets are built
                // up-front in the handler, so no resolve step.
                document_link_provider: Some(DocumentLinkOptions {
                    resolve_provider: Some(false),
                    work_done_progress_options: Default::default(),
                }),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
                // Quick-fixes from diagnostics that carry a `SuggestedFix`
                // payload (CW253/CW282/CW280/CW121/CW281/CW268). No resolve
                // step: the WorkspaceEdit is built up-front in the handler.
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![
                            CodeActionKind::QUICKFIX,
                            // `source.fixAll` is what `editor.codeActionsOnSave`
                            // binds to; without it in this list no client ever
                            // asks for it.
                            CodeActionKind::SOURCE_FIX_ALL,
                        ]),
                        resolve_provider: Some(false),
                        work_done_progress_options: Default::default(),
                    },
                )),
                // Inlay hints: declared statically (loc-title hints default on).
                // The handler gates each kind on its setting and returns nothing
                // when both are off, so a client always-on capability is harmless.
                inlay_hint_provider: Some(OneOf::Left(true)),
                // Semantic tokens: `full` and `range`. `range` runs the same walk
                // with root children outside the viewport skipped, so an edit in
                // a 300-entity file re-resolves the entities on screen instead of
                // all of them. `full/delta` stays off: it needs server-side result
                // caching we have no invalidation story for.
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: crate::semantic::legend(),
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            range: Some(true),
                            work_done_progress_options: Default::default(),
                        },
                    ),
                ),
                // Document colours: `color = { … }` leaves get an inline swatch
                // and the native picker. `colorPresentation` re-reads the source
                // span so the picker writes back the convention it found.
                color_provider: Some(ColorProviderCapability::Simple(true)),
                // Multi-root: the server tracks one primary folder (the first),
                // so a folder change re-points it and re-scans.
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                        supported: Some(true),
                        change_notifications: Some(OneOf::Left(true)),
                    }),
                    file_operations: None,
                }),
                // `position_encoding` (above): utf-32 when the client supports
                // it, else the LSP default (utf-16). The parser counts chars,
                // so on utf-16 clients column offsets are off by the number of
                // astral code points on a line; utf-32 clients get exact
                // columns since UTF-32 code units equal Unicode scalar values.
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "cwtools-server".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    /// Load the `.cwt` rules from `cache_path`, publish any parse errors as
    /// per-file diagnostics plus a popup, and (on success) install the ruleset
    /// and rebuild the modifier-key set. Shared by `initialize` and the
    /// `reloadrulesconfig` command so a live reload behaves exactly like startup.
    /// Returns whether a non-empty ruleset was loaded.
    pub(crate) async fn load_rules_config(&self, cache_path: &std::path::Path) -> bool {
        // Surface a missing rules dir explicitly. The client may hand us a
        // path that doesn't resolve here (e.g. a Windows `rules_folder`
        // that didn't normalise), which otherwise degrades silently to a
        // generic "no rules loaded" with an empty error list.
        if !cache_path.is_dir() {
            self.client
                .log_message(
                    MessageType::WARNING,
                    format!("`rulesCache` dir does not exist: {}", cache_path.display()),
                )
                .await;
        }
        let (combined_ruleset, parse_errors) =
            load_ruleset_from_dir(cache_path, &self.state.string_table);

        // Broken .cwt rules silently degrade every downstream check, so they are
        // reported three ways: the log, a popup, and a diagnostic per file. All
        // three are user-visible and all can run inside `initialize`, where
        // tower-lsp drops outgoing notifications — so each defers through the
        // handshake gate below (#98). Snapshotting once is sound only while the
        // park sites run await-free from here: an `.await` between a stale
        // `false` and a park would let the `initialized` flush slip past and
        // strand the parked message forever.
        let handshake_complete = self
            .state
            .handshake_complete
            .load(std::sync::atomic::Ordering::Relaxed);
        if handshake_complete {
            for err in &parse_errors {
                self.client
                    .log_message(MessageType::ERROR, err.to_string())
                    .await;
            }
        } else {
            self.state.deferred_rules_messages.lock().extend(
                parse_errors
                    .iter()
                    .map(|err| crate::DeferredRulesMessage::Log(err.to_string())),
            );
        }
        let mut diags_by_file: std::collections::HashMap<String, Vec<Diagnostic>> =
            std::collections::HashMap::new();
        for err in &parse_errors {
            // Shared with the live per-file CWT lint (#43). No file text
            // here to widen the squiggle, so pass no line info.
            diags_by_file
                .entry(crate::paths::path_to_uri(&err.file))
                .or_default()
                .push(crate::validate::rule_parse_error_to_diagnostic(
                    err,
                    &crate::validate::DocLines::none(),
                ));
        }
        let mut to_publish: Vec<(String, Vec<Diagnostic>)> = diags_by_file.into_iter().collect();
        // A load only reports files that still have errors, so anything reported
        // last time and absent now has been repaired and needs an explicit clear.
        {
            let current: std::collections::HashSet<String> =
                to_publish.iter().map(|(uri, _)| uri.clone()).collect();
            let mut previous = self.state.published_rule_uris.lock();
            let open = self.state.documents.lock();
            to_publish.extend(
                previous
                    .difference(&current)
                    // An open editor buffer owns its diagnostics: the live `.cwt`
                    // lint republishes it, and clearing here would blank a dirty
                    // buffer's squiggles until the next keystroke.
                    .filter(|uri| !open.contains_key(*uri))
                    .map(|uri| (uri.clone(), Vec::new())),
            );
            *previous = current;
        }

        // Dropped on the floor before `initialized`, so park them for the
        // handshake to flush (#98).
        if handshake_complete {
            for (uri, diags) in to_publish {
                if let Ok(url) = uri.parse() {
                    self.client.publish_diagnostics(url, diags, None).await;
                }
            }
        } else {
            self.state
                .deferred_rule_diagnostics
                .lock()
                .extend(to_publish);
        }
        if let Some(first) = parse_errors.first() {
            // Inline the first error: the client never auto-reveals its output
            // channel (RevealOutputChannelOn.Never), so the popup is the only
            // part a user is guaranteed to see.
            let summary = format!(
                "CWTools: {} rules-config error(s), first: {first}",
                parse_errors.len()
            );
            // Dedupe on the full error set, order-independent: `first` follows
            // read_dir traversal order, so the same set could summarize
            // differently across the boot double-load, and two different sets
            // can share a count and a first error.
            let dedupe_key = {
                let mut errs: Vec<String> = parse_errors.iter().map(|e| e.to_string()).collect();
                errs.sort_unstable();
                errs.join("\n")
            };
            let is_new = {
                let mut last = self.state.last_rules_toast.lock();
                if last.as_deref() == Some(dedupe_key.as_str()) {
                    false
                } else {
                    *last = Some(dedupe_key);
                    true
                }
            };
            if is_new {
                // Re-read the gate: a toast parked after the flush ran would sit
                // forever while the dedupe key above already claimed it.
                if self
                    .state
                    .handshake_complete
                    .load(std::sync::atomic::Ordering::Relaxed)
                {
                    self.client.show_message(MessageType::ERROR, summary).await;
                } else {
                    self.state
                        .deferred_rules_messages
                        .lock()
                        .push(crate::DeferredRulesMessage::Toast(summary));
                }
            }
        } else {
            // A clean load forgets the last toast, so the same errors coming
            // back later in the session toast again.
            *self.state.last_rules_toast.lock() = None;
        }

        let loaded = !combined_ruleset.types.is_empty()
            || !combined_ruleset.enums.is_empty()
            || !combined_ruleset.aliases.is_empty()
            || !combined_ruleset.root_rules.is_empty();

        if loaded {
            self.client
                .log_message(
                    MessageType::INFO,
                    format!(
                        "Loaded rules from {} ({} types, {} enums, {} aliases, {} errors)",
                        cache_path.display(),
                        combined_ruleset.types.len(),
                        combined_ruleset.enums.len(),
                        combined_ruleset.aliases.len(),
                        parse_errors.len(),
                    ),
                )
                .await;
            self.set_ruleset(combined_ruleset);
            // Rebuild modifier_keys now that the ruleset is loaded.
            // The type index is empty at this point; it will be rebuilt
            // again after validate_entire_workspace with the full index.
            self.rebuild_modifier_keys();
        } else {
            self.client
                .log_message(
                    MessageType::WARNING,
                    format!(
                        "No rules loaded from {}. Errors: {:?}",
                        cache_path.display(),
                        parse_errors
                    ),
                )
                .await;
        }
        loaded
    }

    /// React to a workspace folder being added or removed.
    ///
    /// The server tracks ONE primary folder (`config.workspace_uri`, the first
    /// one at initialize) and derives the logical paths, the file scan root and
    /// the type index from it. So the contained behaviour is: re-point that
    /// folder at the first survivor and re-index. A workspace whose FIRST folder
    /// is unchanged therefore only re-indexes; it does not gain the other
    /// folders' content. Indexing several roots at once needs the scan and the
    /// logical-path derivation to become multi-root, which is a bigger change
    /// than this handler.
    pub(crate) async fn did_change_workspace_folders_impl(
        &self,
        params: DidChangeWorkspaceFoldersParams,
    ) {
        let removed: Vec<String> = params
            .event
            .removed
            .iter()
            .map(|f| f.uri.to_string())
            .collect();
        let added: Vec<String> = params
            .event
            .added
            .iter()
            .map(|f| f.uri.to_string())
            .collect();
        let current = self.state.config.read().workspace_uri.clone();
        let current = current.as_deref().map(str::to_string);

        // Re-point only when the primary folder itself went away; otherwise the
        // root the index was built from is still valid.
        let next = match &current {
            Some(uri) if removed.iter().any(|r| r == uri) => added.first().cloned(),
            None => added.first().cloned(),
            _ => current.clone(),
        };
        if next != current {
            match &next {
                Some(uri) => {
                    let mut cfg = self.state.config.write();
                    cfg.workspace_prefix = Some(crate::paths::workspace_prefix_of(uri));
                    cfg.workspace_uri = Some(uri.as_str().into());
                }
                None => {
                    let mut cfg = self.state.config.write();
                    cfg.workspace_prefix = None;
                    cfg.workspace_uri = None;
                }
            }
        }
        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "workspace folders changed (+{} / -{}); primary folder: {}",
                    added.len(),
                    removed.len(),
                    next.as_deref().unwrap_or("<none>"),
                ),
            )
            .await;
        if next.is_none() {
            return;
        }
        // A full rescan, the same path `reindexWorkspace` takes. The generation
        // bump makes the quiet-pass fingerprint stale so the scan can't
        // short-circuit on an unchanged file set from the OLD root.
        self.state
            .settings_generation
            .fetch_add(1, Ordering::SeqCst);
        // `validate_entire_workspace`'s CAS guard returns false when a scan is
        // already running — including the startup scan this notification often
        // races. That scan indexed the old root, so retry until we win the CAS,
        // bounded so a perpetually-busy server reports instead of spinning.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut rescanned = self.validate_entire_workspace(false).await;
        while !rescanned && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            rescanned = self.validate_entire_workspace(false).await;
        }
        if !rescanned {
            self.client
                .log_message(
                    MessageType::WARNING,
                    "workspace folders changed but a scan stayed in progress; index still points at the previous folder",
                )
                .await;
        }
    }

    /// Re-read ignore globs and the background-reindex interval/idle window
    /// when the extension's `cwtools.*` settings change. The shape mirrors
    /// what we accept in `initializationOptions`: the payload is the
    /// `cwtools` namespace object, with optional `ignoreFilePatterns`,
    /// `ignoreDirectories`, `backgroundReindexIntervalMinutes`, and
    /// `backgroundReindexIdleSeconds` — each absent-means-keep, so a partial
    /// payload only touches the keys it carries. The next full-workspace scan
    /// (or reindex cycle) picks up the new values; an in-flight scan finishes
    /// with the snapshot it took.
    pub(crate) async fn did_change_configuration_impl(&self, params: DidChangeConfigurationParams) {
        // The client may send either the whole `cwtools` section (when the
        // section is registered via `configurationSection`) or just the
        // changed slice. `extract_ignore_patterns` looks for the same two
        // keys at the top level — works in both cases. Every key here is
        // absent-means-keep (unlike initialize, where absent means empty):
        // a partial payload carrying only the reindex keys must not wipe
        // the ignore lists. The shipped VS Code client always sends its
        // full section, so this only matters for other clients.
        let (files, dirs) = extract_ignore_patterns(&params.settings);
        let files = params.settings.get("ignoreFilePatterns").map(|_| files);
        let dirs = params.settings.get("ignoreDirectories").map(|_| dirs);
        let codes = params
            .settings
            .get("ignoredErrorCodes")
            .map(|_| extract_ignored_error_codes(&params.settings));
        let counts = (
            files.as_ref().map(Vec::len),
            dirs.as_ref().map(Vec::len),
            codes.as_ref().map(Vec::len),
        );
        let reindex_minutes =
            extract_u64_setting(&params.settings, "backgroundReindexIntervalMinutes");
        let reindex_idle_secs =
            extract_u64_setting(&params.settings, "backgroundReindexIdleSeconds");

        // No-op guard: the client re-sends the whole `cwtools` section on any
        // change to an unrelated key, so an identical payload arrives often.
        // Skip the write and the open-doc revalidate storm (#90) when nothing
        // this handler mutates actually changed. Every field is `None` when
        // its key is absent, and a missing key is never a change.
        {
            let cfg = self.state.config.read();
            let unchanged = files
                .as_ref()
                .is_none_or(|f| *f == cfg.ignore_file_patterns)
                && dirs.as_ref().is_none_or(|d| *d == cfg.ignore_dir_patterns)
                && codes.as_ref().is_none_or(|c| *c == cfg.ignored_error_codes)
                && reindex_minutes.is_none_or(|m| m == cfg.background_reindex_interval_minutes)
                && reindex_idle_secs.is_none_or(|s| s == cfg.background_reindex_idle_seconds);
            if unchanged {
                tracing::debug!("didChangeConfiguration: no relevant change; skipping revalidate");
                return;
            }
        }

        {
            // Any field written here must join the comparison above, or an
            // identical re-send of a changed field will slip past the guard.
            let mut cfg = self.state.config.write();
            if let Some(files) = files {
                cfg.ignore_file_patterns = files;
            }
            if let Some(dirs) = dirs {
                cfg.ignore_dir_patterns = dirs;
            }
            if let Some(codes) = codes {
                cfg.ignored_error_codes = codes;
            }
            if let Some(mins) = reindex_minutes {
                cfg.background_reindex_interval_minutes = mins;
            }
            if let Some(secs) = reindex_idle_secs {
                cfg.background_reindex_idle_seconds = secs;
            }
        }
        // Bump the quiet-pass fingerprint generation: ignore globs or suppressed
        // codes may have changed, so the next background pass must re-run.
        self.state
            .settings_generation
            .fetch_add(1, Ordering::SeqCst);
        let (n_files, n_dirs, n_codes) = counts;
        tracing::info!(
            file_globs = ?n_files,
            dir_globs = ?n_dirs,
            ignored_codes = ?n_codes,
            reindex_minutes = ?reindex_minutes,
            reindex_idle_secs = ?reindex_idle_secs,
            "config updated via didChangeConfiguration"
        );
        // Re-filter the open documents' diagnostics against the updated
        // suppression list without waiting for a reload. Gated on the initial
        // index being ready so we don't publish partial cross-file results
        // before the first scan finishes (that scan republishes anyway).
        if self.state.index_ready.load(Ordering::Relaxed) {
            self.revalidate_all_open_docs(crate::ValidateTrigger::ConfigChange)
                .await;
        }
    }

    pub(crate) async fn execute_command_impl(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<Value>> {
        match params.command.as_str() {
            "getFileTypes" => {
                if let Some(uri_val) = params.arguments.first() {
                    let uri = uri_val.as_str().unwrap_or("");
                    let types = self.determine_file_types(uri).await;
                    let arr: Vec<Value> = types.into_iter().map(Value::String).collect();
                    return Ok(Some(Value::Array(arr)));
                }
                Ok(Some(Value::Array(vec![])))
            }
            "exportProfilingLog" => Ok(Some(Value::String(
                cwtools_profiling::export_profiling_log(),
            ))),
            // Re-index the base-game install and re-write the vanilla cache,
            // even when a fresh-looking cache exists.
            "cacheVanilla" => {
                self.state.vanilla_merged.store(false, Ordering::SeqCst);
                *self.state.vanilla_index.lock() = None;
                *self.state.vanilla_loc_keys.lock() = None;
                self.ensure_vanilla_index(true, false).await;
                self.merge_pending_vanilla_index();
                self.rebuild_modifier_keys();
                // ensure_vanilla_index turns the loading bar on but, unlike a full
                // workspace scan, this command never reaches the code that turns it
                // off; do it here so the status bar doesn't spin forever.
                self.send_loading_bar(false, "").await;
                Ok(Some(Value::String("Vanilla cache rebuilt.".to_string())))
            }
            // Purge every on-disk cache (parse cache + vanilla caches), drop the
            // in-memory vanilla state, and re-scan the workspace from scratch.
            "clearAllCaches" => {
                let dir = self
                    .state
                    .config
                    .read()
                    .cache_dir
                    .clone()
                    .or_else(default_cache_dir);
                let mut failures: Vec<String> = Vec::new();
                if let Some(dir) = &dir {
                    let dir = dir.clone();
                    failures = tokio::task::block_in_place(|| {
                        let mut failures: Vec<String> = Vec::new();
                        let parse_cache = dir.join("parse-cache");
                        if let Err(e) = std::fs::remove_dir_all(&parse_cache)
                            && e.kind() != std::io::ErrorKind::NotFound
                        {
                            tracing::warn!(path = %parse_cache.display(), error = %e, "clearAllCaches: remove parse-cache failed");
                            failures.push(format!("{}: {}", parse_cache.display(), e));
                        }
                        if let Ok(entries) = std::fs::read_dir(&dir) {
                            for e in entries.flatten() {
                                let name = e.file_name();
                                if name.to_string_lossy().starts_with("vanilla-")
                                    && let Err(err) = std::fs::remove_file(e.path())
                                {
                                    tracing::warn!(path = %e.path().display(), error = %err, "clearAllCaches: remove vanilla cache failed");
                                    failures.push(format!("{}: {}", e.path().display(), err));
                                }
                            }
                        }
                        failures
                    });
                }
                self.state.vanilla_merged.store(false, Ordering::SeqCst);
                *self.state.vanilla_index.lock() = None;
                *self.state.vanilla_loc_keys.lock() = None;
                // validate_entire_workspace's CAS guard returns false when a scan
                // (e.g. the periodic background pass) is already running. That
                // scan started before this purge and may already be past its
                // vanilla-index phase, so it can't be trusted to rebuild what we
                // just dropped — retry until we win the CAS and actually
                // re-index, bounded so a perpetually-busy server reports honestly
                // instead of hanging forever.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
                let mut reindexed = self.validate_entire_workspace(false).await;
                while !reindexed && std::time::Instant::now() < deadline {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    reindexed = self.validate_entire_workspace(false).await;
                }
                let status = if reindexed {
                    "workspace re-indexed"
                } else {
                    "re-index still pending (another scan is running)"
                };
                let msg = if failures.is_empty() {
                    format!("Caches cleared; {status}.")
                } else {
                    format!(
                        "Caches cleared with {} error(s); {status}. Failed: {}",
                        failures.len(),
                        failures.join("; ")
                    )
                };
                Ok(Some(Value::String(msg)))
            }
            // Re-read the rules-config dir from disk, rebuild the ruleset, and
            // re-validate the whole workspace against it — no server restart.
            "reloadrulesconfig" => {
                let dir = self.state.config.read().rules_dir.clone();
                match dir {
                    Some(dir) => {
                        let loaded = self.load_rules_config(&dir).await;
                        self.validate_entire_workspace(false).await;
                        let msg = if loaded {
                            "Rules config reloaded; workspace re-validated.".to_string()
                        } else {
                            format!(
                                "No rules loaded from {}; workspace re-validated.",
                                dir.display()
                            )
                        };
                        Ok(Some(Value::String(msg)))
                    }
                    None => Ok(Some(Value::String(
                        "No rules directory configured; nothing to reload.".to_string(),
                    ))),
                }
            }
            // Generate localisation stubs for every missing `## required` loc key
            // and hand them back to the client to open for review (no files are
            // written server-side).
            "genlocall" => Ok(Some(Value::Array(self.generate_missing_loc()))),
            // User-triggered re-index (no cache purge, unlike clearAllCaches).
            // validate_entire_workspace's CAS guard returns false when a scan
            // (foreground or the periodic background pass) is already
            // running; surface that instead of silently no-oping.
            "reindexWorkspace" => {
                let ran = self.validate_entire_workspace(false).await;
                let msg = if ran {
                    "Workspace re-indexed."
                } else {
                    "Re-index already in progress."
                };
                Ok(Some(Value::String(msg.to_string())))
            }
            // `getGraphData(entityType, depth)` — the entity graph the webview
            // renders. See `graph.rs` for the wire format and the bounds.
            "getGraphData" => self.get_graph_data(&params.arguments).await,
            // An error, not a silent `Ok(None)`: the VS Code client renders a
            // null result as success, masking client/engine version drift.
            other => Err(tower_lsp::jsonrpc::Error::invalid_params(format!(
                "unknown command: {other}"
            ))),
        }
    }

    /// Aggregate every `## required` localisation key that no loc file provides
    /// (the same keys the CW100 check flags), grouped into one stub file per
    /// target language. Returned to the client as `[{language,
    /// filename_suggestion, content}]`; the client opens each as an untitled
    /// document for the user to review and save. Nothing is written here.
    pub(crate) fn generate_missing_loc(&self) -> Vec<Value> {
        // Snapshot the target languages first (config is read-clone-dropped, so
        // its guard is never held across the ruleset/info/loc locks below).
        let langs: Vec<cwtools_localization::Lang> = self
            .state
            .config
            .read()
            .loc_languages
            .clone()
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| vec![cwtools_localization::Lang::English]);
        // Live overlay of open `.yml` keys, so a key just typed isn't re-stubbed.
        let overlay = self.loc_overlay_keys();
        // Lock order: rules -> info_service -> loc_index.
        let rules = self.state.rules.read();
        let Some(ruleset) = rules.ruleset.as_ref() else {
            return Vec::new();
        };
        let info = self.state.info_service.read();
        let loc_guard = self.state.loc_index.read();
        // Before the loc index is built every key looks missing; bail so the
        // command never dumps the entire mod's key set as "missing".
        let Some(loc) = loc_guard.as_ref().filter(|l| !l.union().is_empty()) else {
            return Vec::new();
        };
        let exists = |key: &str| loc.exists_any(key) || overlay.contains(key);

        let mut missing: BTreeSet<String> = BTreeSet::new();
        for td in &ruleset.types {
            if td.localisation.is_empty() {
                continue;
            }
            for (_uri, inst) in info.type_index.instances(&td.name) {
                for locdef in &td.localisation {
                    // Only required, name-derived keys — mirrors check_missing_localisation.
                    if !locdef.required || locdef.optional || locdef.explicit_field.is_some() {
                        continue;
                    }
                    let expected = format!("{}{}{}", locdef.prefix, inst.name, locdef.suffix);
                    if !exists(&expected.to_ascii_lowercase()) {
                        missing.insert(expected);
                    }
                }
            }
        }
        if missing.is_empty() {
            return Vec::new();
        }
        langs
            .into_iter()
            .map(|lang| render_loc_stub(lang, &missing))
            .collect()
    }

    pub(crate) async fn determine_file_types(&self, uri: &str) -> Vec<String> {
        let ws_prefix = self.state.config.read().workspace_prefix.clone();
        let rules = self.state.rules.read();

        // Derive from the loaded ruleset when available: any TypeDefinition whose
        // path matches the logical path contributes its name to the result.
        if let Some(rs) = rules.ruleset.as_ref() {
            let logical_path = crate::paths::logical_path_from_uri(uri, &ws_prefix);
            let types: Vec<String> = rs
                .types
                .iter()
                .filter(|td| check_path_dir(&td.path_options, &logical_path))
                .map(|td| td.name.clone())
                .collect();
            if !types.is_empty() {
                return types;
            }
        }
        drop(rules);

        // Fallback when no ruleset is loaded.
        let path = uri.to_lowercase();
        let mut types = Vec::new();

        if path.contains("/events/") {
            types.push("event".to_string());
        }
        if path.contains("/common/") {
            types.push("script".to_string());
        }
        if path.contains("/common/scripted_effects") {
            types.push("scripted_effect".to_string());
        }
        if path.contains("/common/scripted_triggers") {
            types.push("scripted_trigger".to_string());
        }
        if path.ends_with(".txt") {
            types.push("txt".to_string());
        }

        types
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cwtools_localization::Lang;
    use serde_json::json;

    #[test]
    fn extract_ignored_error_codes_lowercases_and_drops_empties() {
        let opts = json!({ "ignoredErrorCodes": ["CW100", "cw246", "", 5] });
        let codes = extract_ignored_error_codes(&opts);
        assert_eq!(codes, vec!["cw100".to_string(), "cw246".to_string()]);
    }

    #[test]
    fn extract_ignored_error_codes_absent_is_empty() {
        assert!(extract_ignored_error_codes(&json!({})).is_empty());
    }

    #[test]
    fn extract_u64_setting_reads_valid_values() {
        let opts = json!({ "backgroundReindexIdleSeconds": 30 });
        assert_eq!(
            extract_u64_setting(&opts, "backgroundReindexIdleSeconds"),
            Some(30)
        );
        assert_eq!(
            extract_u64_setting(&json!({ "k": 0 }), "k"),
            Some(0),
            "0 is a valid value (disables), not an error"
        );
    }

    #[test]
    fn extract_u64_setting_absent_is_silently_none() {
        assert_eq!(extract_u64_setting(&json!({}), "k"), None);
    }

    #[test]
    fn extract_u64_setting_invalid_types_are_none() {
        // Present-but-wrong-type (string, float, negative, null) is ignored;
        // the warn side effect isn't asserted here, just the ignoring.
        for v in [json!("30"), json!(1.5), json!(-5), json!(null), json!([30])] {
            assert_eq!(extract_u64_setting(&json!({ "k": v }), "k"), None);
        }
    }

    #[test]
    fn render_loc_stub_uses_paradox_shape() {
        let mut missing = BTreeSet::new();
        missing.insert("my_focus".to_string());
        missing.insert("my_focus_desc".to_string());
        let stub = render_loc_stub(Lang::English, &missing);
        assert_eq!(stub["language"], "english");
        assert_eq!(stub["filename_suggestion"], "generated_l_english.yml");
        // Header line then one ` KEY:0 "TODO"` entry per key, keys sorted (BTreeSet).
        assert_eq!(
            stub["content"].as_str().unwrap(),
            "l_english:\n my_focus:0 \"TODO\"\n my_focus_desc:0 \"TODO\"\n"
        );
    }
}
