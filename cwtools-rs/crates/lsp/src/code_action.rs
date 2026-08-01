//! `textDocument/codeAction`: turn a diagnostic's [`SuggestedFix`] into a
//! QUICKFIX code action with a `WorkspaceEdit`.
//!
//! The fix is serialized into `Diagnostic.data` at publish time (see
//! [`fix_to_data`], called from `validate.rs`) because the AST span is only in
//! scope there — a diagnostic's start position alone can't reconstruct it. The
//! client round-trips `data` back on a codeAction request, where the raw source
//! range is converted into an LSP range with the document text and the
//! negotiated position encoding (the same `source_position_to_lsp` helper
//! hover/rename use) and wrapped into a `TextEdit`.
//!
//! The payload stores ranges in the parser convention (1-based line, 0-based
//! char column) verbatim; the LSP conversion is deferred to the handler, the one
//! place with both the text and the negotiated encoding.
//!
//! Two kinds are offered: one QUICKFIX per fixable diagnostic, and one
//! `source.fixAll` that applies all of them at once — the kind
//! `editor.codeActionsOnSave` binds to. `source.fixAll` resolves overlaps with
//! `cwtools_parser::fix::plan_file_edits`, the same code the CLI `fix`
//! subcommand runs, so the two agree on what a fixed file looks like.
//!
//! A third kind is CW100-specific: its fix carries no span edit (there's
//! nothing to replace — the key doesn't exist anywhere yet), only a
//! `create_loc_key`. `create_loc_key_actions` builds a dedicated cross-file
//! QUICKFIX for it instead, inserting a stub line into a loc file (or a new
//! one) rather than editing the diagnostic's own document. See
//! `resolve_loc_insert_target` for the three-tier site it picks.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

use cwtools_localization::{Lang, LocService};
use cwtools_parser::ast::{SourcePos, SourceRange};
use cwtools_parser::fix::{SpanEdit, SuggestedFix, plan_file_edits};

use crate::Backend;
use crate::paths::{source_column_to_lsp, source_position_to_lsp, uri_is_in_workspace};

/// Key under which a fix payload lives in `Diagnostic.data`. Namespaced so a
/// codeAction request only treats data it put there as a fix (future diagnostic
/// data of other shapes is ignored).
const FIX_DATA_KEY: &str = "cwtoolsFix";

/// A single span replacement round-tripped through `Diagnostic.data`. Ranges use
/// the parser convention: 1-based line, 0-based char column.
struct FixEdit {
    range: SourceRange,
    replacement: String,
}

/// A named set of edits resolving one diagnostic. `edits` is empty for a
/// "create missing localisation key" fix (CW100): it has no in-file span to
/// replace, and carries `create_loc_key` for the dedicated code action in
/// this module instead. Every other consumer here (`code_actions_from_diagnostics`,
/// `fix_all_action`) treats an empty `edits` as nothing to do.
struct FixPayload {
    title: String,
    edits: Vec<FixEdit>,
    create_loc_key: Option<String>,
}

/// Serialize a [`SuggestedFix`] into a `Diagnostic.data` value, namespaced under
/// [`FIX_DATA_KEY`]. Ranges are stored in the parser convention (1-based line,
/// 0-based char col); the LSP conversion happens in the handler where the
/// document text and the negotiated encoding are available.
pub(crate) fn fix_to_data(fix: &SuggestedFix) -> serde_json::Value {
    let edits: Vec<serde_json::Value> = fix
        .edits
        .iter()
        .map(|e| {
            serde_json::json!({
                "startLine": e.range.start.line,
                "startCol": e.range.start.col,
                "endLine": e.range.end.line,
                "endCol": e.range.end.col,
                "replacement": e.replacement,
            })
        })
        .collect();
    let mut payload = serde_json::json!({
        "title": fix.title,
        "edits": edits,
    });
    if let Some(key) = &fix.create_loc_key {
        payload["createLocKey"] = serde_json::Value::String(key.clone());
    }
    serde_json::json!({ FIX_DATA_KEY: payload })
}

/// Parse a fix payload out of a diagnostic's `data` value. `None` when the value
/// isn't a cwtools fix payload (any other diagnostic-data shape, or a
/// malformed/partial entry).
fn fix_from_data(data: &serde_json::Value) -> Option<FixPayload> {
    let obj = data.get(FIX_DATA_KEY)?;
    let title = obj.get("title")?.as_str()?.to_string();
    let edits_json = obj.get("edits")?.as_array()?;
    let mut edits = Vec::with_capacity(edits_json.len());
    for e in edits_json {
        edits.push(FixEdit {
            range: SourceRange {
                start: SourcePos {
                    line: e.get("startLine")?.as_u64()? as u32,
                    col: e.get("startCol")?.as_u64()? as u16,
                },
                end: SourcePos {
                    line: e.get("endLine")?.as_u64()? as u32,
                    col: e.get("endCol")?.as_u64()? as u16,
                },
            },
            replacement: e.get("replacement")?.as_str()?.to_string(),
        });
    }
    let create_loc_key = obj
        .get("createLocKey")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Some(FixPayload {
        title,
        edits,
        create_loc_key,
    })
}

/// The span edits `diag`'s fix payload carries, tagged with the diagnostic's
/// code — empty when the diagnostic has no payload, its code isn't a string,
/// or (like CW100's create-key fix) it carries no span edits. Shared by the
/// `fixAllWorkspace` store (`validate::publish_filtered`) and available for
/// any future consumer that needs "what would a fix-all apply here" without
/// going through `fix_all_action`'s per-request diagnostic bookkeeping.
pub(crate) fn fixable_span_edits(diag: &Diagnostic) -> Vec<(String, SpanEdit)> {
    let Some(payload) = diag.data.as_ref().and_then(fix_from_data) else {
        return Vec::new();
    };
    let Some(NumberOrString::String(code)) = diag.code.clone() else {
        return Vec::new();
    };
    payload
        .edits
        .into_iter()
        .map(|e| {
            (
                code.clone(),
                SpanEdit {
                    range: e.range,
                    replacement: e.replacement,
                },
            )
        })
        .collect()
}

/// Convert a parser [`SourceRange`] (1-based line, 0-based char col) into an LSP
/// `Range`, using `text` and the negotiated `encoding` — the same
/// `source_position_to_lsp` conversion hover/rename/navigation use, and the one
/// the diagnostic this fix hangs off went through (`validate::DocLines`), so the
/// edit lands on exactly the columns the squiggle covers.
pub(crate) fn source_range_to_lsp(
    range: SourceRange,
    text: &str,
    encoding: &PositionEncodingKind,
) -> Range {
    Range {
        start: source_position_to_lsp(
            text,
            range.start.line.saturating_sub(1),
            range.start.col as u32,
            encoding,
        ),
        end: source_position_to_lsp(
            text,
            range.end.line.saturating_sub(1),
            range.end.col as u32,
            encoding,
        ),
    }
}

/// Build QUICKFIX code actions from the request's context diagnostics: one per
/// diagnostic carrying a cwtools fix payload. Pure (no locks / IO) so the
/// handler and its test exercise the same mapping. `text`/`encoding` drive the
/// range conversion.
fn code_actions_from_diagnostics(
    uri: &Url,
    diagnostics: &[Diagnostic],
    text: &str,
    encoding: &PositionEncodingKind,
) -> Vec<CodeActionOrCommand> {
    let mut actions = Vec::new();
    for diag in diagnostics {
        let Some(payload) = diag.data.as_ref().and_then(fix_from_data) else {
            continue;
        };
        // A create-loc-key fix (CW100) has no span edit to offer here — it
        // gets its own cross-file action from `create_loc_key_actions`.
        if payload.edits.is_empty() {
            continue;
        }
        let text_edits: Vec<TextEdit> = payload
            .edits
            .iter()
            .map(|e| TextEdit {
                range: source_range_to_lsp(e.range, text, encoding),
                new_text: e.replacement.clone(),
            })
            .collect();
        let mut changes = HashMap::new();
        changes.insert(uri.clone(), text_edits);
        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
            title: payload.title,
            kind: Some(CodeActionKind::QUICKFIX),
            diagnostics: Some(vec![diag.clone()]),
            edit: Some(WorkspaceEdit {
                changes: Some(changes),
                document_changes: None,
                change_annotations: None,
            }),
            ..Default::default()
        }));
    }
    actions
}

/// Build the single `source.fixAll` action: every fixable diagnostic in the
/// document, applied at once. `None` when fewer than one edit survives.
///
/// Overlap resolution is [`cwtools_parser::fix::plan_file_edits`] — the same
/// function the CLI `fix` subcommand uses, so "fix all" in the editor and
/// `cwtools fix --apply` produce the same file. An edit dropped for overlapping
/// keeps its own quick fix, which is the right outcome: applying it needs the
/// first edit to land and the document to be re-validated.
///
/// The diagnostics come from `context.diagnostics`, which the client scopes to
/// the requested range. `editor.codeActionsOnSave` requests the whole document,
/// so that is what "all" means here.
fn fix_all_action(
    uri: &Url,
    diagnostics: &[Diagnostic],
    text: &str,
    encoding: &PositionEncodingKind,
) -> Option<CodeActionOrCommand> {
    // A create-loc-key payload (CW100) parses fine but carries no span edit —
    // it must not count as "fixable" here (nothing for `plan_file_edits` to
    // apply) or be claimed by the action below (its own dedicated action
    // resolves it instead).
    let payloads_with_edits: Vec<(usize, FixPayload)> = diagnostics
        .iter()
        .enumerate()
        .filter_map(|(i, d)| Some((i, d.data.as_ref().and_then(fix_from_data)?)))
        .filter(|(_, payload)| !payload.edits.is_empty())
        .collect();
    let fixable: std::collections::HashSet<usize> =
        payloads_with_edits.iter().map(|(i, _)| *i).collect();
    let planned: Vec<(usize, SpanEdit)> = payloads_with_edits
        .into_iter()
        .flat_map(|(i, payload)| {
            payload.edits.into_iter().map(move |e| {
                (
                    i,
                    SpanEdit {
                        range: e.range,
                        replacement: e.replacement,
                    },
                )
            })
        })
        .collect();
    if planned.is_empty() {
        return None;
    }
    let (kept, skipped) = plan_file_edits(text, planned);
    if kept.is_empty() {
        return None;
    }
    // Attribute the action to the diagnostics it actually resolves, so the
    // client can clear them optimistically.
    let resolved: Vec<Diagnostic> = diagnostics
        .iter()
        .enumerate()
        .filter(|(i, _)| fixable.contains(i) && !skipped.contains(i))
        .map(|(_, d)| d.clone())
        .collect();
    let text_edits: Vec<TextEdit> = kept
        .iter()
        .map(|e| TextEdit {
            range: source_range_to_lsp(e.range, text, encoding),
            new_text: e.replacement.clone(),
        })
        .collect();
    let mut changes = HashMap::new();
    changes.insert(uri.clone(), text_edits);
    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title: format!("Fix all ({} auto-fixable)", kept.len()),
        kind: Some(CodeActionKind::SOURCE_FIX_ALL),
        diagnostics: Some(resolved),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }),
        ..Default::default()
    }))
}

/// Whether the request asks for `kind`. An absent `only` means "everything";
/// LSP kinds are hierarchical, so `source` selects `source.fixAll`.
fn wants(only: Option<&Vec<CodeActionKind>>, kind: &CodeActionKind) -> bool {
    match only {
        None => true,
        Some(only) => only.iter().any(|k| {
            kind.as_str() == k.as_str() || kind.as_str().starts_with(&format!("{}.", k.as_str()))
        }),
    }
}

// ── Create missing localisation key (CW100) ───────────────────────────────────

/// Where the create-loc-key action inserts one language's stub line, in
/// resolution-priority order (see [`resolve_loc_insert_target`]).
#[derive(Debug, Clone, PartialEq)]
enum LocInsertTarget {
    /// A sibling required loc key for the same instance already has a
    /// definition site; insert right after it (the best UX — the new key
    /// lands next to the ones it belongs with).
    ExistingFileAfterLine { uri: Url, after_line0: u32 },
    /// No sibling site; append to an existing loc file for the language.
    ExistingFileAppend { uri: Url },
    /// No loc file for the language exists at all; a new one is created.
    NewFile { uri: Url },
}

/// The other required, name-derived localisation keys `instance_name` needs
/// (the same test [`crate` … `check_missing_localisation`] in
/// `cwtools_validation` applies), excluding `create_loc_key` itself — there's
/// nothing to anchor on for the key currently being created. Pure: takes the
/// type's loc defs directly rather than a `RuleSet`, so it's testable without
/// one.
fn sibling_loc_keys(
    defs: &[cwtools_rules::rules_types::TypeLocalisation],
    instance_name: &str,
    create_loc_key: &str,
) -> Vec<String> {
    defs.iter()
        .filter(|loc| loc.required && !loc.optional && loc.explicit_field.is_none())
        .map(|loc| format!("{}{}{}", loc.prefix, instance_name, loc.suffix))
        .filter(|k| k != create_loc_key)
        .collect()
}

/// The first of `siblings` that has a known definition site in a workspace
/// (not vanilla) loc file matching `lang`'s filename convention
/// (`l_<lang>`), if any. Siblings are tried in the type's declaration order,
/// so the choice is deterministic.
fn resolve_sibling_site(
    siblings: &[String],
    loc_locations: &crate::LocLocationMap,
    workspace_prefix: &str,
    lang: Lang,
) -> Option<(std::sync::Arc<str>, u32)> {
    let marker = format!("l_{lang}").to_ascii_lowercase();
    siblings.iter().find_map(|sib| {
        let (uri, line0) = loc_locations.get(sib.to_ascii_lowercase().as_str())?;
        if !uri_is_in_workspace(uri, workspace_prefix) {
            return None;
        }
        let fname = uri.rsplit('/').next().unwrap_or("").to_ascii_lowercase();
        fname
            .contains(&marker)
            .then(|| (std::sync::Arc::clone(uri), *line0))
    })
}

/// The first (sorted, for determinism) discovered loc file whose name matches
/// `lang`'s `l_<lang>` convention.
fn pick_lang_file(discovered: &[PathBuf], lang: Lang) -> Option<PathBuf> {
    let marker = format!("l_{lang}").to_ascii_lowercase();
    let mut matches: Vec<&PathBuf> = discovered
        .iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.to_ascii_lowercase().contains(&marker))
        })
        .collect();
    matches.sort();
    matches.first().map(|p| (*p).clone())
}

/// Path for a brand-new generated loc file, when no loc file at all covers
/// `lang`: `<workspace root>/localisation/cwtools_generated_l_<lang>.yml`.
fn generated_loc_file_path(workspace_root: &Path, lang: Lang) -> PathBuf {
    workspace_root
        .join("localisation")
        .join(format!("cwtools_generated_l_{lang}.yml"))
}

/// Resolve where `lang`'s stub line goes, in the three-tier priority order:
/// a sibling key's site, else an existing loc file for the language, else a
/// new one. `discovered_loc_files` is the workspace's loc-file listing
/// (`LocService::discover_files`), walked once up front by the caller and
/// shared across languages. `None` only when a resolved path fails to parse
/// as a `file://` URI (never observed in practice for an absolute path).
fn resolve_loc_insert_target(
    lang: Lang,
    siblings: &[String],
    loc_locations: &crate::LocLocationMap,
    workspace_prefix: &str,
    discovered_loc_files: &[PathBuf],
    workspace_root: &Path,
) -> Option<LocInsertTarget> {
    if let Some((uri, after_line0)) =
        resolve_sibling_site(siblings, loc_locations, workspace_prefix, lang)
    {
        return Some(LocInsertTarget::ExistingFileAfterLine {
            uri: Url::parse(&uri).ok()?,
            after_line0,
        });
    }
    if let Some(path) = pick_lang_file(discovered_loc_files, lang) {
        return Some(LocInsertTarget::ExistingFileAppend {
            uri: Url::from_file_path(&path).ok()?,
        });
    }
    Some(LocInsertTarget::NewFile {
        uri: Url::from_file_path(generated_loc_file_path(workspace_root, lang)).ok()?,
    })
}

/// One loc stub line, unterminated (the caller appends the target file's EOL).
fn stub_line(key: &str) -> String {
    format!(" {key}:0 \"TODO\"")
}

/// The file's dominant end-of-line marker: CRLF if any line uses it, else LF.
fn eol_of(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

/// Position and text to insert `stub` as a new line immediately after 0-based
/// line `after_line0` of `text` — appending at end-of-file when that is the
/// last line. Handles the file having no trailing newline (there's no valid
/// position to start a new line at, so the insertion anchors at the end of
/// the last line and prepends the EOL instead) and an empty file (nothing to
/// anchor on at all). `encoding` converts the anchor column for the one case
/// that needs it (end-of-line, which can be non-ASCII in a translated loc
/// file); the common case (line start) is always column 0 regardless of
/// encoding.
fn insert_stub_after_line(
    text: &str,
    after_line0: u32,
    stub: &str,
    encoding: &PositionEncodingKind,
) -> (Position, String) {
    let eol = eol_of(text);
    if text.is_empty() {
        return (Position::new(0, 0), format!("{stub}{eol}"));
    }
    let ends_with_nl = text.ends_with('\n');
    let line_count = text.lines().count() as u32;
    let next_line_exists =
        after_line0 + 1 < line_count || (after_line0 + 1 == line_count && ends_with_nl);
    if next_line_exists {
        (Position::new(after_line0 + 1, 0), format!("{stub}{eol}"))
    } else {
        let line_text = text.lines().nth(after_line0 as usize).unwrap_or("");
        let col = source_column_to_lsp(line_text, line_text.chars().count() as u32, encoding);
        (Position::new(after_line0, col), format!("{eol}{stub}"))
    }
}

/// One `TextDocumentEdit` operation inserting `new_text` at `pos` (an empty
/// range: pure insertion). `version: None` — the create-loc-key edit targets
/// a file the client may not even have open, so there's no buffer version to
/// pin against.
fn insert_edit_op(uri: Url, pos: Position, new_text: String) -> DocumentChangeOperation {
    DocumentChangeOperation::Edit(TextDocumentEdit {
        text_document: OptionalVersionedTextDocumentIdentifier { uri, version: None },
        edits: vec![OneOf::Left(TextEdit {
            range: Range::new(pos, pos),
            new_text,
        })],
    })
}

impl Backend {
    pub(crate) async fn code_action_impl(
        &self,
        params: CodeActionParams,
    ) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri;
        // The document text is needed for the encoding-aware column conversion.
        // Without it (doc neither open nor readable) no correct edit can be
        // produced, so offer no action rather than a mis-ranged one.
        let Some(text) = self.file_text_for(uri.as_str()) else {
            return Ok(None);
        };
        let encoding = self.state.config.read().position_encoding.clone();
        let only = params.context.only.as_ref();
        let mut actions = Vec::new();
        if wants(only, &CodeActionKind::QUICKFIX) {
            actions.extend(code_actions_from_diagnostics(
                &uri,
                &params.context.diagnostics,
                &text,
                &encoding,
            ));
            actions.extend(self.create_loc_key_actions(
                &uri,
                &params.context.diagnostics,
                &encoding,
            ));
        }
        if wants(only, &CodeActionKind::SOURCE_FIX_ALL)
            && let Some(action) =
                fix_all_action(&uri, &params.context.diagnostics, &text, &encoding)
        {
            actions.push(action);
        }
        if actions.is_empty() {
            Ok(None)
        } else {
            Ok(Some(actions))
        }
    }

    /// Build the "Create localisation key" action for every context diagnostic
    /// carrying a `create_loc_key` fix payload (CW100). Thin gatherer: resolves
    /// the site with the pure functions above, and only reads locks / disk to
    /// hand them the inputs they need. `None` per diagnostic (silently
    /// dropped) when there's no workspace to anchor a path in, or a target
    /// path fails to parse as a URI.
    fn create_loc_key_actions(
        &self,
        uri: &Url,
        diagnostics: &[Diagnostic],
        encoding: &PositionEncodingKind,
    ) -> Vec<CodeActionOrCommand> {
        let candidates: Vec<(&Diagnostic, String, String)> = diagnostics
            .iter()
            .filter_map(|d| {
                let payload = d.data.as_ref().and_then(fix_from_data)?;
                let key = payload.create_loc_key?;
                Some((d, payload.title, key))
            })
            .collect();
        if candidates.is_empty() {
            return Vec::new();
        }
        let (workspace_root, workspace_prefix) = {
            let cfg = self.state.config.read();
            let Some(ws_uri) = cfg.workspace_uri.clone() else {
                return Vec::new();
            };
            let Some(prefix) = cfg.workspace_prefix.clone() else {
                return Vec::new();
            };
            (
                std::path::PathBuf::from(crate::paths::uri_to_path_str(&ws_uri)),
                prefix,
            )
        };
        let langs: Vec<Lang> = {
            let cfg = self.state.config.read();
            cfg.loc_languages
                .clone()
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| vec![Lang::English])
        };
        // One walk of the loc tree serves every candidate diagnostic and every
        // language — cheap next to the disk read, and shared instead of
        // repeated per candidate.
        let discovered =
            tokio::task::block_in_place(|| LocService::discover_files(&[workspace_root.as_path()]));

        candidates
            .into_iter()
            .filter_map(|(diag, title, key)| {
                self.build_create_loc_key_action(
                    uri,
                    diag,
                    &title,
                    &key,
                    &langs,
                    &discovered,
                    &workspace_root,
                    &workspace_prefix,
                    encoding,
                )
            })
            .map(CodeActionOrCommand::CodeAction)
            .collect()
    }

    /// The other required, name-derived loc keys the diagnostic's own instance
    /// needs (see [`sibling_loc_keys`]), found by matching the diagnostic's
    /// start line against `info_service.type_index.instances_in_file` and
    /// confirming the instance actually produces `create_loc_key` (guards
    /// against a coincidental second instance starting on the same line).
    /// Empty when the ruleset isn't loaded or no instance matches.
    fn sibling_loc_keys_for_diagnostic(
        &self,
        uri: &Url,
        diag: &Diagnostic,
        create_loc_key: &str,
    ) -> Vec<String> {
        let rules = self.state.rules.read();
        let Some(ruleset) = rules.ruleset.as_ref() else {
            return Vec::new();
        };
        let info = self.state.info_service.read();
        // The diagnostic's LSP (0-based) start line is the instance's parser
        // (1-based) location line.
        let target_line = diag.range.start.line + 1;
        let instances = info.type_index.instances_in_file(uri.as_str());
        instances
            .into_iter()
            .find_map(|(type_name, inst)| {
                if inst.location.line != target_line {
                    return None;
                }
                let td = ruleset.types.iter().find(|td| td.name == type_name)?;
                let owns_key = td.localisation.iter().any(|loc| {
                    loc.required
                        && !loc.optional
                        && loc.explicit_field.is_none()
                        && format!("{}{}{}", loc.prefix, inst.name, loc.suffix) == create_loc_key
                });
                owns_key.then(|| sibling_loc_keys(&td.localisation, &inst.name, create_loc_key))
            })
            .unwrap_or_default()
    }

    /// Build one `CodeAction` inserting `create_loc_key`'s stub line for every
    /// language in `langs`. `None` when any language's target can't be
    /// resolved or its file's text can't be read — offering a partial fix
    /// (some languages stubbed, some not) would be a worse outcome than none.
    #[allow(clippy::too_many_arguments)]
    fn build_create_loc_key_action(
        &self,
        uri: &Url,
        diag: &Diagnostic,
        title: &str,
        create_loc_key: &str,
        langs: &[Lang],
        discovered: &[PathBuf],
        workspace_root: &Path,
        workspace_prefix: &str,
        encoding: &PositionEncodingKind,
    ) -> Option<CodeAction> {
        let siblings = self.sibling_loc_keys_for_diagnostic(uri, diag, create_loc_key);
        let loc_locations = self.state.loc_locations.read();
        let stub = stub_line(create_loc_key);
        let mut operations = Vec::new();
        for &lang in langs {
            let target = resolve_loc_insert_target(
                lang,
                &siblings,
                &loc_locations,
                workspace_prefix,
                discovered,
                workspace_root,
            )?;
            operations.extend(self.loc_insert_operations(target, &stub, lang, encoding)?);
        }
        drop(loc_locations);
        Some(CodeAction {
            title: title.to_string(),
            kind: Some(CodeActionKind::QUICKFIX),
            diagnostics: Some(vec![diag.clone()]),
            edit: Some(WorkspaceEdit {
                changes: None,
                document_changes: Some(DocumentChanges::Operations(operations)),
                change_annotations: None,
            }),
            ..Default::default()
        })
    }

    /// The one or two `DocumentChangeOperation`s (`Create` + `Edit`, or just
    /// `Edit`) that realize `target`. Reads the target file's current text via
    /// `file_text_for` (open-doc buffer or disk) for the two existing-file
    /// variants; `NewFile` needs no read since the content is built whole.
    fn loc_insert_operations(
        &self,
        target: LocInsertTarget,
        stub: &str,
        lang: Lang,
        encoding: &PositionEncodingKind,
    ) -> Option<Vec<DocumentChangeOperation>> {
        match target {
            LocInsertTarget::ExistingFileAfterLine { uri, after_line0 } => {
                let text = self.file_text_for(uri.as_str())?;
                let (pos, insert_text) = insert_stub_after_line(&text, after_line0, stub, encoding);
                Some(vec![insert_edit_op(uri, pos, insert_text)])
            }
            LocInsertTarget::ExistingFileAppend { uri } => {
                let text = self.file_text_for(uri.as_str())?;
                let last_line0 = text.lines().count().saturating_sub(1) as u32;
                let (pos, insert_text) = insert_stub_after_line(&text, last_line0, stub, encoding);
                Some(vec![insert_edit_op(uri, pos, insert_text)])
            }
            LocInsertTarget::NewFile { uri } => {
                let content = format!("\u{FEFF}l_{lang}:\n{stub}\n");
                Some(vec![
                    DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                        uri: uri.clone(),
                        options: Some(CreateFileOptions {
                            overwrite: Some(false),
                            ignore_if_exists: Some(true),
                        }),
                        annotation_id: None,
                    })),
                    insert_edit_op(uri, Position::new(0, 0), content),
                ])
            }
        }
    }
}

// ── fixAllWorkspace ────────────────────────────────────────────────────────
//
// The `fixAllWorkspace` execute-command applies every currently-fixable
// diagnostic across the whole workspace in one `workspace/applyEdit`,
// mirroring `cwtools fix --apply`. It snapshots `DocumentState::fixable_edits`
// (kept current by every `validate::publish_filtered` call) instead of
// re-running validation, so it fixes exactly what the Problems panel shows.
// CW100's create-key fix never appears in that store — see its doc comment.

/// One URI's resolved fix-all-workspace edits: the survivors after overlap
/// resolution (`plan_file_edits`), and how many were dropped for overlapping
/// another kept edit.
struct PlannedFileFixes {
    uri: String,
    kept: Vec<SpanEdit>,
    skipped: usize,
}

/// Resolve every URI's stored fixable edits against its current text via
/// `plan_file_edits` — the same overlap resolution `source.fixAll` and the
/// CLI `fix` subcommand use, so all three agree on what a fixed workspace
/// looks like. `texts` maps URI -> current text; a URI missing from it (the
/// file couldn't be read when the command ran) is dropped. Pure: the caller
/// does the reads and hands the results in.
fn plan_workspace_fixes(
    snapshot: &HashMap<String, Vec<(String, SpanEdit)>>,
    texts: &HashMap<String, String>,
) -> Vec<PlannedFileFixes> {
    snapshot
        .iter()
        .filter_map(|(uri, entries)| {
            let text = texts.get(uri)?;
            let (kept, skipped) = plan_file_edits(text, entries.clone());
            Some(PlannedFileFixes {
                uri: uri.clone(),
                kept,
                skipped: skipped.len(),
            })
        })
        .collect()
}

/// The `workspace/applyEdit` `changes` map for every file with at least one
/// surviving edit, converted to LSP `TextEdit`s with the negotiated encoding
/// against each file's own text. A URI that fails to parse (never observed —
/// it round-tripped through `publish_filtered` as a valid URI) is dropped
/// rather than panicking.
fn workspace_edit_changes(
    planned: &[PlannedFileFixes],
    texts: &HashMap<String, String>,
    encoding: &PositionEncodingKind,
) -> HashMap<Url, Vec<TextEdit>> {
    let mut changes = HashMap::new();
    for pf in planned {
        if pf.kept.is_empty() {
            continue;
        }
        let Ok(uri) = Url::parse(&pf.uri) else {
            continue;
        };
        let text = texts.get(&pf.uri).map(String::as_str).unwrap_or("");
        let edits: Vec<TextEdit> = pf
            .kept
            .iter()
            .map(|e| TextEdit {
                range: source_range_to_lsp(e.range, text, encoding),
                new_text: e.replacement.clone(),
            })
            .collect();
        changes.insert(uri, edits);
    }
    changes
}

/// The command's result message on success:
/// `"Applied N fix(es) across M file(s)"`, with an `"; K skipped
/// (overlapping)"` suffix when any edit was dropped for overlapping — the
/// same wording the CLI `fix --apply` subcommand uses.
fn fix_all_workspace_summary(edits_applied: usize, files_changed: usize, skipped: usize) -> String {
    let mut msg = format!("Applied {edits_applied} fix(es) across {files_changed} file(s)");
    if skipped > 0 {
        msg.push_str(&format!("; {skipped} skipped (overlapping)"));
    }
    msg
}

impl Backend {
    /// `fixAllWorkspace` execute-command handler. Returns the message shown to
    /// the user (no result payload otherwise): a "nothing to do" message when
    /// the store is empty or every entry resolved to zero edits, an error
    /// message when the client rejects the `workspace/applyEdit`, else the
    /// summary from [`fix_all_workspace_summary`].
    pub(crate) async fn fix_all_workspace_impl(&self) -> String {
        let snapshot = self.state.fixable_edits.lock().clone();
        if snapshot.is_empty() {
            return "No auto-fixable problems in the workspace.".to_string();
        }
        let texts: HashMap<String, String> = snapshot
            .keys()
            .filter_map(|uri| self.file_text_for(uri).map(|t| (uri.clone(), t)))
            .collect();
        let planned = plan_workspace_fixes(&snapshot, &texts);
        let encoding = self.state.config.read().position_encoding.clone();
        let changes = workspace_edit_changes(&planned, &texts, &encoding);
        if changes.is_empty() {
            return "No auto-fixable problems in the workspace.".to_string();
        }
        let edits_applied: usize = changes.values().map(Vec::len).sum();
        let files_changed = changes.len();
        let skipped: usize = planned.iter().map(|pf| pf.skipped).sum();
        let edit = WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        };
        match self.client.apply_edit(edit).await {
            Ok(resp) if resp.applied => {
                fix_all_workspace_summary(edits_applied, files_changed, skipped)
            }
            Ok(resp) => format!(
                "The client rejected the workspace edit{}.",
                resp.failure_reason
                    .map(|r| format!(": {r}"))
                    .unwrap_or_default()
            ),
            Err(e) => format!("The client rejected the workspace edit: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cwtools_parser::fix::{SpanEdit, apply_edits};

    fn range(sl: u32, sc: u16, el: u32, ec: u16) -> SourceRange {
        SourceRange {
            start: SourcePos { line: sl, col: sc },
            end: SourcePos { line: el, col: ec },
        }
    }

    #[test]
    fn payload_round_trips_through_data() {
        // A fix serialized into `Diagnostic.data` and read back must reproduce
        // the title, ranges, and replacement exactly (the client hands `data`
        // back verbatim on the codeAction request).
        let fix = SuggestedFix::replace("Wrap the value in quotes", range(5, 3, 5, 8), "\"hi\"");
        let data = fix_to_data(&fix);
        let parsed = fix_from_data(&data).expect("payload parses");
        assert_eq!(parsed.title, "Wrap the value in quotes");
        assert_eq!(parsed.edits.len(), 1);
        assert_eq!(parsed.edits[0].range, range(5, 3, 5, 8));
        assert_eq!(parsed.edits[0].replacement, "\"hi\"");
        assert_eq!(parsed.create_loc_key, None);
    }

    #[test]
    fn create_loc_key_payload_round_trips_through_data() {
        // CW100's fix has no span edit, only the key; both the empty `edits`
        // and the `createLocKey` field must survive the `Diagnostic.data`
        // round trip so `create_loc_key_actions` can build its action.
        let fix =
            SuggestedFix::create_loc_key("Create localisation key my_thing_desc", "my_thing_desc");
        let data = fix_to_data(&fix);
        let parsed = fix_from_data(&data).expect("payload parses");
        assert_eq!(parsed.title, "Create localisation key my_thing_desc");
        assert!(parsed.edits.is_empty());
        assert_eq!(parsed.create_loc_key.as_deref(), Some("my_thing_desc"));
    }

    #[test]
    fn non_fix_data_is_ignored() {
        // Diagnostic data that isn't a cwtools fix payload must not parse as one.
        assert!(fix_from_data(&serde_json::json!({ "other": 1 })).is_none());
        assert!(fix_from_data(&serde_json::json!(null)).is_none());
        // Missing the replacement field → not a usable edit → whole payload None.
        let bad = serde_json::json!({
            FIX_DATA_KEY: { "title": "x", "edits": [{ "startLine": 1, "startCol": 0, "endLine": 1, "endCol": 1 }] }
        });
        assert!(fix_from_data(&bad).is_none());
    }

    #[test]
    fn diagnostic_with_fix_maps_to_quickfix_action() {
        // The handler mapping: a diagnostic carrying a fix payload becomes one
        // QUICKFIX CodeAction whose edit, applied to the source, yields the fixed
        // text. Mirrors the CW253 `set_empire_name` -> `set_name` key rename.
        let text = "set_empire_name = { }\n";
        let fix = SuggestedFix::replace("Rename to set_name", range(1, 0, 1, 15), "set_name");
        let uri: Url = "file:///mod/common/x.txt".parse().unwrap();
        let diag = Diagnostic {
            range: Range::default(),
            severity: Some(DiagnosticSeverity::WARNING),
            code: Some(NumberOrString::String("CW253".into())),
            source: Some("cwtools".into()),
            message: "renamed effect".into(),
            data: Some(fix_to_data(&fix)),
            ..Default::default()
        };

        let actions = code_actions_from_diagnostics(
            &uri,
            std::slice::from_ref(&diag),
            text,
            &PositionEncodingKind::UTF16,
        );
        assert_eq!(actions.len(), 1, "one fix -> one action");
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected a CodeAction, not a Command");
        };
        assert_eq!(action.title, "Rename to set_name");
        assert_eq!(action.kind, Some(CodeActionKind::QUICKFIX));
        assert_eq!(action.diagnostics.as_ref().unwrap()[0].code, diag.code);

        // The edit's range + new_text must reproduce the CLI `fix` result.
        let edits = action
            .edit
            .as_ref()
            .and_then(|e| e.changes.as_ref())
            .and_then(|c| c.get(&uri))
            .expect("edit targets the document");
        assert_eq!(edits.len(), 1);
        assert_eq!(
            edits[0].range,
            Range::new(Position::new(0, 0), Position::new(0, 15))
        );
        assert_eq!(edits[0].new_text, "set_name");

        // Apply via the engine's own applier to confirm the span is right.
        let span = SpanEdit {
            range: range(1, 0, 1, 15),
            replacement: "set_name".into(),
        };
        assert_eq!(apply_edits(text, &[span]), "set_name = { }\n");
    }

    #[test]
    fn diagnostic_range_and_fix_edit_agree_on_non_bmp_line() {
        // A non-BMP char before the token makes the parser's char column (2) and
        // the client's UTF-16 column (3) differ. The published diagnostic and the
        // quick fix it carries must land on the same range, or applying the fix
        // rewrites a different span than the one highlighted.
        let text = "😀 set_empire_name = { }\n";
        let fix = SuggestedFix::replace("Rename to set_name", range(1, 2, 1, 17), "set_name");
        let err = cwtools_validation::ValidationError {
            message: "renamed effect".into(),
            severity: cwtools_validation::ErrorSeverity::Warning,
            line: 1,
            col: 2,
            file: "f".into(),
            code: Some("CW253"),
            fix: Some(fix),
            end: Some((1, 17)),
        };
        let lines = crate::validate::DocLines::new(text, PositionEncodingKind::UTF16);
        let diag = crate::validate::validation_error_to_diagnostic(&err, &lines);
        assert_eq!(
            diag.range,
            Range::new(Position::new(0, 3), Position::new(0, 18)),
            "diagnostic range must use UTF-16 columns"
        );

        let uri: Url = "file:///mod/common/x.txt".parse().unwrap();
        let actions = code_actions_from_diagnostics(
            &uri,
            std::slice::from_ref(&diag),
            text,
            &PositionEncodingKind::UTF16,
        );
        let edits = match &actions[0] {
            CodeActionOrCommand::CodeAction(a) => a
                .edit
                .as_ref()
                .and_then(|e| e.changes.as_ref())
                .and_then(|c| c.get(&uri))
                .expect("edit targets the document"),
            _ => panic!("expected a CodeAction"),
        };
        assert_eq!(
            edits[0].range, diag.range,
            "quick fix must edit exactly the highlighted span"
        );
        // The parser-space span the payload carries is the right one.
        let span = SpanEdit {
            range: range(1, 2, 1, 17),
            replacement: "set_name".into(),
        };
        assert_eq!(apply_edits(text, &[span]), "😀 set_name = { }\n");
    }

    #[test]
    fn diagnostic_without_data_yields_no_action() {
        let uri: Url = "file:///mod/common/x.txt".parse().unwrap();
        let diag = Diagnostic {
            message: "plain".into(),
            ..Default::default()
        };
        let actions = code_actions_from_diagnostics(
            &uri,
            std::slice::from_ref(&diag),
            "x = y\n",
            &PositionEncodingKind::UTF16,
        );
        assert!(actions.is_empty());
    }

    fn create_loc_key_diag() -> Diagnostic {
        let fix =
            SuggestedFix::create_loc_key("Create localisation key my_thing_desc", "my_thing_desc");
        Diagnostic {
            range: Range::default(),
            severity: Some(DiagnosticSeverity::WARNING),
            code: Some(NumberOrString::String("CW100".into())),
            source: Some("cwtools".into()),
            message: "my_thing is missing localisation: my_thing_desc".into(),
            data: Some(fix_to_data(&fix)),
            ..Default::default()
        }
    }

    #[test]
    fn zero_edit_payload_yields_no_plain_quickfix() {
        // CW100's fix has no span edit — `code_actions_from_diagnostics` must
        // not offer a quickfix with an empty `TextEdit` list for it; the
        // dedicated create-loc-key action (built elsewhere) covers it.
        let uri: Url = "file:///mod/common/x.txt".parse().unwrap();
        let diag = create_loc_key_diag();
        let actions = code_actions_from_diagnostics(
            &uri,
            std::slice::from_ref(&diag),
            "my_thing = { x = yes }\n",
            &PositionEncodingKind::UTF16,
        );
        assert!(actions.is_empty(), "got: {actions:?}");
    }

    // ── fixable_span_edits: direct unit coverage ──────────────────────────────

    #[test]
    fn fixable_span_edits_normal_payload_yields_pairs() {
        let fix = SuggestedFix::replace("Rename to set_name", range(1, 0, 1, 15), "set_name");
        let diag = Diagnostic {
            code: Some(NumberOrString::String("CW253".into())),
            data: Some(fix_to_data(&fix)),
            ..Default::default()
        };
        let pairs = fixable_span_edits(&diag);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "CW253");
        assert_eq!(pairs[0].1.range, range(1, 0, 1, 15));
        assert_eq!(pairs[0].1.replacement, "set_name");
    }

    #[test]
    fn fixable_span_edits_no_data_yields_empty() {
        let diag = Diagnostic {
            code: Some(NumberOrString::String("CW253".into())),
            ..Default::default()
        };
        assert!(fixable_span_edits(&diag).is_empty());
    }

    #[test]
    fn fixable_span_edits_numeric_code_yields_empty() {
        // `fix_from_data` parses fine, but a numeric `code` can't be stored as a
        // key in the fixAllWorkspace store, so the pair is dropped rather than
        // stringified.
        let fix = SuggestedFix::replace("Rename to set_name", range(1, 0, 1, 15), "set_name");
        let diag = Diagnostic {
            code: Some(NumberOrString::Number(253)),
            data: Some(fix_to_data(&fix)),
            ..Default::default()
        };
        assert!(fixable_span_edits(&diag).is_empty());
    }

    #[test]
    fn fixable_span_edits_create_loc_key_payload_yields_empty() {
        // The invariant that keeps CW100 out of the fixAllWorkspace store
        // (`publish_filtered`'s doc comment): a create-loc-key payload parses
        // fine but carries no span edits, so this must report nothing for it.
        let diag = create_loc_key_diag();
        assert!(fixable_span_edits(&diag).is_empty());
    }

    #[test]
    fn fix_all_action_does_not_claim_create_loc_key_diagnostics() {
        // Mix a normal span-edit fix with a CW100 create-loc-key fix.
        // `source.fixAll` must apply only the real edit and must not list the
        // CW100 diagnostic among the ones it resolves.
        let text = "set_empire_name = { }\nmy_thing = { x = yes }\n";
        let real_fix = SuggestedFix::replace("Rename to set_name", range(1, 0, 1, 15), "set_name");
        let real_diag = Diagnostic {
            range: Range::default(),
            severity: Some(DiagnosticSeverity::WARNING),
            code: Some(NumberOrString::String("CW253".into())),
            source: Some("cwtools".into()),
            message: "renamed effect".into(),
            data: Some(fix_to_data(&real_fix)),
            ..Default::default()
        };
        let loc_diag = create_loc_key_diag();
        let uri: Url = "file:///mod/common/x.txt".parse().unwrap();
        let action = fix_all_action(
            &uri,
            &[real_diag.clone(), loc_diag],
            text,
            &PositionEncodingKind::UTF16,
        )
        .expect("one real edit survives");
        let CodeActionOrCommand::CodeAction(action) = action else {
            panic!("expected a CodeAction");
        };
        assert_eq!(action.title, "Fix all (1 auto-fixable)");
        let resolved = action.diagnostics.expect("resolved diagnostics");
        assert_eq!(resolved.len(), 1, "must not claim the CW100 diagnostic");
        assert_eq!(resolved[0].code, real_diag.code);
    }

    // ── Create missing localisation key: pure-function coverage ───────────────
    // These exercise the resolution/insertion core directly, with no Backend —
    // per the design note in the module doc, the Backend method is a thin
    // gatherer over these.

    fn loc_def(
        name: &str,
        prefix: &str,
        suffix: &str,
    ) -> cwtools_rules::rules_types::TypeLocalisation {
        cwtools_rules::rules_types::TypeLocalisation {
            name: name.to_string(),
            prefix: prefix.to_string(),
            suffix: suffix.to_string(),
            required: true,
            optional: false,
            explicit_field: None,
            replace_scopes: None,
            primary: false,
        }
    }

    #[test]
    fn sibling_loc_keys_excludes_self_and_non_required() {
        let mut optional_def = loc_def("desc", "", "_desc");
        optional_def.optional = true;
        let defs = vec![
            loc_def("name", "", ""),
            loc_def("desc", "", "_desc"),
            optional_def,
            loc_def("title", "", "_title"),
        ];
        let siblings = sibling_loc_keys(&defs, "my_thing", "my_thing_desc");
        // `name` -> "my_thing" (a sibling), `desc` -> "my_thing_desc" (self,
        // excluded), the optional dup is dropped by the `required` filter,
        // `title` -> "my_thing_title" (a sibling).
        assert_eq!(siblings, vec!["my_thing", "my_thing_title"]);
    }

    fn loc_locations_with(entries: &[(&str, &str, u32)]) -> crate::LocLocationMap {
        entries
            .iter()
            .map(|(key, uri, line)| {
                (
                    std::sync::Arc::from(key.to_ascii_lowercase()),
                    (std::sync::Arc::from(*uri), *line),
                )
            })
            .collect()
    }

    #[test]
    fn resolve_sibling_site_picks_the_matching_workspace_language_file() {
        let locs = loc_locations_with(&[(
            "my_thing",
            "file:///ws/localisation/things_l_english.yml",
            4,
        )]);
        let siblings = vec!["my_thing".to_string()];
        let hit = resolve_sibling_site(&siblings, &locs, "/ws", Lang::English).expect("a hit");
        assert_eq!(
            hit.0.as_ref(),
            "file:///ws/localisation/things_l_english.yml"
        );
        assert_eq!(hit.1, 4);
    }

    #[test]
    fn resolve_sibling_site_rejects_a_vanilla_definition() {
        let locs = loc_locations_with(&[(
            "my_thing",
            "file:///vanilla/localisation/things_l_english.yml",
            4,
        )]);
        let siblings = vec!["my_thing".to_string()];
        assert!(resolve_sibling_site(&siblings, &locs, "/ws", Lang::English).is_none());
    }

    #[test]
    fn resolve_sibling_site_rejects_the_wrong_language_file() {
        let locs =
            loc_locations_with(&[("my_thing", "file:///ws/localisation/things_l_french.yml", 4)]);
        let siblings = vec!["my_thing".to_string()];
        assert!(resolve_sibling_site(&siblings, &locs, "/ws", Lang::English).is_none());
    }

    #[test]
    fn resolve_sibling_site_tries_siblings_in_order() {
        // The first sibling has no known site; the second does.
        let locs = loc_locations_with(&[(
            "my_thing_title",
            "file:///ws/localisation/things_l_english.yml",
            9,
        )]);
        let siblings = vec!["my_thing".to_string(), "my_thing_title".to_string()];
        let hit = resolve_sibling_site(&siblings, &locs, "/ws", Lang::English).expect("a hit");
        assert_eq!(hit.1, 9);
    }

    #[test]
    fn pick_lang_file_matches_by_name_and_sorts_for_determinism() {
        let files = vec![
            PathBuf::from("/ws/localisation/z_l_english.yml"),
            PathBuf::from("/ws/localisation/a_l_english.yml"),
            PathBuf::from("/ws/localisation/other_l_french.yml"),
        ];
        let picked = pick_lang_file(&files, Lang::English).expect("a match");
        assert_eq!(picked, PathBuf::from("/ws/localisation/a_l_english.yml"));
    }

    #[test]
    fn pick_lang_file_none_when_no_file_matches() {
        let files = vec![PathBuf::from("/ws/localisation/other_l_french.yml")];
        assert!(pick_lang_file(&files, Lang::English).is_none());
    }

    #[test]
    fn insert_stub_after_line_mid_file_inserts_on_the_next_line() {
        let text = "l_english:\n my_thing:0 \"Thing\"\n my_thing_title:0 \"Title\"\n";
        let (pos, insert) = insert_stub_after_line(
            text,
            1,
            " my_thing_desc:0 \"TODO\"",
            &PositionEncodingKind::UTF16,
        );
        assert_eq!(pos, Position::new(2, 0));
        assert_eq!(insert, " my_thing_desc:0 \"TODO\"\n");
    }

    #[test]
    fn insert_stub_after_line_at_eof_with_trailing_newline() {
        let text = "l_english:\n my_thing:0 \"Thing\"\n";
        let (pos, insert) = insert_stub_after_line(
            text,
            1,
            " my_thing_desc:0 \"TODO\"",
            &PositionEncodingKind::UTF16,
        );
        assert_eq!(pos, Position::new(2, 0));
        assert_eq!(insert, " my_thing_desc:0 \"TODO\"\n");
    }

    #[test]
    fn insert_stub_after_line_at_eof_without_trailing_newline() {
        // No trailing newline: there's no valid position to start a new line
        // at, so the insertion anchors at the end of the last line and
        // prepends the EOL instead of appending it.
        let text = "l_english:\n my_thing:0 \"Thing\"";
        let (pos, insert) = insert_stub_after_line(
            text,
            1,
            " my_thing_desc:0 \"TODO\"",
            &PositionEncodingKind::UTF16,
        );
        assert_eq!(
            pos,
            Position::new(1, " my_thing:0 \"Thing\"".chars().count() as u32)
        );
        assert_eq!(insert, "\n my_thing_desc:0 \"TODO\"");
    }

    #[test]
    fn insert_stub_after_line_on_an_empty_file() {
        let (pos, insert) =
            insert_stub_after_line("", 0, " my_thing:0 \"TODO\"", &PositionEncodingKind::UTF16);
        assert_eq!(pos, Position::new(0, 0));
        assert_eq!(insert, " my_thing:0 \"TODO\"\n");
    }

    #[test]
    fn insert_stub_after_line_uses_the_files_dominant_crlf_eol() {
        let text = "l_english:\r\n my_thing:0 \"Thing\"\r\n";
        let (pos, insert) = insert_stub_after_line(
            text,
            1,
            " my_thing_desc:0 \"TODO\"",
            &PositionEncodingKind::UTF16,
        );
        assert_eq!(pos, Position::new(2, 0));
        assert_eq!(insert, " my_thing_desc:0 \"TODO\"\r\n");
    }

    #[test]
    fn insert_stub_after_line_end_column_is_encoding_aware() {
        // A non-BMP char before the anchor column makes the UTF-16 column (3)
        // differ from the char count (2); the anchor must use the negotiated
        // encoding, since loc lines can carry non-ASCII translations.
        let text = "l_english:\n 😀desc:0 \"x\"";
        let (pos, _) =
            insert_stub_after_line(text, 1, " k:0 \"TODO\"", &PositionEncodingKind::UTF16);
        assert_eq!(
            pos,
            Position::new(1, " 😀desc:0 \"x\"".encode_utf16().count() as u32)
        );
    }

    #[test]
    fn insert_stub_after_line_degrades_when_the_anchor_line_no_longer_exists() {
        // `after_line0` comes from a scan-time `loc_locations` entry. If the loc
        // file shrank since that scan (lines removed by hand or by another
        // fix), the anchor line can point past the current end of the file.
        // This must stay an append-style edit anchored at/after the end of the
        // real content (a position the client clamps into range), never a
        // panic and never a mid-file edit landing on the wrong line.
        let text = "l_english:\n my_thing:0 \"Thing\"\n";
        let (pos, insert) = insert_stub_after_line(
            text,
            10,
            " my_thing_desc:0 \"TODO\"",
            &PositionEncodingKind::UTF16,
        );
        assert_eq!(
            pos,
            Position::new(10, 0),
            "anchors past the real content rather than splicing mid-file"
        );
        assert_eq!(insert, "\n my_thing_desc:0 \"TODO\"");
    }

    #[test]
    fn resolve_loc_insert_target_resolves_independently_per_language() {
        // `build_create_loc_key_action` resolves one target per configured
        // language and requires every one of them to succeed (`?` inside its
        // loop) before it commits to an action. This pins the pure
        // per-language piece that loop depends on: English has a sibling
        // site, French has none of the first two tiers and falls through to
        // NewFile. The Backend-level all-or-nothing short-circuit itself
        // needs a running Backend (`file_text_for`, config locks) to exercise
        // — not covered here; the CreateFile-tier integration test covers the
        // NewFile branch end to end for a single language instead.
        let root = Path::new("/ws");
        let locs = loc_locations_with(&[(
            "my_thing",
            "file:///ws/localisation/things_l_english.yml",
            4,
        )]);
        let siblings = vec!["my_thing".to_string()];

        let en =
            resolve_loc_insert_target(Lang::English, &siblings, &locs, "/ws", &[], root).unwrap();
        assert_eq!(
            en,
            LocInsertTarget::ExistingFileAfterLine {
                uri: "file:///ws/localisation/things_l_english.yml"
                    .parse()
                    .unwrap(),
                after_line0: 4,
            }
        );

        let fr =
            resolve_loc_insert_target(Lang::French, &siblings, &locs, "/ws", &[], root).unwrap();
        assert_eq!(
            fr,
            LocInsertTarget::NewFile {
                uri: Url::from_file_path("/ws/localisation/cwtools_generated_l_french.yml")
                    .unwrap(),
            }
        );
    }

    #[test]
    fn resolve_loc_insert_target_prefers_sibling_over_existing_file_over_new() {
        let root = Path::new("/ws");
        // Case 1: a sibling site exists -> ExistingFileAfterLine.
        let locs = loc_locations_with(&[(
            "my_thing",
            "file:///ws/localisation/things_l_english.yml",
            4,
        )]);
        let siblings = vec!["my_thing".to_string()];
        let target =
            resolve_loc_insert_target(Lang::English, &siblings, &locs, "/ws", &[], root).unwrap();
        assert_eq!(
            target,
            LocInsertTarget::ExistingFileAfterLine {
                uri: "file:///ws/localisation/things_l_english.yml"
                    .parse()
                    .unwrap(),
                after_line0: 4,
            }
        );

        // Case 2: no sibling site, but a discovered loc file for the language.
        let empty_locs = crate::LocLocationMap::default();
        let discovered = vec![PathBuf::from("/ws/localisation/things_l_english.yml")];
        let target = resolve_loc_insert_target(
            Lang::English,
            &siblings,
            &empty_locs,
            "/ws",
            &discovered,
            root,
        )
        .unwrap();
        assert_eq!(
            target,
            LocInsertTarget::ExistingFileAppend {
                uri: Url::from_file_path("/ws/localisation/things_l_english.yml").unwrap(),
            }
        );

        // Case 3: nothing at all -> a brand new generated file.
        let target =
            resolve_loc_insert_target(Lang::English, &siblings, &empty_locs, "/ws", &[], root)
                .unwrap();
        assert_eq!(
            target,
            LocInsertTarget::NewFile {
                uri: Url::from_file_path("/ws/localisation/cwtools_generated_l_english.yml")
                    .unwrap(),
            }
        );
    }

    // ── fixAllWorkspace: pure planning/summary pieces ─────────────────────────

    fn span(sl: u32, sc: u16, el: u32, ec: u16, repl: &str) -> SpanEdit {
        SpanEdit {
            range: range(sl, sc, el, ec),
            replacement: repl.to_string(),
        }
    }

    #[test]
    fn plan_workspace_fixes_drops_uris_with_no_readable_text() {
        let mut snapshot = HashMap::new();
        snapshot.insert(
            "file:///a.txt".to_string(),
            vec![("CW253".to_string(), span(1, 0, 1, 4, "X"))],
        );
        snapshot.insert(
            "file:///unreadable.txt".to_string(),
            vec![("CW253".to_string(), span(1, 0, 1, 4, "X"))],
        );
        let mut texts = HashMap::new();
        texts.insert("file:///a.txt".to_string(), "aaaa bbbb\n".to_string());
        let planned = plan_workspace_fixes(&snapshot, &texts);
        assert_eq!(planned.len(), 1, "the unreadable URI is dropped");
        assert_eq!(planned[0].uri, "file:///a.txt");
        assert_eq!(planned[0].kept.len(), 1);
        assert_eq!(planned[0].skipped, 0);
    }

    #[test]
    fn plan_workspace_fixes_reports_overlap_skips_per_file() {
        let mut snapshot = HashMap::new();
        // "aaaa b" and "bbbb" share column 5 — the same overlap fixture as
        // `plan_file_edits`'s own test.
        snapshot.insert(
            "file:///a.txt".to_string(),
            vec![
                ("A".to_string(), span(1, 0, 1, 6, "X")),
                ("B".to_string(), span(1, 5, 1, 9, "Y")),
            ],
        );
        let mut texts = HashMap::new();
        texts.insert("file:///a.txt".to_string(), "aaaa bbbb\n".to_string());
        let planned = plan_workspace_fixes(&snapshot, &texts);
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].kept.len(), 1);
        assert_eq!(planned[0].skipped, 1);
    }

    #[test]
    fn workspace_edit_changes_skips_files_with_nothing_kept() {
        let planned = vec![
            PlannedFileFixes {
                uri: "file:///a.txt".to_string(),
                kept: vec![SpanEdit {
                    range: range(1, 0, 1, 4),
                    replacement: "X".to_string(),
                }],
                skipped: 0,
            },
            PlannedFileFixes {
                uri: "file:///b.txt".to_string(),
                kept: Vec::new(),
                skipped: 1,
            },
        ];
        let mut texts = HashMap::new();
        texts.insert("file:///a.txt".to_string(), "aaaa bbbb\n".to_string());
        texts.insert("file:///b.txt".to_string(), "cccc\n".to_string());
        let changes = workspace_edit_changes(&planned, &texts, &PositionEncodingKind::UTF16);
        assert_eq!(changes.len(), 1, "only the file with a surviving edit");
        let uri: Url = "file:///a.txt".parse().unwrap();
        assert_eq!(changes[&uri][0].new_text, "X");
    }

    #[test]
    fn fix_all_workspace_summary_wording() {
        assert_eq!(
            fix_all_workspace_summary(3, 2, 0),
            "Applied 3 fix(es) across 2 file(s)"
        );
        assert_eq!(
            fix_all_workspace_summary(3, 2, 1),
            "Applied 3 fix(es) across 2 file(s); 1 skipped (overlapping)"
        );
    }
}
