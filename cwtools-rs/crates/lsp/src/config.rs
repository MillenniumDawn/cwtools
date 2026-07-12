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
        self.state.info_service.write().set_var_effects(var_effects);
        drop(rules);
        self.bump_info_revision();
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

            // Persistent cache directory for the base-game index (so it isn't
            // re-parsed every startup). The client should pass its global
            // storage path; we fall back to an OS cache dir otherwise.
            if let Some(cd) = opts.get("cacheDir").and_then(|v| v.as_str()) {
                self.state.config.write().cache_dir = Some(std::path::PathBuf::from(cd));
            }

            // Minutes between quiet background re-index passes (0 disables).
            // A live change comes through `did_change_configuration_impl`.
            if let Some(mins) = opts
                .get("backgroundReindexIntervalMinutes")
                .and_then(|v| v.as_u64())
            {
                self.state
                    .config
                    .write()
                    .background_reindex_interval_minutes = mins;
            }

            // Seconds of user inactivity a background pass waits for (default
            // 15). A live change comes through `did_change_configuration_impl`
            // and applies on the next reindex cycle.
            if let Some(secs) = opts
                .get("backgroundReindexIdleSeconds")
                .and_then(|v| v.as_u64())
            {
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
        if let Some(folders) = &params.workspace_folders
            && let Some(first) = folders.first()
        {
            self.state.config.write().workspace_uri = Some(first.uri.to_string().into());
        } else if let Some(root_uri) = &params.root_uri {
            self.state.config.write().workspace_uri = Some(root_uri.to_string().into());
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
                    trigger_characters: Some(vec!["=".to_string(), "<".to_string()]),
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
                    ],
                    work_done_progress_options: Default::default(),
                }),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
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

        // Rules-config parse/read errors mean the .cwt rules are broken,
        // which silently degrades every downstream check. Emit at ERROR so
        // the client reveals its output channel (it auto-reveals on Error),
        // surface a one-line popup so it's noticed even when the panel is
        // closed, and publish a diagnostic on each offending .cwt file so
        // the Problems panel points at the exact line.
        let mut diags_by_file: std::collections::HashMap<String, Vec<Diagnostic>> =
            std::collections::HashMap::new();
        for err in &parse_errors {
            self.client
                .log_message(MessageType::ERROR, err.to_string())
                .await;
            // Shared with the live per-file CWT lint (#43). No file text
            // here to widen the squiggle, so pass empty line-ends.
            diags_by_file
                .entry(crate::paths::path_to_uri(&err.file))
                .or_default()
                .push(crate::validate::rule_parse_error_to_diagnostic(err, &[]));
        }
        for (uri, diags) in diags_by_file {
            if let Ok(url) = uri.parse() {
                self.client.publish_diagnostics(url, diags, None).await;
            }
        }
        if !parse_errors.is_empty() {
            self.client
                .show_message(
                    MessageType::ERROR,
                    format!(
                        "CWTools: {} rules-config error(s). See Output → CWTools for details.",
                        parse_errors.len()
                    ),
                )
                .await;
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

    /// Re-read ignore globs and the background-reindex interval/idle window
    /// when the extension's `cwtools.*` settings change. The shape mirrors
    /// what we accept in `initializationOptions`: the payload is the
    /// `cwtools` namespace object, with optional `ignoreFilePatterns`,
    /// `ignoreDirectories`, `backgroundReindexIntervalMinutes`, and
    /// `backgroundReindexIdleSeconds`. The next full-workspace scan (or
    /// reindex cycle) picks up the new values; an in-flight scan finishes
    /// with the snapshot it took.
    pub(crate) async fn did_change_configuration_impl(&self, params: DidChangeConfigurationParams) {
        // The client may send either the whole `cwtools` section (when the
        // section is registered via `configurationSection`) or just the
        // changed slice. `extract_ignore_patterns` looks for the same two
        // keys at the top level — works in both cases.
        let (files, dirs) = extract_ignore_patterns(&params.settings);
        let codes = extract_ignored_error_codes(&params.settings);
        let (n_files, n_dirs, n_codes) = (files.len(), dirs.len(), codes.len());
        let reindex_minutes = params
            .settings
            .get("backgroundReindexIntervalMinutes")
            .and_then(|v| v.as_u64());
        let reindex_idle_secs = params
            .settings
            .get("backgroundReindexIdleSeconds")
            .and_then(|v| v.as_u64());

        // No-op guard: the client re-sends the whole `cwtools` section on any
        // change to an unrelated key, so an identical payload arrives often.
        // Skip the write and the open-doc revalidate storm (#90) when nothing
        // this handler mutates actually changed. `reindex_minutes` is `None`
        // when the key is absent, and a missing key is never a change.
        {
            let cfg = self.state.config.read();
            let unchanged = cfg.ignore_file_patterns == files
                && cfg.ignore_dir_patterns == dirs
                && cfg.ignored_error_codes == codes
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
            cfg.ignore_file_patterns = files;
            cfg.ignore_dir_patterns = dirs;
            cfg.ignored_error_codes = codes;
            if let Some(mins) = reindex_minutes {
                cfg.background_reindex_interval_minutes = mins;
            }
            if let Some(secs) = reindex_idle_secs {
                cfg.background_reindex_idle_seconds = secs;
            }
        }
        tracing::info!(
            file_globs = n_files,
            dir_globs = n_dirs,
            ignored_codes = n_codes,
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
        let ws_uri = self.state.config.read().workspace_uri.clone();
        let rules = self.state.rules.read();

        // Derive from the loaded ruleset when available: any TypeDefinition whose
        // path matches the logical path contributes its name to the result.
        if let Some(rs) = rules.ruleset.as_ref() {
            let logical_path = crate::paths::logical_path_from_uri(uri, &ws_uri);
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
