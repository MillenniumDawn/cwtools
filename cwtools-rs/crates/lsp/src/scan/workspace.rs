use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tower_lsp::lsp_types::*;

use cwtools_cache::workspace as workspace_cache;
use cwtools_parser::parser::parse_string_without_comments;
use cwtools_rules::rules_types::RuleSet;
use cwtools_validation::inline_ignore::{InlineIgnoreMap, extract_inline_ignored_codes};
use cwtools_validation::references::{UsedInstances, check_unused_instances, needs_use_tracking};
use cwtools_validation::validate_prepared_tracking_uses;

use crate::Backend;
use crate::command_progress::{
    CommandProgress, Phase, ScanOutcome, cancel_flag_of, phase_percentage, start_phase,
};
use crate::paths::{logical_path_from_uri, path_to_uri, uri_to_path_str};
use crate::validate::{
    DocLines, make_prepared, parse_error_to_diagnostic, validate_parsed_with_indexes,
    validation_error_to_diagnostic,
};

use super::{
    OpenDocSnapshot, ScanGuard, ScannedFile, hold_scan_for_tests, quiet_pass_can_skip,
    spawn_logging_panics, stat_signature_for,
};

impl Backend {
    /// Public entry to the workspace scan. Runs the scan and ALWAYS clears the
    /// status-bar loading indicator on return, regardless of which internal path
    /// exited — including the early returns for an absent/empty workspace, which
    /// previously left the bar spinning on "Indexing workspace…" forever, a
    /// panic inside the scan, and a client cancelling the command that started
    /// it. See [`ScanGuard`].
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
        matches!(
            self.validate_entire_workspace_tracked(quiet, None).await,
            ScanOutcome::Ran
        )
    }

    /// [`validate_entire_workspace`] under a client-cancellable command.
    ///
    /// `progress` supplies the cancel flag the scan polls and the sink its
    /// phase samplers report to; `None` is the startup scan and the periodic
    /// background pass, which nobody can cancel and which report over the
    /// server's own indicator.
    ///
    /// Cancellation is best-effort in one direction only: the scan stops
    /// promptly, but indexing it already did is kept rather than rolled back.
    /// What it must not do is *record* a partial pass — the walk fingerprint
    /// and loc signature are written at the end, past every cancel check, so a
    /// later quiet pass can't short-circuit on work that never finished.
    ///
    /// [`validate_entire_workspace`]: Backend::validate_entire_workspace
    pub(crate) async fn validate_entire_workspace_tracked(
        &self,
        quiet: bool,
        progress: Option<&CommandProgress>,
    ) -> ScanOutcome {
        if self
            .state
            .scan_in_progress
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            tracing::debug!("workspace scan already in progress; skipping");
            return ScanOutcome::Busy;
        }
        let guard = ScanGuard::for_scan(self, quiet);
        let completed = self.validate_entire_workspace_inner(quiet, progress).await;
        guard.finish().await;
        // Drain any watched events an over-cap batch requeued after losing the
        // CAS to this scan — the loser suppresses its own re-arm so it doesn't
        // retry against the flag every window for the scan's whole duration.
        // Each guard scoped to its own `let` so the two queue locks are never
        // held at once.
        let requeued_pending = !self.state.watched_pending.lock().is_empty();
        let requeued_deleted = !self.state.watched_deleted.lock().is_empty();
        if requeued_pending || requeued_deleted {
            self.arm_watched_batch();
        }
        if completed {
            ScanOutcome::Ran
        } else {
            ScanOutcome::Cancelled
        }
    }

    /// Retry a revalidation in the background, bounded, for a caller that gave
    /// up on landing one itself.
    ///
    /// Two callers, both leaving the index in a state that has to be repaired
    /// even though the command is over: `reloadrulesconfig` when a scan holds
    /// the guard past its response bound (the rules are live, so only the
    /// re-validation is outstanding), and `clearAllCaches` when the user
    /// cancels after the purge already dropped the base-game index.
    ///
    /// `context` names the caller in the give-up log line. Bounded at 180s; if
    /// a scan still holds the guard that long, the retry stops and says so in
    /// the output channel instead of spinning forever.
    pub(crate) fn spawn_deferred_revalidation(&self, context: &'static str) {
        let client = self.client.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            spawn_logging_panics("deferred revalidation", async move {
                let backend = Backend { client, state };
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_secs(180);
                let mut revalidated = backend.validate_entire_workspace(false).await;
                while !revalidated && std::time::Instant::now() < deadline {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    revalidated = backend.validate_entire_workspace(false).await;
                }
                if !revalidated {
                    backend
                        .client
                        .log_message(
                            MessageType::WARNING,
                            format!(
                                "{context}: deferred re-validation gave up; a scan held the workspace the whole time"
                            ),
                        )
                        .await;
                }
            })
            .await;
        });
    }

    /// Scan the entire workspace for relevant game files and validate them all.
    ///
    /// Returns `false` when the user cancelled partway. Every `return false`
    /// below sits before the fingerprint/signature writes at the end, so a
    /// cancelled pass records nothing and the next scan redoes it in full.
    #[tracing::instrument(skip_all)]
    async fn validate_entire_workspace_inner(
        &self,
        quiet: bool,
        progress: Option<&CommandProgress>,
    ) -> bool {
        let cancel = cancel_flag_of(progress);
        cwtools_profiling::log_rss("workspace_scan_start");
        if !quiet {
            self.send_loading_bar_pct(progress, true, Phase::Discover.label(), Some(0))
                .await;
        }
        hold_scan_for_tests().await;

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
                return true;
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
                cwtools_file_manager::file_manager::ScanBudget::default(),
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
            return true;
        }

        // First cancel check: the walk is one uninterruptible `block_in_place`,
        // so this is the earliest point the flag can be observed. Nothing has
        // been mutated yet, so bailing here is a true no-op.
        if cancel.is_cancelled() {
            return false;
        }

        let scan_files: Vec<ScannedFile> = files_to_validate
            .into_iter()
            .map(|path| ScannedFile {
                uri: path_to_uri(&path),
                path,
            })
            .collect();

        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "Validating {} workspace files under {:?} ...",
                    scan_files.len(),
                    root_path
                ),
            )
            .await;

        // Resolve the parse-cache directory and settings fingerprint. The
        // fingerprint encodes the game, workspace root, and cache format so
        // stale caches are cleared automatically when any of those change.
        // A rules edit is not one of those: a `.cwb` is a parsed AST.
        let (cache_info, cache_status) = {
            let (cache_dir, language) = {
                let cfg = self.state.config.read();
                (cfg.cache_dir.clone(), cfg.language.clone())
            };
            match cache_dir {
                Some(cd) => {
                    let fp = workspace_cache::settings_fingerprint(&language, &root_path);
                    match workspace_cache::validate_or_clear(&cd, fp) {
                        Ok(true) => (Some((cd, fp)), "Parse cache: hit (settings match)"),
                        Ok(false) => (Some((cd, fp)), "Parse cache: settings changed, cleared"),
                        Err(error) => {
                            tracing::warn!(dir = %cd.display(), error = %error, "parse cache unavailable");
                            (None, "Parse cache: unavailable")
                        }
                    }
                }
                None => (None, "Parse cache: disabled"),
            }
        };
        self.client
            .log_message(MessageType::INFO, cache_status)
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
            self.send_loading_bar_pct(
                progress,
                true,
                Phase::Parse.label(),
                Some(phase_percentage(Phase::Parse, 0, 1)),
            )
            .await;
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
        // corresponds to `scan_files[i]`.
        use rayon::prelude::*;
        // (cache_hit, parsed, source_hash, inline_ignored) per file; None = open
        // doc, parse failure, or read error.
        type ParseOutcome = (
            bool,
            cwtools_parser::ast::ParsedFile,
            Option<u64>,
            InlineIgnoreMap,
        );
        // block_in_place tells tokio this thread is about to do synchronous
        // blocking I/O; the runtime shifts its remaining tasks to other workers
        // so the LSP request loop is not starved while rayon parses.
        let scan_bytes = cwtools_file_manager::file_manager::ScanBytes::new();
        // The rayon section can neither await nor reach the client, so progress
        // rides an atomic the closure bumps and a sampler task turns into
        // `$/progress` traffic. The cancel check is the same shape: a latch the
        // closure polls, so a cancelled pass drains through the remaining files
        // at one atomic load each instead of parsing them.
        let parse_ticker = start_phase(progress, Phase::Parse, scan_files.len());
        let outcomes: Vec<Option<ParseOutcome>> = tokio::task::block_in_place(|| {
            scan_files
                .par_iter()
                .map(|file| {
                    if cancel.is_cancelled() {
                        return None;
                    }
                    parse_ticker.tick();
                    // Open docs are already indexed from their in-memory text;
                    // skip so we don't re-index stale disk content on top of the
                    // live version.
                    if open_uris.contains(&file.uri) {
                        return None;
                    }
                    if let Some((ref cd, fp)) = cache_info
                        && let Some((parsed, source_key)) = workspace_cache::load_path(
                            cd,
                            fp,
                            &file.path,
                            &self.state.string_table,
                        )
                    {
                        // The text is read to re-check the metadata key; the
                        // inline-ignore directives come from that same read.
                        let (source_hash, inline_ignored) =
                            match cwtools_file_manager::file_manager::read_text_capped(
                                &file.path,
                                crate::access::MAX_URI_READ_BYTES,
                            ) {
                                Ok((text, _)) => (
                                    (workspace_cache::source_cache_key(&file.path).as_ref()
                                        == Some(&source_key))
                                    .then(|| cwtools_cache::workspace::content_hash(&text)),
                                    extract_inline_ignored_codes(&text),
                                ),
                                Err(_) => (None, InlineIgnoreMap::new()),
                            };
                        return Some((true, parsed, source_hash, inline_ignored));
                    }
                    let source_key = (workspace_cache::PATH_METADATA_CACHE_SUPPORTED)
                        .then(|| workspace_cache::source_cache_key(&file.path))
                        .flatten();
                    let use_content_cache =
                        !workspace_cache::PATH_METADATA_CACHE_SUPPORTED || source_key.is_none();
                    // Read via the file manager so cp1252-encoded script files
                    // (pre-Jomini mods) are indexed instead of silently dropped.
                    // The read is capped and counted against the scan byte budget.
                    let text = match cwtools_file_manager::file_manager::read_text_capped(
                        &file.path,
                        crate::access::MAX_URI_READ_BYTES,
                    ) {
                        Ok((t, n)) => {
                            if !scan_bytes.try_reserve(
                                n,
                                cwtools_file_manager::file_manager::ScanBudget::default().max_bytes,
                            ) {
                                tracing::warn!(
                                    path = %file.path.display(),
                                    "scan: skipping file, byte budget exceeded"
                                );
                                return None;
                            }
                            t
                        }
                        Err(e) => {
                            tracing::warn!(path = %file.path.display(), error = %e, "scan: skipping unreadable file");
                            return None;
                        }
                    };
                    if use_content_cache
                        && let Some((cd, fp)) = cache_info.as_ref()
                        && let Some(parsed) = workspace_cache::load(
                            cd,
                            *fp,
                            &text,
                            &self.state.string_table,
                        )
                    {
                        return Some((
                            true,
                            parsed,
                            Some(cwtools_cache::workspace::content_hash(&text)),
                            extract_inline_ignored_codes(&text),
                        ));
                    }
                    let parsed = parse_string_without_comments(&text, &self.state.string_table);
                    if let Some((cd, fp)) = cache_info.as_ref() {
                        if let Some(source_key) = source_key.as_ref() {
                            workspace_cache::store_path(
                                cd,
                                *fp,
                                &file.path,
                                source_key,
                                &parsed,
                                &self.state.string_table,
                            );
                        } else {
                            workspace_cache::store(
                                cd,
                                *fp,
                                &text,
                                &parsed,
                                &self.state.string_table,
                            );
                        }
                    }
                    Some((
                        false,
                        parsed,
                        Some(cwtools_cache::workspace::content_hash(&text)),
                        extract_inline_ignored_codes(&text),
                    ))
                })
                .collect()
        });
        parse_ticker.stop();
        // A cancelled parse pass produced a hole-ridden `outcomes`, and every
        // later phase (the index merge, the prune, validation) would read it as
        // "these files have nothing in them". Stop before any of that lands.
        if cancel.is_cancelled() {
            return false;
        }
        let wrote_cache = outcomes
            .iter()
            .any(|outcome| outcome.as_ref().is_some_and(|(hit, _, _, _)| !hit));
        if wrote_cache && let Some((cache_dir, fingerprint)) = cache_info.as_ref() {
            workspace_cache::prune(cache_dir, *fingerprint);
        }

        // Serial index phase, in file order.
        let mut parsed_files: Vec<Option<cwtools_parser::ast::ParsedFile>> =
            Vec::with_capacity(scan_files.len());
        let mut source_hashes: Vec<Option<u64>> = Vec::with_capacity(scan_files.len());
        let mut inline_ignores: Vec<InlineIgnoreMap> = Vec::with_capacity(scan_files.len());
        for (i, (file, outcome)) in scan_files.iter().zip(outcomes).enumerate() {
            let parsed = match outcome {
                Some((cache_hit, parsed, source_hash, inline_ignored)) => {
                    self.index_parsed_file(&file.uri, &parsed, None);
                    if cache_hit {
                        cache_hits += 1;
                    } else {
                        cache_misses += 1;
                    }
                    source_hashes.push(source_hash);
                    inline_ignores.push(inline_ignored);
                    Some(parsed)
                }
                None => {
                    source_hashes.push(None);
                    inline_ignores.push(InlineIgnoreMap::new());
                    None
                }
            };
            parsed_files.push(parsed);
            // A quiet background pass shares the runtime with live requests
            // (hover, completion, did_change); yield periodically through this
            // serial loop so it doesn't hog a worker thread for the whole
            // index phase. Mirrors pass 2's yield-every-50 below.
            if quiet && i % 64 == 63 {
                tokio::task::yield_now().await;
            }
            // Same cadence for the cancel check. The merge is cheap next to
            // parsing, so this rarely fires — but on a mod big enough that the
            // user reached for Cancel, "cheap" is still seconds.
            if i % 64 == 63 && cancel.is_cancelled() {
                return false;
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
            scan_files.iter().map(|file| file.uri.clone()).collect();
        // An empty walk almost always means the root was transiently
        // unreadable — walk_workspace_files swallows I/O errors and returns an
        // empty Vec — not that the user deleted every file. Pruning against an
        // empty set would wipe the whole index on a hiccup, so skip it; real
        // deletions still arrive as per-file DELETE watched events.
        let removed_uris: Vec<String> = if scan_files.is_empty() {
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
            // both loc overlays are keyed per-file too and must forget the
            // same URIs, or loc checks keep serving stale entries for a file
            // `info_service` just dropped.
            {
                let mut overlay = self.loc_live_overlay_mut();
                for uri in &removed_uris {
                    overlay.remove(uri);
                }
            }
            {
                let mut watched = self.loc_watched_overlay_mut();
                for uri in &removed_uris {
                    watched.remove(uri);
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
                    self.publish_filtered(uri_obj, vec![], None, None).await;
                }
            }
        }

        if cancel.is_cancelled() {
            return false;
        }

        // Build the base-game index from a `vanilla` dir (or auto-discovery) if
        // we have one and haven't indexed it yet. Populates `vanilla_index`.
        //
        // This phase is the one that can't be interrupted mid-flight: the base
        // game is indexed by a single `spawn_blocking` call into the engine, so
        // there is no per-file seam to poll the flag at. Cancelling during it
        // takes effect when it returns.
        if !quiet {
            self.send_loading_bar_pct(
                progress,
                true,
                Phase::Vanilla.label(),
                Some(phase_percentage(Phase::Vanilla, 0, 1)),
            )
            .await;
        }
        self.ensure_vanilla_index(progress, false, quiet).await;
        if cancel.is_cancelled() {
            return false;
        }

        // Merge the pre-generated vanilla index (if loaded) so base-game
        // references resolve. Walks the workspace root for the file index, so
        // it runs off the executor.
        tokio::task::block_in_place(|| self.merge_pending_vanilla_index());

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
        if !quiet {
            self.send_loading_bar_pct(
                progress,
                true,
                Phase::Localisation.label(),
                Some(phase_percentage(Phase::Localisation, 0, 1)),
            )
            .await;
        }
        let loc_signature = tokio::task::block_in_place(|| self.compute_loc_signature(&root_path));
        let loc_unchanged = *self.state.last_loc_signature.lock() == Some(loc_signature);
        if quiet && loc_unchanged {
            tracing::info!("quiet scan: loc signature unchanged, skipping loc rebuild");
        } else {
            self.rebuild_and_publish_loc(&root_path).await;
        }
        // Recorded only on a pass that got this far. A cancel between the
        // rebuild and here would otherwise pin a signature for an index the
        // scan never finished assembling, and the next quiet pass would trust
        // it and skip the rebuild.
        if cancel.is_cancelled() {
            return false;
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
            self.send_loading_bar_pct(
                progress,
                true,
                Phase::Validate.label(),
                Some(phase_percentage(Phase::Validate, 0, 1)),
            )
            .await;
        }
        let mut total_errors = 0usize;
        let total_files = scan_files.len();
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
        // Whether this config tracks `<type>` uses at all (a `should_be_used`
        // type, or Stellaris technologies). When it does, pass 2 doubles as the
        // batch driver's two-phase unused-instance pass: every file's uses are
        // recorded, merged into the store the per-edit path keeps current, and
        // CW239/CW231 appended per file against the merged view.
        let track_uses = scan_ruleset
            .as_ref()
            .is_some_and(|rs| needs_use_tracking(rs, scan_game));
        // Cached ASTs of the open docs, snapshotted BEFORE the info guard below
        // (request handlers lock `documents` before the info service; taking
        // them in the other order here would be an ABBA deadlock). Both scan
        // passes skip open docs, but their `<type>` uses still count: without
        // them a definition referenced only from an open buffer would scan up
        // as unused.
        let open_doc_asts: Vec<(String, Arc<cwtools_parser::ast::ParsedFile>)> = if track_uses {
            let docs = self.state.documents.lock();
            docs.iter()
                .filter_map(|(u, d)| d.ast.clone().map(|ast| (u.clone(), ast)))
                .collect()
        } else {
            Vec::new()
        };
        type ValidationOutcome = (
            String,
            Vec<Diagnostic>,
            Option<UsedInstances>,
            Option<u64>,
            InlineIgnoreMap,
        );
        // Snapshot the indexes so the CPU-bound rayon phase holds no
        // `info_service` / `loc_index` read guards. A concurrent keystroke
        // needs `info_service.write()` (validate.rs) and previously blocked
        // for the whole validate phase; the snapshot is taken under brief
        // read guards and the remainder runs lock-free, matching the loc
        // rebuild pointer-swap shape (build outside locks, install with swap).
        // Chunking the rayon work yields to the async runtime between chunks
        // so cancellation and progress remain responsive on large mods.
        let type_index_snap = self.state.info_service.read().type_index.clone();
        let loc_index_snap = self.state.loc_index.read().clone();
        let validate_ticker = start_phase(progress, Phase::Validate, scan_files.len());
        const PASS2_CHUNK: usize = 256;
        let registry = scan_registry.as_ref();
        let prepared = scan_ruleset.as_ref().map(|ruleset| {
            make_prepared(
                ruleset,
                &self.state.string_table,
                scan_game,
                &type_index_snap,
                &modifier_keys_snap,
                loc_index_snap.as_ref(),
                None,
                registry,
                scope_checks,
                var_checks,
            )
        });
        let mut chunked_results: Vec<ValidationOutcome> = Vec::with_capacity(scan_files.len());
        let mut cancelled = false;
        for chunk_start in (0..scan_files.len()).step_by(PASS2_CHUNK) {
            if cancel.is_cancelled() {
                cancelled = true;
                break;
            }
            let chunk_end = (chunk_start + PASS2_CHUNK).min(scan_files.len());
            let chunk_results: Vec<ValidationOutcome> = scan_files[chunk_start..chunk_end]
                .par_iter()
                .zip(parsed_files[chunk_start..chunk_end].par_iter())
                .zip(source_hashes[chunk_start..chunk_end].par_iter())
                .zip(inline_ignores[chunk_start..chunk_end].par_iter())
                .filter_map(|(((file, parsed_opt), source_hash), inline_ignored)| {
                    if cancel.is_cancelled() {
                        return None;
                    }
                    validate_ticker.tick();
                    let parsed = parsed_opt.as_ref()?;
                    if open_uris.contains(&file.uri) {
                        return None;
                    }
                    let no_lines = DocLines::none();
                    let (diagnostics, used) = match &prepared {
                        Some(prepared) => validate_parsed_with_indexes(
                            &file.uri, parsed, prepared, &no_lines, track_uses,
                        ),
                        None => (
                            parsed
                                .errors
                                .iter()
                                .map(|e| parse_error_to_diagnostic(e, &no_lines))
                                .collect(),
                            None,
                        ),
                    };
                    Some((
                        file.uri.clone(),
                        diagnostics,
                        used,
                        *source_hash,
                        inline_ignored.clone(),
                    ))
                })
                .collect();
            // `filter_map` returns `None` for cancelled files, so a mid-chunk
            // cancel shows up as a short chunk; check the flag before the next
            // chunk rather than inferring from length.
            chunked_results.extend(chunk_results);
            // Yield to the async runtime between chunks so an `info_service.write()`
            // waiter (keystroke) and `validate_ticker` progress can interleave.
            // The rayon phase itself holds no index locks (snapshot), so this
            // yield is for cancellation / progress, not lock release — the
            // snapshot creation above was the only guarded window.
            tokio::task::yield_now().await;
            if cancel.is_cancelled() {
                cancelled = true;
                break;
            }
        }
        if cancelled {
            validate_ticker.stop();
            return false;
        }
        let mut results: Vec<ValidationOutcome> = chunked_results;
        // Unused-instance second phase: same global merge as before, but against
        // the snapshot's `type_index_snap` so it doesn't re-acquire the lock.
        // Skipped on cancel: a partial `results` would prune unscanned files.
        if track_uses
            && !cancel.is_cancelled()
            && let Some(prepared) = &prepared
        {
            let unrecorded: Vec<(String, Arc<cwtools_parser::ast::ParsedFile>)> = {
                let store = self.state.type_uses.read();
                open_doc_asts
                    .iter()
                    .filter(|(u, _)| !store.contains_key(u))
                    .cloned()
                    .collect()
            };
            let open_uses: Vec<(String, UsedInstances)> = unrecorded
                .par_iter()
                .map(|(u, ast)| {
                    let (_, used) = validate_prepared_tracking_uses(ast, u, prepared);
                    (u.clone(), used)
                })
                .collect();
            let merged = {
                let mut store = self.state.type_uses.write();
                store.retain(|uri, _| open_uris.contains(uri));
                for (uri, _, used, _, _) in &mut results {
                    store.insert(uri.clone(), used.take().unwrap_or_default());
                }
                for (uri, used) in open_uses {
                    store.insert(uri, used);
                }
                let mut merged = UsedInstances::default();
                for uses in store.values() {
                    merged.merge_from(uses);
                }
                merged
            };
            self.state
                .type_uses_revision
                .fetch_add(1, Ordering::Release);
            let no_lines = DocLines::none();
            for (uri, diagnostics, _, _, _) in &mut results {
                let file: cwtools_validation::FilePath = uri.as_str().into();
                for err in check_unused_instances(
                    prepared.ruleset,
                    scan_game,
                    &type_index_snap.instances_in_file(uri),
                    &merged,
                    &file,
                ) {
                    diagnostics.push(validation_error_to_diagnostic(&err, &no_lines));
                }
            }
        }
        let results: Vec<(String, Vec<Diagnostic>, Option<u64>, InlineIgnoreMap)> = results
            .into_iter()
            .map(|(uri, diagnostics, _, source_hash, inline_ignored)| {
                (uri, diagnostics, source_hash, inline_ignored)
            })
            .collect();
        validate_ticker.stop();
        if cancel.is_cancelled() {
            return false;
        }
        let publish_total = results.len();
        for (i, (uri, mut diagnostics, source_hash, inline_ignored)) in
            results.into_iter().enumerate()
        {
            // Inline `# cwtools-ignore` directives, before the error count and
            // the publish so both see the same set the editor's Problems panel
            // gets.
            crate::validate::drop_inline_suppressed(&mut diagnostics, &inline_ignored);
            total_errors += diagnostics
                .iter()
                .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
                .count();

            if let Ok(uri_obj) = Url::parse(&uri) {
                self.publish_filtered(uri_obj, diagnostics, None, source_hash)
                    .await;
            }
            if i % 50 == 49 {
                tokio::task::yield_now().await;
                // Publishing is serial and async, so unlike the rayon passes it
                // reports for itself rather than through a sampler.
                if cancel.is_cancelled() {
                    return false;
                }
                if !quiet {
                    self.send_loading_bar_pct(
                        progress,
                        true,
                        Phase::Publish.label(),
                        Some(phase_percentage(Phase::Publish, i + 1, publish_total)),
                    )
                    .await;
                }
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
        let file_list: Vec<serde_json::Value> = scan_files
            .iter()
            .map(|file| {
                let logical_path = logical_path_from_uri(&file.uri, &ws_prefix);
                let scope = logical_path
                    .split('/')
                    .next()
                    .unwrap_or("unknown")
                    .to_string();
                serde_json::json!({
                    "scope": scope,
                    "uri": file.uri.clone(),
                    "logicalpath": logical_path
                })
            })
            .collect();
        self.send_update_file_list(file_list).await;

        // Never record for an empty walk: a transiently-unreadable root would
        // otherwise pin a bogus fingerprint and suppress the recovery pass.
        if !scan_files.is_empty() {
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
        // Type index changed globally; semantic tokens cached under the old
        // ruleset are stale and would patch incorrectly via delta. Invalidate
        // and ask the client to re-request visible files (#184).
        self.invalidate_all_semantic_tokens();
        self.request_semantic_refresh().await;
        // The status bar is cleared by the `validate_entire_workspace` wrapper on
        // return, so every exit path (this one and the early returns above) clears
        // it uniformly.
        true
    }

    /// Re-validate every currently-open document against the current (complete)
    /// index and re-publish, skipping any whose version changed meanwhile. Called
    /// once after the workspace scan so open docs validated against a partial
    /// index don't keep stale cross-file diagnostics, and on a live
    /// `didChangeConfiguration` so a changed suppression list re-filters at once.
    pub(crate) async fn revalidate_all_open_docs(&self, trigger: crate::ValidateTrigger) {
        let Ok(_validation_permit) = self.state.validation_permits.acquire().await else {
            return;
        };
        // Snapshot each open doc's uri/text/version, plus its cached AST *only
        // when that AST matches the current version*. A current AST lets us
        // re-validate against the now-complete index via the prebuilt path — no
        // re-parse, no re-index — the same route the dependent sweep takes.
        //
        // Skipping the re-index is sound because the scan never touches open
        // docs: pass 1 skips `open_uris`, and the on-disk prune excludes them
        // too, so an open doc's index entry still reflects exactly the content
        // its cached AST was parsed from (written by did_open / did_change via
        // index_parsed_file, kept in lock-step with `ast_version`). Only the
        // *global* index grew — the other workspace files the scan indexed —
        // which is precisely what we want the open doc re-validated against.
        //
        // A stale AST (a mid-edit fatal parse, or an edit whose debounce hasn't
        // run so `ast_version` < `version`) or none at all (loc/.cwt files, or a
        // never-parsed doc) falls back to the full parse path, which re-parses
        // the current text and re-indexes — so its parse-error diagnostics and
        // loc/.cwt handling stay identical to the prior behavior.
        let open_docs: Vec<OpenDocSnapshot> = {
            let docs = self.state.documents.lock();
            docs.iter()
                .map(|(uri, doc)| {
                    let current_ast = match &doc.ast {
                        Some(ast) if doc.ast_version == Some(doc.version) => Some(ast.clone()),
                        _ => None,
                    };
                    (uri.clone(), doc.text.clone(), doc.version, current_ast)
                })
                .collect()
        };
        // Ruleset family snapshotted once for the whole pass (none of it changes
        // while we run); mirrors the workspace scan's pass-2 snapshot so the
        // prebuilt validations share one clone instead of re-locking per doc.
        let (game, encoding) = {
            let cfg = self.state.config.read();
            (cfg.game(), cfg.position_encoding.clone())
        };
        let (ruleset_snap, registry_snap, modifier_keys_snap) = {
            let rules = self.state.rules.read();
            (
                rules.ruleset.clone(),
                rules.scope_registry.clone(),
                rules.modifier_keys.clone(),
            )
        };
        for (uri, text, version, current_ast) in open_docs {
            if self.is_ignored_uri(&uri) {
                self.clear_ignored_file_state(&uri);
                self.update_doc_tokens(&uri, None);
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
                    self.publish_filtered(
                        uri_obj,
                        Vec::new(),
                        Some(version),
                        Some(cwtools_cache::workspace::content_hash(&text)),
                    )
                    .await;
                }
                continue;
            }
            let diagnostics = match current_ast {
                Some(ast) => {
                    // Prebuilt path: validate the stored AST against the complete
                    // index. `validate_parsed_prebuilt` prepends the AST's own
                    // parse errors and applies the same MAX_FILE_ERRORS truncation
                    // and loc-overlay handling the full path uses. Emit the
                    // `[validate] (trigger)` profiling line the full path would
                    // (so per-doc revalidation stays observable in the log),
                    // tagged `prebuilt` to make the no-reparse route legible.
                    let lines = DocLines::new(&text, encoding.clone());
                    let diags = match ruleset_snap.as_ref() {
                        Some(ruleset) => self.validate_parsed_prebuilt(
                            &uri,
                            &ast,
                            &modifier_keys_snap,
                            ruleset,
                            game,
                            registry_snap.as_ref(),
                            &lines,
                        ),
                        None => ast
                            .errors
                            .iter()
                            .map(|e| parse_error_to_diagnostic(e, &lines))
                            .collect(),
                    };
                    tracing::info!(
                        target: "cwtools::profile",
                        "[validate] ({}) {} diagnostics (prebuilt, no reparse)",
                        trigger.as_str(),
                        diags.len()
                    );
                    diags
                }
                // No current AST — re-parse + re-index via the full path,
                // preserving prior behavior for loc/.cwt files and mid-edit
                // fatal parses (which must re-parse to surface the live error).
                None => {
                    self.parse_and_validate(&uri, &text, trigger, Some(version))
                        .await
                        .0
                }
            };
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
                self.publish_filtered(
                    uri_obj,
                    diagnostics,
                    Some(version),
                    Some(cwtools_cache::workspace::content_hash(&text)),
                )
                .await;
            }
        }
    }
}
