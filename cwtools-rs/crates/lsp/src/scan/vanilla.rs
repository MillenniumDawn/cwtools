use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tower_lsp::lsp_types::*;

use cwtools_localization::Lang;
use cwtools_rules::rules_types::RuleSet;

use crate::command_progress::CommandProgress;
use crate::paths::{default_cache_dir, discover_vanilla_dir, path_to_uri};
use crate::{Backend, LocLocationMap, LocTextMap};

use super::loc::collect_loc_display;

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

/// The base-game install's share of the loc maps, built once per session by
/// [`Backend::vanilla_loc`] and merged under the freshly-walked workspace on
/// every loc rebuild.
pub(crate) struct VanillaLoc {
    /// Unscoped key index — the merge applies the caller's language scoping,
    /// exactly as the vanilla-cache key merge does.
    pub(crate) index: cwtools_localization::LocIndex,
    /// Hover text, already narrowed to the languages hover shows.
    pub(crate) text: LocTextMap,
    /// Goto-definition sites, one per key.
    pub(crate) locations: LocLocationMap,
}

/// What a [`VanillaLoc`] was built for: the install dir, the primary language
/// and the hover-all-languages toggle. All three come from the initialize
/// options and never change afterwards, but keying on them keeps the memo
/// honest if that ever stops being true.
pub(crate) type VanillaLocKey = (std::path::PathBuf, Lang, bool);

impl VanillaLoc {
    pub(crate) fn build(
        service: &cwtools_localization::LocService,
        primary_lang: Lang,
        hover_all: bool,
    ) -> Self {
        let index = cwtools_localization::LocIndex::build_scoped(service, None);
        let mut text = LocTextMap::default();
        let mut locations = LocLocationMap::default();
        collect_loc_display(
            service,
            &index,
            primary_lang,
            hover_all,
            &mut text,
            &mut locations,
        );
        Self {
            index,
            text,
            locations,
        }
    }
}

impl Backend {
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
            // must set it here too. See rule_core/leaf.rs gate on `idx.complete`.
            info_guard.type_index.complete = true;
            // `vanilla_index` is now None — mark it merged so
            // ensure_vanilla_index does not re-run on the next scan.
            self.state.vanilla_merged.store(true, Ordering::SeqCst);
            drop(info_guard);
            self.bump_info_revision();
        }
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
    ///
    /// `progress` is the command that asked for the index, so its phase report
    /// lands on that command's own stream rather than whichever command started
    /// most recently.
    pub(crate) async fn ensure_vanilla_index(
        &self,
        progress: Option<&CommandProgress>,
        force_rebuild: bool,
        quiet: bool,
    ) {
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
        let was_explicit = explicit_dir.is_some();
        let dir = explicit_dir.or_else(|| discover_vanilla_dir(&game));
        let dir = match dir {
            Some(d) if d.is_dir() => d,
            _ => return,
        };
        // An install we found ourselves has to go back into the config, or the
        // URI access boundary refuses every base-game file this then indexes.
        if !was_explicit {
            let mut cfg = self.state.config.write();
            cfg.vanilla_dir = Some(dir.clone());
            cfg.refresh_roots();
        }

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
            self.send_loading_bar_pct(progress, true, "Indexing base game…", None)
                .await;
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
        Some(base.join(cwtools_info::vanilla_cache::cache_file_name(
            game,
            fingerprint,
        )))
    }
}
