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

use std::collections::HashMap;

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

use cwtools_parser::ast::{SourcePos, SourceRange};
use cwtools_parser::fix::SuggestedFix;

use crate::Backend;
use crate::paths::source_position_to_lsp;

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

/// A named set of edits resolving one diagnostic.
struct FixPayload {
    title: String,
    edits: Vec<FixEdit>,
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
    serde_json::json!({
        FIX_DATA_KEY: {
            "title": fix.title,
            "edits": edits,
        }
    })
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
    Some(FixPayload { title, edits })
}

/// Convert a parser [`SourceRange`] (1-based line, 0-based char col) into an LSP
/// `Range`, using `text` and the negotiated `encoding` — the same
/// `source_position_to_lsp` conversion hover/rename/navigation use, so the edit
/// lands on exactly the same columns the client sees.
fn source_range_to_lsp(range: SourceRange, text: &str, encoding: &PositionEncodingKind) -> Range {
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
        let actions =
            code_actions_from_diagnostics(&uri, &params.context.diagnostics, &text, &encoding);
        if actions.is_empty() {
            Ok(None)
        } else {
            Ok(Some(actions))
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
}
