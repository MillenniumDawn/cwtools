use std::collections::HashSet;
use std::sync::Arc;

use tower_lsp::lsp_types::*;

use cwtools_parser::ast::{ParseError, ParsedFile};
use cwtools_parser::parser::parse_string;
use cwtools_rules::rules_types::RuleSet;
use cwtools_validation::{Prepared, ValidationError, validate_prepared};

use crate::Backend;
use crate::paths::{logical_path_from_uri, uri_to_path_str};

#[allow(clippy::too_many_arguments)]
pub(crate) fn make_prepared<'a>(
    ruleset: &'a cwtools_rules::rules_types::RuleSet,
    table: &'a cwtools_string_table::string_table::StringTable,
    game: Option<cwtools_game::constants::Game>,
    type_index: &'a cwtools_info::TypeIndex,
    modifier_keys: &'a std::collections::HashSet<String>,
    loc_index: Option<&'a cwtools_localization::LocIndex>,
    registry: Option<&'a std::sync::Arc<cwtools_game::scope_registry::ScopeRegistry>>,
    scope_checks: bool,
    var_checks: bool,
) -> Prepared<'a> {
    Prepared {
        ruleset,
        table,
        game,
        type_index: Some(type_index),
        modifier_keys: Some(modifier_keys),
        loc_index,
        registry,
        scope_checks,
        var_checks,
    }
}

/// Per-file diagnostic cap. Beyond this, a file's errors are truncated with a
/// summary marker so one broken file can't flood the editor.
pub(crate) const MAX_FILE_ERRORS: usize = 100;

/// Convert a loc-file diagnostic into a `ValidationError` so it shares the
/// `validation_error_to_diagnostic` rendering path. Loc positions are 1-based;
/// `ValidationError.col` is 0-based (used directly by the renderer).
pub(crate) fn loc_diag_to_validation_error(
    d: &cwtools_localization::LocDiagnostic,
) -> ValidationError {
    let severity = match d.severity {
        cwtools_localization::LocSeverity::Error => cwtools_validation::ErrorSeverity::Error,
        cwtools_localization::LocSeverity::Warning => cwtools_validation::ErrorSeverity::Warning,
        cwtools_localization::LocSeverity::Information => {
            cwtools_validation::ErrorSeverity::Information
        }
    };
    ValidationError {
        message: d.message.clone(),
        severity,
        line: d.line as u32,
        col: d.col.saturating_sub(1) as u16,
        file: d.file.clone(),
        code: Some(d.code.to_string()),
    }
}

/// Cap a file's validation errors at [`MAX_FILE_ERRORS`], appending a summary
/// marker for the remainder. Returns the pre-truncation total (for logging).
/// Shared by the batch and single-file paths so the cap stays consistent.
pub(crate) fn truncate_validation_errors(
    errs: &mut Vec<cwtools_validation::ValidationError>,
    uri: &str,
) -> usize {
    let total = errs.len();
    if total > MAX_FILE_ERRORS {
        errs.truncate(MAX_FILE_ERRORS);
        errs.push(cwtools_validation::ValidationError {
            message: format!(
                "... {} additional errors truncated",
                total - MAX_FILE_ERRORS
            ),
            severity: cwtools_validation::ErrorSeverity::Information,
            line: 0,
            col: 0,
            file: uri.to_string(),
            code: None,
        });
    }
    total
}

/// Validate one already-parsed file against a caller-supplied [`Prepared`],
/// returning LSP diagnostics. The prebuilt state is passed in (not re-locked
/// here) so the full-workspace pass can take its read guards once and share the
/// `Prepared` across rayon threads — it is `Copy` and all-borrows, so `Sync`.
pub(crate) fn validate_parsed_with_indexes(
    uri: &str,
    parsed: &ParsedFile,
    prepared: &Prepared,
) -> Vec<Diagnostic> {
    let mut diagnostics: Vec<Diagnostic> = parsed
        .errors
        .iter()
        .map(parse_error_to_diagnostic)
        .collect();
    let mut errs = validate_prepared(parsed, uri, prepared);
    // CW100: objects defined here whose `## required` localisation keys aren't
    // provided by any loc file. Gated on the loc index being built — before the
    // initial scan finishes it's empty and everything would falsely report
    // missing.
    if let Some(loc) = prepared.loc_index
        && !loc.union().is_empty()
    {
        errs.extend(cwtools_validation::missing_loc::check_missing_localisation(
            parsed,
            uri,
            uri,
            prepared.ruleset,
            prepared.table,
            |k| loc.exists_any(k),
        ));
    }
    truncate_validation_errors(&mut errs, uri);
    for err in &errs {
        diagnostics.push(validation_error_to_diagnostic(err));
    }
    diagnostics
}

pub(crate) fn parse_error_to_diagnostic(e: &ParseError) -> Diagnostic {
    let (line, col, msg) = match e {
        ParseError::Pos(_f, line, col, msg) => (line.saturating_sub(1), *col as u32, msg.clone()),
        ParseError::General(msg) => (0, 0, msg.clone()),
    };
    Diagnostic {
        range: Range {
            start: Position {
                line,
                character: col,
            },
            end: Position {
                line,
                character: col + 1,
            },
        },
        severity: Some(DiagnosticSeverity::ERROR),
        code: None,
        code_description: None,
        source: Some("cwtools".to_string()),
        message: msg,
        related_information: None,
        tags: None,
        data: None,
    }
}

pub(crate) fn validation_error_to_diagnostic(err: &ValidationError) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line: err.line.saturating_sub(1),
                character: err.col as u32,
            },
            end: Position {
                line: err.line.saturating_sub(1),
                character: err.col as u32 + 1,
            },
        },
        severity: match err.severity {
            cwtools_validation::ErrorSeverity::Error => Some(DiagnosticSeverity::ERROR),
            cwtools_validation::ErrorSeverity::Warning => Some(DiagnosticSeverity::WARNING),
            cwtools_validation::ErrorSeverity::Information => Some(DiagnosticSeverity::INFORMATION),
            cwtools_validation::ErrorSeverity::Hint => Some(DiagnosticSeverity::HINT),
        },
        code: err
            .code
            .as_deref()
            .map(|c| NumberOrString::String(c.to_string())),
        code_description: None,
        source: Some("cwtools".to_string()),
        message: err.message.clone(),
        related_information: None,
        tags: None,
        data: None,
    }
}

/// Collect the lowercased identifier-like tokens a parsed file mentions: every
/// key and every (quoted or unquoted) string value, plus key/value prefixes.
/// Used by the dependent sweep to decide which open docs reference a changed
/// export. Deliberately broad (an over-approximation): including a token that
/// isn't really a cross-file reference only costs an extra revalidation, while
/// missing one would silently skip a file that should be revalidated.
pub(crate) fn collect_doc_tokens(
    ast: &ParsedFile,
    table: &cwtools_string_table::string_table::StringTable,
) -> HashSet<String> {
    use cwtools_parser::ast::Value;
    let mut tokens = HashSet::new();
    let push = |id: cwtools_string_table::string_table::StringId, set: &mut HashSet<String>| {
        if let Some(s) = table.get_string(id)
            && !s.is_empty()
        {
            set.insert(s);
        }
    };
    // The arena holds every element flatly, so iterating the per-kind vectors
    // covers the whole tree without a recursive walk. `.lower` is the canonical
    // lowercased form, so the resulting set is already case-folded.
    let arena = &ast.arena;
    for leaf in &arena.leaves {
        push(leaf.key.lower, &mut tokens);
        if let Value::String(t) | Value::QString(t) = &leaf.value {
            push(t.lower, &mut tokens);
        }
    }
    for lv in &arena.leaf_values {
        if let Value::String(t) | Value::QString(t) = &lv.value {
            push(t.lower, &mut tokens);
        }
    }
    for vc in &arena.value_clauses {
        for k in &vc.keys {
            push(k.lower, &mut tokens);
        }
    }
    tokens
}

impl Backend {
    /// Parse a file and add it to the symbol + info (type) indexes WITHOUT
    /// validating. The first pass of a full-workspace scan calls this for every
    /// file so cross-file references (scripted triggers/effects, type instances,
    /// templated modifiers) resolve before ANY file is validated. Without this,
    /// a file validated early can't see definitions that live in later files.
    ///
    /// This is synchronous — the original async wrapper was removed because the
    /// body never `.await`s and `block_in_place` callers need a sync variant.
    pub(crate) fn index_document_sync(&self, uri: &str, text: &str) -> Option<ParsedFile> {
        let parsed = parse_string(text, &self.state.string_table).ok()?;
        self.index_parsed_file(uri, &parsed);
        Some(parsed)
    }

    /// Refresh the per-document token set used to scope the dependent sweep.
    /// `ast = None` (e.g. a file that failed to parse) clears the set, so the
    /// sweep treats the doc as "unknown" and always includes it.
    pub(crate) fn update_doc_tokens(&self, uri: &str, ast: Option<&Arc<ParsedFile>>) {
        // Build the token set BEFORE taking the write lock. collect_doc_tokens
        // walks the whole arena; holding doc_tokens.write() across it blocks the
        // dependent sweep's readers (doc_tokens.read()) for the whole walk.
        match ast {
            Some(ast) => {
                let toks = collect_doc_tokens(ast, &self.state.string_table);
                self.state.doc_tokens.write().insert(uri.to_string(), toks);
            }
            None => {
                self.state.doc_tokens.write().remove(uri);
            }
        }
    }

    /// Index an already-parsed AST into the symbol + info indexes. Extracted
    /// from `index_document` so the workspace scan can index cache-hit ASTs
    /// without re-parsing.
    pub(crate) fn index_parsed_file(&self, uri: &str, parsed: &ParsedFile) {
        {
            let mut index = self.state.symbol_index.lock();
            index.clear_document(uri);
            index.index_document(uri, parsed, &self.state.string_table);
        }
        let ws_uri = self.state.config.read().workspace_uri.clone();
        let logical_path = logical_path_from_uri(uri, &ws_uri);
        // Lock order: rules -> info_service.
        let rules_guard = self.state.rules.read();
        let mut info = self.state.info_service.write();
        info.clear_file(uri);
        if let Some(ruleset) = rules_guard.ruleset.as_ref() {
            info.index_file_with_path(
                uri,
                parsed,
                &self.state.string_table,
                ruleset,
                &logical_path,
            );
        }
    }

    /// Validate an already-parsed document against the (already-built) workspace
    /// index, with the ruleset already locked and the per-run scope registry
    /// prebuilt by the caller. Multi-file callers (the workspace scan, the
    /// dependent sweep) build those ONCE outside their loop and reuse them.
    pub(crate) fn validate_parsed_prebuilt(
        &self,
        uri: &str,
        parsed: &ParsedFile,
        modifier_keys: &std::collections::HashSet<String>,
        ruleset: &RuleSet,
        game: Option<cwtools_game::constants::Game>,
        registry: Option<&std::sync::Arc<cwtools_game::scope_registry::ScopeRegistry>>,
    ) -> Vec<Diagnostic> {
        let info_guard = self.state.info_service.read();
        let loc_guard = self.state.loc_index.read();
        let (scope_checks, var_checks) = {
            let cfg = self.state.config.read();
            (cfg.scope_checks, cfg.var_checks)
        };
        let prepared = make_prepared(
            ruleset,
            &self.state.string_table,
            game,
            &info_guard.type_index,
            modifier_keys,
            loc_guard.as_ref(),
            registry,
            scope_checks,
            var_checks,
        );
        validate_parsed_with_indexes(uri, parsed, &prepared)
    }

    /// Publish diagnostics, but suppress them (publish an empty set) until the
    /// initial workspace index is ready. Before the index is built, a cross-file
    /// reference whose defining file isn't indexed yet would be flagged as
    /// undefined; the scan publishes the real diagnostics once it completes.
    pub(crate) async fn publish_gated(
        &self,
        uri: tower_lsp::lsp_types::Url,
        diagnostics: Vec<Diagnostic>,
        version: Option<i32>,
    ) {
        let ready = self
            .state
            .index_ready
            .load(std::sync::atomic::Ordering::Relaxed);
        let diags = if ready { diagnostics } else { Vec::new() };
        self.client.publish_diagnostics(uri, diags, version).await;
    }

    /// Parse and validate a single document.
    /// Validate `uri` at `expected_version` after the debounce, but only if it is
    /// still the latest edit (a newer change supersedes it). Publishes the
    /// changed file's diagnostics, then refreshes the other open documents so
    /// cross-file references reflect the edit instead of showing stale results.
    #[tracing::instrument(skip_all, fields(uri = %uri, version = expected_version))]
    pub(crate) async fn debounced_validate(
        &self,
        uri: String,
        expected_version: i32,
        generation: u64,
    ) {
        // A newer change landed during the debounce — let that one validate.
        let text = {
            let docs = self.state.documents.lock();
            match docs.get(&uri) {
                Some(d) if d.version == expected_version => d.text.clone(),
                _ => return,
            }
        };

        // Snapshot the file's cross-file exports before re-indexing, so we can
        // tell whether this edit can affect any other file (see below). The
        // name set lets the dependent sweep target only docs that reference a
        // name that changed.
        let (exports_before, names_before) = {
            let info = self.state.info_service.read();
            (info.export_fingerprint(&uri), info.export_names(&uri))
        };

        let (diagnostics, parsed) = self.parse_and_validate(&uri, &text).await;
        {
            let ast = parsed.map(Arc::new);
            // Update tokens before taking documents lock (doc_tokens must be
            // acquired before documents everywhere to avoid ABBA deadlock with
            // revalidate_open_dependents, which takes doc_tokens then documents).
            self.update_doc_tokens(&uri, ast.as_ref());
            let mut docs = self.state.documents.lock();
            // TOCTOU guard: did_close may have arrived while parse_and_validate
            // was running. Only store the AST if the document is still open at
            // the same version; if it closed, the index was already cleaned up by
            // did_close and we must not re-populate or re-publish it.
            if let Some(d) = docs.get_mut(&uri)
                && d.version == expected_version
            {
                d.ast = ast;
            } else {
                // Doc closed (or version changed) — discard results entirely.
                return;
            }
        }
        if let Ok(uri_obj) = Url::parse(&uri) {
            self.publish_gated(uri_obj, diagnostics, Some(expected_version))
                .await;
        }

        // Only sweep the other open files if this edit actually changed what the
        // file exports (a definition added/renamed/removed). Editing inside a
        // rule body leaves the exports identical, so no dependent can change and
        // the sweep is skipped entirely — the common case stays cheap.
        let (exports_after, names_after) = {
            let info = self.state.info_service.read();
            (info.export_fingerprint(&uri), info.export_names(&uri))
        };
        if exports_before != exports_after {
            // Only the names that were added or removed can change another
            // file's diagnostics. Revalidate the open docs that reference any of
            // them (symmetric difference of the before/after name sets).
            //
            // The fingerprint also tracks multiplicity, so it can differ while
            // the name SET is unchanged (e.g. a duplicate definition added, or a
            // type changed under the same name) — a case that can still flip a
            // dependent's diagnostic. When that happens `changed_names` is empty;
            // fall back to `None` (revalidate every dependent) so we never miss
            // one. Soundness beats scoping here.
            let mut changed_names: HashSet<String> = names_before
                .symmetric_difference(&names_after)
                .cloned()
                .collect();
            // Drain any names accumulated from preempted prior sweeps so this
            // sweep covers their dependents too.
            {
                let mut pending = self.state.pending_changed_names.lock();
                changed_names.extend(pending.drain());
            }
            let scope = if changed_names.is_empty() {
                None
            } else {
                Some(&changed_names)
            };
            // Tagged with this edit's generation so a newer edit preempts it.
            self.revalidate_open_dependents(&uri, generation, scope)
                .await;
        } else {
            tracing::debug!(uri = %uri, "exports unchanged; skipping dependent sweep");
        }
    }

    /// Re-validate and republish every open document except `changed_uri`, using
    /// the freshly updated indexes. Bounded by the number of open files, so a
    /// definition edit propagates to the gui/event/etc. files that reference it.
    ///
    /// `generation` is the edit counter at the time the triggering change landed.
    /// If a newer edit bumps the counter while the sweep is running, the sweep
    /// stops: the newer edit's own sweep will revalidate everything against the
    /// fully-updated index, so finishing this one is wasted work (and would
    /// double-validate). Each dependent's diagnostics are published with that
    /// doc's current version, and skipped if the doc changed mid-sweep, so the
    /// sweep never clobbers a fresher in-flight result for a file being edited.
    ///
    /// `changed_names`, when `Some`, scopes the sweep to the open docs whose
    /// token set mentions one of the (lowercased) names that were added or
    /// removed. A doc with no recorded token set is always included (sound
    /// over-approximation: never skip a file that might depend on the change).
    ///
    /// `None` revalidates every open dependent (used when the exact set of
    /// changed names can't be pinned down, e.g. a multiplicity-only change).
    ///
    /// On preemption (newer edit arrives mid-sweep), the `changed_names` are
    /// saved to `state.pending_changed_names` so the next sweep drains and
    /// includes them, preventing stale dependents from falling through the gap.
    pub(crate) async fn revalidate_open_dependents(
        &self,
        changed_uri: &str,
        generation: u64,
        changed_names: Option<&HashSet<String>>,
    ) {
        use std::sync::atomic::Ordering;

        // Snapshot each open dependent's cached AST (a cheap `Arc` clone) with
        // its version. The dependents' own text didn't change, so they don't
        // need re-parsing or re-indexing — only re-validation against the
        // now-updated global index. When `changed_names` is `Some`, skip docs
        // whose token set references none of the changed names.
        let others: Vec<(String, i32, Arc<ParsedFile>)> = {
            let tokens = self.state.doc_tokens.read();
            let docs = self.state.documents.lock();
            docs.iter()
                .filter(|(u, _)| u.as_str() != changed_uri)
                .filter(|(u, _)| match changed_names {
                    None => true,
                    Some(names) => match tokens.get(u.as_str()) {
                        // No token set recorded for this doc — include it rather
                        // than risk missing a real dependent.
                        None => true,
                        Some(doc_set) => names.iter().any(|n| doc_set.contains(n)),
                    },
                })
                .filter_map(|(u, d)| d.ast.clone().map(|ast| (u.clone(), d.version, ast)))
                .collect()
        };
        if others.is_empty() {
            return;
        }
        tracing::debug!(
            count = others.len(),
            generation,
            "revalidate_open_dependents"
        );
        let game = self.state.config.read().game();
        // Validate every dependent synchronously, then publish. No await is held
        // across the rules lock. The single `rules` read guard covers the ruleset,
        // the cached scope registry, and the modifier-key set (none change during
        // the sweep). Do NOT lock documents inside this block (ABBA: request
        // handlers take documents then rules; we must take rules then
        // nothing-or-documents-after).
        let validated: Vec<(String, i32, Vec<Diagnostic>)> = {
            let rules_guard = self.state.rules.read();
            let mut out = Vec::with_capacity(others.len());
            for (uri, snapshot_version, ast) in others {
                // Preempt: a newer edit arrived. Save our changed_names into the
                // shared pending set so the newer sweep drains and covers them;
                // without this, dependents of the preempted edit stay stale.
                if self.state.edit_generation.load(Ordering::SeqCst) != generation {
                    tracing::debug!(generation, "revalidate_open_dependents superseded");
                    if let Some(names) = changed_names {
                        let mut pending = self.state.pending_changed_names.lock();
                        pending.extend(names.iter().cloned());
                    }
                    // Stop computing further dependents, but fall through to
                    // publish the ones already validated this sweep instead of
                    // discarding them. The newer sweep (draining
                    // pending_changed_names) covers the rest.
                    break;
                }
                let diagnostics = match rules_guard.ruleset.as_ref() {
                    Some(ruleset) => self.validate_parsed_prebuilt(
                        &uri,
                        &ast,
                        &rules_guard.modifier_keys,
                        ruleset,
                        game,
                        rules_guard.scope_registry.as_ref(),
                    ),
                    None => ast.errors.iter().map(parse_error_to_diagnostic).collect(),
                };
                out.push((uri, snapshot_version, diagnostics));
            }
            out
        };
        // Now check still_current without holding ruleset (documents first is
        // the order used by request handlers, so this is safe).
        let to_publish: Vec<(String, i32, Vec<Diagnostic>)> = validated
            .into_iter()
            .filter(|(uri, snapshot_version, _)| {
                // Skip if this dependent was itself edited while we validated it —
                // its own debounced pass owns the fresher result.
                let docs = self.state.documents.lock();
                docs.get(uri.as_str())
                    .map(|d| d.version == *snapshot_version)
                    .unwrap_or(false)
            })
            .collect();
        for (uri, snapshot_version, diagnostics) in to_publish {
            if let Ok(uri_obj) = Url::parse(&uri) {
                self.client
                    .publish_diagnostics(uri_obj, diagnostics, Some(snapshot_version))
                    .await;
            }
        }
    }

    #[tracing::instrument(skip_all, fields(uri = %uri, bytes = text.len()))]
    pub(crate) async fn parse_and_validate(
        &self,
        uri: &str,
        text: &str,
    ) -> (Vec<Diagnostic>, Option<ParsedFile>) {
        let mut diagnostics = Vec::new();

        // Localisation files are parsed and validated as loc, not config.
        if crate::paths::is_loc_file(uri) {
            let path = uri_to_path_str(uri);
            // Names a `$ref$` may resolve to besides loc keys (`$modifier$` /
            // `$idea$` embeds). Built before the loc_index guard to honour the
            // info_service -> loc_index lock order.
            let extra_valid_refs: HashSet<String> = {
                // Lock order: rules -> info_service.
                let mut extra = self.state.rules.read().modifier_keys.clone();
                let info = self.state.info_service.read();
                for (_uri, inst) in info.type_index.instances("idea") {
                    extra.insert(inst.name.to_lowercase());
                }
                extra
            };
            // Hold the read guard across the validate call to avoid cloning the
            // full loc-key union (~2M Strings on Millennium Dawn).
            let loc_guard = self.state.loc_index.read();
            let empty_union: HashSet<String> = HashSet::new();
            let union: &HashSet<String> = loc_guard
                .as_ref()
                .map(|idx| idx.union())
                .unwrap_or(&empty_union);
            for d in
                cwtools_localization::validate_loc_file_text(text, &path, union, &extra_valid_refs)
            {
                let ve = loc_diag_to_validation_error(&d);
                diagnostics.push(validation_error_to_diagnostic(&ve));
            }
            return (diagnostics, None);
        }

        tracing::debug!(%uri, "[validate] parsing");

        match parse_string(text, &self.state.string_table) {
            Ok(parsed) => {
                for parse_err in &parsed.errors {
                    diagnostics.push(parse_error_to_diagnostic(parse_err));
                }

                // Update symbol index
                {
                    let mut index = self.state.symbol_index.lock();
                    index.clear_document(uri);
                    index.index_document(uri, &parsed, &self.state.string_table);
                }

                // Derive logical path for type-instance indexing
                let ws_uri = self.state.config.read().workspace_uri.clone();
                let logical_path = logical_path_from_uri(uri, &ws_uri);

                // Update info service. Lock order: rules -> info_service.
                {
                    let rules_guard = self.state.rules.read();
                    let mut info = self.state.info_service.write();
                    info.clear_file(uri);
                    if let Some(ruleset) = rules_guard.ruleset.as_ref() {
                        info.index_file_with_path(
                            uri,
                            &parsed,
                            &self.state.string_table,
                            ruleset,
                            &logical_path,
                        );
                    }
                }

                // Validation. Lock order: rules -> info_service -> loc_index.
                let (errors, log_msg) = {
                    let game = self.state.config.read().game();
                    let rules_guard = self.state.rules.read();
                    if let Some(ruleset) = rules_guard.ruleset.as_ref() {
                        let start = std::time::Instant::now();
                        // Pass the workspace TypeIndex for cross-file type reference checking.
                        let info_guard = self.state.info_service.read();
                        let type_index = &info_guard.type_index;
                        let loc_guard = self.state.loc_index.read();
                        // Single-file path: the scope registry is cached (built
                        // once at ruleset load).
                        let (scope_checks, var_checks) = {
                            let cfg = self.state.config.read();
                            (cfg.scope_checks, cfg.var_checks)
                        };
                        let prepared = make_prepared(
                            ruleset,
                            &self.state.string_table,
                            game,
                            type_index,
                            &rules_guard.modifier_keys,
                            loc_guard.as_ref(),
                            rules_guard.scope_registry.as_ref(),
                            scope_checks,
                            var_checks,
                        );
                        let mut errs = validate_prepared(&parsed, uri, &prepared);
                        drop(loc_guard);
                        drop(info_guard);
                        let elapsed = start.elapsed();
                        let total = truncate_validation_errors(&mut errs, uri);
                        let msg = format!(
                            "[validate] {} errors in {:?} ({} types, {} enums, {} aliases)",
                            total,
                            elapsed,
                            ruleset.types.len(),
                            ruleset.enums.len(),
                            ruleset.aliases.len()
                        );
                        (errs, Some(msg))
                    } else {
                        (Vec::new(), None)
                    }
                };

                if let Some(msg) = log_msg {
                    self.client.log_message(MessageType::INFO, msg).await;
                }

                for err in &errors {
                    diagnostics.push(validation_error_to_diagnostic(err));
                }
                (diagnostics, Some(parsed))
            }
            Err(e) => {
                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position::default(),
                        end: Position::default(),
                    },
                    severity: Some(DiagnosticSeverity::ERROR),
                    code: None,
                    code_description: None,
                    source: Some("cwtools".to_string()),
                    message: format!("Parse error: {}", e),
                    related_information: None,
                    tags: None,
                    data: None,
                });
                (diagnostics, None)
            }
        }
    }
}
