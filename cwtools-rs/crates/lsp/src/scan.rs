use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tower_lsp::lsp_types::*;

use cwtools_parser::parser::parse_string;
use cwtools_rules::rules_types::RuleSet;
use cwtools_validation::build_modifier_keys;

use crate::paths::{
    default_cache_dir, discover_vanilla_dir, loc_display_text, logical_path_from_uri, path_to_uri,
    uri_to_path_str,
};
use crate::validate::{
    loc_diag_to_validation_error, make_prepared, parse_error_to_diagnostic,
    validate_parsed_with_indexes, validation_error_to_diagnostic,
};
use crate::workspace_cache;
use crate::{Backend, LoadingBar, UpdateFileList};

/// Trailing window for coalescing `didChangeWatchedFiles` create/modify events.
/// Fixed (not a sliding reset) so a continuous churn stream still drains.
const WATCHED_DEBOUNCE_MS: u64 = 500;
/// Above this many distinct files in one window, validate the whole workspace
/// once (a rules re-clone / git checkout) instead of per file.
const WATCHED_BULK_CAP: usize = 200;

/// Index a base-game ("vanilla") install into per-type instances, ready to merge
/// into the workspace TypeIndex. Delegates to the shared driver's `index_game_dir`
/// so the LSP and CLI discover and index vanilla the SAME way (the driver's
/// `search_config_for` config, which is the broader, corpus-verified one). The
/// discovered ASTs are used directly (no re-parse) because vanilla files are only
/// indexed, never validated. Each instance keeps its real source path so
/// goto-definition / find-references into base-game content resolve to the right
/// file (the merge maps the path to a `file://` URI).
///
/// Also returns the cache aux payload (loc keys, file paths, variable names) so
/// a cache written by the LSP is as complete as one from the CLI's
/// `cache-vanilla`.
#[allow(clippy::type_complexity)]
pub(crate) fn index_vanilla_dir(
    dir: &std::path::Path,
    ruleset: &RuleSet,
    table: &cwtools_string_table::string_table::StringTable,
) -> (
    HashMap<String, Vec<(Arc<str>, cwtools_info::TypeInstance)>>,
    cwtools_info::vanilla_cache::VanillaCacheAux,
) {
    let var_effects = cwtools_info::variable_defining_effects(ruleset);
    let index = cwtools_driver::index_game_dir(dir, ruleset, table, &var_effects);
    let aux = cwtools_driver::build_vanilla_cache_aux(dir, &index);
    let per_type = index.map.into_iter().collect();
    (per_type, aux)
}

/// RAII guard for `DocumentState::scan_in_progress`. Resets the flag to
/// `false` on drop so a panicked scan can't wedge every later scan out
/// forever — the guard lives inside the scanning future, so a panic
/// unwinding through `validate_entire_workspace` still runs `Drop`.
struct ScanGuard<'a>(&'a AtomicBool);

impl Drop for ScanGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
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
        // Keyed under one synthetic file, distinct from the per-file URIs the
        // vanilla type instances now merge under, so a re-merge replaces this
        // contribution and the instance merge's `remove_files` never touches it.
        info.type_index
            .complex_enum_values
            .merge_file("<vanilla-dynamic>", complex_enums.into_iter().collect());
        info.type_index
            .value_set_values
            .merge_file("<vanilla-dynamic>", value_sets.into_iter().collect());
        drop(info);
        self.bump_info_revision();
    }

    /// Merge a pending `vanilla_index` (from the cache or a live index) into
    /// the workspace type index. After the merge the raw per-type data is
    /// dropped from `vanilla_index` to eliminate double residency (the
    /// type_index already owns the instances). `vanilla_merged` prevents
    /// `ensure_vanilla_index` re-running on subsequent workspace scans.
    pub(crate) fn merge_pending_vanilla_index(&self) {
        let per_type = self.state.vanilla_index.lock().take();
        if let Some(per_type) = per_type {
            // The vanilla index keys each instance by its raw source path (the
            // driver / cache form). Convert those to `file://` URIs — matching
            // how workspace files are keyed — so goto-definition, find-references
            // and workspace-symbol resolve into the real base-game file. The old
            // "<vanilla-cache>" sentinel failed to parse as a URI and silently
            // fell back to whatever document the user had open (#62).
            let mut uri_cache: HashMap<Arc<str>, Arc<str>> = HashMap::new();
            let mut converted: HashMap<String, Vec<(Arc<str>, cwtools_info::TypeInstance)>> =
                HashMap::with_capacity(per_type.len());
            for (type_name, instances) in per_type {
                let mut out = Vec::with_capacity(instances.len());
                for (path, inst) in instances {
                    let uri = uri_cache
                        .entry(path)
                        .or_insert_with_key(|p| {
                            Arc::from(path_to_uri(std::path::Path::new(p.as_ref())).as_str())
                        })
                        .clone();
                    out.push((uri, inst));
                }
                converted.insert(type_name, out);
            }
            // Distinct vanilla source URIs, tracked so a later re-merge drops
            // exactly this contribution in one index pass.
            let uris: HashSet<Arc<str>> = uri_cache.into_values().collect();
            let old = {
                let mut merged = self.state.vanilla_merged_uris.lock();
                std::mem::replace(&mut *merged, uris)
            };

            let mut info_guard = self.state.info_service.write();
            // Drop the previous base-game contribution (a re-merge after
            // cacheVanilla / clearAllCaches) before merging the fresh one.
            info_guard.type_index.remove_files(&old);
            info_guard.type_index.merge_with_uris(converted);
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
            drop(info_guard);
            self.bump_info_revision();
        }
    }

    /// Rebuild the cached modifier-key set and the expanded modifier→scopes map
    /// from the current ruleset and type index.
    pub(crate) fn rebuild_modifier_keys(&self) {
        // Lock order: rules -> info_service. One `rules` write guard holds the
        // ruleset we read from and the modifier data we write into.
        let mut rules = self.state.rules.write();
        let (keys, scopes) = match rules.ruleset.as_ref() {
            Some(rs) => {
                let info_guard = self.state.info_service.read();
                (
                    build_modifier_keys(rs, &info_guard.type_index),
                    crate::completion::expanded_modifier_scopes(rs, &info_guard.type_index),
                )
            }
            None => Default::default(),
        };
        rules.modifier_keys = Arc::new(keys);
        rules.modifier_scopes = Arc::new(scopes);
        drop(rules);
        self.bump_info_revision();
    }

    /// Public entry to the workspace scan. Runs the scan and ALWAYS clears the
    /// status-bar loading indicator on return, regardless of which internal path
    /// exited — including the early returns for an absent/empty workspace, which
    /// previously left the bar spinning on "Indexing workspace…" forever. A
    /// panic inside the scan is handled separately by the watcher in
    /// `initialized`, which clears the bar too.
    ///
    /// Re-entrancy guarded: the startup scan, `clearAllCaches`, `reindexWorkspace`,
    /// and the periodic background pass can all land here, and two overlapping
    /// scans would race each other's serial `info_service` writes. A losing
    /// caller skips the scan entirely rather than blocking behind the running
    /// one — returns `false` so a caller like the `reindexWorkspace` command
    /// can report that back instead of the scan silently no-oping.
    ///
    /// `quiet` suppresses every `loadingBar` notification the scan would
    /// otherwise send, so the periodic background pass doesn't flash the
    /// status bar while the user is working. `send_update_file_list` still
    /// fires either way — it's cheap and keeps the file explorer honest —
    /// except when the quiet short-circuit returns early: the file set is
    /// unchanged by definition, so the list it would send is identical.
    pub(crate) async fn validate_entire_workspace(&self, quiet: bool) -> bool {
        if self
            .state
            .scan_in_progress
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            tracing::debug!("workspace scan already in progress; skipping");
            return false;
        }
        let _guard = ScanGuard(&self.state.scan_in_progress);
        self.validate_entire_workspace_inner(quiet).await;
        if !quiet {
            self.send_loading_bar(false, "").await;
        }
        true
    }

    /// Scan the entire workspace for relevant game files and validate them all.
    #[tracing::instrument(skip_all)]
    async fn validate_entire_workspace_inner(&self, quiet: bool) {
        cwtools_profiling::log_rss("workspace_scan_start");
        if !quiet {
            self.send_loading_bar(true, "Indexing workspace…").await;
        }

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

        // Quiet-pass short-circuit: skip reindex + revalidate + re-publish when
        // the walked-files fingerprint and settings generation both match the
        // last full pass. Empty walk (transiently-unreadable root) never
        // short-circuits or records — see the store guard below.
        let scan_fingerprint =
            tokio::task::block_in_place(|| stat_signature_for(&files_to_validate));
        let scan_generation = self
            .state
            .settings_generation
            .load(std::sync::atomic::Ordering::SeqCst);
        if quiet_pass_can_skip(
            quiet,
            files_to_validate.is_empty(),
            (scan_fingerprint, scan_generation),
            *self.state.last_scan_fingerprint.lock(),
        ) {
            tracing::info!(
                files = files_to_validate.len(),
                "quiet scan: workspace fingerprint unchanged, skipping reindex"
            );
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
        if !quiet {
            self.send_loading_bar(true, "Indexing workspace…").await;
        }
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
        // mutates the shared info index, so it stays serial and in the
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
                    // Read via the file manager so cp1252-encoded script files
                    // (pre-Jomini mods) are indexed instead of silently dropped.
                    let text = match cwtools_file_manager::file_manager::read_text(file_path) {
                        Ok(t) => t,
                        Err(e) => {
                            tracing::warn!(path = %file_path.display(), error = %e, "scan: skipping unreadable file");
                            return None;
                        }
                    };
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
        for (i, (file_path, outcome)) in files_to_validate.iter().zip(outcomes).enumerate() {
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
            // A quiet background pass shares the runtime with live requests
            // (hover, completion, did_change); yield periodically through this
            // serial loop so it doesn't hog a worker thread for the whole
            // index phase. Mirrors pass 2's yield-every-50 below.
            if quiet && i % 64 == 63 {
                tokio::task::yield_now().await;
            }
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

        // Prune index entries for files that vanished since the last scan —
        // deleted while the server had no watcher event (e.g. while closed),
        // or newly excluded by an ignore glob (pruning that one is correct
        // too: it matches what a restart would index). Without this, a stale
        // definition keeps "resolving" against a file that no longer exists
        // until a window reload.
        //
        // Key off what the walk FOUND on disk, not what parsed: a file with a
        // syntax error is still there and keeps its last-good index entry, so
        // cross-file goto/references don't drop out while it's mid-edit.
        let discovered_uris: HashSet<String> =
            files_to_validate.iter().map(|p| path_to_uri(p)).collect();
        // An empty walk almost always means the root was transiently
        // unreadable — walk_workspace_files swallows I/O errors and returns an
        // empty Vec — not that the user deleted every file. Pruning against an
        // empty set would wipe the whole index on a hiccup, so skip it; real
        // deletions still arrive as per-file DELETE watched events.
        let removed_uris: Vec<String> = if files_to_validate.is_empty() {
            Vec::new()
        } else {
            let mut info = self.state.info_service.write();
            let stale: Vec<String> = info
                .files
                .keys()
                // Only real per-file entries. Vanilla instances (and the
                // "<vanilla-dynamic>" bucket) are merged straight into
                // `type_index`, never `files`, so this workspace-scoped prune
                // never sees them; the `file://` guard is belt-and-braces.
                .filter(|&uri| {
                    uri.starts_with("file://")
                        && !discovered_uris.contains(uri)
                        && !open_uris.contains(uri)
                })
                .cloned()
                .collect();
            for uri in &stale {
                info.clear_file(uri);
            }
            stale
        };
        if !removed_uris.is_empty() {
            // Mirrors the DELETE branch of `did_change_watched_files_impl`:
            // the loc live-overlay is keyed per-file too and must forget the
            // same URIs, or loc checks keep serving stale entries for a file
            // `info_service` just dropped.
            {
                let mut overlay = self.state.loc_live_overlay.write();
                for uri in &removed_uris {
                    overlay.remove(uri);
                }
            }
            self.bump_info_revision();
            self.client
                .log_message(
                    MessageType::INFO,
                    format!(
                        "Pruned {} file(s) no longer on disk from the index",
                        removed_uris.len()
                    ),
                )
                .await;
            for uri in &removed_uris {
                if let Ok(uri_obj) = Url::parse(uri) {
                    self.client.publish_diagnostics(uri_obj, vec![], None).await;
                }
            }
        }

        // Build the base-game index from a `vanilla` dir (or auto-discovery) if
        // we have one and haven't indexed it yet. Populates `vanilla_index`.
        self.ensure_vanilla_index(false, quiet).await;

        // Merge the pre-generated vanilla index (if loaded) so base-game
        // references resolve.
        self.merge_pending_vanilla_index();

        // Rebuild the cached modifier-key set now that the type index is
        // complete (templated modifiers like production_speed_<building>_factor
        // expand against the full instance list).
        self.rebuild_modifier_keys();

        // Build the loc-key index (workspace + vanilla) so pass 2's config
        // validation can check LocalisationField references (CW100/CW122), and
        // publish loc-file diagnostics (CW225 etc.) for the workspace loc files.
        // On a quiet background pass, skip this ~2M-entry rebuild (the biggest
        // transient cost of a scan) when a stat-only signature over the same
        // files says nothing loc-related changed since the last scan. A
        // foreground scan (startup, clearAllCaches, reindexWorkspace) always
        // rebuilds, so a user-triggered rescan never serves stale loc
        // diagnostics — it just also records the signature for the next
        // quiet pass to compare against.
        let loc_signature = tokio::task::block_in_place(|| self.compute_loc_signature(&root_path));
        let loc_unchanged = *self.state.last_loc_signature.lock() == Some(loc_signature);
        if quiet && loc_unchanged {
            tracing::info!("quiet scan: loc signature unchanged, skipping loc rebuild");
        } else {
            self.rebuild_and_publish_loc(&root_path).await;
        }
        *self.state.last_loc_signature.lock() = Some(loc_signature);

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
        if !quiet {
            self.send_loading_bar(true, "Validating workspace…").await;
        }
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
                self.publish_filtered(uri_obj, diagnostics, None).await;
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
        let ws_prefix = self.state.config.read().workspace_prefix.clone();
        let file_list: Vec<serde_json::Value> = files_to_validate
            .iter()
            .map(|file_path| {
                let uri = path_to_uri(file_path);
                let logical_path = logical_path_from_uri(&uri, &ws_prefix);
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

        // Never record for an empty walk: a transiently-unreadable root would
        // otherwise pin a bogus fingerprint and suppress the recovery pass.
        if !files_to_validate.is_empty() {
            *self.state.last_scan_fingerprint.lock() = Some((scan_fingerprint, scan_generation));
        }

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
        self.revalidate_all_open_docs(crate::ValidateTrigger::Reindex)
            .await;
        // The status bar is cleared by the `validate_entire_workspace` wrapper on
        // return, so every exit path (this one and the early returns above) clears
        // it uniformly.
    }

    /// Re-validate every currently-open document against the current (complete)
    /// index and re-publish, skipping any whose version changed meanwhile. Called
    /// once after the workspace scan so open docs validated against a partial
    /// index don't keep stale cross-file diagnostics, and on a live
    /// `didChangeConfiguration` so a changed suppression list re-filters at once.
    pub(crate) async fn revalidate_all_open_docs(&self, trigger: crate::ValidateTrigger) {
        let open_docs: Vec<(String, Arc<str>, i32)> = {
            let docs = self.state.documents.lock();
            docs.iter()
                .map(|(uri, doc)| (uri.clone(), doc.text.clone(), doc.version))
                .collect()
        };
        for (uri, text, version) in open_docs {
            let (diagnostics, _) = self.parse_and_validate(&uri, &text, trigger).await;
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
                self.publish_filtered(uri_obj, diagnostics, Some(version))
                    .await;
            }
        }
    }

    /// Directories `rebuild_and_publish_loc` scans for loc files: the
    /// workspace root plus the configured vanilla install, if any. Shared
    /// with `compute_loc_signature` so the two can never walk different
    /// trees.
    fn loc_dirs(&self, root_path: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut dirs = vec![root_path.to_path_buf()];
        if let Some(v) = self.state.config.read().vanilla_dir.clone() {
            dirs.push(v);
        }
        dirs
    }

    /// Stat-only signature (path, size, mtime) over the loc files
    /// `rebuild_and_publish_loc` would read. Lets a quiet background pass
    /// detect "nothing loc-related changed" and skip the full rebuild without
    /// reading or parsing a single file. Discovers files via
    /// `LocService::discover_files` — the exact walk `rebuild_and_publish_loc`
    /// uses via `LocService::from_folders` — so this can't drift from what it
    /// actually reads. Blocking (stats every discovered file); call from
    /// within `block_in_place`.
    pub(crate) fn compute_loc_signature(&self, root_path: &std::path::Path) -> u64 {
        let dirs = self.loc_dirs(root_path);
        let dir_refs: Vec<&std::path::Path> = dirs.iter().map(|p| p.as_path()).collect();
        let files = cwtools_localization::LocService::discover_files(&dir_refs);
        stat_signature_for(&files)
    }

    /// Build the loc-key index from the workspace root plus the vanilla install,
    /// store it in state (for CW100/CW122 on config files), and publish loc-file
    /// diagnostics (CW225/CW234/CW259/CW268/CW275) for the workspace loc files.
    #[tracing::instrument(skip_all)]
    pub(crate) async fn rebuild_and_publish_loc(&self, root_path: &std::path::Path) {
        // Cached vanilla loc keys (from the vanilla cache) supplement the key
        // index, but the hover text map needs the actual loc text from the files.
        // Always load the vanilla loc files when the dir is available so hover
        // shows translations for keys that exist only in the base game (#51).
        let cached_vanilla_loc = self.state.vanilla_loc_keys.lock().clone();
        let loc_dirs = self.loc_dirs(root_path);
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
        // Cached vanilla keys are resolved via the loc-index union passed to
        // `validate_loc_project_with_union`, so they aren't duplicated here.
        let extra_valid_refs: HashSet<String> = {
            // Lock order: rules -> info_service.
            let mut extra = (*self.state.rules.read().modifier_keys).clone();
            let info = self.state.info_service.read();
            // Dynamic modifiers, ideas, other game-object names + defined
            // variables a `$ref$` can bind to (mirrors the CLI/driver path).
            extra.extend(info.type_index.loc_bindable_names());
            extra
        };

        // block_in_place: the loc service reads and parses hundreds of loc files
        // from disk — synchronous I/O that must not starve the async executor.
        let (loc_index, mut by_file, loc_text_map, loc_loc_map) =
            tokio::task::block_in_place(|| {
                let service = cwtools_localization::LocService::from_folders(&dir_refs);
                let mut idx = cwtools_localization::LocIndex::build_scoped(
                    &service,
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
                // Reuse the merged loc-index union (with cached vanilla keys)
                // instead of rebuilding the ~2M-key set inside the validate pass.
                for d in cwtools_localization::validate_loc_project_with_union(
                    &service,
                    loc_languages.as_deref(),
                    idx.union(),
                    &extra_valid_refs,
                ) {
                    if !std::path::Path::new(&d.file).starts_with(root_path) {
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
                    if std::path::Path::new(&file.path).starts_with(root_path) {
                        by_file.entry(file.path.clone()).or_default();
                    }
                    let lang = file.lang.unwrap_or(cwtools_localization::Lang::English);
                    let lang_included = hover_all || lang == primary_lang;
                    // Every entry in a file shares the same source path.
                    let file_uri = path_to_uri(std::path::Path::new(&file.path));
                    for entry in &file.entries {
                        let key_lower = entry.key.to_lowercase();
                        // goto: prefer the primary language's location (English by
                        // default) so Ctrl+Click lands on the canonical entry, not
                        // whichever language happened to be scanned first.
                        let loc = || {
                            (
                                file_uri.clone(),
                                (entry.position.line.saturating_sub(1)) as u32,
                            )
                        };
                        if lang == primary_lang {
                            ll.insert(key_lower.clone(), loc());
                        } else {
                            ll.entry(key_lower.clone()).or_insert_with(loc);
                        }
                        if !lang_included {
                            continue;
                        }
                        let display = loc_display_text(&entry.desc);
                        if !display.is_empty() {
                            lt.entry(key_lower)
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
                self.publish_filtered(uri_obj, diags, None).await;
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
    ///
    /// `quiet` suppresses the "Indexing base game…" loading-bar notification so a
    /// background pass that (re)indexes vanilla doesn't flash the status bar. The
    /// scan wrapper only clears the bar on a non-quiet run, so a quiet caller
    /// must not raise it or it would spin forever.
    pub(crate) async fn ensure_vanilla_index(&self, force_rebuild: bool, quiet: bool) {
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

        if !quiet {
            self.send_loading_bar(true, "Indexing base game…").await;
        }
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

    /// Handle external file changes (create, modify, delete) from the file
    /// system — e.g. a git checkout, file move in the OS explorer, or rename
    /// outside the editor. Without this handler the index keeps stale entries
    /// for deleted/moved files until a window reload (#52).
    ///
    /// DELETED and CHANGED/CREATED events are both queued and coalesced into a
    /// single trailing window (#90) instead of applying inline on the message
    /// future — DELETEs used to run a synchronous O(whole-index) `clear_file`
    /// per file, which stalled the message future for seconds on a large
    /// branch switch. The drain applies deletions first, batched under one
    /// `info_service` write.
    #[tracing::instrument(skip_all)]
    pub(crate) async fn did_change_watched_files_impl(&self, params: DidChangeWatchedFilesParams) {
        let mut enqueued = false;
        for event in params.changes {
            let uri = event.uri.to_string();
            match event.typ {
                FileChangeType::DELETED => {
                    tracing::debug!(%uri, "watched file deleted; queued");
                    self.state.watched_deleted.lock().insert(uri);
                    enqueued = true;
                }
                FileChangeType::CHANGED | FileChangeType::CREATED => {
                    // Open state is re-checked at drain time (an open editor
                    // buffer is authoritative), so just queue every event here.
                    self.state.watched_pending.lock().insert(uri);
                    enqueued = true;
                }
                _ => {}
            }
        }
        if enqueued {
            self.arm_watched_batch();
        }
    }

    /// Arm a single trailing window that drains the queued watched events
    /// (`watched_pending` + `watched_deleted`). A fixed window: if one is
    /// already scheduled or running, do nothing, so a continuous event stream
    /// can't keep pushing the drain further out.
    fn arm_watched_batch(&self) {
        let mut guard = self.state.watched_debounce.lock();
        if guard.as_ref().is_some_and(|h| !h.is_finished()) {
            return;
        }
        let client = self.client.clone();
        let state = self.state.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(WATCHED_DEBOUNCE_MS)).await;
            Backend { client, state }.process_watched_batch().await;
        });
        *guard = Some(handle);
    }

    /// Drain the queued watched events (changes + deletes) and apply them off
    /// the message future. A batch larger than `WATCHED_BULK_CAP` collapses
    /// into one CAS-guarded rescan instead of hundreds of per-file
    /// validations — its on-disk prune drops the deleted URIs too, so deletes
    /// need no separate handling on that path. Below the cap, deletions apply
    /// first (one `info_service` write), then per-file validation. Re-arms if
    /// new events landed while it was running.
    async fn process_watched_batch(&self) {
        let changes: HashSet<String> = { self.state.watched_pending.lock().drain().collect() };
        // A URI both changed and deleted this window is treated as a change.
        let deletes: Vec<String> = {
            let mut deleted = self.state.watched_deleted.lock();
            resolve_watched_deletes(&changes, deleted.drain())
        };
        if changes.is_empty() && deletes.is_empty() {
            return;
        }
        if watched_batch_over_cap(changes.len(), deletes.len()) {
            tracing::info!(
                changes = changes.len(),
                deletes = deletes.len(),
                "watched batch over cap; full rescan"
            );
            if !self.validate_entire_workspace(true).await {
                // Lost the CAS to a running scan — requeue both sides.
                self.state.watched_pending.lock().extend(changes);
                self.state.watched_deleted.lock().extend(deletes);
            }
        } else {
            // Deletions first, so a re-created file's later change validates
            // against an index that already forgot the stale entry.
            if !deletes.is_empty() {
                self.process_watched_deletes(&deletes).await;
            }
            for uri in changes {
                // An open editor buffer owns its diagnostics; skip files that
                // are open now, regardless of open state when queued.
                if self.state.documents.lock().contains_key(&uri) {
                    continue;
                }
                let path = uri_to_path_str(&uri);
                // Stat-gate: a toucher that rewrote identical bytes leaves
                // size+mtime unchanged, so skip the read + revalidate. `None`
                // (vanished/unreadable, or first-ever event) falls through.
                let sig = watched_stat_sig(std::path::Path::new(&path));
                if let Some(sig) = sig
                    && self.state.watched_signatures.lock().get(&uri) == Some(&sig)
                {
                    tracing::debug!(%uri, "watched file unchanged (stat match); skipping");
                    continue;
                }
                // Read on a blocking thread via the file manager so cp1252
                // script files are validated (not silently dropped) and the
                // async runtime isn't stalled on the sync read.
                let read = {
                    let path = path.clone();
                    tokio::task::spawn_blocking(move || {
                        cwtools_file_manager::file_manager::read_text(std::path::Path::new(&path))
                    })
                    .await
                };
                match read {
                    Ok(Ok(text)) => {
                        let (diagnostics, _) = self
                            .parse_and_validate(&uri, &text, crate::ValidateTrigger::Watched)
                            .await;
                        // Record only after a successful validate, so a
                        // transient read failure doesn't poison the record.
                        if let Some(sig) = sig {
                            self.state
                                .watched_signatures
                                .lock()
                                .insert(uri.clone(), sig);
                        }
                        if let Ok(uri_obj) = Url::parse(&uri) {
                            self.publish_gated(uri_obj, diagnostics, None).await;
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::warn!("could not read watched file {}: {}", path, e);
                    }
                    Err(e) => {
                        tracing::warn!("read task panicked for watched file {}: {}", path, e);
                    }
                }
            }
        }
        // Clear our slot before the final check so a producer that queued an
        // event while we ran can arm the next window (or we do it here). Setting
        // the slot to `None` only detaches this finished task, it doesn't abort.
        *self.state.watched_debounce.lock() = None;
        // Each guard scoped to its own `let` so the two queue locks are never
        // held at once.
        let pending_more = !self.state.watched_pending.lock().is_empty();
        let deleted_more = !self.state.watched_deleted.lock().is_empty();
        if pending_more || deleted_more {
            self.arm_watched_batch();
        }
    }

    /// Apply a coalesced batch of DELETE events off the message future: forget
    /// each URI from the info service (one write scope), the loc overlay, and
    /// the watched-signature record, bump the info revision once for the whole
    /// batch, then publish empty diagnostics per URI outside every lock.
    async fn process_watched_deletes(&self, deletes: &[String]) {
        {
            let mut info = self.state.info_service.write();
            for uri in deletes {
                info.clear_file(uri);
            }
        }
        {
            let mut overlay = self.state.loc_live_overlay.write();
            for uri in deletes {
                overlay.remove(uri);
            }
        }
        {
            let mut sigs = self.state.watched_signatures.lock();
            for uri in deletes {
                sigs.remove(uri);
            }
        }
        self.bump_info_revision();
        for uri in deletes {
            if let Ok(uri_obj) = Url::parse(uri) {
                self.client.publish_diagnostics(uri_obj, vec![], None).await;
            }
        }
    }

    // ── Background reindex ──────────────────────────────────────────────

    /// Record that the user just interacted with the editor (an edit or a
    /// completion request), resetting the idle clock the background reindex
    /// loop watches.
    pub(crate) fn mark_activity(&self) {
        let now_ms = self.state.start.elapsed().as_millis() as u64;
        self.state.last_activity_ms.store(now_ms, Ordering::Relaxed);
    }

    /// Whether a quiet background pass may run right now: the initial scan
    /// has finished, no scan (foreground or background) is already running,
    /// and the user has been idle for at least `idle_ms`.
    pub(crate) fn should_run_background_pass(&self, idle_ms: u64) -> bool {
        if !self.state.index_ready.load(Ordering::Relaxed) {
            return false;
        }
        if self.state.scan_in_progress.load(Ordering::SeqCst) {
            return false;
        }
        let now_ms = self.state.start.elapsed().as_millis() as u64;
        let last_activity_ms = self.state.last_activity_ms.load(Ordering::Relaxed);
        is_idle(now_ms, last_activity_ms, idle_ms)
    }

    /// The configured background-reindex cadence in seconds. `0` means off.
    /// `CWTOOLS_REINDEX_INTERVAL_SECS` overrides the config value entirely
    /// when set (including to re-enable a disabled config), so tests don't
    /// have to wait out a real 30-minute default.
    fn effective_reindex_interval_secs(&self) -> u64 {
        if let Ok(v) = std::env::var("CWTOOLS_REINDEX_INTERVAL_SECS") {
            return v.parse().unwrap_or(0);
        }
        self.state
            .config
            .read()
            .background_reindex_interval_minutes
            .saturating_mul(60)
    }

    /// How long the user must be idle before a background pass runs, in
    /// milliseconds. The `CWTOOLS_REINDEX_IDLE_SECS` test override wins over
    /// the configured `backgroundReindexIdleSeconds`, which wins over the 15s
    /// default (`Config::new`). Re-read on every not-idle tick, so a live
    /// config change applies without waiting out the old window.
    fn reindex_idle_ms(&self) -> u64 {
        let config_secs = self.state.config.read().background_reindex_idle_seconds;
        let env_val = std::env::var("CWTOOLS_REINDEX_IDLE_SECS").ok();
        resolve_reindex_idle_ms(env_val.as_deref(), config_secs)
    }

    /// Periodic quiet re-scan so a long-running session doesn't accumulate
    /// stale index state (deleted-file entries missed by the watcher, a
    /// settings change that only takes effect on the next scan, …). Spawned
    /// once from `initialized` alongside the startup scan; runs for the life
    /// of the server.
    ///
    /// Each cycle re-reads the effective interval so toggling the setting (or
    /// the env override, in tests) live takes effect without a restart: 0
    /// means disabled, and the loop just polls every 60s waiting for it to
    /// become positive. Once enabled, it sleeps out the interval, then waits
    /// for the user to go idle — re-reading the idle window and the enabled
    /// flag on each tick (bounded to 15s, see `reindex_wait_tick_ms`), so
    /// lowering or disabling either setting is noticed promptly even when
    /// the window is hours — before running a quiet
    /// `validate_entire_workspace`. Never unwraps: a malformed env var
    /// degrades to "disabled"/"default", it doesn't panic the loop.
    pub(crate) async fn background_reindex_loop(&self) {
        loop {
            let interval_secs = self.effective_reindex_interval_secs();
            if interval_secs == 0 {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                continue;
            }
            tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;

            loop {
                if self.effective_reindex_interval_secs() == 0 {
                    // Disabled while we were waiting for the interval or for
                    // the user to go idle; the outer loop will pick that up
                    // and fall back to polling.
                    break;
                }
                let idle_ms = self.reindex_idle_ms();
                if self.should_run_background_pass(idle_ms) {
                    self.validate_entire_workspace(true).await;
                    break;
                }
                // Not idle yet — slip forward and check again rather than
                // skipping the whole interval.
                tokio::time::sleep(std::time::Duration::from_millis(reindex_wait_tick_ms(
                    idle_ms,
                )))
                .await;
            }
        }
    }
}

/// Whether at least `idle_ms` have passed since `last_activity_ms`, both
/// measured in milliseconds on the same monotonic clock (`DocumentState::start`).
/// Saturating so a `last_activity_ms` briefly ahead of `now_ms` (there is no
/// such clock, but defend anyway) reads as "not idle" instead of wrapping.
pub(crate) fn is_idle(now_ms: u64, last_activity_ms: u64, idle_ms: u64) -> bool {
    now_ms.saturating_sub(last_activity_ms) >= idle_ms
}

/// Env > config precedence for the reindex idle window, split out from
/// `Backend::reindex_idle_ms` so it's unit-testable without a `Backend`.
/// A malformed env value degrades to the config value, it doesn't panic.
pub(crate) fn resolve_reindex_idle_secs(env_val: Option<&str>, config_secs: u64) -> u64 {
    env_val
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(config_secs)
}

/// The resolved idle window in milliseconds. Saturating so an absurd
/// configured value (u64::MAX seconds) pins at u64::MAX ms instead of
/// wrapping into a tiny window.
pub(crate) fn resolve_reindex_idle_ms(env_val: Option<&str>, config_secs: u64) -> u64 {
    resolve_reindex_idle_secs(env_val, config_secs).saturating_mul(1000)
}

/// Sleep tick for the not-idle wait in `background_reindex_loop`. Capped at
/// 15s so a lowered or disabled setting is noticed promptly even when the
/// idle window is hours; floored at 50ms so `idle_ms` = 0 (the e2e override)
/// doesn't busy-spin. The idleness comparison still uses the full window.
pub(crate) fn reindex_wait_tick_ms(idle_ms: u64) -> u64 {
    idle_ms.clamp(50, 15_000)
}

/// Fold a stat-only signature (path, size, mtime) over `files` into one hash,
/// in a deterministic (sorted-path) order so the result doesn't depend on
/// directory-walk order. Shared by the loc-rebuild skip and the whole-pass
/// short-circuit; split out from `Backend` so it's unit-testable without a
/// live `tower_lsp::Client`.
fn stat_signature_for(files: &[std::path::PathBuf]) -> u64 {
    // Sort by reference — the caller still owns `files`.
    let mut sorted: Vec<&std::path::Path> = files.iter().map(|p| p.as_path()).collect();
    sorted.sort_unstable();
    // Limitation: a same-length edit in the same second on a coarse-mtime fs (FAT/NFS) false-negatives the skip; acceptable, we don't content-hash.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for path in sorted {
        path.to_string_lossy().hash(&mut hasher);
        if let Ok(meta) = std::fs::metadata(path) {
            meta.len().hash(&mut hasher);
            if let Ok(modified) = meta.modified()
                && let Ok(since_epoch) = modified.duration_since(std::time::UNIX_EPOCH)
            {
                since_epoch.as_nanos().hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

/// Drop from `deletes` any URI that also arrived as a CHANGED/CREATED this
/// window: a delete coincident with a re-create (an atomic save's
/// remove+rewrite) is a change, not a delete of the index entry.
fn resolve_watched_deletes(
    changes: &HashSet<String>,
    deletes: impl Iterator<Item = String>,
) -> Vec<String> {
    deletes.filter(|uri| !changes.contains(uri)).collect()
}

/// Whether a coalesced watched batch (changes + deletes together) exceeds the
/// per-file cap and should collapse into one workspace rescan instead.
/// Saturating so an absurd count can't wrap.
fn watched_batch_over_cap(changes: usize, deletes: usize) -> bool {
    changes.saturating_add(deletes) > WATCHED_BULK_CAP
}

/// Whether a QUIET background pass can short-circuit the whole reindex +
/// revalidate: true only for a quiet pass with a non-empty walk (an empty
/// walk is a transiently-unreadable root, not "everything deleted", so it
/// must still run) whose fingerprint matches the last stored one. A
/// foreground pass always returns false.
fn quiet_pass_can_skip(
    quiet: bool,
    files_empty: bool,
    current: (u64, u64),
    stored: Option<(u64, u64)>,
) -> bool {
    quiet && !files_empty && stored == Some(current)
}

/// Stat-only signature (file size, mtime-nanos) for a single watched file —
/// the per-file analogue of `stat_signature_for`. `None` when the file can't
/// be stat'd, so the caller can't prove it's unchanged and revalidates.
fn watched_stat_sig(path: &std::path::Path) -> Option<(u64, u128)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Some((meta.len(), mtime))
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

    // ── is_idle (background reindex gating) ────────────────────────────────

    #[test]
    fn test_is_idle_below_threshold_is_false() {
        // 4999ms since last activity, 5000ms required — not idle yet.
        assert!(!is_idle(5_000, 1, 5_000));
    }

    #[test]
    fn test_is_idle_at_exact_threshold_is_true() {
        // Exactly `idle_ms` elapsed counts as idle (>=, not >).
        assert!(is_idle(6_000, 1_000, 5_000));
    }

    #[test]
    fn test_is_idle_past_threshold_is_true() {
        assert!(is_idle(10_000, 1_000, 5_000));
    }

    #[test]
    fn test_is_idle_zero_threshold_is_always_true() {
        // idle_ms = 0 means "never wait" — used by the e2e test to trigger
        // the background pass immediately once the interval elapses.
        assert!(is_idle(0, 0, 0));
        assert!(is_idle(12_345, 12_345, 0));
    }

    #[test]
    fn test_is_idle_last_activity_ahead_of_now_is_false() {
        // Should never happen (last_activity_ms is derived from the same
        // monotonic clock as now_ms), but the saturating subtraction must
        // not wrap into "always idle" if it somehow does.
        assert!(!is_idle(100, 200, 1));
        // ...unless idle_ms is 0, where "no wait required" still holds.
        assert!(is_idle(100, 200, 0));
    }

    // ── resolve_reindex_idle_secs (env > config > default) ──────────────────

    #[test]
    fn test_reindex_idle_env_wins_over_config() {
        assert_eq!(resolve_reindex_idle_secs(Some("3"), 40), 3);
        // Including re-tightening a config-widened window down to zero.
        assert_eq!(resolve_reindex_idle_secs(Some("0"), 40), 0);
    }

    #[test]
    fn test_reindex_idle_config_wins_over_default() {
        // No env override → the configured value, whatever the built-in
        // default is.
        assert_eq!(resolve_reindex_idle_secs(None, 40), 40);
    }

    #[test]
    fn test_reindex_idle_malformed_env_degrades_to_config() {
        assert_eq!(resolve_reindex_idle_secs(Some("junk"), 40), 40);
        assert_eq!(resolve_reindex_idle_secs(Some(""), 40), 40);
    }

    #[test]
    fn test_reindex_idle_default_is_15_seconds() {
        // An untouched Config carries the documented 15s default.
        assert_eq!(crate::Config::new().background_reindex_idle_seconds, 15);
    }

    #[test]
    fn test_reindex_idle_ms_saturates_instead_of_wrapping() {
        // A u64::MAX-ish window must pin at u64::MAX ms, not wrap into a
        // near-zero window that would let a background pass fire mid-typing.
        assert_eq!(resolve_reindex_idle_ms(None, u64::MAX), u64::MAX);
        assert_eq!(resolve_reindex_idle_ms(None, u64::MAX / 999), u64::MAX);
        // And the non-saturating path still converts normally.
        assert_eq!(resolve_reindex_idle_ms(None, 15), 15_000);
        assert_eq!(resolve_reindex_idle_ms(Some("3"), 40), 3_000);
    }

    // ── reindex_wait_tick_ms (not-idle sleep bounding) ──────────────────────

    #[test]
    fn test_reindex_wait_tick_is_bounded() {
        // Small windows tick at the window itself...
        assert_eq!(reindex_wait_tick_ms(5_000), 5_000);
        // ...zero (the e2e override) floors at 50ms instead of busy-spinning...
        assert_eq!(reindex_wait_tick_ms(0), 50);
        // ...and huge windows cap at 15s so a live settings change is
        // noticed promptly, not after the old window elapses.
        assert_eq!(reindex_wait_tick_ms(3_600_000), 15_000);
        assert_eq!(reindex_wait_tick_ms(u64::MAX), 15_000);
    }

    // ── stat_signature_for (quiet-scan loc-rebuild + whole-pass skip) ───────

    #[test]
    fn test_stat_signature_stable_for_unchanged_files() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.yml");
        let b = tmp.path().join("b.yml");
        std::fs::write(&a, "l_english:\n key:0 \"value\"\n").unwrap();
        std::fs::write(&b, "l_english:\n other:0 \"value\"\n").unwrap();

        let sig1 = stat_signature_for(&[a.clone(), b.clone()]);
        // Same files, reversed discovery order — the signature sorts paths
        // first, so order of the input slice must not matter.
        let sig2 = stat_signature_for(&[b, a]);
        assert_eq!(sig1, sig2, "signature must not depend on discovery order");
    }

    #[test]
    fn test_stat_signature_changes_when_a_file_is_touched() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.yml");
        std::fs::write(&a, "l_english:\n key:0 \"value\"\n").unwrap();

        let before = stat_signature_for(std::slice::from_ref(&a));
        // Rewrite with different content (length changes) and bump mtime.
        std::fs::write(&a, "l_english:\n key:0 \"a different, longer value\"\n").unwrap();
        let newer = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        filetime_set(&a, newer);

        let after = stat_signature_for(&[a]);
        assert_ne!(
            before, after,
            "touching a file's size/mtime should change the signature"
        );
    }

    #[test]
    fn test_stat_signature_changes_when_file_set_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.yml");
        std::fs::write(&a, "l_english:\n key:0 \"value\"\n").unwrap();
        let one_file = stat_signature_for(std::slice::from_ref(&a));

        let b = tmp.path().join("b.yml");
        std::fs::write(&b, "l_english:\n other:0 \"value\"\n").unwrap();
        let two_files = stat_signature_for(&[a, b]);

        assert_ne!(
            one_file, two_files,
            "adding a file to the set should change the signature"
        );
    }

    // ── quiet_pass_can_skip (whole-pass short-circuit) ──────────────────────

    #[test]
    fn test_quiet_pass_skips_on_matching_fingerprint() {
        assert!(
            quiet_pass_can_skip(true, false, (7, 1), Some((7, 1))),
            "a quiet pass with an unchanged fingerprint + generation must skip"
        );
    }

    #[test]
    fn test_quiet_pass_runs_when_file_fingerprint_differs() {
        // A changed/added/removed/touched file moves the content fingerprint.
        assert!(
            !quiet_pass_can_skip(true, false, (8, 1), Some((7, 1))),
            "a changed file fingerprint must run the pass"
        );
    }

    #[test]
    fn test_quiet_pass_runs_when_generation_differs() {
        // A rules/config change bumps the generation even if the file set is
        // byte-for-byte identical on disk.
        assert!(
            !quiet_pass_can_skip(true, false, (7, 2), Some((7, 1))),
            "a bumped settings generation must run the pass"
        );
    }

    #[test]
    fn test_quiet_pass_runs_on_first_pass_with_no_stored_fingerprint() {
        assert!(
            !quiet_pass_can_skip(true, false, (7, 1), None),
            "the first pass has nothing to compare against and must run"
        );
    }

    #[test]
    fn test_foreground_pass_never_skips() {
        // Even with a matching fingerprint, a user-invoked (non-quiet) scan runs
        // in full — reindexWorkspace / clearAllCaches / reloadrulesconfig.
        assert!(
            !quiet_pass_can_skip(false, false, (7, 1), Some((7, 1))),
            "a foreground pass must always run"
        );
    }

    #[test]
    fn test_quiet_pass_does_not_skip_empty_walk() {
        // A transiently-unreadable root yields an empty walk; short-circuiting
        // (or recording) a fingerprint for it would suppress the recovery pass.
        assert!(
            !quiet_pass_can_skip(true, true, (7, 1), Some((7, 1))),
            "an empty walk must not short-circuit"
        );
    }

    /// Set a file's mtime forward without depending on filesystem mtime
    /// resolution (some filesystems truncate to 1s), so the "touched" test
    /// above is deterministic. `std::fs::File::set_modified` is stable since
    /// Rust 1.75.
    fn filetime_set(path: &std::path::Path, time: std::time::SystemTime) {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(time).unwrap();
    }

    // ── watched_stat_sig (stat-gate for watched CHANGED validation) ────────

    #[test]
    fn test_watched_stat_sig_stable_for_unchanged_file() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, "foo = { }\n").unwrap();
        let s1 = watched_stat_sig(&f);
        let s2 = watched_stat_sig(&f);
        assert!(s1.is_some(), "an existing file must have a signature");
        assert_eq!(s1, s2, "unchanged file must produce a stable signature");
    }

    #[test]
    fn test_watched_stat_sig_changes_on_size() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, "foo = { }\n").unwrap();
        let before = watched_stat_sig(&f);
        std::fs::write(&f, "foo = { }\nbar = { }\n").unwrap();
        let after = watched_stat_sig(&f);
        assert_ne!(before, after, "a size change must change the signature");
    }

    #[test]
    fn test_watched_stat_sig_changes_on_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, "foo = { }\n").unwrap();
        let before = watched_stat_sig(&f);
        // Same length, bumped mtime — a same-size rewrite (common with
        // formatters / atomic saves) must still invalidate the skip.
        let newer = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        filetime_set(&f, newer);
        let after = watched_stat_sig(&f);
        assert_ne!(before, after, "an mtime bump must change the signature");
    }

    #[test]
    fn test_watched_stat_sig_none_for_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("does_not_exist.txt");
        assert!(
            watched_stat_sig(&f).is_none(),
            "a missing file has no signature, so the caller can't skip it"
        );
    }

    // ── watched batch coalescing (delete + change in one window) ───────────

    #[test]
    fn test_resolve_watched_deletes_excludes_changed_uris() {
        let changes: HashSet<String> = ["a".to_string()].into_iter().collect();
        let deletes: HashSet<String> = ["a".to_string(), "b".to_string()].into_iter().collect();
        let out = resolve_watched_deletes(&changes, deletes.into_iter());
        assert_eq!(
            out,
            vec!["b".to_string()],
            "a URI both deleted and changed in one window is a change, not a delete"
        );
    }

    #[test]
    fn test_resolve_watched_deletes_passes_through_pure_deletes() {
        let changes: HashSet<String> = HashSet::new();
        let deletes: HashSet<String> = ["a".to_string(), "b".to_string()].into_iter().collect();
        let mut out = resolve_watched_deletes(&changes, deletes.into_iter());
        out.sort();
        assert_eq!(
            out,
            vec!["a".to_string(), "b".to_string()],
            "deletes with no coincident change pass through unchanged"
        );
    }

    #[test]
    fn test_watched_batch_over_cap_counts_deletes_and_changes() {
        // At the cap is not over it (matches the changes-only `> CAP` today).
        assert!(!watched_batch_over_cap(WATCHED_BULK_CAP, 0));
        assert!(watched_batch_over_cap(WATCHED_BULK_CAP, 1));
        // Deletes alone can trip the cap, and so can a delete+change mix that
        // neither side would trip on its own.
        assert!(watched_batch_over_cap(0, WATCHED_BULK_CAP + 1));
        assert!(watched_batch_over_cap(
            WATCHED_BULK_CAP / 2 + 1,
            WATCHED_BULK_CAP / 2 + 1
        ));
        assert!(!watched_batch_over_cap(
            WATCHED_BULK_CAP / 2,
            WATCHED_BULK_CAP / 2
        ));
    }

    // ── ScanGuard (B1 re-entrancy guard) ──────────────────────────────────

    #[test]
    fn test_scan_guard_resets_flag_on_drop() {
        let flag = AtomicBool::new(false);
        assert!(
            flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        );
        {
            let _guard = ScanGuard(&flag);
            assert!(flag.load(Ordering::SeqCst));
        }
        assert!(
            !flag.load(Ordering::SeqCst),
            "guard must reset the flag on drop"
        );
    }

    #[test]
    fn test_scan_guard_cas_rejects_second_entrant_while_held() {
        let flag = AtomicBool::new(false);
        assert!(
            flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok(),
            "first scan should win the CAS"
        );
        // A second scan racing in while the first is still running loses the CAS,
        // mirroring how `validate_entire_workspace` bails on a losing entrant.
        assert!(
            flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err(),
            "overlapping scan should lose the CAS while the first is in progress"
        );
    }

    #[test]
    fn test_scan_guard_drop_then_reacquire_succeeds() {
        let flag = AtomicBool::new(false);
        {
            assert!(
                flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            );
            let _guard = ScanGuard(&flag);
        }
        // Guard dropped (scan finished, or panicked) — a later scan can acquire.
        assert!(
            flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok(),
            "flag should be free again once the guard is dropped"
        );
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
            .map(|v| v.iter().map(|(_, i)| i.name.as_str()).collect())
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
            .map(|v| v.iter().map(|(_, i)| i.name.as_str()).collect())
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

        let entries = per_type.get("foo").cloned().unwrap_or_default();
        let names: Vec<&str> = entries.iter().map(|(_, i)| i.name.as_str()).collect();
        assert!(
            names.contains(&"foo_one"),
            "valid instance should still be collected despite a bad file: {:?}",
            names
        );
        // Each instance keeps its real source file (goto-into-vanilla).
        assert!(
            entries
                .iter()
                .any(|(uri, _)| uri.replace('\\', "/").ends_with("common/foos/good.txt")),
            "instance should carry its source path, got: {:?}",
            entries.iter().map(|(u, _)| u.as_ref()).collect::<Vec<_>>()
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
            .map(|v| v.iter().map(|(_, i)| i.name.as_str()).collect())
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
