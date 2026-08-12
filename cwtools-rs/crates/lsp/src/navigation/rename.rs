use std::collections::{HashMap, HashSet};

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

use crate::Backend;
use crate::paths::{logical_path_from_uri, lsp_pos_to_source_in_text};

use super::{
    at_var_at_cursor, code_token_cols_in_line, prepare_rename_range, rename_refused,
    source_range_in_text,
};

impl Backend {
    pub(crate) async fn prepare_rename_impl(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri = params.text_document.uri.to_string();
        let pos = params.position;
        let ws_prefix = self.state.config.read().workspace_prefix.clone();
        let logical_path = logical_path_from_uri(&uri, &ws_prefix);

        // `@` script constant first: the sigil marks it unambiguously, and the
        // rule walk can misclassify an `@` read as a type reference.
        let text = self.file_text_for(&uri).await;
        let position_encoding = self.state.config.read().position_encoding.clone();
        if let Some(text) = text.as_deref()
            && let Some((_, range)) = Self::at_var_rename_target(text, pos, &position_encoding)
        {
            return Ok(Some(PrepareRenameResponse::Range(range)));
        }

        let type_ref = self.type_ref_at_cursor(&uri, pos, &logical_path);

        if let Some((_, instance_name)) = type_ref {
            // Return a range covering the whole instance-name token. Anchor the
            // start at the token's beginning (so a mid-token cursor doesn't
            // rename a shifted span) and extend by the name's length.
            let range =
                prepare_rename_range(text.as_deref(), pos, &instance_name, &position_encoding);
            return Ok(Some(PrepareRenameResponse::Range(range)));
        }
        Ok(None)
    }

    /// File-local rename of an `@name` script constant: every comment-aware
    /// whole-token occurrence in this one document. `@` constants are
    /// per-file in the game's scripting, so no cross-file work is needed.
    fn rename_at_var(
        &self,
        uri: &str,
        text: &str,
        name: &str,
        new_name: &str,
    ) -> Result<Option<WorkspaceEdit>> {
        let encoding = self.state.config.read().position_encoding.clone();
        let edits: Vec<TextEdit> = text
            .lines()
            .enumerate()
            .flat_map(|(line0, line)| {
                let encoding = &encoding;
                code_token_cols_in_line(line, name)
                    .into_iter()
                    .map(move |col| TextEdit {
                        range: source_range_in_text(text, line0 as u32, col, name, encoding),
                        new_text: new_name.to_string(),
                    })
            })
            .collect();
        if edits.is_empty() {
            return Ok(None);
        }
        let by_uri = vec![(uri.to_string(), edits)];
        if let Some(refused) = self.first_refused_edit_target(&by_uri, uri) {
            return Err(refused);
        }
        Ok(Some(self.build_workspace_edit(by_uri)))
    }

    fn at_var_rename_target(
        text: &str,
        pos: Position,
        encoding: &PositionEncodingKind,
    ) -> Option<(String, Range)> {
        let (_, col) = lsp_pos_to_source_in_text(text, pos, encoding);
        let (name, start_col) = at_var_at_cursor(text, pos.line, col as u32)?;
        let range = source_range_in_text(text, pos.line, start_col, &name, encoding);
        Some((name, range))
    }

    /// The refusal for the first URI in `by_uri` a generated edit may not write
    /// to, if any. `own_uri` is the document the request named.
    ///
    /// Rename's edit sites come from the type index, which has the base game's
    /// instances merged into it, so a rename can reach a definition inside the
    /// install (#160). Dropping those quietly would apply a rename the user
    /// only saw half of and leave the other half dangling, so one refused
    /// target cancels the whole rename — the same stance `rename_impl` already
    /// takes for a reference it can't locate in text.
    ///
    /// An edit set that touches nothing but the request's own document is
    /// exempt, which is the rule the per-diagnostic quick fixes and
    /// `source.fixAll` already run on: the URI is the client's own, echoed
    /// back, so there is no server-derived path to contain. Without the
    /// exemption a file-local `@const` rename would break in exactly the
    /// sessions where the boundary has nothing to check against — an
    /// `untitled:` buffer that has never been on disk, or a window opened on a
    /// single file with no workspace folder at all.
    fn first_refused_edit_target(
        &self,
        by_uri: &[(String, Vec<TextEdit>)],
        own_uri: &str,
    ) -> Option<tower_lsp::jsonrpc::Error> {
        if let [(only, _)] = by_uri
            && only == own_uri
        {
            return None;
        }
        let edit_roots = self.state.config.read().editable_roots.clone();
        by_uri.iter().find_map(|(uri, _)| {
            let refusal = crate::access::editable_path(uri, &edit_roots).err()?;
            Some(rename_refused(uri, refusal))
        })
    }

    /// Assemble the `WorkspaceEdit` shape the client negotiated: versioned
    /// `documentChanges` when it advertised support (open docs carry their
    /// version so a stale buffer rejects the edit, closed files `None`), the
    /// legacy `changes` map otherwise.
    fn build_workspace_edit(&self, by_uri: Vec<(String, Vec<TextEdit>)>) -> WorkspaceEdit {
        if self
            .state
            .workspace_edit_document_changes
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            let docs = self.state.documents.lock();
            let edits = by_uri
                .into_iter()
                .filter_map(|(uri, edits)| {
                    let url = uri.parse::<Url>().ok()?;
                    Some(TextDocumentEdit {
                        text_document: OptionalVersionedTextDocumentIdentifier {
                            uri: url,
                            version: docs.get(&uri).map(|d| d.version),
                        },
                        edits: edits.into_iter().map(OneOf::Left).collect(),
                    })
                })
                .collect();
            WorkspaceEdit {
                changes: None,
                document_changes: Some(DocumentChanges::Edits(edits)),
                change_annotations: None,
            }
        } else {
            let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
            for (uri, edits) in by_uri {
                if let Ok(url) = uri.parse::<Url>() {
                    changes.entry(url).or_default().extend(edits);
                }
            }
            WorkspaceEdit {
                changes: Some(changes),
                document_changes: None,
                change_annotations: None,
            }
        }
    }

    pub(crate) async fn rename_impl(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = params.text_document_position.text_document.uri.to_string();
        let pos = params.text_document_position.position;
        let new_name = params.new_name.clone();
        let ws_prefix = self.state.config.read().workspace_prefix.clone();
        let logical_path = logical_path_from_uri(&uri, &ws_prefix);

        // `@` script constant first: the sigil marks it unambiguously, and the
        // rule walk can misclassify an `@` read as a type reference.
        let position_encoding = self.state.config.read().position_encoding.clone();
        let source_text = self.file_text_for(&uri).await;
        if let Some(text) = source_text.as_deref()
            && let Some((name, _)) = Self::at_var_rename_target(text, pos, &position_encoding)
        {
            return self.rename_at_var(&uri, text, &name, &new_name);
        }

        // Identify what's under the cursor
        let type_ref = self.type_ref_at_cursor(&uri, pos, &logical_path);

        let (type_name, instance_name) = match type_ref {
            Some(r) => r,
            None => return Ok(None),
        };

        // Edit positions as (file_uri, line0, col). Definition sites are the
        // instance name itself (the node key), so their key position IS the name
        // and needs no text lookup. Use-site value columns are resolved from
        // text — this also reaches closed files via the reverse index, so rename
        // no longer refuses when a reference lives in a file that isn't open.
        let mut edits: Vec<(String, u32, u32)> = Vec::new();

        {
            let info = self.state.info_service.read();
            let instances = info.type_index.instances(&type_name);
            for (file_uri, inst) in instances.iter().filter(|(_, i)| i.name == instance_name) {
                edits.push((
                    file_uri.to_string(),
                    inst.location.line.saturating_sub(1),
                    inst.location.col as u32,
                ));
            }
        }

        // Use sites: resolve each value column from text. Refuse (rather than
        // corrupt) if any recorded reference can't be located in text.
        let sites = self.collect_use_sites(&type_name, &instance_name);
        let mut text_uris: Vec<String> = edits.iter().map(|(uri, _, _)| uri.clone()).collect();
        text_uris.extend(sites.iter().map(|(uri, _)| uri.clone()));
        let texts = self.file_text_snapshots_for(&text_uris).await;
        let resolved = self.resolve_value_sites(&sites, &instance_name, &texts);
        let unresolved = resolved.iter().filter(|(_, _, _, ok)| !ok).count();
        if unresolved > 0 {
            return Err(tower_lsp::jsonrpc::Error {
                // -32002 = RequestFailed (LSP extension to JSON-RPC)
                code: tower_lsp::jsonrpc::ErrorCode::ServerError(-32002),
                message: format!(
                    "Rename cancelled: {} reference(s) to '{}' could not be located in text; \
                     rename is limited to indexed references.",
                    unresolved, instance_name
                )
                .into(),
                data: None,
            });
        }
        for (file_uri, line0, col, _) in resolved {
            edits.push((file_uri, line0, col));
        }

        if edits.is_empty() {
            return Ok(None);
        }

        // Group text edits by file URI, deduping so overlapping edits (a
        // definition that also classifies as a use site) aren't emitted twice.
        let mut seen: HashSet<(String, u32, u32)> = HashSet::new();
        let mut by_uri: HashMap<String, Vec<TextEdit>> = HashMap::new();
        for (file_uri, line0, col) in edits {
            if !seen.insert((file_uri.clone(), line0, col)) {
                continue;
            }
            let edit = TextEdit {
                range: self.source_range_with_text(
                    texts.get(&file_uri).map(|snapshot| snapshot.text.as_str()),
                    line0,
                    col,
                    &instance_name,
                ),
                new_text: new_name.clone(),
            };
            by_uri.entry(file_uri).or_default().push(edit);
        }

        let by_uri: Vec<(String, Vec<TextEdit>)> = by_uri.into_iter().collect();
        if let Some(refused) = self.first_refused_edit_target(&by_uri, &uri) {
            return Err(refused);
        }
        Ok(Some(self.build_workspace_edit(by_uri)))
    }
}
