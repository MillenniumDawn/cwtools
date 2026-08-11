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
//! `resolve_loc_insert_target` for the three-tier site it picks, and
//! `build_create_loc_key_batch` for how a batch of several missing keys
//! shares one not-yet-existing file instead of each recreating it (#142).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

use cwtools_localization::{Lang, LocService};
use cwtools_parser::ast::{SourcePos, SourceRange};
use cwtools_parser::fix::{SpanEdit, SuggestedFix, plan_file_edits};

use crate::Backend;
use crate::paths::{source_column_to_lsp, source_position_to_lsp};

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
        .filter(|loc| loc.is_required_name_derived())
        .map(|loc| loc.derived_key(instance_name))
        .filter(|k| k != create_loc_key)
        .collect()
}

/// The first of `siblings` that has a known definition site in a loc file the
/// edit boundary will accept (`edit_roots`, so never vanilla) matching `lang`'s
/// filename convention (`l_<lang>`), if any. Siblings are tried in the type's
/// declaration order, so the choice is deterministic.
///
/// `loc_locations` mixes workspace and base-game definition sites and records
/// the path the loc walk saw, which can be a symlink; a site the boundary
/// refuses is skipped and the next sibling tried.
fn resolve_sibling_site(
    siblings: &[String],
    loc_locations: &crate::LocLocationMap,
    edit_roots: &[PathBuf],
    lang: Lang,
) -> Option<(std::sync::Arc<str>, u32)> {
    let marker = format!("l_{lang}").to_ascii_lowercase();
    siblings.iter().find_map(|sib| {
        let (uri, line0) = loc_locations.get(sib.to_ascii_lowercase().as_str())?;
        // Name first, boundary second: the filename test is free, the boundary
        // stats the disk.
        let fname = uri.rsplit('/').next().unwrap_or("").to_ascii_lowercase();
        if !fname.contains(&marker) {
            return None;
        }
        crate::access::editable_path(uri, edit_roots).ok()?;
        Some((std::sync::Arc::clone(uri), *line0))
    })
}

/// The first (sorted, for determinism) discovered loc file whose name matches
/// `lang`'s `l_<lang>` convention and which the edit boundary accepts. The loc
/// walk follows symlinks, so a discovered path can name a file outside the
/// workspace; the boundary runs after the sort, so a workspace full of loc
/// files costs one `canonicalize`, not one per file.
fn pick_lang_file(discovered: &[PathBuf], lang: Lang, edit_roots: &[PathBuf]) -> Option<PathBuf> {
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
    matches
        .into_iter()
        .find(|p| crate::access::editable_target(p, edit_roots).is_ok())
        .cloned()
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
/// shared across languages.
///
/// Every tier is filtered by the edit boundary against `edit_roots`, so the
/// target this returns is one a generated edit may write to (#160). Filtering
/// per tier rather than once at the end keeps the fall-through: a site the
/// boundary refuses drops to the next tier instead of costing the whole action.
/// `None` when even the new-file path is refused, or when a resolved path fails
/// to parse as a `file://` URI.
fn resolve_loc_insert_target(
    lang: Lang,
    siblings: &[String],
    loc_locations: &crate::LocLocationMap,
    edit_roots: &[PathBuf],
    discovered_loc_files: &[PathBuf],
    workspace_root: &Path,
) -> Option<LocInsertTarget> {
    if let Some((uri, after_line0)) =
        resolve_sibling_site(siblings, loc_locations, edit_roots, lang)
    {
        return Some(LocInsertTarget::ExistingFileAfterLine {
            uri: Url::parse(&uri).ok()?,
            after_line0,
        });
    }
    if let Some(path) = pick_lang_file(discovered_loc_files, lang, edit_roots) {
        return Some(LocInsertTarget::ExistingFileAppend {
            uri: Url::from_file_path(&path).ok()?,
        });
    }
    // The new file doesn't exist yet, so nothing else will ever check it: this
    // is the only gate between a symlinked `localisation/` and a file created
    // outside the workspace.
    let new_path = generated_loc_file_path(workspace_root, lang);
    crate::access::editable_target(&new_path, edit_roots).ok()?;
    Some(LocInsertTarget::NewFile {
        uri: Url::from_file_path(new_path).ok()?,
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

/// One create-loc-key candidate with every language's insert site already
/// resolved — everything `create_loc_key_actions` derives before batching.
struct LocKeyCandidate<'d> {
    diag: &'d Diagnostic,
    title: String,
    key: String,
    targets: Vec<(Lang, LocInsertTarget)>,
}

/// The first candidate (batch order) needing a not-yet-existing loc file, per
/// language — the one whose action gets the `CreateFile` + header. A later
/// candidate needing the same language's new file is folded into it instead
/// of independently re-creating the file (#142): two `NewFile` targets built
/// from the same file-discovery snapshot both assume an empty file, and
/// applied together would duplicate the header.
fn new_file_owners<'a>(
    all_targets: impl IntoIterator<Item = &'a [(Lang, LocInsertTarget)]>,
) -> HashMap<Lang, usize> {
    let mut owners = HashMap::new();
    for (idx, targets) in all_targets.into_iter().enumerate() {
        for (lang, target) in targets {
            if matches!(target, LocInsertTarget::NewFile { .. }) {
                owners.entry(*lang).or_insert(idx);
            }
        }
    }
    owners
}

/// One candidate's own `DocumentChangeOperation`s: a `Create` + BOM/header
/// insert for each language it owns (per `owners`), a stub-only insert for
/// each `NewFile` language it doesn't (folded into whoever does, by the
/// caller), and `existing_file_ops`'s result for the two existing-file
/// variants. `None` if any language fails to build — a partial fix across a
/// candidate's own languages is a worse outcome than none, the same
/// principle the single-language case always followed.
fn candidate_operations(
    cand: &LocKeyCandidate<'_>,
    owners: &HashMap<Lang, usize>,
    self_idx: usize,
    existing_file_ops: &mut impl FnMut(&LocInsertTarget, &str) -> Option<Vec<DocumentChangeOperation>>,
) -> Option<Vec<DocumentChangeOperation>> {
    let stub = stub_line(&cand.key);
    let mut operations = Vec::new();
    for (lang, target) in &cand.targets {
        let ops = match target {
            LocInsertTarget::NewFile { uri } if owners.get(lang) == Some(&self_idx) => Some(vec![
                DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                    uri: uri.clone(),
                    options: Some(CreateFileOptions {
                        overwrite: Some(false),
                        ignore_if_exists: Some(true),
                    }),
                    annotation_id: None,
                })),
                insert_edit_op(
                    uri.clone(),
                    Position::new(0, 0),
                    format!("\u{FEFF}l_{lang}:\n{stub}\n"),
                ),
            ]),
            // Owned by another candidate in this batch: contribute only the
            // stub line, at the same (0, 0) start the owner's header uses —
            // LSP defines array order as the output order for edits sharing
            // a start position, so `merge_operations` appending this after
            // the owner's own edit is enough; no position bookkeeping needed.
            LocInsertTarget::NewFile { uri } => Some(vec![insert_edit_op(
                uri.clone(),
                Position::new(0, 0),
                format!("{stub}\n"),
            )]),
            other => existing_file_ops(other, &stub),
        };
        operations.extend(ops?);
    }
    Some(operations)
}

/// Fold `new_ops` into `into`. A new `Edit` for a document `into` already has
/// an `Edit` for joins that document's existing `TextDocumentEdit.edits`
/// array instead of adding a second operation for the same document — every
/// edit in a `TextDocumentEdit` applies against the document's original
/// state, and array order is the spec-defined tiebreak for edits sharing a
/// start position, so appending here reproduces exactly "the owner's edit,
/// then the folded one" with no assumption about the order a client applies
/// separate operations in. Anything else (a document `into` has nothing for
/// yet, or a `Create`, which a folded candidate never contributes) is
/// appended as its own operation.
fn merge_operations(
    into: &mut Vec<DocumentChangeOperation>,
    new_ops: Vec<DocumentChangeOperation>,
) {
    for new_op in new_ops {
        let DocumentChangeOperation::Edit(new_edit) = new_op else {
            into.push(new_op);
            continue;
        };
        let existing = into.iter_mut().find_map(|op| match op {
            DocumentChangeOperation::Edit(e)
                if e.text_document.uri == new_edit.text_document.uri =>
            {
                Some(e)
            }
            _ => None,
        });
        match existing {
            Some(e) => e.edits.extend(new_edit.edits),
            None => into.push(DocumentChangeOperation::Edit(new_edit)),
        }
    }
}

/// Build one `CodeAction` per `candidates` entry that isn't folded into an
/// earlier one, deduping `NewFile` targets via [`new_file_owners`] so a batch
/// never offers two independent actions that would both create the same file
/// (#142). `existing_file_ops` builds the two existing-file `LocInsertTarget`
/// variants (needs `Backend::file_text_for` to read the current text, so
/// it's injected — keeps this function testable without a `Backend`).
///
/// Folding is whole-candidate, not per-language: if any of a candidate's
/// languages is a `NewFile` owned by an earlier one, ALL of its operations —
/// including its other languages' existing-file edits — join that owner's
/// action, and its diagnostic is added to the owner's `diagnostics` list.
/// Splitting a candidate's own fix across two actions would mean applying
/// either alone leaves it still firing, the same "worse than no fix"
/// principle [`candidate_operations`] applies within one candidate. This
/// relies on `NewFile`-ness being the same answer for every candidate per
/// language — tier 3 only fires when the language has no loc file anywhere,
/// which every candidate resolves identically — so a candidate never needs
/// two different owners for two different languages, and the fold target is
/// unique. If the owner's own action fails to build, every candidate folded
/// into it is dropped too rather than promoting a new owner.
fn build_create_loc_key_batch<'d>(
    candidates: &[LocKeyCandidate<'d>],
    mut existing_file_ops: impl FnMut(&LocInsertTarget, &str) -> Option<Vec<DocumentChangeOperation>>,
) -> Vec<CodeAction> {
    let owners = new_file_owners(candidates.iter().map(|c| c.targets.as_slice()));
    let fold_into: Vec<Option<usize>> = candidates
        .iter()
        .enumerate()
        .map(|(idx, cand)| {
            cand.targets.iter().find_map(|(lang, target)| {
                if !matches!(target, LocInsertTarget::NewFile { .. }) {
                    return None;
                }
                let &owner_idx = owners.get(lang)?;
                (owner_idx != idx).then_some(owner_idx)
            })
        })
        .collect();

    let mut actions: Vec<CodeAction> = Vec::new();
    let mut built_index: Vec<Option<usize>> = vec![None; candidates.len()];

    for (idx, cand) in candidates.iter().enumerate() {
        if fold_into[idx].is_some() {
            continue;
        }
        let Some(operations) = candidate_operations(cand, &owners, idx, &mut existing_file_ops)
        else {
            continue;
        };
        built_index[idx] = Some(actions.len());
        actions.push(CodeAction {
            title: cand.title.clone(),
            kind: Some(CodeActionKind::QUICKFIX),
            diagnostics: Some(vec![cand.diag.clone()]),
            edit: Some(WorkspaceEdit {
                changes: None,
                document_changes: Some(DocumentChanges::Operations(operations)),
                change_annotations: None,
            }),
            ..Default::default()
        });
    }

    for (idx, cand) in candidates.iter().enumerate() {
        let Some(owner_idx) = fold_into[idx] else {
            continue;
        };
        let Some(owner_action) = built_index[owner_idx].and_then(|pos| actions.get_mut(pos)) else {
            continue; // the owner's own action failed to build; drop this too
        };
        let Some(folded_ops) = candidate_operations(cand, &owners, idx, &mut existing_file_ops)
        else {
            continue; // this candidate's own languages didn't all build
        };
        if let Some(WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(ops)),
            ..
        }) = owner_action.edit.as_mut()
        {
            merge_operations(ops, folded_ops);
        }
        match owner_action.diagnostics.as_mut() {
            Some(diags) => diags.push(cand.diag.clone()),
            None => owner_action.diagnostics = Some(vec![cand.diag.clone()]),
        }
    }

    actions
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
        let Some(text) = self.file_text_for(uri.as_str()).await else {
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
            actions.extend(
                self.create_loc_key_actions(&uri, &params.context.diagnostics, &encoding)
                    .await,
            );
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
    /// every candidate's per-language site with the pure functions above, then
    /// hands the whole batch to [`build_create_loc_key_batch`] so two keys
    /// that both land in the same not-yet-existing file share one header
    /// instead of each inserting their own (#142). A candidate is silently
    /// dropped when there's no workspace to anchor a path in, or any of its
    /// languages fails to resolve.
    async fn create_loc_key_actions(
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
        // The roots are read here, per request, so a folder added or removed
        // since the last one is already reflected.
        let (workspace_root, edit_roots) = {
            let cfg = self.state.config.read();
            let Some(ws_uri) = cfg.workspace_uri.clone() else {
                return Vec::new();
            };
            // The strict conversion, not `uri_to_path_str`: this path is a root
            // for everything below, and the lax converter's fallback would turn
            // a malformed workspace URI into a path relative to the CWD.
            let Some(root) = crate::access::file_uri_to_path(&ws_uri) else {
                return Vec::new();
            };
            (root, cfg.editable_roots.clone())
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
        // repeated per candidate. The walk is memoized per workspace root
        // (#134): a code-action request fires on cursor movement, so without a
        // cache every request re-walked the whole tree. The cache is valid only
        // while the scan's `last_loc_signature` still matches the value stored
        // at population time (a cheap read, no walk) — that catches `.yaml`/
        // `.csv` loc changes the client watcher misses (it only watches
        // `*.yml`) and clients that send no watched events — and watched
        // create/delete events invalidate it immediately. `sig` stores the
        // scan's value, not a freshly-computed signature, so a watched-event
        // re-walk doesn't leave the cache permanently mismatched against the
        // scan's stale value.
        let discovered = {
            let cur_sig = *self.state.last_loc_signature.lock();
            // Clone into owned data so no lock guard crosses the await below.
            let cached = self.state.loc_discovery_cache.lock().clone();
            match cached {
                Some((root, files, sig)) if root == workspace_root && sig == cur_sig => files,
                _ => {
                    let discovered_root = workspace_root.clone();
                    let files = tokio::task::spawn_blocking(move || {
                        let roots = [discovered_root.as_path()];
                        LocService::discover_files(
                            &roots,
                            cwtools_file_manager::file_manager::ScanBudget::default(),
                        )
                    })
                    .await
                    .unwrap_or_default();
                    *self.state.loc_discovery_cache.lock() =
                        Some((workspace_root.clone(), files.clone(), cur_sig));
                    files
                }
            }
        };

        // Resolve every candidate's per-language target up front so the whole
        // batch is visible to `build_create_loc_key_batch` before any
        // operation is built.
        let resolved: Vec<LocKeyCandidate<'_>> = {
            let loc_locations = self.state.loc_locations.read();
            candidates
                .into_iter()
                .filter_map(|(diag, title, key)| {
                    let siblings = self.sibling_loc_keys_for_diagnostic(uri, diag, &key);
                    let targets = langs
                        .iter()
                        .map(|&lang| {
                            resolve_loc_insert_target(
                                lang,
                                &siblings,
                                &loc_locations,
                                &edit_roots,
                                &discovered,
                                &workspace_root,
                            )
                            .map(|target| (lang, target))
                        })
                        .collect::<Option<Vec<_>>>()?;
                    Some(LocKeyCandidate {
                        diag,
                        title,
                        key,
                        targets,
                    })
                })
                .collect()
        };

        let target_uris: Vec<String> = resolved
            .iter()
            .flat_map(|candidate| candidate.targets.iter())
            .map(|(_, target)| match target {
                LocInsertTarget::ExistingFileAfterLine { uri, .. }
                | LocInsertTarget::ExistingFileAppend { uri }
                | LocInsertTarget::NewFile { uri } => uri.to_string(),
            })
            .collect();
        let texts = self.file_text_snapshots_for(&target_uris).await;

        build_create_loc_key_batch(&resolved, |target, stub| {
            self.loc_insert_operations(target, stub, encoding, &texts)
        })
        .into_iter()
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
                    loc.is_required_name_derived() && loc.derived_key(&inst.name) == create_loc_key
                });
                owns_key.then(|| sibling_loc_keys(&td.localisation, &inst.name, create_loc_key))
            })
            .unwrap_or_default()
    }

    /// The `Edit` operations for the two existing-file `LocInsertTarget`
    /// variants. `NewFile` is `None` here — `build_create_loc_key_batch` builds
    /// and dedupes it directly instead, since it needs no file read (#142).
    fn loc_insert_operations(
        &self,
        target: &LocInsertTarget,
        stub: &str,
        encoding: &PositionEncodingKind,
        texts: &HashMap<String, crate::FileTextSnapshot>,
    ) -> Option<Vec<DocumentChangeOperation>> {
        match target {
            LocInsertTarget::ExistingFileAfterLine { uri, after_line0 } => {
                let text = texts.get(uri.as_str())?.text.as_str();
                let (pos, insert_text) = insert_stub_after_line(text, *after_line0, stub, encoding);
                Some(vec![insert_edit_op(uri.clone(), pos, insert_text)])
            }
            LocInsertTarget::ExistingFileAppend { uri } => {
                let text = texts.get(uri.as_str())?.text.as_str();
                let last_line0 = text.lines().count().saturating_sub(1) as u32;
                let (pos, insert_text) = insert_stub_after_line(text, last_line0, stub, encoding);
                Some(vec![insert_edit_op(uri.clone(), pos, insert_text)])
            }
            LocInsertTarget::NewFile { .. } => None,
        }
    }
}

// ── fixAllWorkspace ────────────────────────────────────────────────────────
//
// The `fixAllWorkspace` execute-command applies every currently-fixable
// diagnostic across the whole workspace in one `workspace/applyEdit`,
// mirroring `cwtools fix --apply`. It snapshots `DocumentState::fixable_edits`
// (kept current by every `validate::publish_filtered` call) instead of
// re-running validation, so it fixes exactly what the Problems panel shows,
// minus any file the edit boundary refuses to write to.
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

/// Build the negotiated workspace edit from the exact snapshots used to
/// calculate its ranges. Versioned document changes prevent a supported client
/// from applying edits to a newer open-document version.
fn workspace_edit_for_snapshots(
    changes: HashMap<Url, Vec<TextEdit>>,
    snapshots: &HashMap<String, crate::FileTextSnapshot>,
    document_changes: bool,
) -> WorkspaceEdit {
    if document_changes {
        let edits = changes
            .into_iter()
            .map(|(uri, edits)| TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    version: snapshots.get(uri.as_str()).and_then(|s| s.version),
                    uri,
                },
                edits: edits.into_iter().map(OneOf::Left).collect(),
            })
            .collect();
        WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Edits(edits)),
            change_annotations: None,
        }
    } else {
        WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }
    }
}

/// The command's result message when every fixable file was refused by the
/// edit boundary. Distinct from the empty-store message: the problems are real
/// and the user can see them in the panel, they just aren't in a file cwtools
/// will write to.
fn outside_workspace_summary(refused: usize) -> String {
    format!("Skipped {refused} file(s) with auto-fixable problems: they are outside the workspace.")
}

fn fixable_edits_match(
    expected: &crate::FixableEdits,
    current: Option<&crate::FileTextSnapshot>,
) -> bool {
    let Some(current) = current else {
        return false;
    };
    expected.version.map_or_else(
        || {
            expected
                .content_hash
                .is_some_and(|hash| hash == current.content_hash)
        },
        |version| current.version == Some(version),
    )
}

/// The command's result message on success. Stale and overlapping edits are
/// reported separately from files refused by the workspace edit boundary.
fn fix_all_workspace_summary(
    edits_applied: usize,
    files_changed: usize,
    skipped_stale: usize,
    skipped_overlapping: usize,
    skipped_outside: usize,
) -> String {
    let mut msg = format!("Applied {edits_applied} fix(es) across {files_changed} file(s)");
    let skipped = skipped_stale + skipped_overlapping;
    if skipped > 0 {
        let reason = match (skipped_stale > 0, skipped_overlapping > 0) {
            (true, true) => format!("{skipped_stale} stale, {skipped_overlapping} overlapping"),
            (true, false) => "stale".to_string(),
            (false, true) => "overlapping".to_string(),
            (false, false) => unreachable!(),
        };
        msg.push_str(&format!("; {skipped} skipped ({reason})"));
    }
    if skipped_outside > 0 {
        msg.push_str(&format!(
            "; {skipped_outside} file(s) skipped (outside workspace)"
        ));
    }
    msg
}

impl Backend {
    /// `fixAllWorkspace` execute-command handler. Returns the message shown to
    /// the user (no result payload otherwise): a "nothing to do" message when
    /// the store is empty or every entry resolved to zero edits, a "skipped"
    /// message when the only fixable files were outside the workspace, stale
    /// edits are dropped and counted, an error message when the client rejects
    /// the `workspace/applyEdit`, else the summary from
    /// [`fix_all_workspace_summary`].
    pub(crate) async fn fix_all_workspace_impl(&self) -> String {
        let mut snapshot = self.state.fixable_edits.lock().clone();
        if snapshot.is_empty() {
            return "No auto-fixable problems in the workspace.".to_string();
        }

        // The edit boundary is workspace-only and performs synchronous
        // canonicalization, so keep it off the request worker too (#160).
        let edit_roots = self.state.config.read().editable_roots.clone();
        let candidate_uris: Vec<String> = snapshot.keys().cloned().collect();
        let editable = tokio::task::spawn_blocking(move || {
            candidate_uris
                .into_iter()
                .filter(|uri| crate::access::editable_path(uri, &edit_roots).is_ok())
                .collect::<std::collections::HashSet<_>>()
        })
        .await
        .unwrap_or_default();
        let fixable = snapshot.len();
        snapshot.retain(|uri, _| editable.contains(uri));
        let refused = fixable - snapshot.len();
        if snapshot.is_empty() {
            return outside_workspace_summary(refused);
        }

        let uris: Vec<String> = snapshot.keys().cloned().collect();
        let current = self.file_text_snapshots_for(&uris).await;
        let mut stale_entries = Vec::new();
        snapshot.retain(|uri, expected| {
            let matches = fixable_edits_match(expected, current.get(uri));
            if !matches {
                stale_entries.push((uri.clone(), expected.clone()));
            }
            matches
        });
        let skipped_stale: usize = stale_entries
            .iter()
            .map(|(_, entry)| entry.entries.len())
            .sum();
        if !stale_entries.is_empty() {
            let mut store = self.state.fixable_edits.lock();
            for (uri, expected) in stale_entries {
                if store.get(&uri) == Some(&expected) {
                    store.remove(&uri);
                }
            }
        }
        if snapshot.is_empty() {
            return fix_all_workspace_summary(0, 0, skipped_stale, 0, refused);
        }

        let edits_snapshot: HashMap<String, Vec<(String, SpanEdit)>> = snapshot
            .into_iter()
            .map(|(uri, entry)| (uri, entry.entries))
            .collect();
        let texts: HashMap<String, String> = current
            .iter()
            .filter(|(uri, _)| edits_snapshot.contains_key(*uri))
            .map(|(uri, snapshot)| (uri.clone(), snapshot.text.clone()))
            .collect();
        let planned = plan_workspace_fixes(&edits_snapshot, &texts);
        let encoding = self.state.config.read().position_encoding.clone();
        let skipped_overlapping: usize = planned.iter().map(|pf| pf.skipped).sum();
        let changes = workspace_edit_changes(&planned, &texts, &encoding);
        if changes.is_empty() {
            if skipped_stale > 0 || skipped_overlapping > 0 {
                return fix_all_workspace_summary(
                    0,
                    0,
                    skipped_stale,
                    skipped_overlapping,
                    refused,
                );
            }
            return "No auto-fixable problems in the workspace.".to_string();
        }
        let edits_applied: usize = changes.values().map(Vec::len).sum();
        let files_changed = changes.len();
        let document_changes = self
            .state
            .workspace_edit_document_changes
            .load(std::sync::atomic::Ordering::Relaxed);
        let edit = workspace_edit_for_snapshots(changes, &current, document_changes);
        match self.client.apply_edit(edit).await {
            Ok(resp) if resp.applied => fix_all_workspace_summary(
                edits_applied,
                files_changed,
                skipped_stale,
                skipped_overlapping,
                refused,
            ),
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

    /// The edit boundary compares canonical paths, and a tempdir path isn't one
    /// (macOS hands out `/var/folders/…` for `/private/var/folders/…`), so the
    /// roots these fixtures pass in go through `canonicalize` exactly as
    /// `Config::refresh_roots` does for the real ones.
    fn edit_roots(dirs: [&Path; 1]) -> Vec<PathBuf> {
        dirs.iter()
            .map(|d| std::fs::canonicalize(d).expect("canonical root"))
            .collect()
    }

    /// A real loc file at `rel` under `root`, returned as the `file://` URI the
    /// loc scan would have recorded for it.
    fn write_loc_file(root: &Path, rel: &str) -> (PathBuf, String) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("a parent")).unwrap();
        std::fs::write(&path, "l_english:\n my_thing:0 \"Thing\"\n").unwrap();
        let uri = Url::from_file_path(&path)
            .expect("absolute path")
            .to_string();
        (path, uri)
    }

    #[test]
    fn resolve_sibling_site_picks_the_matching_workspace_language_file() {
        let ws = tempfile::tempdir().unwrap();
        let (_, uri) = write_loc_file(ws.path(), "localisation/things_l_english.yml");
        let locs = loc_locations_with(&[("my_thing", &uri, 4)]);
        let siblings = vec!["my_thing".to_string()];
        let hit = resolve_sibling_site(&siblings, &locs, &edit_roots([ws.path()]), Lang::English)
            .expect("a hit");
        assert_eq!(hit.0.as_ref(), uri);
        assert_eq!(hit.1, 4);
    }

    #[test]
    fn resolve_sibling_site_rejects_a_vanilla_definition() {
        let ws = tempfile::tempdir().unwrap();
        let vanilla = tempfile::tempdir().unwrap();
        let (_, uri) = write_loc_file(vanilla.path(), "localisation/things_l_english.yml");
        let locs = loc_locations_with(&[("my_thing", &uri, 4)]);
        let siblings = vec!["my_thing".to_string()];
        assert!(
            resolve_sibling_site(&siblings, &locs, &edit_roots([ws.path()]), Lang::English)
                .is_none()
        );
    }

    /// The reported bug (#160): workspace `…/mod` and base game `…/mod-vanilla`.
    /// A string-prefix containment test calls the vanilla file a workspace file
    /// and the action offers to edit the game install.
    #[test]
    fn resolve_sibling_site_rejects_a_sibling_directory_sharing_a_name_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("mod");
        std::fs::create_dir(&ws).unwrap();
        let (_, uri) = write_loc_file(
            &tmp.path().join("mod-vanilla"),
            "localisation/things_l_english.yml",
        );
        let locs = loc_locations_with(&[("my_thing", &uri, 4)]);
        let siblings = vec!["my_thing".to_string()];
        assert!(
            resolve_sibling_site(&siblings, &locs, &edit_roots([&ws]), Lang::English).is_none(),
            "`mod-vanilla` is not inside `mod`"
        );
    }

    /// A hostile workspace can make its loc file a symlink to a file outside.
    /// The site is skipped, and resolution falls through to the next sibling —
    /// the gate costs the user nothing when a real site exists.
    #[cfg(unix)]
    #[test]
    fn resolve_sibling_site_skips_a_symlinked_site_and_falls_through() {
        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let (target, _) = write_loc_file(outside.path(), "secrets_l_english.yml");
        let link = ws.path().join("localisation/linked_l_english.yml");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let link_uri = Url::from_file_path(&link).unwrap().to_string();
        let (_, real_uri) = write_loc_file(ws.path(), "localisation/real_l_english.yml");

        let locs =
            loc_locations_with(&[("my_thing", &link_uri, 1), ("my_thing_title", &real_uri, 9)]);
        let siblings = vec!["my_thing".to_string(), "my_thing_title".to_string()];
        let hit = resolve_sibling_site(&siblings, &locs, &edit_roots([ws.path()]), Lang::English)
            .expect("falls through to the real site");
        assert_eq!(hit.0.as_ref(), real_uri);
        assert_eq!(hit.1, 9);
    }

    #[test]
    fn resolve_sibling_site_rejects_the_wrong_language_file() {
        let ws = tempfile::tempdir().unwrap();
        let (_, uri) = write_loc_file(ws.path(), "localisation/things_l_french.yml");
        let locs = loc_locations_with(&[("my_thing", &uri, 4)]);
        let siblings = vec!["my_thing".to_string()];
        assert!(
            resolve_sibling_site(&siblings, &locs, &edit_roots([ws.path()]), Lang::English)
                .is_none()
        );
    }

    #[test]
    fn resolve_sibling_site_tries_siblings_in_order() {
        // The first sibling has no known site; the second does.
        let ws = tempfile::tempdir().unwrap();
        let (_, uri) = write_loc_file(ws.path(), "localisation/things_l_english.yml");
        let locs = loc_locations_with(&[("my_thing_title", &uri, 9)]);
        let siblings = vec!["my_thing".to_string(), "my_thing_title".to_string()];
        let hit = resolve_sibling_site(&siblings, &locs, &edit_roots([ws.path()]), Lang::English)
            .expect("a hit");
        assert_eq!(hit.1, 9);
    }

    #[test]
    fn pick_lang_file_matches_by_name_and_sorts_for_determinism() {
        let ws = tempfile::tempdir().unwrap();
        let (z, _) = write_loc_file(ws.path(), "localisation/z_l_english.yml");
        let (a, _) = write_loc_file(ws.path(), "localisation/a_l_english.yml");
        let (fr, _) = write_loc_file(ws.path(), "localisation/other_l_french.yml");
        let picked = pick_lang_file(&[z, a.clone(), fr], Lang::English, &edit_roots([ws.path()]))
            .expect("a match");
        assert_eq!(picked, a);
    }

    #[test]
    fn pick_lang_file_none_when_no_file_matches() {
        let ws = tempfile::tempdir().unwrap();
        let (fr, _) = write_loc_file(ws.path(), "localisation/other_l_french.yml");
        assert!(pick_lang_file(&[fr], Lang::English, &edit_roots([ws.path()])).is_none());
    }

    /// The loc walk follows symlinks (`walk_folder_inner` stats through them),
    /// so a discovered path can point outside the workspace. It is skipped even
    /// though it sorts first.
    #[cfg(unix)]
    #[test]
    fn pick_lang_file_skips_a_symlinked_candidate() {
        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let (target, _) = write_loc_file(outside.path(), "secrets_l_english.yml");
        let link = ws.path().join("localisation/a_l_english.yml");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let (real, _) = write_loc_file(ws.path(), "localisation/b_l_english.yml");

        let picked = pick_lang_file(
            &[link, real.clone()],
            Lang::English,
            &edit_roots([ws.path()]),
        )
        .expect("the real file");
        assert_eq!(picked, real);
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
        // `create_loc_key_actions` resolves one target per configured
        // language and requires every one of them to succeed (`collect::<Option<_>>`
        // over its per-language map) before a candidate joins the batch. This
        // pins the pure per-language piece that resolution depends on:
        // English has a sibling site, French has none of the first two tiers
        // and falls through to NewFile. The Backend-level all-or-nothing
        // short-circuit itself needs a running Backend (`file_text_for`,
        // config locks) to exercise — not covered here; the CreateFile-tier
        // integration test covers the NewFile branch end to end for a single
        // language instead.
        let ws = tempfile::tempdir().unwrap();
        let roots = edit_roots([ws.path()]);
        let (_, uri) = write_loc_file(ws.path(), "localisation/things_l_english.yml");
        let locs = loc_locations_with(&[("my_thing", &uri, 4)]);
        let siblings = vec!["my_thing".to_string()];

        let en = resolve_loc_insert_target(Lang::English, &siblings, &locs, &roots, &[], ws.path())
            .unwrap();
        assert_eq!(
            en,
            LocInsertTarget::ExistingFileAfterLine {
                uri: uri.parse().unwrap(),
                after_line0: 4,
            }
        );

        let fr = resolve_loc_insert_target(Lang::French, &siblings, &locs, &roots, &[], ws.path())
            .unwrap();
        assert_eq!(
            fr,
            LocInsertTarget::NewFile {
                uri: Url::from_file_path(generated_loc_file_path(ws.path(), Lang::French)).unwrap(),
            }
        );
    }

    #[test]
    fn resolve_loc_insert_target_prefers_sibling_over_existing_file_over_new() {
        let ws = tempfile::tempdir().unwrap();
        let roots = edit_roots([ws.path()]);
        let (path, uri) = write_loc_file(ws.path(), "localisation/things_l_english.yml");
        let siblings = vec!["my_thing".to_string()];

        // Case 1: a sibling site exists -> ExistingFileAfterLine.
        let locs = loc_locations_with(&[("my_thing", &uri, 4)]);
        let target =
            resolve_loc_insert_target(Lang::English, &siblings, &locs, &roots, &[], ws.path())
                .unwrap();
        assert_eq!(
            target,
            LocInsertTarget::ExistingFileAfterLine {
                uri: uri.parse().unwrap(),
                after_line0: 4,
            }
        );

        // Case 2: no sibling site, but a discovered loc file for the language.
        let empty_locs = crate::LocLocationMap::default();
        let target = resolve_loc_insert_target(
            Lang::English,
            &siblings,
            &empty_locs,
            &roots,
            std::slice::from_ref(&path),
            ws.path(),
        )
        .unwrap();
        assert_eq!(
            target,
            LocInsertTarget::ExistingFileAppend {
                uri: Url::from_file_path(&path).unwrap(),
            }
        );

        // Case 3: nothing at all -> a brand new generated file.
        let target = resolve_loc_insert_target(
            Lang::English,
            &siblings,
            &empty_locs,
            &roots,
            &[],
            ws.path(),
        )
        .unwrap();
        assert_eq!(
            target,
            LocInsertTarget::NewFile {
                uri: Url::from_file_path(generated_loc_file_path(ws.path(), Lang::English))
                    .unwrap(),
            }
        );
    }

    /// The `NewFile` tier writes a file that doesn't exist yet, so no read ever
    /// checks it. If `<workspace>/localisation` is a symlink out of the
    /// workspace, the created file lands outside — the action must not be
    /// offered at all.
    #[cfg(unix)]
    #[test]
    fn resolve_loc_insert_target_refuses_a_new_file_under_a_symlinked_loc_dir() {
        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.path().join("localisation")).unwrap();
        assert_eq!(
            resolve_loc_insert_target(
                Lang::English,
                &[],
                &crate::LocLocationMap::default(),
                &edit_roots([ws.path()]),
                &[],
                ws.path(),
            ),
            None
        );
    }

    // ── Batch dedup (#142): pure-function coverage ─────────────────────────────

    #[test]
    fn new_file_owners_assigns_the_first_candidate_per_language() {
        let en_uri: Url = "file:///ws/localisation/cwtools_generated_l_english.yml"
            .parse()
            .unwrap();
        let fr_uri: Url = "file:///ws/localisation/cwtools_generated_l_french.yml"
            .parse()
            .unwrap();
        let targets = [
            vec![(Lang::English, LocInsertTarget::NewFile { uri: en_uri })],
            vec![
                (
                    Lang::English,
                    LocInsertTarget::NewFile {
                        uri: "file:///ws/localisation/cwtools_generated_l_english.yml"
                            .parse()
                            .unwrap(),
                    },
                ),
                (Lang::French, LocInsertTarget::NewFile { uri: fr_uri }),
            ],
        ];
        let owners = new_file_owners(targets.iter().map(Vec::as_slice));
        assert_eq!(
            owners.get(&Lang::English),
            Some(&0),
            "the first candidate claims english"
        );
        assert_eq!(
            owners.get(&Lang::French),
            Some(&1),
            "only the second candidate needs french at all"
        );
    }

    /// The `newText` of every `TextEdit` a `DocumentChangeOperation::Edit`
    /// targeting `uri` carries, in array order — for edits that share a start
    /// position (as every insert this module builds does), LSP defines array
    /// order as the order they appear in the resulting text, so concatenating
    /// them reproduces exactly what a client applying the edit would see.
    fn edit_texts<'a>(ops: &'a [DocumentChangeOperation], uri: &Url) -> Vec<&'a str> {
        ops.iter()
            .filter_map(|op| match op {
                DocumentChangeOperation::Edit(e) if &e.text_document.uri == uri => Some(e),
                _ => None,
            })
            .flat_map(|e| &e.edits)
            .filter_map(|c| match c {
                OneOf::Left(t) => Some(t.new_text.as_str()),
                OneOf::Right(_) => None,
            })
            .collect()
    }

    #[test]
    fn build_create_loc_key_batch_dedupes_a_shared_new_file() {
        // #142: two candidates resolving to the identical not-yet-existing
        // `NewFile` target in one batch must not each carry their own
        // `CreateFile` + header — the merged action must open the file once,
        // with both stubs.
        let uri: Url = "file:///ws/localisation/cwtools_generated_l_english.yml"
            .parse()
            .unwrap();
        let diag_a = create_loc_key_diag();
        let diag_b = create_loc_key_diag();
        let candidates = vec![
            LocKeyCandidate {
                diag: &diag_a,
                title: "Create localisation key my_thing_desc".to_string(),
                key: "my_thing_desc".to_string(),
                targets: vec![(Lang::English, LocInsertTarget::NewFile { uri: uri.clone() })],
            },
            LocKeyCandidate {
                diag: &diag_b,
                title: "Create localisation key my_thing_title".to_string(),
                key: "my_thing_title".to_string(),
                targets: vec![(Lang::English, LocInsertTarget::NewFile { uri: uri.clone() })],
            },
        ];

        let actions = build_create_loc_key_batch(&candidates, |_, _| {
            panic!("no existing-file target in this batch")
        });

        assert_eq!(
            actions.len(),
            1,
            "the second candidate must join the first's action, not get its own"
        );
        assert_eq!(
            actions[0].diagnostics.as_ref().map(Vec::len),
            Some(2),
            "both diagnostics must be attributed to the merged action"
        );

        let Some(WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(ops)),
            ..
        }) = &actions[0].edit
        else {
            panic!("expected document_changes operations");
        };
        assert_eq!(
            ops.len(),
            2,
            "one Create plus a SINGLE TextDocumentEdit for the file, not one per key: {ops:?}"
        );
        assert!(matches!(
            ops[0],
            DocumentChangeOperation::Op(ResourceOp::Create(_))
        ));

        let texts = edit_texts(ops, &uri);
        assert_eq!(
            texts.len(),
            2,
            "the header edit and the folded stub, as two edits in one TextDocumentEdit: {texts:?}"
        );
        let combined = texts.concat();
        assert_eq!(
            combined.matches("l_english:").count(),
            1,
            "the header must appear exactly once: {combined:?}"
        );
        assert!(combined.contains(" my_thing_desc:0 "), "got: {combined:?}");
        assert!(combined.contains(" my_thing_title:0 "), "got: {combined:?}");
    }

    #[test]
    fn build_create_loc_key_batch_folds_a_whole_candidate_across_mixed_tiers() {
        // #142 follow-up: English already has a loc file but French does
        // not, so every candidate resolves English to an existing-file
        // insert and French to the identical not-yet-existing file. A
        // candidate whose French half folds into another's action must not
        // keep an independent action for its English half — that would
        // split its own fix across two actions, and applying only one would
        // leave its diagnostic still firing.
        let en_uri: Url = "file:///ws/localisation/things_l_english.yml"
            .parse()
            .unwrap();
        let fr_uri: Url = "file:///ws/localisation/cwtools_generated_l_french.yml"
            .parse()
            .unwrap();
        let diag_a = create_loc_key_diag();
        let diag_b = create_loc_key_diag();
        let candidates = vec![
            LocKeyCandidate {
                diag: &diag_a,
                title: "Create localisation key my_thing_desc".to_string(),
                key: "my_thing_desc".to_string(),
                targets: vec![
                    (
                        Lang::English,
                        LocInsertTarget::ExistingFileAppend {
                            uri: en_uri.clone(),
                        },
                    ),
                    (
                        Lang::French,
                        LocInsertTarget::NewFile {
                            uri: fr_uri.clone(),
                        },
                    ),
                ],
            },
            LocKeyCandidate {
                diag: &diag_b,
                title: "Create localisation key my_thing_title".to_string(),
                key: "my_thing_title".to_string(),
                targets: vec![
                    (
                        Lang::English,
                        LocInsertTarget::ExistingFileAppend {
                            uri: en_uri.clone(),
                        },
                    ),
                    (
                        Lang::French,
                        LocInsertTarget::NewFile {
                            uri: fr_uri.clone(),
                        },
                    ),
                ],
            },
        ];

        let actions = build_create_loc_key_batch(&candidates, |target, stub| {
            let LocInsertTarget::ExistingFileAppend { uri } = target else {
                panic!("no other existing-file target in this batch: {target:?}");
            };
            Some(vec![insert_edit_op(
                uri.clone(),
                Position::new(3, 0),
                format!("{stub}\n"),
            )])
        });

        assert_eq!(
            actions.len(),
            1,
            "the second candidate's whole fix must fold into the first's action, \
             not split across two: got {} actions",
            actions.len()
        );
        assert_eq!(
            actions[0].diagnostics.as_ref().map(Vec::len),
            Some(2),
            "both diagnostics must be attributed to the merged action"
        );

        let Some(WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(ops)),
            ..
        }) = &actions[0].edit
        else {
            panic!("expected document_changes operations");
        };
        let creates = ops
            .iter()
            .filter(|op| matches!(op, DocumentChangeOperation::Op(ResourceOp::Create(_))))
            .count();
        assert_eq!(
            creates, 1,
            "exactly one Create for the shared file: {ops:?}"
        );

        let en_texts = edit_texts(ops, &en_uri);
        assert_eq!(
            en_texts.len(),
            2,
            "both candidates' English edits must land in the merged action: {ops:?}"
        );
        assert!(en_texts.iter().any(|t| t.contains(" my_thing_desc:0 ")));
        assert!(en_texts.iter().any(|t| t.contains(" my_thing_title:0 ")));

        let fr_texts = edit_texts(ops, &fr_uri);
        assert_eq!(
            fr_texts.len(),
            2,
            "the header edit and the folded stub, as two edits in one TextDocumentEdit: {fr_texts:?}"
        );
        let fr_combined = fr_texts.concat();
        assert_eq!(
            fr_combined.matches("l_french:").count(),
            1,
            "the French header must appear exactly once: {fr_combined:?}"
        );
        assert!(
            fr_combined.contains(" my_thing_desc:0 "),
            "got: {fr_combined:?}"
        );
        assert!(
            fr_combined.contains(" my_thing_title:0 "),
            "got: {fr_combined:?}"
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
    fn workspace_edit_for_snapshots_uses_captured_versions() {
        let uri: Url = "file:///a.txt".parse().unwrap();
        let mut changes = HashMap::new();
        changes.insert(
            uri.clone(),
            vec![TextEdit {
                range: Range::new(Position::new(0, 0), Position::new(0, 4)),
                new_text: "X".to_string(),
            }],
        );
        let mut snapshots = HashMap::new();
        snapshots.insert(
            uri.to_string(),
            crate::FileTextSnapshot {
                text: "aaaa\n".to_string(),
                version: Some(7),
                content_hash: 0,
            },
        );

        let edit = workspace_edit_for_snapshots(changes, &snapshots, true);
        let Some(DocumentChanges::Edits(edits)) = edit.document_changes else {
            panic!("expected versioned document changes");
        };
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].text_document.uri, uri);
        assert_eq!(edits[0].text_document.version, Some(7));
        assert!(edit.changes.is_none());
    }

    #[test]
    fn fix_all_workspace_summary_wording() {
        assert_eq!(
            fix_all_workspace_summary(3, 2, 0, 0, 0),
            "Applied 3 fix(es) across 2 file(s)"
        );
        assert_eq!(
            fix_all_workspace_summary(3, 2, 0, 1, 0),
            "Applied 3 fix(es) across 2 file(s); 1 skipped (overlapping)"
        );
        assert_eq!(
            fix_all_workspace_summary(3, 2, 0, 0, 1),
            "Applied 3 fix(es) across 2 file(s); 1 file(s) skipped (outside workspace)"
        );
    }

    #[test]
    fn fix_all_workspace_rejects_stale_versions_and_content() {
        let edit = SpanEdit {
            range: range(1, 0, 1, 4),
            replacement: "X".to_string(),
        };
        let versioned = crate::FixableEdits {
            entries: vec![("CW281".to_string(), edit.clone())],
            version: Some(1),
            content_hash: None,
        };
        let open_v2 = crate::FileTextSnapshot {
            text: "aaaa\n".to_string(),
            version: Some(2),
            content_hash: 1,
        };
        assert!(!fixable_edits_match(&versioned, Some(&open_v2)));

        let closed = crate::FixableEdits {
            entries: vec![("CW281".to_string(), edit)],
            version: None,
            content_hash: Some(7),
        };
        let changed = crate::FileTextSnapshot {
            text: "bbbb\n".to_string(),
            version: None,
            content_hash: 8,
        };
        assert!(!fixable_edits_match(&closed, Some(&changed)));
        let unchanged = crate::FileTextSnapshot {
            content_hash: 7,
            ..changed
        };
        assert!(fixable_edits_match(&closed, Some(&unchanged)));
    }

    /// "Nothing to fix" and "everything fixable is off-limits" are different
    /// answers: the second leaves problems the user can still see in the panel,
    /// so saying there are none would read as a bug in the command.
    #[test]
    fn outside_workspace_summary_does_not_claim_there_is_nothing_to_fix() {
        let msg = outside_workspace_summary(2);
        assert_eq!(
            msg,
            "Skipped 2 file(s) with auto-fixable problems: they are outside the workspace."
        );
        assert!(!msg.contains("No auto-fixable problems"));
    }
}
