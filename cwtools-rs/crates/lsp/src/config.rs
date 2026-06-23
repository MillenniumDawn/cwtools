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

            // Load .cwt rules from rulesCache if provided
            if let Some(cache) = opts.get("rulesCache").and_then(|v| v.as_str()) {
                let cache_path = std::path::Path::new(cache);
                // Surface a missing rules dir explicitly. The client may hand us a
                // path that doesn't resolve here (e.g. a Windows `rules_folder`
                // that didn't normalise), which otherwise degrades silently to a
                // generic "no rules loaded" with an empty error list.
                if !cache_path.is_dir() {
                    self.client
                        .log_message(
                            MessageType::WARNING,
                            format!("`rulesCache` dir does not exist: {}", cache),
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
                                cache,
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
                            format!("No rules loaded from {}. Errors: {:?}", cache, parse_errors),
                        )
                        .await;
                }
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
            if !files.is_empty() || !dirs.is_empty() {
                let (n_files, n_dirs) = (files.len(), dirs.len());
                {
                    let mut cfg = self.state.config.write();
                    cfg.ignore_file_patterns = files;
                    cfg.ignore_dir_patterns = dirs;
                }
                self.client
                    .log_message(
                        MessageType::INFO,
                        format!(
                            "ignore patterns: {} files, {} dirs (engine defaults still apply)",
                            n_files, n_dirs,
                        ),
                    )
                    .await;
            }
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
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
                    resolve_provider: Some(false),
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
                    ],
                    work_done_progress_options: Default::default(),
                }),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
                // Position encoding: we do not negotiate position_encoding here,
                // so clients default to UTF-16 (LSP spec default). The parser
                // counts Unicode scalar values (chars), NOT UTF-16 code units.
                // On BMP-only files (no surrogate pairs) the counts agree; on
                // files containing emoji or non-BMP characters, column offsets
                // will be off by the number of astral code points on the line.
                // TODO: negotiate utf-8 or utf-32 once tower-lsp exposes
                // PositionEncodingKind in InitializeResult, so clients that
                // support utf-32 get exact columns for free.
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "cwtools-server".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    /// Re-read ignore globs when the extension's `cwtools.ignore.*` settings
    /// change. The shape mirrors what we accept in `initializationOptions`:
    /// the payload is the `cwtools` namespace object, with optional
    /// `ignoreFilePatterns` and `ignoreDirectories` arrays. The next
    /// full-workspace scan will pick up the new values; an in-flight scan
    /// finishes with the snapshot it took.
    pub(crate) async fn did_change_configuration_impl(&self, params: DidChangeConfigurationParams) {
        // The client may send either the whole `cwtools` section (when the
        // section is registered via `configurationSection`) or just the
        // changed slice. `extract_ignore_patterns` looks for the same two
        // keys at the top level — works in both cases.
        let (files, dirs) = extract_ignore_patterns(&params.settings);
        let (n_files, n_dirs) = (files.len(), dirs.len());
        {
            let mut cfg = self.state.config.write();
            cfg.ignore_file_patterns = files;
            cfg.ignore_dir_patterns = dirs;
        }
        tracing::info!(
            file_globs = n_files,
            dir_globs = n_dirs,
            "ignore patterns updated via didChangeConfiguration"
        );
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
                self.ensure_vanilla_index(true).await;
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
                if let Some(dir) = &dir {
                    let dir = dir.clone();
                    tokio::task::block_in_place(|| {
                        let _ = std::fs::remove_dir_all(dir.join("parse-cache"));
                        if let Ok(entries) = std::fs::read_dir(&dir) {
                            for e in entries.flatten() {
                                let name = e.file_name();
                                if name.to_string_lossy().starts_with("vanilla-") {
                                    let _ = std::fs::remove_file(e.path());
                                }
                            }
                        }
                    });
                }
                self.state.vanilla_merged.store(false, Ordering::SeqCst);
                *self.state.vanilla_index.lock() = None;
                *self.state.vanilla_loc_keys.lock() = None;
                self.validate_entire_workspace().await;
                Ok(Some(Value::String(
                    "Caches cleared; workspace re-indexed.".to_string(),
                )))
            }
            _ => Ok(None),
        }
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
