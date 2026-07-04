use std::collections::{HashMap, HashSet};
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
    extra_loc_keys: Option<&'a std::collections::HashSet<String>>,
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
        extra_loc_keys,
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
    ValidationError {
        message: d.message.clone(),
        severity: d.severity,
        line: d.line as u32,
        col: d.col.saturating_sub(1) as u16,
        file: d.file.clone(),
        code: Some(d.code),
    }
}

/// Lowercased loc keys defined in a single loc file's text. A cheap single-file
/// parse used to keep the live overlay current on edit (#36).
fn loc_keys_of(text: &str, path: &str) -> HashSet<String> {
    let svc =
        cwtools_localization::LocService::from_files(vec![(path.to_string(), text.to_string())]);
    let mut keys = HashSet::new();
    for file in svc.files() {
        for entry in &file.entries {
            keys.insert(entry.key.to_lowercase());
        }
    }
    keys
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
    line_ends: &[u32],
) -> Vec<Diagnostic> {
    let mut diagnostics: Vec<Diagnostic> = parsed
        .errors
        .iter()
        .map(|e| parse_error_to_diagnostic(e, line_ends))
        .collect();
    let mut errs = validate_prepared(parsed, uri, prepared);
    // CW100: objects defined here whose `## required` localisation keys aren't
    // provided by any loc file. Gated on the loc index being built — before the
    // initial scan finishes it's empty and everything would falsely report
    // missing.
    if let Some(loc) = prepared.loc_index
        && !loc.union().is_empty()
    {
        let overlay = prepared.extra_loc_keys;
        errs.extend(cwtools_validation::missing_loc::check_missing_localisation(
            parsed,
            uri,
            uri,
            prepared.ruleset,
            prepared.table,
            |k| loc.exists_any(k) || overlay.is_some_and(|o| o.contains(k)),
        ));
    }
    truncate_validation_errors(&mut errs, uri);
    for err in &errs {
        diagnostics.push(validation_error_to_diagnostic(err, line_ends));
    }
    diagnostics
}

/// Trimmed char-length of each line in `text`, indexed by 0-based line number.
/// Used to widen a 1-column diagnostic so the squiggle covers the whole
/// statement instead of a single character at the start. Counts chars (not
/// bytes) to match the parser's column unit.
pub(crate) fn line_end_cols(text: &str) -> Vec<u32> {
    text.lines()
        .map(|l| l.trim_end().chars().count() as u32)
        .collect()
}

/// End column for a diagnostic starting at `col` on 0-based `line`: the end of
/// that line's content, but always at least one past `col` so the range is
/// never empty. With no line info (`line_ends` empty or line out of range),
/// falls back to a single-character span.
fn diag_end_col(line_ends: &[u32], line: u32, col: u32) -> u32 {
    line_ends
        .get(line as usize)
        .copied()
        .unwrap_or(0)
        .max(col + 1)
}

/// Build a whole-statement-line diagnostic at `(line, col)` (0-based). The
/// squiggle spans from `col` to the line's end column via [`diag_end_col`].
/// Shared skeleton behind the `*_to_diagnostic` builders.
fn diagnostic_at(
    line: u32,
    col: u32,
    line_ends: &[u32],
    severity: DiagnosticSeverity,
    source: &str,
    code: Option<NumberOrString>,
    message: String,
) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line,
                character: col,
            },
            end: Position {
                line,
                character: diag_end_col(line_ends, line, col),
            },
        },
        severity: Some(severity),
        code,
        code_description: None,
        source: Some(source.to_string()),
        message,
        related_information: None,
        tags: None,
        data: None,
    }
}

pub(crate) fn parse_error_to_diagnostic(e: &ParseError, line_ends: &[u32]) -> Diagnostic {
    let (line, col, msg) = match e {
        ParseError::Pos(_f, line, col, msg) => (line.saturating_sub(1), *col as u32, msg.clone()),
        ParseError::General(msg) => (0, 0, msg.clone()),
    };
    diagnostic_at(
        line,
        col,
        line_ends,
        DiagnosticSeverity::ERROR,
        "cwtools",
        None,
        msg,
    )
}

/// Convert a `.cwt` rule-config error (parse or structural reference) into an LSP
/// diagnostic. `RuleParseError.line` is 1-based; `col` is a 0-based character.
/// Shared by the load-time path (`config.rs`) and the live per-file CWT lint.
pub(crate) fn rule_parse_error_to_diagnostic(
    err: &cwtools_rules::ruleset_loader::RuleParseError,
    line_ends: &[u32],
) -> Diagnostic {
    let line = err.line.saturating_sub(1);
    let col = err.col as u32;
    diagnostic_at(
        line,
        col,
        line_ends,
        DiagnosticSeverity::ERROR,
        "cwtools-rules",
        None,
        err.message.clone(),
    )
}

/// Whether a diagnostic carrying `code` should be dropped given the user's
/// lowercased suppression list (`errors.ignore` → `ignoredErrorCodes`). Only the
/// string codes the validator emits (e.g. `CW100`) can be suppressed; compared
/// case-insensitively. Numeric/absent codes are never suppressed.
pub(crate) fn code_is_suppressed(code: Option<&NumberOrString>, ignored: &[String]) -> bool {
    match code {
        Some(NumberOrString::String(c)) => ignored.contains(&c.to_ascii_lowercase()),
        _ => false,
    }
}

pub(crate) fn validation_error_to_diagnostic(
    err: &ValidationError,
    line_ends: &[u32],
) -> Diagnostic {
    let line = err.line.saturating_sub(1);
    let col = err.col as u32;
    let severity = match err.severity {
        cwtools_validation::ErrorSeverity::Error => DiagnosticSeverity::ERROR,
        cwtools_validation::ErrorSeverity::Warning => DiagnosticSeverity::WARNING,
        cwtools_validation::ErrorSeverity::Information => DiagnosticSeverity::INFORMATION,
        cwtools_validation::ErrorSeverity::Hint => DiagnosticSeverity::HINT,
    };
    diagnostic_at(
        line,
        col,
        line_ends,
        severity,
        "cwtools",
        err.code.map(|c| NumberOrString::String(c.to_string())),
        err.message.clone(),
    )
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
    tokens
}

impl Backend {
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
            // Subtype-qualified membership (`equipment.naval_equip` …) so
            // `<type.subtype>` references resolve. Re-run on every (re)index so an
            // edit to an archetype keeps its subtype tag fresh.
            info.type_index.merge(
                uri,
                cwtools_validation::collect_subtype_instances(
                    ruleset,
                    parsed,
                    &logical_path,
                    &self.state.string_table,
                ),
            );
        }
        drop(info);
        drop(rules_guard);
        self.bump_info_revision();
    }

    /// Validate an already-parsed document against the (already-built) workspace
    /// index, with the ruleset already locked and the per-run scope registry
    /// prebuilt by the caller. Multi-file callers (the workspace scan, the
    /// dependent sweep) build those ONCE outside their loop and reuse them.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn validate_parsed_prebuilt(
        &self,
        uri: &str,
        parsed: &ParsedFile,
        modifier_keys: &std::collections::HashSet<String>,
        ruleset: &RuleSet,
        game: Option<cwtools_game::constants::Game>,
        registry: Option<&std::sync::Arc<cwtools_game::scope_registry::ScopeRegistry>>,
        line_ends: &[u32],
    ) -> Vec<Diagnostic> {
        // Overlay computed before the other guards (its lock is independent and
        // never nested inside info/loc — see validate_loc_text).
        let overlay = self.loc_overlay_keys();
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
            Some(&overlay),
            registry,
            scope_checks,
            var_checks,
        );
        validate_parsed_with_indexes(uri, parsed, &prepared, line_ends)
    }

    /// Publish diagnostics after dropping any whose code the user suppressed via
    /// `errors.ignore` (`ignoredErrorCodes`). Every publish path funnels through
    /// here so a suppressed code can't slip out from whichever validation route
    /// produced it. A no-op (and off the hot path's cost) when nothing is
    /// suppressed, which is the common case.
    pub(crate) async fn publish_filtered(
        &self,
        uri: tower_lsp::lsp_types::Url,
        mut diagnostics: Vec<Diagnostic>,
        version: Option<i32>,
    ) {
        {
            let cfg = self.state.config.read();
            if !cfg.ignored_error_codes.is_empty() {
                diagnostics
                    .retain(|d| !code_is_suppressed(d.code.as_ref(), &cfg.ignored_error_codes));
            }
        }
        self.client
            .publish_diagnostics(uri, diagnostics, version)
            .await;
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
        self.publish_filtered(uri, diags, version).await;
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
                // Preserve the last good AST on a transient parse failure (None):
                // a fatal mid-edit syntax error shouldn't wipe the tree that
                // completion/hover/goto resolve context from, or they collapse to
                // a generic word list until the next clean parse. The parse error
                // is still published. (#41) Loc/.cwt files always parse to None
                // here, so their (absent) AST is unaffected.
                if ast.is_some() {
                    d.ast = ast;
                }
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
        // Capture each dependent's per-line end columns while the docs lock is
        // held (cheaper than cloning the whole text) so the republished
        // diagnostics get whole-line squiggles, same as the edited file.
        let others: Vec<(String, i32, Arc<ParsedFile>, Vec<u32>)> = {
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
                .filter_map(|(u, d)| {
                    d.ast
                        .clone()
                        .map(|ast| (u.clone(), d.version, ast, line_end_cols(&d.text)))
                })
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
            for (uri, snapshot_version, ast, line_ends) in others {
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
                        &line_ends,
                    ),
                    None => ast
                        .errors
                        .iter()
                        .map(|e| parse_error_to_diagnostic(e, &line_ends))
                        .collect(),
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
                self.publish_filtered(uri_obj, diagnostics, Some(snapshot_version))
                    .await;
            }
        }
    }

    /// Flatten the live loc overlay (per-open-`.yml` key sets) into one set of
    /// lowercased keys, for the game-file loc-existence checks (CW100/CW122) so a
    /// key just typed into an open `.yml` resolves without a full rescan (#36).
    /// Bounded by the number of open loc files; returns empty when none are open.
    pub(crate) fn loc_overlay_keys(&self) -> HashSet<String> {
        let mut keys = HashSet::new();
        for set in self.state.loc_live_overlay.read().values() {
            keys.extend(set.iter().cloned());
        }
        keys
    }

    /// Update the hover loc_text map with entries from a single loc file,
    /// replacing any previous entries for the same file. Called on every loc
    /// file edit so tooltips reflect the latest changes without a full
    /// workspace rescan (#53).
    fn update_loc_text_for_file(&self, _uri: &str, text: &str, path: &str) {
        let svc = cwtools_localization::LocService::from_files(vec![(
            path.to_string(),
            text.to_string(),
        )]);
        let hover_all = self
            .state
            .hover_show_all_languages
            .load(std::sync::atomic::Ordering::Relaxed);
        let loc_languages = self.state.config.read().loc_languages.clone();
        let primary_lang = loc_languages
            .as_deref()
            .and_then(|l| l.first().copied())
            .unwrap_or(cwtools_localization::Lang::English);

        // Collect the new entries for this file.
        let mut new_entries: HashMap<String, Vec<(cwtools_localization::Lang, String)>> =
            HashMap::new();
        for file in svc.files() {
            let lang = file.lang.unwrap_or(cwtools_localization::Lang::English);
            let lang_included = hover_all || lang == primary_lang;
            if !lang_included {
                continue;
            }
            for entry in &file.entries {
                let display = crate::paths::loc_display_text(&entry.desc);
                if !display.is_empty() {
                    new_entries
                        .entry(entry.key.to_lowercase())
                        .or_default()
                        .push((lang, display.to_string()));
                }
            }
        }

        // Merge into the global loc_text map: remove old entries for this
        // file's keys, then insert the new ones. A simple remove-and-replace
        // per key would lose entries from OTHER files that share the same key.
        // Instead, rebuild the affected keys from all sources.
        let mut loc_text = self.state.loc_text.write();
        for key in new_entries.keys() {
            // Remove any existing entry for this key that came from this file.
            // We can't track per-file contributions in loc_text (it's a flat
            // map), so just overwrite — the full rescan will correct any
            // cross-file ordering issues.
            loc_text.remove(key);
        }
        for (key, translations) in new_entries {
            loc_text.entry(key).or_default().extend(translations);
        }
    }

    /// Validate one loc file's text into diagnostics. Builds the set of names a
    /// `$ref$` may resolve to — modifier keys, idea names, and the live loc
    /// overlay (the current keys of every open `.yml`) — then checks against the
    /// scanned union. Pure: it neither updates the overlay nor triggers any
    /// cross-file work, so the cross-file sweep can call it safely (#36).
    fn validate_loc_text(&self, path: &str, text: &str, line_ends: &[u32]) -> Vec<Diagnostic> {
        // Lock order: rules -> info_service -> loc_index. The overlay lock is
        // independent and taken between, never nested inside the others.
        let mut extra: HashSet<String> = (*self.state.rules.read().modifier_keys).clone();
        {
            let info = self.state.info_service.read();
            let ideas = info.type_index.instances("idea");
            extra.reserve(ideas.len());
            for (_uri, inst) in ideas {
                // Idea names are ASCII identifiers; skip the lowercasing alloc
                // when already lowercase ASCII (the common case).
                let needs_fold = inst
                    .name
                    .bytes()
                    .any(|b| b.is_ascii_uppercase() || !b.is_ascii());
                if needs_fold {
                    extra.insert(inst.name.to_lowercase());
                } else {
                    extra.insert(inst.name.clone());
                }
            }
        }
        // Live overlay: every open loc file's current keys. Lets a key just added
        // to an open `.yml` resolve immediately, in this file and cross-file.
        for keys in self.state.loc_live_overlay.read().values() {
            extra.extend(keys.iter().cloned());
        }
        // Hold the read guard across the validate call to avoid cloning the full
        // loc-key union (~2M Strings on Millennium Dawn).
        let loc_guard = self.state.loc_index.read();
        let empty_union: HashSet<String> = HashSet::new();
        let union: &HashSet<String> = loc_guard
            .as_ref()
            .map(|idx| idx.union())
            .unwrap_or(&empty_union);
        cwtools_localization::validate_loc_file_text(text, path, union, &extra)
            .iter()
            .map(|d| validation_error_to_diagnostic(&loc_diag_to_validation_error(d), line_ends))
            .collect()
    }

    /// Re-validate and republish every OTHER open loc file. Called when an edited
    /// loc file's key set changed, so a `$ref$` to a key that was just added or
    /// removed updates in the other open `.yml` files without a reload (#36).
    /// Bounded by the number of open loc files.
    async fn revalidate_other_open_loc_files(&self, except_uri: &str) {
        let targets: Vec<(String, String)> = {
            let docs = self.state.documents.lock();
            docs.iter()
                .filter(|(u, _)| u.as_str() != except_uri && crate::paths::is_loc_file(u))
                .map(|(u, d)| (u.clone(), d.text.clone()))
                .collect()
        };
        for (u, text) in targets {
            let path = uri_to_path_str(&u);
            let line_ends = line_end_cols(&text);
            let diags = self.validate_loc_text(&path, &text, &line_ends);
            if let Ok(obj) = Url::parse(&u) {
                self.publish_gated(obj, diags, None).await;
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
        // Per-line end columns so every squiggle spans the whole statement line
        // rather than a single character at its start.
        let line_ends = line_end_cols(text);

        // Localisation files are parsed and validated as loc, not config.
        if crate::paths::is_loc_file(uri) {
            let path = uri_to_path_str(uri);
            // Keep the live overlay current so this file's own keys (and any just
            // added) resolve immediately in `$ref$` checks, without waiting for a
            // full rescan. Record whether the key set actually changed. (#36)
            let changed = {
                let new_keys = loc_keys_of(text, &path);
                let mut overlay = self.state.loc_live_overlay.write();
                let changed = overlay
                    .get(uri)
                    .map(|prev| prev != &new_keys)
                    .unwrap_or(true);
                overlay.insert(uri.to_string(), new_keys);
                changed
            };
            let diagnostics = self.validate_loc_text(&path, text, &line_ends);
            // Update the hover loc_text map so tooltips reflect the latest
            // edits without waiting for a full workspace rescan (#53).
            self.update_loc_text_for_file(uri, text, &path);
            // A change to this file's key set can fix or break `$ref$` checks in
            // other open loc files, so refresh them — that's the cross-file part
            // of the index that previously only updated on a window reload.
            // It can also fix or break a missing-localisation (CW100/CW122)
            // diagnostic on open GAME files that reference the added/removed key
            // (e.g. a new event option's loc), so re-validate those too — the
            // overlay now feeds the game-file loc checks, so they resolve the new
            // key without a full rescan. (#36)
            if changed {
                self.revalidate_other_open_loc_files(uri).await;
                let generation = self
                    .state
                    .edit_generation
                    .load(std::sync::atomic::Ordering::SeqCst);
                self.revalidate_open_dependents(uri, generation, None).await;
            }
            return (diagnostics, None);
        }

        // `.cwt` rule-config files are the schema the engine is built from, not
        // game content. Lint them structurally — parse errors plus references to
        // undefined types/enums/single_aliases — against the loaded merged
        // ruleset, rather than running the game-script validator (which would
        // flag every rule field as unknown). See #43.
        if crate::paths::is_cwt_file(uri) {
            match parse_string(text, &self.state.string_table) {
                Ok(parsed) => {
                    for parse_err in &parsed.errors {
                        diagnostics.push(parse_error_to_diagnostic(parse_err, &line_ends));
                    }
                    // Structural reference check against the merged ruleset. Only
                    // runs once rules are loaded; before then there's nothing to
                    // resolve references against (and everything would falsely
                    // report undefined).
                    let rules_guard = self.state.rules.read();
                    if let Some(ruleset) = rules_guard.ruleset.as_ref() {
                        let path = std::path::PathBuf::from(uri_to_path_str(uri));
                        let files = [(path, parsed)];
                        for err in cwtools_rules::config_validation::validate_ruleset_references(
                            &files,
                            ruleset,
                            &self.state.string_table,
                        ) {
                            diagnostics.push(rule_parse_error_to_diagnostic(&err, &line_ends));
                        }
                    }
                }
                Err(e) => {
                    diagnostics.push(Diagnostic {
                        range: Range {
                            start: Position::default(),
                            end: Position::default(),
                        },
                        severity: Some(DiagnosticSeverity::ERROR),
                        source: Some("cwtools-rules".to_string()),
                        message: format!("Parse error: {}", e),
                        ..Default::default()
                    });
                }
            }
            return (diagnostics, None);
        }

        tracing::debug!(%uri, "[validate] parsing");

        match parse_string(text, &self.state.string_table) {
            Ok(parsed) => {
                for parse_err in &parsed.errors {
                    diagnostics.push(parse_error_to_diagnostic(parse_err, &line_ends));
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
                    drop(info);
                    drop(rules_guard);
                    self.bump_info_revision();
                }

                // Validation. Lock order: rules -> info_service -> loc_index.
                let (errors, log_msg) = {
                    let game = self.state.config.read().game();
                    // Live overlay of unsaved loc keys in open `.yml` files, so a
                    // key just added there resolves in this file's loc checks
                    // (CW100/CW122) without a full rescan (#36). Computed before
                    // the other guards (independent lock).
                    let overlay = self.loc_overlay_keys();
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
                            Some(&overlay),
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
                    diagnostics.push(validation_error_to_diagnostic(err, &line_ends));
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

#[cfg(test)]
mod whole_line_range_tests {
    use super::*;
    use cwtools_validation::ErrorSeverity;

    #[test]
    fn loc_keys_of_extracts_lowercased_keys() {
        // Live-overlay key extraction for #36: keys are lowercased to match the
        // case-insensitive union the `$ref$` check resolves against.
        let keys = loc_keys_of(
            "l_english:\n MY_Key: \"hi\"\n other_key: \"x\"\n",
            "a_l_english.yml",
        );
        assert!(keys.contains("my_key"), "got: {:?}", keys);
        assert!(keys.contains("other_key"), "got: {:?}", keys);
        assert!(!keys.contains("absent"));
    }

    #[test]
    fn line_end_cols_reports_trimmed_char_lengths() {
        let text = "abc\n  hello world  \n";
        let ends = line_end_cols(text);
        assert_eq!(ends[0], 3, "\"abc\" has 3 chars");
        assert_eq!(
            ends[1], 13,
            "\"  hello world\" (trailing ws trimmed) has 13 chars"
        );
    }

    #[test]
    fn diagnostic_spans_from_field_to_end_of_line() {
        let text = "decision = {\n    custom_cost_text = a\n}\n";
        let ends = line_end_cols(text);
        let err = ValidationError {
            message: "x".into(),
            severity: ErrorSeverity::Warning,
            line: 2, // 1-based: the custom_cost_text line
            col: 4,  // start of the field, after the indentation
            file: "f".into(),
            code: Some("CW242"),
        };
        let diag = validation_error_to_diagnostic(&err, &ends);
        assert_eq!(diag.range.start.line, 1);
        assert_eq!(diag.range.start.character, 4);
        assert_eq!(diag.range.end.line, 1);
        // "    custom_cost_text = a" is 24 chars.
        assert_eq!(diag.range.end.character, 24);
    }

    #[test]
    fn diagnostic_falls_back_to_one_char_without_line_info() {
        let err = ValidationError {
            message: "x".into(),
            severity: ErrorSeverity::Warning,
            line: 5,
            col: 2,
            file: "f".into(),
            code: None,
        };
        let diag = validation_error_to_diagnostic(&err, &[]);
        assert_eq!(diag.range.start.character, 2);
        assert_eq!(diag.range.end.character, 3);
    }

    #[test]
    fn suppression_matches_codes_case_insensitively() {
        let ignored = vec!["cw100".to_string()];
        // Suppression list is stored lowercased; the diagnostic code can be any case.
        assert!(code_is_suppressed(
            Some(&NumberOrString::String("CW100".into())),
            &ignored
        ));
        assert!(code_is_suppressed(
            Some(&NumberOrString::String("cw100".into())),
            &ignored
        ));
        assert!(!code_is_suppressed(
            Some(&NumberOrString::String("CW246".into())),
            &ignored
        ));
        // Absent and numeric codes are never suppressed.
        assert!(!code_is_suppressed(None, &ignored));
        assert!(!code_is_suppressed(
            Some(&NumberOrString::Number(100)),
            &ignored
        ));
        // Empty list suppresses nothing.
        assert!(!code_is_suppressed(
            Some(&NumberOrString::String("CW100".into())),
            &[]
        ));
    }
}
