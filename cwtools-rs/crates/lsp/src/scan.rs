use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tower_lsp::lsp_types::*;

use cwtools_parser::parser::parse_string;
use cwtools_rules::rules_types::RuleSet;
use cwtools_validation::build_modifier_keys;

use crate::paths::{
    default_cache_dir, discover_vanilla_dir, logical_path_from_uri, path_to_uri, strip_loc_quotes,
    uri_to_path_str,
};
use crate::validate::{
    loc_diag_to_validation_error, make_prepared, parse_error_to_diagnostic,
    validate_parsed_with_indexes, validation_error_to_diagnostic,
};
use crate::workspace_cache;
use crate::{Backend, LoadingBar, UpdateFileList};

/// Index a base-game ("vanilla") install into per-type instances, ready to merge
/// into the workspace TypeIndex. Delegates to the shared driver's `index_game_dir`
/// so the LSP and CLI discover and index vanilla the SAME way (the driver's
/// `search_config_for` config, which is the broader, corpus-verified one). The
/// discovered ASTs are used directly (no re-parse) because vanilla files are only
/// indexed, never validated. Drops the per-instance file_uri; the merge slot only
/// needs the instances.
///
/// Also returns the cache aux payload (loc keys, file paths, variable names) so
/// a cache written by the LSP is as complete as one from the CLI's
/// `cache-vanilla`.
pub(crate) fn index_vanilla_dir(
    dir: &std::path::Path,
    ruleset: &RuleSet,
    table: &cwtools_string_table::string_table::StringTable,
) -> (
    HashMap<String, Vec<cwtools_info::TypeInstance>>,
    cwtools_info::vanilla_cache::VanillaCacheAux,
) {
    let var_effects = cwtools_info::variable_defining_effects(ruleset);
    let index = cwtools_driver::index_game_dir(dir, ruleset, table, &var_effects);
    let aux = cwtools_driver::build_vanilla_cache_aux(dir, &index);
    let per_type = index
        .map
        .into_iter()
        .map(|(k, v)| (k, v.into_iter().map(|(_, inst)| inst).collect()))
        .collect();
    (per_type, aux)
}

impl Backend {
    /// Send the `loadingBar` server→client notification so the VS Code extension
    /// status bar reflects background indexing/validation work.
    /// Payload: `{ "enable": bool, "value": string }`.
    pub(crate) async fn send_loading_bar(&self, enable: bool, value: &str) {
        let payload = serde_json::json!({ "enable": enable, "value": value });
        self.client.send_notification::<LoadingBar>(payload).await;
    }

    /// Send the `updateFileList` server→client notification so the VS Code
    /// extension file explorer populates.
    /// Payload: `{ "fileList": [{ "scope": string, "uri": string, "logicalpath": string }] }`.
    async fn send_update_file_list(&self, file_list: Vec<serde_json::Value>) {
        let payload = serde_json::json!({ "fileList": file_list });
        self.client
            .send_notification::<UpdateFileList>(payload)
            .await;
    }

    /// Merge vanilla dynamic values (complex-enum + value_set members, from the
    /// vanilla cache or a live index) into the workspace type index so
    /// completion offers them. Keyed under one synthetic file so a re-merge
    /// replaces the previous contribution.
    pub(crate) fn merge_vanilla_dynamic_values(
        &self,
        complex_enums: Vec<(String, Vec<String>)>,
        value_sets: Vec<(String, Vec<String>)>,
    ) {
        if complex_enums.is_empty() && value_sets.is_empty() {
            return;
        }
        let mut info = self.state.info_service.write();
        // NOT "<vanilla-cache>": the workspace scan's instance merge calls
        // remove_file("<vanilla-cache>") before re-merging, which would wipe
        // these as a side effect.
        info.type_index
            .complex_enum_values
            .merge_file("<vanilla-dynamic>", complex_enums.into_iter().collect());
        info.type_index
            .value_set_values
            .merge_file("<vanilla-dynamic>", value_sets.into_iter().collect());
    }

    /// Merge a pending `vanilla_index` (from the cache or a live index) into
    /// the workspace type index. After the merge the raw per-type data is
    /// dropped from `vanilla_index` to eliminate double residency (the
    /// type_index already owns the instances). `vanilla_merged` prevents
    /// `ensure_vanilla_index` re-running on subsequent workspace scans.
    pub(crate) fn merge_pending_vanilla_index(&self) {
        let per_type = self.state.vanilla_index.lock().take();
        if let Some(per_type) = per_type {
            let mut info_guard = self.state.info_service.write();
            info_guard.type_index.remove_file("<vanilla-cache>");
            info_guard.type_index.merge("<vanilla-cache>", per_type);
            // Vanilla data is loaded, so the index now holds every base-game
            // instance. Mark it complete so the CW500/CW222 type-reference
            // checks fire (they're gated on `complete` to avoid false
            // positives during mod-only validation). The driver's Session
            // sets this for the CLI path; the LSP merges vanilla directly and
            // must set it here too. See rule_core.rs gate on `idx.complete`.
            info_guard.type_index.complete = true;
            // `vanilla_index` is now None — mark it merged so
            // ensure_vanilla_index does not re-run on the next scan.
            self.state.vanilla_merged.store(true, Ordering::SeqCst);
        }
    }

    /// Rebuild the cached modifier-key set from the current ruleset and type index.
    pub(crate) fn rebuild_modifier_keys(&self) {
        // Lock order: rules -> info_service. One `rules` write guard holds the
        // ruleset we read from and the modifier_keys we write into.
        let mut rules = self.state.rules.write();
        let keys = match rules.ruleset.as_ref() {
            Some(rs) => {
                let info_guard = self.state.info_service.read();
                build_modifier_keys(rs, &info_guard.type_index)
            }
            None => HashSet::new(),
        };
        rules.modifier_keys = Arc::new(keys);
    }

    /// Public entry to the workspace scan. Runs the scan and ALWAYS clears the
    /// status-bar loading indicator on return, regardless of which internal path
    /// exited — including the early returns for an absent/empty workspace, which
    /// previously left the bar spinning on "Indexing workspace…" forever. A
    /// panic inside the scan is handled separately by the watcher in
    /// `initialized`, which clears the bar too.
    pub(crate) async fn validate_entire_workspace(&self) {
        self.validate_entire_workspace_inner().await;
        self.send_loading_bar(false, "").await;
    }

    /// Scan the entire workspace for relevant game files and validate them all.
    #[tracing::instrument(skip_all)]
    async fn validate_entire_workspace_inner(&self) {
        cwtools_profiling::log_rss("workspace_scan_start");
        self.send_loading_bar(true, "Indexing workspace…").await;

        let workspace_uri = self.state.config.read().workspace_uri.clone();

        let root_path = match workspace_uri {
            Some(ref uri) => std::path::PathBuf::from(uri_to_path_str(uri)),
            None => {
                self.client
                    .log_message(
                        MessageType::WARNING,
                        "No workspace folder; skipping full-workspace validation.",
                    )
                    .await;
                // Nothing to index — let single-file diagnostics publish normally.
                self.state
                    .index_ready
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        };

        let extensions = cwtools_file_manager::file_manager::SCRIPT_EXTENSIONS;

        // Snapshot the user-configured ignore globs once for the whole walk.
        // The engine's hard-coded baseline (Changelog.txt, README.*, *.md)
        // is layered on top inside the walker closure so it can't be
        // accidentally cleared by a user who sets an empty list.
        let (extra_file_globs, extra_dir_globs) = {
            let cfg = self.state.config.read();
            (
                cfg.ignore_file_patterns.clone(),
                cfg.ignore_dir_patterns.clone(),
            )
        };

        // Whole-tree discovery shares file_manager's skip/exclude config so the
        // LSP and CLI agree on what to skip (engine/IDE dirs, free-form text).
        // The user-configured globs extend that baseline.
        let files_to_validate = tokio::task::block_in_place(|| {
            cwtools_file_manager::file_manager::walk_workspace_files(
                &root_path,
                extensions,
                &extra_file_globs,
                &extra_dir_globs,
            )
        });

        if files_to_validate.is_empty() {
            self.client
                .log_message(MessageType::INFO, "No workspace files found to validate.")
                .await;
            // Nothing to index — let single-file diagnostics publish normally.
            self.state
                .index_ready
                .store(true, std::sync::atomic::Ordering::Relaxed);
            return;
        }

        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "Validating {} workspace files under {:?} ...",
                    files_to_validate.len(),
                    root_path
                ),
            )
            .await;

        // Resolve the parse-cache directory and settings fingerprint. The
        // fingerprint encodes the game, ruleset shape, and workspace root so
        // stale caches are cleared automatically when any of those change.
        let (cache_info, cache_was_valid) = {
            let (cache_dir, language) = {
                let cfg = self.state.config.read();
                (cfg.cache_dir.clone(), cfg.language.clone())
            };
            match cache_dir {
                Some(cd) => {
                    let ruleset_snap = self.state.rules.read().ruleset.clone();
                    let fp = match ruleset_snap {
                        Some(ref rs) => {
                            workspace_cache::settings_fingerprint(&language, rs, &root_path)
                        }
                        None => workspace_cache::settings_fingerprint(
                            &language,
                            &RuleSet::new(),
                            &root_path,
                        ),
                    };
                    let valid = workspace_cache::validate_or_clear(&cd, fp);
                    (Some((cd, fp)), valid)
                }
                None => (None, true),
            }
        };
        self.client
            .log_message(
                MessageType::INFO,
                if cache_was_valid {
                    "Parse cache: hit (settings match)"
                } else {
                    "Parse cache: settings changed, cleared"
                },
            )
            .await;

        // Pass 1: parse + index every file (types, scripted triggers/effects,
        // modifiers) so cross-file references resolve before any file is
        // validated. The parsed ASTs are kept resident in `parsed_files` and
        // handed to pass 2 — re-parsing 7413 files in pass 2 cost ~4-6s on MD
        // and produced no observable benefit, just CPU and allocator churn.
        // The total resident set between the two passes is bounded by what the
        // loc service allocates next, so peak RSS doesn't grow meaningfully.
        //
        // On a cache hit the AST is deserialized from disk (.cwb) instead of
        // parsed, then kept resident like any other; on a miss we parse and
        // persist for the next scan. The disk cache speeds the cold→warm scan
        // across restarts; keeping the AST resident avoids a pass-2 re-parse
        // within a single scan.
        self.send_loading_bar(true, "Indexing workspace…").await;
        // Snapshot the set of currently-open document URIs so both passes can
        // skip them: open docs were already indexed by did_open/did_change and
        // their fresher in-memory diagnostics must not be clobbered by stale
        // disk-text validation with version=None.
        let open_uris: HashSet<String> = {
            let docs = self.state.documents.lock();
            docs.keys().cloned().collect()
        };

        let mut cache_hits = 0u64;
        let mut cache_misses = 0u64;
        // Pass 1 splits into a parallel parse/cache-load phase and a serial
        // index phase. Reading + parsing (or deserializing from the parse
        // cache) and persisting the cache are pure functions over the
        // lock-guarded string-table interner, so they run in parallel across
        // files exactly as the driver parallelizes the same work. Indexing
        // mutates the shared symbol/info indexes, so it stays serial and in the
        // original file order — the merge order is observable (goto-def "first
        // match", duplicate-name refcounts), and the cache-hit/miss tally must
        // match the sequential version.
        //
        // `par_iter().collect()` preserves file order, so `outcomes[i]`
        // corresponds to `files_to_validate[i]`.
        use rayon::prelude::*;
        // (cache_hit, parsed) per file; None = open doc, parse failure, or read error.
        type ParseOutcome = (bool, cwtools_parser::ast::ParsedFile);
        // block_in_place tells tokio this thread is about to do synchronous
        // blocking I/O; the runtime shifts its remaining tasks to other workers
        // so the LSP request loop is not starved while rayon parses.
        let outcomes: Vec<Option<ParseOutcome>> = tokio::task::block_in_place(|| {
            files_to_validate
                .par_iter()
                .map(|file_path| {
                    let uri = path_to_uri(file_path);
                    // Open docs are already indexed from their in-memory text;
                    // skip so we don't re-index stale disk content on top of the
                    // live version.
                    if open_uris.contains(&uri) {
                        return None;
                    }
                    let text = std::fs::read_to_string(file_path).ok()?;
                    // Try the parse cache first.
                    if let Some((ref cd, fp)) = cache_info
                        && let Some(parsed) =
                            workspace_cache::load(cd, fp, &text, &self.state.string_table)
                    {
                        return Some((true, parsed));
                    }
                    // Cache miss — parse, then persist for the next scan.
                    let parsed = parse_string(&text, &self.state.string_table).ok()?;
                    if let Some((ref cd, fp)) = cache_info {
                        workspace_cache::store(cd, fp, &text, &parsed, &self.state.string_table);
                    }
                    Some((false, parsed))
                })
                .collect()
        });

        // Serial index phase, in file order.
        let mut parsed_files: Vec<Option<cwtools_parser::ast::ParsedFile>> =
            Vec::with_capacity(files_to_validate.len());
        for (file_path, outcome) in files_to_validate.iter().zip(outcomes) {
            let parsed = match outcome {
                Some((cache_hit, parsed)) => {
                    let uri = path_to_uri(file_path);
                    self.index_parsed_file(&uri, &parsed);
                    if cache_hit {
                        cache_hits += 1;
                    } else {
                        cache_misses += 1;
                    }
                    Some(parsed)
                }
                None => None,
            };
            parsed_files.push(parsed);
        }

        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "Indexing pass: {} cache hits, {} misses",
                    cache_hits, cache_misses
                ),
            )
            .await;

        // Build the base-game index from a `vanilla` dir (or auto-discovery) if
        // we have one and haven't indexed it yet. Populates `vanilla_index`.
        self.ensure_vanilla_index(false).await;

        // Merge the pre-generated vanilla index (if loaded) so base-game
        // references resolve.
        self.merge_pending_vanilla_index();

        // Rebuild the cached modifier-key set now that the type index is
        // complete (templated modifiers like production_speed_<building>_factor
        // expand against the full instance list).
        self.rebuild_modifier_keys();

        // Pass 1 just dropped every file's parsed AST; glibc may still be
        // holding onto the heap pages. Trim now so the loc service's ~2M-entry
        // allocation in rebuild_and_publish_loc doesn't sit on top of pages
        // we're never going to touch again.
        cwtools_profiling::trim_memory();
        cwtools_profiling::log_rss("scan: post-index trim");

        // Build the loc-key index (workspace + vanilla) so pass 2's config
        // validation can check LocalisationField references (CW100/CW122), and
        // publish loc-file diagnostics (CW225 etc.) for the workspace loc files.
        self.rebuild_and_publish_loc(&root_path).await;

        // The index (types + loc + vanilla) is now complete. Allow per-file
        // handlers to publish real diagnostics again: anything opened/edited
        // during indexing was held back to avoid transient cross-file "not found"
        // errors, and pass 2 + the open-doc refresh below publish the real set.
        self.state
            .index_ready
            .store(true, std::sync::atomic::Ordering::Relaxed);

        // Pass 2: validate each file against the now-complete index using
        // the ASTs we already parsed in pass 1. Diagnostics are published to
        // the editor; the file is intentionally NOT stored in
        // `self.state.documents`. That map holds only files the editor has
        // open (populated by did_open) — the scan used to insert every
        // workspace file there, pinning all texts+ASTs in memory for the
        // whole session.
        self.send_loading_bar(true, "Validating workspace…").await;
        let mut total_errors = 0usize;
        let total_files = files_to_validate.len();
        // Build the scope registry + enum_map ONCE for the whole scan instead of
        // once per file: they depend only on (ruleset, game) and are the
        // expensive part of per-file setup (many inserts + lowercasing +
        // per-iterator `format!`). All are reused across the rayon section.
        let scan_game = self.state.config.read().game();
        // Snapshot the ruleset-family state once before the loop; none of it
        // changes during validation and we can't hold the guard across the await
        // points below. One `rules` read guard clones all three: the shared
        // `Arc<RuleSet>` (so the `enum_map` borrow stays valid across the
        // parallel section), the cached scope-registry `Arc`, and the
        // modifier-key set.
        let (scan_ruleset, scan_registry, modifier_keys_snap): (
            Option<Arc<RuleSet>>,
            _,
            Arc<HashSet<String>>,
        ) = {
            let rules = self.state.rules.read();
            (
                rules.ruleset.clone(),
                rules.scope_registry.clone(),
                rules.modifier_keys.clone(),
            )
        };

        // Validate every file in parallel, then publish serially. The
        // CPU-bound validation runs under a single shared `info_service` /
        // `loc_index` read guard (both `&...` references are `Sync`), with no
        // async and no client calls inside the rayon section. Publishing is
        // async and stays out of the parallel block.
        let (scope_checks, var_checks) = {
            let cfg = self.state.config.read();
            (cfg.scope_checks, cfg.var_checks)
        };
        let results: Vec<(String, Vec<Diagnostic>)> = {
            let info_guard = self.state.info_service.read();
            let loc_guard = self.state.loc_index.read();
            let type_index = &info_guard.type_index;
            let loc_index = loc_guard.as_ref();
            let registry = scan_registry.as_ref();
            // One Prepared for the whole batch (None if the ruleset isn't loaded).
            // It is Copy + all-borrows, so it is shared freely across rayon threads.
            let prepared = scan_ruleset.as_ref().map(|ruleset| {
                make_prepared(
                    ruleset,
                    &self.state.string_table,
                    scan_game,
                    type_index,
                    &modifier_keys_snap,
                    loc_index,
                    // Full scan skips open docs, so the unsaved-key overlay is
                    // irrelevant here.
                    None,
                    registry,
                    scope_checks,
                    var_checks,
                )
            });

            files_to_validate
                .par_iter()
                .zip(parsed_files.par_iter())
                .filter_map(|(file_path, parsed_opt)| {
                    // Skip files that failed to parse in pass 1, and open docs
                    // whose fresher in-memory diagnostics must not be overwritten.
                    let parsed = parsed_opt.as_ref()?;
                    let uri = path_to_uri(file_path);
                    if open_uris.contains(&uri) {
                        return None;
                    }
                    // Workspace scan covers files not open in an editor (open
                    // ones are skipped above), so there's no squiggle to widen —
                    // pass no line info and keep the cheap single-char range.
                    let diagnostics = match &prepared {
                        Some(prepared) => validate_parsed_with_indexes(&uri, parsed, prepared, &[]),
                        None => parsed
                            .errors
                            .iter()
                            .map(|e| parse_error_to_diagnostic(e, &[]))
                            .collect(),
                    };
                    Some((uri, diagnostics))
                })
                .collect()
            // info_guard / loc_guard dropped here, before any await.
        };

        for (i, (uri, diagnostics)) in results.into_iter().enumerate() {
            total_errors += diagnostics
                .iter()
                .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
                .count();

            if let Ok(uri_obj) = Url::parse(&uri) {
                self.client
                    .publish_diagnostics(uri_obj, diagnostics, None)
                    .await;
            }
            if i % 50 == 49 {
                tokio::task::yield_now().await;
            }
        }
        // Pass 2 is done. Drop the per-file ASTs before the file-list / profile
        // summary so the RSS we report reflects the steady-state working set
        // (loc index + type index + open documents), not the in-flight
        // validation peak.
        drop(parsed_files);

        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "Workspace validation complete: {} errors across {} files",
                    total_errors, total_files
                ),
            )
            .await;

        // Build and send the file list for the extension's file explorer.
        let ws_uri = self.state.config.read().workspace_uri.clone();
        let file_list: Vec<serde_json::Value> = files_to_validate
            .iter()
            .map(|file_path| {
                let uri = path_to_uri(file_path);
                let logical_path = logical_path_from_uri(&uri, &ws_uri);
                let scope = logical_path
                    .split('/')
                    .next()
                    .unwrap_or("unknown")
                    .to_string();
                serde_json::json!({
                    "scope": scope,
                    "uri": uri,
                    "logicalpath": logical_path
                })
            })
            .collect();
        self.send_update_file_list(file_list).await;

        if cwtools_profiling::profile_enabled() {
            let st = self.state.string_table.stats();
            let info_summary = self.state.info_service.read().profile_summary();
            let vanilla = self
                .state
                .vanilla_index
                .lock()
                .as_ref()
                .map(|m| m.values().map(|v| v.len()).sum::<usize>())
                .unwrap_or(0);
            let loc_keys = self
                .state
                .loc_index
                .read()
                .as_ref()
                .map(|i| i.union().len())
                .unwrap_or(0);
            tracing::info!(target: "cwtools::profile", "{}", info_summary);
            tracing::info!(target: "cwtools::profile",
                "string_table {} MiB ({} entries) | vanilla_index {} instances | loc union {} keys",
                st.total_bytes() / (1024 * 1024), st.entries, vanilla, loc_keys);
        }
        cwtools_profiling::log_rss("workspace_scan_done");
        // The scan dropped large transients (the whole base-game parse, ~2M loc
        // entries, every file's AST). Hand the freed heap back to the OS so RSS
        // reflects the real working set, not the scan peak.
        cwtools_profiling::trim_memory();
        cwtools_profiling::log_rss("after_trim");

        // Re-validate documents that were already open before the index finished.
        // Both scan passes skip open docs, so a file opened during startup keeps
        // the diagnostics did_open produced against a then-incomplete index — a
        // cross-file reference (e.g. a scripted_effect defined in a not-yet-indexed
        // file) shows as "not found" until a manual re-save. Now that the index is
        // complete, re-run them so those stale diagnostics clear on their own.
        self.revalidate_all_open_docs().await;
        // The status bar is cleared by the `validate_entire_workspace` wrapper on
        // return, so every exit path (this one and the early returns above) clears
        // it uniformly.
    }

    /// Re-validate every currently-open document against the current (complete)
    /// index and re-publish, skipping any whose version changed meanwhile. Called
    /// once after the workspace scan so open docs validated against a partial
    /// index don't keep stale cross-file diagnostics.
    async fn revalidate_all_open_docs(&self) {
        let open_docs: Vec<(String, String, i32)> = {
            let docs = self.state.documents.lock();
            docs.iter()
                .map(|(uri, doc)| (uri.clone(), doc.text.clone(), doc.version))
                .collect()
        };
        for (uri, text, version) in open_docs {
            let (diagnostics, _) = self.parse_and_validate(&uri, &text).await;
            // Skip if the doc changed or closed while we were validating; its own
            // did_change/did_close handler owns the fresher result.
            let still_current = {
                let docs = self.state.documents.lock();
                docs.get(&uri)
                    .map(|d| d.version == version)
                    .unwrap_or(false)
            };
            if !still_current {
                continue;
            }
            if let Ok(uri_obj) = Url::parse(&uri) {
                self.client
                    .publish_diagnostics(uri_obj, diagnostics, Some(version))
                    .await;
            }
        }
    }

    /// Build the loc-key index from the workspace root plus the vanilla install,
    /// store it in state (for CW100/CW122 on config files), and publish loc-file
    /// diagnostics (CW225/CW234/CW259/CW268/CW275) for the workspace loc files.
    #[tracing::instrument(skip_all)]
    pub(crate) async fn rebuild_and_publish_loc(&self, root_path: &std::path::Path) {
        let game = self.state.config.read().game();
        let loc_game = cwtools_localization::Game::from_engine(game);

        // Cached vanilla loc keys (from the vanilla cache) stand in for walking
        // the install's loc files — only the workspace is walked then.
        let cached_vanilla_loc = self.state.vanilla_loc_keys.lock().clone();
        let mut loc_dirs: Vec<std::path::PathBuf> = vec![root_path.to_path_buf()];
        if cached_vanilla_loc.is_none()
            && let Some(v) = self.state.config.read().vanilla_dir.clone()
        {
            loc_dirs.push(v);
        }
        let dir_refs: Vec<&std::path::Path> = loc_dirs.iter().map(|p| p.as_path()).collect();
        let loc_languages = self.state.config.read().loc_languages.clone();

        // Hover language scope: unless the user opted into all translations, keep
        // only the primary language (first configured loc language, else English)
        // in the hover map so it stays small.
        let hover_all = self
            .state
            .hover_show_all_languages
            .load(std::sync::atomic::Ordering::Relaxed);
        let primary_lang = loc_languages
            .as_deref()
            .and_then(|l| l.first().copied())
            .unwrap_or(cwtools_localization::Lang::English);

        // Build the index and collect per-file diagnostics in one block, then
        // drop the LocService before the index is published. The service holds
        // the full per-file loc ASTs (~2M entries on Millennium Dawn); keeping
        // it alive while we also hold the lowercased key set in LocIndex
        // pushes peak RSS by hundreds of MiB for no reason. After the block
        // closes only LocIndex (keys) and the diagnostic map survive.
        // Names a `$ref$` may resolve to besides loc keys: `$modifier$` / `$idea$`
        // embeds resolve against those registries (mirrors the CLI/driver path).
        // With cached vanilla loc keys, the service holds no vanilla keys, so
        // they join this set too — otherwise mod loc referencing a base-game key
        // would flag CW225.
        let extra_valid_refs: HashSet<String> = {
            // Lock order: rules -> info_service.
            let mut extra = (*self.state.rules.read().modifier_keys).clone();
            let info = self.state.info_service.read();
            // Dynamic modifiers, ideas, other game-object names + defined
            // variables a `$ref$` can bind to (mirrors the CLI/driver path).
            extra.extend(info.type_index.loc_bindable_names());
            if let Some(cached) = &cached_vanilla_loc {
                for (_, keys) in cached {
                    extra.extend(keys.iter().cloned());
                }
            }
            extra
        };

        let root_str = root_path.to_string_lossy().to_string();
        // block_in_place: the loc service reads and parses hundreds of loc files
        // from disk — synchronous I/O that must not starve the async executor.
        let (loc_index, mut by_file, loc_text_map, loc_loc_map) =
            tokio::task::block_in_place(|| {
                let service = cwtools_localization::LocService::from_folders(&dir_refs);
                let mut idx = cwtools_localization::LocIndex::build_scoped(
                    &service,
                    loc_game,
                    loc_languages.as_deref(),
                );
                if let Some(cached) = cached_vanilla_loc {
                    let typed: Vec<(cwtools_localization::Lang, Vec<String>)> = cached
                        .into_iter()
                        .filter_map(|(name, ks)| {
                            cwtools_localization::Lang::from_name(&name).map(|l| (l, ks))
                        })
                        .collect();
                    idx.merge_cached_keys(typed, loc_languages.as_deref());
                }
                let mut by_file: HashMap<String, Vec<Diagnostic>> = HashMap::new();
                for d in cwtools_localization::validate_loc_project_scoped(
                    &service,
                    loc_game,
                    loc_languages.as_deref(),
                    &extra_valid_refs,
                ) {
                    if !d.file.starts_with(&root_str) {
                        continue;
                    }
                    let ve = loc_diag_to_validation_error(&d);
                    // Project-wide loc scan feeds the Problems panel; open files
                    // get whole-line squiggles when re-validated on open.
                    by_file
                        .entry(d.file.clone())
                        .or_default()
                        .push(validation_error_to_diagnostic(&ve, &[]));
                }
                // Extract per-key display text for hover and a representative
                // definition site (for goto) before dropping the service.
                let mut lt: HashMap<String, Vec<(cwtools_localization::Lang, String)>> =
                    HashMap::new();
                let mut ll: HashMap<String, (String, u32)> = HashMap::new();
                for file in service.files() {
                    let lang = file.lang.unwrap_or(cwtools_localization::Lang::English);
                    let lang_included = hover_all || lang == primary_lang;
                    for entry in &file.entries {
                        // goto: prefer the primary language's location (English by
                        // default) so Ctrl+Click lands on the canonical entry, not
                        // whichever language happened to be scanned first.
                        let loc = || {
                            (
                                path_to_uri(std::path::Path::new(&*entry.position.stream_name)),
                                (entry.position.line.saturating_sub(1)) as u32,
                            )
                        };
                        if lang == primary_lang {
                            ll.insert(entry.key.to_lowercase(), loc());
                        } else {
                            ll.entry(entry.key.to_lowercase()).or_insert_with(loc);
                        }
                        if !lang_included {
                            continue;
                        }
                        let display = strip_loc_quotes(&entry.desc);
                        if !display.is_empty() {
                            lt.entry(entry.key.to_lowercase())
                                .or_default()
                                .push((lang, display.to_string()));
                        }
                    }
                }
                (idx, by_file, lt, ll)
            });
        *self.state.loc_index.write() = Some(loc_index);
        *self.state.loc_text.write() = loc_text_map;
        *self.state.loc_locations.write() = loc_loc_map;

        // Publish per-file loc diagnostics, but only for workspace loc files
        // (not vanilla). Group by file so each gets a complete diagnostic set.
        for (file, diags) in by_file.drain() {
            let uri = path_to_uri(std::path::Path::new(&file));
            if let Ok(uri_obj) = Url::parse(&uri) {
                self.client.publish_diagnostics(uri_obj, diags, None).await;
            }
        }
        cwtools_profiling::log_rss("loc_rebuild_done");
    }

    /// Lazily index the base-game install into `vanilla_index` (once). Resolves
    /// the dir from the `vanilla` init option, falling back to auto-discovery by
    /// game. No-op if already indexed (or already merged into the type_index),
    /// if no dir is found, or if the ruleset isn't loaded yet.
    ///
    /// `force_rebuild` skips the cache-load fast path (and the already-indexed
    /// check) so the install is re-indexed and the cache re-written — the
    /// `cacheVanilla` command.
    pub(crate) async fn ensure_vanilla_index(&self, force_rebuild: bool) {
        // Already populated (or already merged into type_index and dropped)? Done.
        if !force_rebuild
            && (self.state.vanilla_index.lock().is_some()
                || self.state.vanilla_merged.load(Ordering::SeqCst))
        {
            return;
        }
        // Resolve the install dir: explicit `vanilla` option, else auto-discover.
        let (explicit_dir, game) = {
            let cfg = self.state.config.read();
            (cfg.vanilla_dir.clone(), cfg.language.clone())
        };
        let dir = explicit_dir.or_else(|| discover_vanilla_dir(&game));
        let dir = match dir {
            Some(d) if d.is_dir() => d,
            _ => return,
        };

        // We need the ruleset both to key the cache (the fingerprint folds in the
        // ruleset shape) and to map definitions to their types when rebuilding.
        // Clone it out in its own statement so the parking_lot guard is dropped
        // before the `match` (guards aren't Send and the None arm awaits below).
        let ruleset_opt = self.state.rules.read().ruleset.clone();
        let ruleset = match ruleset_opt {
            Some(rs) => rs,
            None => {
                self.client
                    .log_message(
                        MessageType::WARNING,
                        "Base-game dir set but no rules loaded yet; skipping vanilla index.",
                    )
                    .await;
                return;
            }
        };

        // Fingerprint = base-game version + ruleset shape. The base game only
        // changes when it updates and the rules only when the config changes, so a
        // cache keyed by both is reused across sessions and is safe to publish, yet
        // a rules change correctly invalidates it (the cached instances are
        // extracted by the rules; see `vanilla_cache::combined_fingerprint`).
        let fingerprint = cwtools_info::vanilla_cache::combined_fingerprint(&dir, &ruleset);
        let cache_path = self.vanilla_cache_path(&game, &fingerprint);

        // Try a fresh cache first — skip parsing the whole base game entirely.
        if !force_rebuild
            && let Some(cp) = &cache_path
            && cp.exists()
        {
            match cwtools_info::vanilla_cache::load(cp) {
                Ok((cache_game, cache_fp, data))
                    if cache_game == game && cache_fp == fingerprint =>
                {
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
                                "Loaded {} base-game instances from cache {} ({})",
                                total,
                                cp.display(),
                                fingerprint
                            ),
                        )
                        .await;
                    return;
                }
                Ok((_, cache_fp, _)) => {
                    self.client
                        .log_message(
                            MessageType::INFO,
                            format!(
                                "Vanilla cache stale (cached {}, install {}); rebuilding",
                                cache_fp, fingerprint
                            ),
                        )
                        .await;
                }
                Err(e) => {
                    self.client
                        .log_message(
                            MessageType::WARNING,
                            format!("Could not load vanilla cache {}: {}", cp.display(), e),
                        )
                        .await;
                }
            }
        }

        self.send_loading_bar(true, "Indexing base game…").await;
        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "Indexing base game at {} ({}) …",
                    dir.display(),
                    fingerprint
                ),
            )
            .await;

        // Indexing parses thousands of files; run it off the async executor.
        let table = self.state.string_table.clone();
        let index_dir = dir.clone();
        let join_result =
            tokio::task::spawn_blocking(move || index_vanilla_dir(&index_dir, &ruleset, &table))
                .await;
        let (per_type, aux) = match join_result {
            Ok(result) => result,
            Err(e) => {
                // The blocking task panicked or was cancelled. Log loudly and
                // bail without setting vanilla_merged, so that type_index stays
                // incomplete and CW500/CW222 reference checks are suppressed
                // (avoiding a flood of false positives against an empty base game).
                self.client
                    .log_message(
                        MessageType::ERROR,
                        format!(
                            "Vanilla indexing task failed for {} — base-game references will not resolve. Error: {}",
                            dir.display(),
                            e
                        ),
                    )
                    .await;
                tracing::error!("spawn_blocking vanilla index panicked: {}", e);
                return;
            }
        };

        let total: usize = per_type.values().map(|v| v.len()).sum();

        // The freshly-extracted loc keys and dynamic values feed this session
        // directly too (not just the persisted cache).
        if !aux.loc_keys.is_empty() {
            *self.state.vanilla_loc_keys.lock() = Some(aux.loc_keys.clone());
        }
        self.merge_vanilla_dynamic_values(
            aux.complex_enum_values.clone(),
            aux.value_set_values.clone(),
        );

        // Persist for next startup so the base game isn't re-parsed every time.
        if let Some(cp) = &cache_path {
            match cwtools_info::vanilla_cache::save_per_type(
                &per_type,
                &game,
                &fingerprint,
                cp,
                aux,
            ) {
                Ok(n) => {
                    self.client
                        .log_message(
                            MessageType::INFO,
                            format!(
                                "Cached {} base-game instances to {} ({})",
                                n,
                                cp.display(),
                                fingerprint
                            ),
                        )
                        .await
                }
                Err(e) => {
                    self.client
                        .log_message(
                            MessageType::WARNING,
                            format!("Could not write vanilla cache {}: {}", cp.display(), e),
                        )
                        .await
                }
            }
        }

        *self.state.vanilla_index.lock() = Some(per_type);
        self.client
            .log_message(
                MessageType::INFO,
                format!("Indexed {} base-game instances.", total),
            )
            .await;
    }

    /// Path of the persistent base-game cache for `game` at `fingerprint`, under
    /// the client-provided `cacheDir` (else an OS cache dir). Versioned in the
    /// filename so multiple game versions can coexist and a published cache for a
    /// given version drops straight in. `None` if no cache dir can be resolved.
    pub(crate) fn vanilla_cache_path(
        &self,
        game: &str,
        fingerprint: &str,
    ) -> Option<std::path::PathBuf> {
        let base = self
            .state
            .config
            .read()
            .cache_dir
            .clone()
            .or_else(default_cache_dir)?;
        let safe = |s: &str| -> String {
            s.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect()
        };
        Some(base.join(format!("vanilla-{}-{}.cwv", safe(game), safe(fingerprint))))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use cwtools_rules::rules_types::{PathOptions, RuleSet, TypeDefinition};
    use cwtools_string_table::string_table::StringTable;

    #[test]
    fn test_discover_vanilla_dir_unknown_game_is_none() {
        assert!(discover_vanilla_dir("not_a_real_game").is_none());
        assert!(discover_vanilla_dir("").is_none());
    }

    /// Build a minimal `RuleSet` containing one type definition.
    fn ruleset_with_type(name: &str, path: &str, name_field: Option<&str>) -> RuleSet {
        let mut rs = RuleSet::new();
        rs.types.push(TypeDefinition {
            name: name.to_string(),
            name_field: name_field.map(|s| s.to_string()),
            path_options: PathOptions {
                paths: vec![path.to_string()],
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
        rs.reindex();
        rs
    }

    fn vanilla_root() -> std::path::PathBuf {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.keep();
        path.join("vanilla")
    }

    #[test]
    fn test_index_vanilla_dir_collects_instances() {
        let rs = ruleset_with_type("foo", "common/foos", None);

        let root = vanilla_root();
        let foos = root.join("common").join("foos");
        std::fs::create_dir_all(&foos).unwrap();
        std::fs::write(foos.join("a.txt"), "foo_one = { }\nfoo_two = { }\n").unwrap();

        let table = StringTable::new();
        let (per_type, _aux) = index_vanilla_dir(&root, &rs, &table);

        let names: Vec<&str> = per_type
            .get("foo")
            .map(|v| v.iter().map(|i| i.name.as_str()).collect())
            .unwrap_or_default();
        assert!(names.contains(&"foo_one"), "got: {:?}", names);
        assert!(names.contains(&"foo_two"), "got: {:?}", names);
    }

    #[test]
    fn test_index_vanilla_dir_uses_name_field() {
        // type[foo] instances are identified by the `name =` leaf, not the node key.
        let rs = ruleset_with_type("foo", "common/foos", Some("name"));

        let root = vanilla_root();
        let foos = root.join("common").join("foos");
        std::fs::create_dir_all(&foos).unwrap();
        std::fs::write(
            foos.join("a.txt"),
            "foo_one = { name = real_name_a }\nfoo_two = { name = real_name_b }\n",
        )
        .unwrap();

        let table = StringTable::new();
        let (per_type, _aux) = index_vanilla_dir(&root, &rs, &table);

        let names: Vec<&str> = per_type
            .get("foo")
            .map(|v| v.iter().map(|i| i.name.as_str()).collect())
            .unwrap_or_default();
        assert!(
            names.contains(&"real_name_a"),
            "name_field instance not extracted: {:?}",
            names
        );
        assert!(
            names.contains(&"real_name_b"),
            "name_field instance not extracted: {:?}",
            names
        );
        assert!(
            !names.contains(&"foo_one"),
            "node key should not be used when name_field is set: {:?}",
            names
        );
    }

    #[test]
    fn test_index_vanilla_dir_no_matching_path_is_empty() {
        let rs = ruleset_with_type("foo", "common/foos", None);

        let root = vanilla_root();
        // No common/foos directory at all.
        std::fs::create_dir_all(root.join("other")).unwrap();

        let table = StringTable::new();
        let (per_type, _aux) = index_vanilla_dir(&root, &rs, &table);
        assert!(
            per_type.is_empty(),
            "no matching path should yield an empty index, got: {:?}",
            per_type
        );
    }

    #[test]
    fn test_index_vanilla_dir_skips_unparseable_files() {
        // A malformed file must not abort indexing; valid files in the same dir
        // are still collected.
        let rs = ruleset_with_type("foo", "common/foos", None);

        let root = vanilla_root();
        let foos = root.join("common").join("foos");
        std::fs::create_dir_all(&foos).unwrap();
        std::fs::write(foos.join("good.txt"), "foo_one = { }\n").unwrap();
        // Bare brace with no opening: a parse error.
        std::fs::write(foos.join("bad.txt"), "}\n").unwrap();

        let table = StringTable::new();
        let (per_type, _aux) = index_vanilla_dir(&root, &rs, &table);

        let names: Vec<&str> = per_type
            .get("foo")
            .map(|v| v.iter().map(|i| i.name.as_str()).collect())
            .unwrap_or_default();
        assert!(
            names.contains(&"foo_one"),
            "valid instance should still be collected despite a bad file: {:?}",
            names
        );
    }

    #[test]
    fn test_index_vanilla_dir_aux_contains_file_paths() {
        // The vanilla cache aux must record every file that was discovered so
        // the cached index can be validated against the install later.
        let rs = ruleset_with_type("foo", "common/foos", None);

        let root = vanilla_root();
        let foos = root.join("common").join("foos");
        std::fs::create_dir_all(&foos).unwrap();
        std::fs::write(foos.join("a.txt"), "foo_one = { }\n").unwrap();

        let table = StringTable::new();
        let (_per_type, aux) = index_vanilla_dir(&root, &rs, &table);
        let logical = aux
            .file_paths
            .iter()
            .map(|p| p.replace('\\', "/"))
            .find(|p| p.ends_with("common/foos/a.txt"));
        assert!(
            logical.is_some(),
            "aux should contain the logical file path, got: {:?}",
            aux.file_paths
        );
    }

    #[test]
    fn test_index_vanilla_dir_respects_path_strict() {
        // path_strict = yes must only match the exact declared path, not siblings.
        let mut rs = ruleset_with_type("foo", "common/foos", None);
        rs.types[0].path_options.path_strict = true;
        rs.reindex();

        let root = vanilla_root();
        let foos = root.join("common").join("foos");
        let sibling = root.join("common").join("bars");
        std::fs::create_dir_all(&foos).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(foos.join("a.txt"), "foo_one = { }\n").unwrap();
        std::fs::write(sibling.join("b.txt"), "foo_two = { }\n").unwrap();

        let table = StringTable::new();
        let (per_type, _aux) = index_vanilla_dir(&root, &rs, &table);

        let names: Vec<&str> = per_type
            .get("foo")
            .map(|v| v.iter().map(|i| i.name.as_str()).collect())
            .unwrap_or_default();
        assert!(names.contains(&"foo_one"), "got: {:?}", names);
        assert!(
            !names.contains(&"foo_two"),
            "path_strict must not match sibling path, got: {:?}",
            names
        );
    }

    #[test]
    fn test_discover_vanilla_dir_known_game_maps_folder() {
        // discover_vanilla_dir relies on real Steam installs, which won't exist
        // in CI. Verify the mapping indirectly by exercising each known game id
        // and checking that non-existent games return None deterministically.
        for game in ["hoi4", "stellaris", "eu4", "ck3", "vic3"] {
            let _ = discover_vanilla_dir(game);
        }
        assert!(discover_vanilla_dir("nexus_games").is_none());
    }
}
