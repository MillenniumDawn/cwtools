use std::collections::{HashMap, HashSet};

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

use cwtools_info::PositionElement;
use cwtools_rules::rules_types::{NewField, RootRule, RuleSet, RuleType};
use cwtools_string_table::string_table::StringTable;

use crate::paths::{
    current_token_range_with_encoding, encoded_position_len, logical_path_from_uri,
    lsp_pos_to_source_in_text, parse_uri, source_position_to_lsp,
};
use crate::{Backend, ParsedDoc, RuleCursorInfo};
use cwtools_info::ReferenceHint;

impl Backend {
    /// Look up the TypeRef (type_name, instance_name) under the cursor.
    ///
    /// Shared by goto_definition, references, prepare_rename, and rename to
    /// avoid the same 25-line block being copy-pasted in four places.
    pub(crate) fn type_ref_at_cursor(
        &self,
        uri: &str,
        pos: tower_lsp::lsp_types::Position,
        logical_path: &str,
    ) -> Option<(String, String)> {
        match self.rule_info_at_cursor(uri, pos, logical_path) {
            Some(RuleCursorInfo {
                hint: ReferenceHint::TypeRef { type_name, value },
                ..
            }) => Some((type_name, unquote(&value).to_string())),
            _ => None,
        }
    }

    /// Goto for a `$KEY$` reference in a `.yml` loc file: jump to the entry the
    /// key names. `None` when the cursor isn't on a known loc-key reference.
    fn loc_ref_goto(
        &self,
        uri: &str,
        pos: Position,
        fallback: &Url,
    ) -> Option<GotoDefinitionResponse> {
        let (key, _, _) = self.loc_ref_at_cursor_doc(uri, pos)?;
        let target = {
            let map = self.state.loc_locations.read();
            map.get(&key.to_lowercase()).cloned()
        }?;
        Some(GotoDefinitionResponse::Array(vec![
            self.source_location_at(&target.0, target.1, 0, &key, fallback),
        ]))
    }

    pub(crate) async fn goto_definition_impl(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let pos = params.text_document_position_params.position;
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .to_string();

        let ws_prefix = self.state.config.read().workspace_prefix.clone();
        let logical_path = logical_path_from_uri(&uri, &ws_prefix);
        let fallback = &params.text_document_position_params.text_document.uri;

        // Localisation file: goto on a `$KEY$` reference jumps to the loc entry
        // it names. .yml isn't a game AST, so handle it before the rule walk.
        if crate::paths::is_loc_file(&uri) {
            return Ok(self.loc_ref_goto(&uri, pos, fallback));
        }

        // `.cwt` rule files aren't game content — no goto into rule definitions. (#43)
        if crate::paths::is_cwt_file(&uri) {
            return Ok(None);
        }

        // Rule-aware lookup via the position resolver. The classified hint tells
        // us how to find the definition; mirror the kinds hover handles.
        if let Some(info) = self.rule_info_at_cursor(&uri, pos, &logical_path) {
            let locations = match &info.hint {
                ReferenceHint::TypeRef { type_name, value } => {
                    let value = unquote(value);
                    let defs = {
                        let svc = self.state.info_service.read();
                        svc.type_index
                            .instances(type_name)
                            .iter()
                            .filter(|(_, inst)| inst.name == value)
                            .map(|(file_uri, inst)| (file_uri.to_string(), inst.location))
                            .collect::<Vec<_>>()
                    };
                    locations_at(self, defs, value, fallback)
                }
                ReferenceHint::Variable { name, .. } => {
                    let defs = {
                        let svc = self.state.info_service.read();
                        svc.find_variable_definitions(name)
                    };
                    locations_at(self, defs, name, fallback)
                }
                ReferenceHint::LocRef { key } => {
                    let target = {
                        let map = self.state.loc_locations.read();
                        map.get(&key.to_lowercase()).cloned()
                    };
                    target
                        .map(|(file_uri, line)| {
                            vec![self.source_location_at(&file_uri, line, 0, key, fallback)]
                        })
                        .unwrap_or_default()
                }
                ReferenceHint::FileRef { path } => self.file_ref_locations(path, fallback).await,
                _ => Vec::new(),
            };
            let locations = dedup_locations(locations);
            if !locations.is_empty() {
                return Ok(Some(GotoDefinitionResponse::Array(locations)));
            }
        }

        // Fallback: heuristic symbol-based lookup. Try the leaf VALUE before the
        // key — an event/decision reference like `id = some.1` or
        // `trigger_event = some.1` resolves by its dotted id (the instance name),
        // which the rule-aware path misses when the field is typed `scalar`. The
        // key is tried second so a definition node (e.g. `decision = { … }`)
        // still resolves. (#39)
        if let Some(element) = self.element_at_cursor(&uri, pos) {
            let candidates: Vec<String> = match &element {
                PositionElement::Leaf { key, value } if !value.is_empty() => {
                    vec![unquote(value).to_string(), key.clone()]
                }
                PositionElement::Leaf { key, .. } => vec![key.clone()],
                PositionElement::LeafValue { value } => vec![unquote(value).to_string()],
            };
            let candidates_with_locations = {
                let info = self.state.info_service.read();
                candidates
                    .iter()
                    .map(|symbol| {
                        // Type-instance index first: events/decisions are keyed by id.
                        let instances = info
                            .type_index
                            .instance_locations(symbol)
                            .into_iter()
                            .map(|(uri, loc)| (uri.to_string(), loc))
                            .collect::<Vec<_>>();
                        let definitions = if instances.is_empty() {
                            info.find_definitions(symbol).cloned().unwrap_or_default()
                        } else {
                            Vec::new()
                        };
                        (symbol.clone(), instances, definitions)
                    })
                    .collect::<Vec<_>>()
            };
            for (symbol, instances, definitions) in candidates_with_locations {
                let pairs = if instances.is_empty() {
                    definitions
                } else {
                    instances
                };
                let locations = dedup_locations(locations_at(self, pairs, &symbol, fallback));
                if !locations.is_empty() {
                    return Ok(Some(GotoDefinitionResponse::Array(locations)));
                }
            }
        }
        Ok(None)
    }

    /// Resolve a `FilepathField` reference (a game-relative path like
    /// `gfx/…/foo.dds`) to a file Location by probing the workspace root, then
    /// the configured vanilla install. Returns an empty Vec when nothing exists.
    async fn file_ref_locations(&self, path: &str, fallback: &Url) -> Vec<Location> {
        let path = unquote(path).trim();
        if path.is_empty() {
            return Vec::new();
        }
        let rel = std::path::Path::new(path.trim_start_matches('/'));
        let (ws_uri, vanilla_dir) = {
            let cfg = self.state.config.read();
            (cfg.workspace_uri.clone(), cfg.vanilla_dir.clone())
        };
        let mut roots: Vec<std::path::PathBuf> = Vec::new();
        if let Some(ws) = &ws_uri {
            roots.push(std::path::PathBuf::from(crate::paths::uri_to_path_str(ws)));
        }
        if let Some(v) = vanilla_dir {
            roots.push(v);
        }
        for root in roots {
            let candidate = root.join(rel);
            // Async stat: a goto request must not block the runtime on a sync
            // filesystem syscall (at most two candidate roots, so no batching).
            if tokio::fs::metadata(&candidate).await.is_ok() {
                return vec![Location {
                    uri: parse_uri(crate::paths::path_to_uri(&candidate), fallback),
                    range: Range::default(),
                }];
            }
        }
        Vec::new()
    }

    pub(crate) async fn references_impl(
        &self,
        params: ReferenceParams,
    ) -> Result<Option<Vec<Location>>> {
        let pos = params.text_document_position.position;
        let uri = params.text_document_position.text_document.uri.to_string();

        let ws_prefix = self.state.config.read().workspace_prefix.clone();
        let logical_path = logical_path_from_uri(&uri, &ws_prefix);

        // Rule-aware: identify a TypeRef at cursor, then gather every location
        // where that instance is defined or used. Definitions come from the
        // TypeIndex; use sites from the live AST of open docs plus the workspace
        // reverse index for closed files. Use-site columns are resolved from
        // text (the parser records the leaf key, not the value, precisely).
        let type_ref = self.type_ref_at_cursor(&uri, pos, &logical_path);

        if let Some((type_name, instance_name)) = type_ref {
            let fallback = &params.text_document_position.text_document.uri;
            let mut all_locs: Vec<Location> = Vec::new();

            // 1. Definition location(s) from TypeIndex.
            let definitions = {
                let info = self.state.info_service.read();
                info.type_index
                    .instances(&type_name)
                    .iter()
                    .filter(|(_, inst)| inst.name == instance_name)
                    .map(|(file_uri, inst)| (file_uri.to_string(), inst.location))
                    .collect::<Vec<_>>()
            };
            all_locs.extend(locations_at(self, definitions, &instance_name, fallback));

            // 2. Use-sites (open docs via live AST + closed files via index).
            let sites = self.collect_use_sites(&type_name, &instance_name);
            for (file_uri, line0, col, _) in self.resolve_value_sites(&sites, &instance_name) {
                all_locs.push(Location {
                    uri: parse_uri(&file_uri, fallback),
                    range: self.source_range_at(&file_uri, line0, col, &instance_name),
                });
            }

            let all_locs = dedup_locations(all_locs);
            if !all_locs.is_empty() {
                return Ok(Some(all_locs));
            }
        }

        // Fallback: heuristic-based approach
        if let Some(element) = self.element_at_cursor(&uri, pos) {
            let symbol = match &element {
                PositionElement::Leaf { key, .. } => key.clone(),
                PositionElement::LeafValue { value } => value.clone(),
            };
            let fallback = &params.text_document_position.text_document.uri;
            let (definitions, references) = {
                let info = self.state.info_service.read();
                (
                    info.find_definitions(&symbol).cloned().unwrap_or_default(),
                    info.find_references(&symbol).unwrap_or_default(),
                )
            };
            let indexed_references = {
                let index = self.state.symbol_index.lock();
                index.find_references(&symbol).cloned().unwrap_or_default()
            };
            let mut all_locs = locations_at(self, definitions, &symbol, fallback);
            all_locs.extend(locations_at(self, references, &symbol, fallback));
            all_locs.extend(indexed_references.into_iter().map(|l| Location {
                uri: parse_uri(&l.uri, fallback),
                range: self.source_range_at(
                    &l.uri,
                    l.line.saturating_sub(1),
                    l.col as u32,
                    &symbol,
                ),
            }));
            if !all_locs.is_empty() {
                return Ok(Some(all_locs));
            }
        }
        Ok(None)
    }

    /// Gather all use sites `(file_uri, key location)` of `instance_name` as a
    /// `type_name` reference: open docs from their live AST, closed files from
    /// the workspace reverse index. Open docs are taken only from the live scan
    /// (their index entry can lag a keystroke), so the reverse-index half skips
    /// them.
    fn collect_use_sites(
        &self,
        type_name: &str,
        instance_name: &str,
    ) -> Vec<(String, cwtools_info::SourceLocation)> {
        let open_uris: HashSet<String> = {
            let docs = self.state.documents.lock();
            docs.keys().cloned().collect()
        };
        let mut sites: Vec<(String, cwtools_info::SourceLocation)> = Vec::new();
        {
            let docs = self.state.documents.lock();
            let rules_guard = self.state.rules.read();
            let ws_prefix = self.state.config.read().workspace_prefix.clone();
            if let Some(rs) = rules_guard.ruleset.as_ref() {
                sites.extend(scan_use_sites(
                    type_name,
                    instance_name,
                    &docs,
                    rs,
                    &ws_prefix,
                    &self.state.string_table,
                ));
            }
        }
        {
            let info = self.state.info_service.read();
            for (file_uri, loc) in info.reference_index.references(type_name, instance_name) {
                if !open_uris.contains(file_uri.as_ref()) {
                    sites.push((file_uri.to_string(), loc));
                }
            }
        }
        sites
    }

    /// Resolve each `(file_uri, key_loc)` use site to `(file_uri, value_line0,
    /// value_col, resolved)`. Reads each file once (open-doc text or disk) and
    /// locates `name` as a whole token on the key line (falling back to the next
    /// line). When the value can't be located, `resolved` is false and the key
    /// position is returned unchanged.
    fn resolve_value_sites(
        &self,
        sites: &[(String, cwtools_info::SourceLocation)],
        name: &str,
    ) -> Vec<(String, u32, u32, bool)> {
        let mut by_file: HashMap<&str, Vec<cwtools_info::SourceLocation>> = HashMap::new();
        for (uri, loc) in sites {
            by_file.entry(uri.as_str()).or_default().push(*loc);
        }
        let mut out = Vec::new();
        for (uri, locs) in by_file {
            let lines: Option<Vec<String>> = self
                .file_text_for(uri)
                .map(|t| t.lines().map(str::to_string).collect());
            for loc in locs {
                let key_line0 = loc.line.saturating_sub(1);
                let key_col = loc.col as u32;
                let mut resolved = None;
                if let Some(lines) = &lines {
                    // Value on the key line, after the `=` that follows the key.
                    if let Some(line) = lines.get(key_line0 as usize)
                        && let Some(from) = value_start_after_eq(line, key_col)
                        && let Some(col) = value_col_in_line(line, name, from)
                    {
                        resolved = Some((key_line0, col));
                    }
                    // Fallback: `key =` with the value on the next line.
                    if resolved.is_none()
                        && let Some(line) = lines.get(key_line0 as usize + 1)
                        && let Some(col) = value_col_in_line(line, name, 0)
                    {
                        resolved = Some((key_line0 + 1, col));
                    }
                }
                match resolved {
                    Some((line0, col)) => out.push((uri.to_string(), line0, col, true)),
                    None => out.push((uri.to_string(), key_line0, key_col, false)),
                }
            }
        }
        out
    }

    /// The current text of `uri`: the open-doc buffer if open, else read from
    /// disk (encoding-aware). `None` when neither is available.
    fn file_text_for(&self, uri: &str) -> Option<String> {
        {
            let docs = self.state.documents.lock();
            if let Some(doc) = docs.get(uri) {
                return Some(doc.text.to_string());
            }
        }
        let path = crate::paths::uri_to_path_str(uri);
        cwtools_file_manager::file_manager::read_text(std::path::Path::new(&path)).ok()
    }

    fn source_range(&self, uri: &str, loc: cwtools_info::SourceLocation, token: &str) -> Range {
        self.source_range_at(uri, loc.line.saturating_sub(1), loc.col as u32, token)
    }

    fn source_range_at(&self, uri: &str, line: u32, column: u32, token: &str) -> Range {
        let encoding = self.state.config.read().position_encoding.clone();
        self.file_text_for(uri).map_or_else(
            || source_range_without_text(line, column, token, &encoding),
            |text| source_range_in_text(&text, line, column, token, &encoding),
        )
    }

    fn source_location(
        &self,
        uri: &str,
        loc: cwtools_info::SourceLocation,
        token: &str,
        fallback: &Url,
    ) -> Location {
        self.source_location_at(
            uri,
            loc.line.saturating_sub(1),
            loc.col as u32,
            token,
            fallback,
        )
    }

    fn source_location_at(
        &self,
        uri: &str,
        line: u32,
        column: u32,
        token: &str,
        fallback: &Url,
    ) -> Location {
        Location {
            uri: parse_uri(uri, fallback),
            range: self.source_range_at(uri, line, column, token),
        }
    }

    pub(crate) async fn folding_range_impl(
        &self,
        params: FoldingRangeParams,
    ) -> Result<Option<Vec<FoldingRange>>> {
        let uri = params.text_document.uri.to_string();
        let Some(text) = self.file_text_for(&uri) else {
            return Ok(None);
        };
        // Brace-matched folding over the text: the parser drops the exact `}`
        // line (it consumes trailing whitespace after a clause), so a direct
        // scan is more accurate than the AST for the closing-brace line.
        let ranges = brace_folding_ranges(&text);
        if ranges.is_empty() {
            Ok(None)
        } else {
            Ok(Some(ranges))
        }
    }

    pub(crate) async fn document_highlight_impl(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .to_string();
        let pos = params.text_document_position_params.position;
        let Some(text) = self.file_text_for(&uri) else {
            return Ok(None);
        };
        // The identifier under the cursor: prefer the rule-resolved type-ref
        // instance name, falling back to the raw token in the text.
        let ws_prefix = self.state.config.read().workspace_prefix.clone();
        let logical_path = logical_path_from_uri(&uri, &ws_prefix);
        let position_encoding = self.state.config.read().position_encoding.clone();
        let (_, source_col) = lsp_pos_to_source_in_text(&text, pos, &position_encoding);
        let symbol = self
            .type_ref_at_cursor(&uri, pos, &logical_path)
            .map(|(_, name)| name)
            .or_else(|| word_at_position(&text, pos.line, source_col as u32))
            .filter(|s| !s.is_empty());
        let Some(symbol) = symbol else {
            return Ok(None);
        };
        let symbol = symbol.as_str();
        let highlights: Vec<DocumentHighlight> = text
            .lines()
            .enumerate()
            .flat_map(|(line0, line)| {
                let position_encoding = &position_encoding;
                let text = &text;
                all_token_cols_in_line(line, symbol)
                    .into_iter()
                    .map(move |col| DocumentHighlight {
                        range: source_range_in_text(
                            text,
                            line0 as u32,
                            col,
                            symbol,
                            position_encoding,
                        ),
                        kind: Some(DocumentHighlightKind::TEXT),
                    })
            })
            .collect();
        if highlights.is_empty() {
            Ok(None)
        } else {
            Ok(Some(highlights))
        }
    }

    pub(crate) async fn document_symbol_impl(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri.to_string();

        // Hierarchical outline walked straight from the retained AST, when the
        // client advertises `hierarchicalDocumentSymbolSupport`. Falls through to
        // the flat instance/variable list otherwise (or when the AST is empty).
        if self
            .state
            .hierarchical_symbols
            .load(std::sync::atomic::Ordering::Relaxed)
            && let Some(ast) = self.ast_for(&uri)
        {
            let text = self.file_text_for(&uri).unwrap_or_default();
            let position_encoding = self.state.config.read().position_encoding.clone();
            let syms = build_doc_symbols(
                &ast.root_children,
                &ast.arena,
                &self.state.string_table,
                &text,
                &position_encoding,
            );
            if !syms.is_empty() {
                return Ok(Some(DocumentSymbolResponse::Nested(syms)));
            }
        }

        let (instances, variables) = {
            let info = self.state.info_service.read();
            let instances = info
                .type_index
                .instances_in_file(&uri)
                .into_iter()
                .map(|(type_name, inst)| (type_name.to_string(), inst.name.clone(), inst.location))
                .collect::<Vec<_>>();
            let variables = info
                .files
                .get(&uri)
                .map(|file_info| file_info.defined_variables.clone())
                .unwrap_or_default();
            (instances, variables)
        };

        // Emit type instances as document symbols (one per named instance),
        // derived from the cross-file index — `FileInfo` no longer keeps a
        // per-file copy of these.
        let mut symbols: Vec<SymbolInformation> = instances
            .into_iter()
            .map(|(type_name, name, loc)| {
                make_symbol(
                    name.clone(),
                    SymbolKind::STRUCT,
                    Location {
                        uri: params.text_document.uri.clone(),
                        range: self.source_range(&uri, loc, &name),
                    },
                    Some(type_name),
                )
            })
            .collect();

        // Also include @-variables as symbols (still tracked per-file).
        for (name, loc) in variables {
            symbols.push(make_symbol(
                name.clone(),
                SymbolKind::CONSTANT,
                Location {
                    uri: params.text_document.uri.clone(),
                    range: self.source_range(&uri, loc, &name),
                },
                None,
            ));
        }

        if symbols.is_empty() {
            Ok(None)
        } else {
            Ok(Some(DocumentSymbolResponse::Flat(symbols)))
        }
    }

    pub(crate) async fn symbol_impl(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let query = params.query.to_lowercase();
        let indexed_symbols = {
            let info = self.state.info_service.read();
            let mut entries = Vec::new();
            for (type_name, instances) in &info.type_index.map {
                for (file_uri, inst) in instances {
                    if query.is_empty() || inst.name.to_lowercase().contains(&query) {
                        entries.push((
                            type_name.clone(),
                            file_uri.to_string(),
                            inst.name.clone(),
                            inst.location,
                        ));
                    }
                    if entries.len() >= 500 {
                        break;
                    }
                }
                if entries.len() >= 500 {
                    break;
                }
            }
            entries
        };
        let mut symbols: Vec<SymbolInformation> = Vec::with_capacity(indexed_symbols.len());
        // No request document to fall back to for a workspace-wide query.
        let fallback = Url::parse("file:///unknown").expect("static URI");
        for (type_name, file_uri, name, loc) in indexed_symbols {
            symbols.push(make_symbol(
                name.clone(),
                SymbolKind::STRUCT,
                self.source_location(&file_uri, loc, &name, &fallback),
                Some(type_name),
            ));
        }

        if symbols.is_empty() {
            Ok(None)
        } else {
            Ok(Some(symbols))
        }
    }

    pub(crate) async fn prepare_rename_impl(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri = params.text_document.uri.to_string();
        let pos = params.position;
        let ws_prefix = self.state.config.read().workspace_prefix.clone();
        let logical_path = logical_path_from_uri(&uri, &ws_prefix);

        let type_ref = self.type_ref_at_cursor(&uri, pos, &logical_path);

        if let Some((_, instance_name)) = type_ref {
            // Return a range covering the whole instance-name token. Anchor the
            // start at the token's beginning (so a mid-token cursor doesn't
            // rename a shifted span) and extend by the name's length.
            let text = {
                let docs = self.state.documents.lock();
                docs.get(&uri).map(|d| d.text.clone())
            };
            let position_encoding = self.state.config.read().position_encoding.clone();
            let range =
                prepare_rename_range(text.as_deref(), pos, &instance_name, &position_encoding);
            return Ok(Some(PrepareRenameResponse::Range(range)));
        }
        Ok(None)
    }

    pub(crate) async fn rename_impl(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = params.text_document_position.text_document.uri.to_string();
        let pos = params.text_document_position.position;
        let new_name = params.new_name.clone();
        let ws_prefix = self.state.config.read().workspace_prefix.clone();
        let logical_path = logical_path_from_uri(&uri, &ws_prefix);

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
        let resolved = self.resolve_value_sites(&sites, &instance_name);
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
        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        for (file_uri, line0, col) in edits {
            if !seen.insert((file_uri.clone(), line0, col)) {
                continue;
            }
            let url = match file_uri.parse::<Url>() {
                Ok(u) => u,
                Err(_) => continue,
            };
            let edit = TextEdit {
                range: self.source_range_at(&file_uri, line0, col, &instance_name),
                new_text: new_name.clone(),
            };
            changes.entry(url).or_default().push(edit);
        }

        Ok(Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }))
    }
}

// ── Use-site scanning ─────────────────────────────────────────────────────────

/// Scan all documents indexed in `info` (whose text is in `docs`) for leaves
/// whose value equals `instance_name` and whose rule context is a TypeField
/// for `type_name`.
///
/// Returns a list of (file_uri, SourceLocation) use-sites.
///
/// Implementation: walks every leaf in every indexed file's AST.  For each
/// leaf whose value equals the target name, `is_type_ref_leaf` classifies the
/// key against the ruleset; matches are recorded as use-sites.
///
/// This is O(files × leaves) but runs only on demand (find-references / rename)
/// so is acceptable for mod-sized workspaces.
pub(crate) fn scan_use_sites(
    type_name: &str,
    instance_name: &str,
    docs: &HashMap<String, ParsedDoc>,
    ruleset: &RuleSet,
    workspace_prefix: &Option<std::sync::Arc<str>>,
    string_table: &cwtools_string_table::string_table::StringTable,
) -> Vec<(String, cwtools_info::SourceLocation)> {
    let mut results = Vec::new();

    for (file_uri, parsed_doc) in docs {
        let ast = match &parsed_doc.ast {
            Some(a) => a,
            None => continue,
        };
        let logical_path = logical_path_from_uri(file_uri, workspace_prefix);

        scan_ast_for_type_ref(
            &ast.root_children,
            &ast.arena,
            &TypeRefSearch {
                type_name,
                instance_name,
                file_uri,
                ruleset,
                logical_path: &logical_path,
                table: string_table,
            },
            &mut results,
        );
    }

    results
}

/// Recursively walk children and record leaves whose value classifies as a
/// TypeRef for the specified type+name.
/// What [`scan_ast_for_type_ref`] is looking for: the reference target plus the
/// rules/table/path needed to classify a candidate. Invariant across the walk of
/// one file, so it is threaded by reference through the recursion.
struct TypeRefSearch<'a> {
    type_name: &'a str,
    instance_name: &'a str,
    file_uri: &'a str,
    ruleset: &'a RuleSet,
    logical_path: &'a str,
    table: &'a StringTable,
}

fn scan_ast_for_type_ref(
    children: &[cwtools_parser::ast::Child],
    arena: &cwtools_parser::ast::Arena,
    search: &TypeRefSearch,
    out: &mut Vec<(String, cwtools_info::SourceLocation)>,
) {
    use cwtools_parser::ast::{Child, Value};
    let &TypeRefSearch {
        type_name,
        instance_name,
        file_uri,
        ruleset,
        logical_path,
        table,
    } = search;

    // Only keyed leaves are classified; LeafValue type refs would need
    // parent-context classification, which this shallow walk doesn't do.
    for child in children {
        let Child::Leaf(idx) = child else { continue };
        let leaf = &arena.leaves[*idx as usize];
        let key = table.get_string(leaf.key.normal).unwrap_or_default();
        let raw_val = match &leaf.value {
            Value::String(t) | Value::QString(t) => table.get_string(t.normal).unwrap_or_default(),
            _ => String::new(),
        };
        let val = unquote(&raw_val);
        if val == instance_name && is_type_ref_leaf(ruleset, &key, type_name, logical_path) {
            out.push((
                file_uri.to_string(),
                cwtools_info::SourceLocation {
                    line: leaf.pos.start.line,
                    col: leaf.pos.start.col,
                },
            ));
        }
        // Recurse into clause values
        if let Value::Clause(ch) = &leaf.value {
            scan_ast_for_type_ref(ch, arena, search, out);
        }
    }
}

/// Check if a leaf with key `leaf_key` is a TypeField reference to `type_name`.
/// Walks root_rules shallowly (depth 1) looking for a LeafRule whose left
/// is SpecificField(leaf_key) and right is TypeField(Simple(type_name)).
pub(crate) fn is_type_ref_leaf(
    ruleset: &RuleSet,
    leaf_key: &str,
    type_name: &str,
    logical_path: &str,
) -> bool {
    for root_rule in &ruleset.root_rules {
        let (rule_type_name, (rule_type, _)) = match root_rule {
            RootRule::TypeRule(n, r) => (Some(n.as_str()), r),
            RootRule::AliasRule(n, r) => (Some(n.as_str()), r),
            RootRule::SingleAliasRule(n, r) => (Some(n.as_str()), r),
        };

        // For TypeRules, check path filter
        if let RootRule::TypeRule(..) = root_rule
            && let Some(name) = rule_type_name
            && let Some(&idx) = ruleset.type_by_name.get(name)
        {
            let td = &ruleset.types[idx];
            if !cwtools_info::check_path_dir(&td.path_options, logical_path) {
                continue;
            }
        }

        let rules = match rule_type {
            RuleType::NodeRule { rules, .. } => rules.as_slice(),
            _ => continue,
        };

        for (inner, _) in rules {
            if let RuleType::LeafRule {
                left: NewField::SpecificField(k),
                right: NewField::TypeField(cwtools_rules::rules_types::TypeType::Simple(t)),
            } = inner
                && k.eq_ignore_ascii_case(leaf_key)
                && t == type_name
            {
                return true;
            }
        }
    }
    false
}

/// Remove duplicate `Location` values from a goto-definition result, keeping
/// the first occurrence of each `(uri, start_line, start_char)` triple.
///
/// Identical entries arise when the same definition is reached through more than
/// one path (the type-instance index and the heuristic node-key index, say).
/// Genuinely distinct locations (different file or different position) are
/// preserved — a mod and vanilla file defining the same entity are two real
/// sites and both survive.
fn dedup_locations(locs: Vec<Location>) -> Vec<Location> {
    let mut seen = HashSet::new();
    locs.into_iter()
        .filter(|l| {
            seen.insert((
                l.uri.to_string(),
                l.range.start.line,
                l.range.start.character,
            ))
        })
        .collect()
}

/// Whether `c` continues an identifier token (bare id charset plus `.` for
/// dotted event ids). Used to word-bound the token searches below.
fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.'
}

/// Every 0-based char column where `name` appears on `line` as a whole
/// identifier (bounded by non-identifier chars). Char-based to match the
/// parser's column counting.
fn all_token_cols_in_line(line: &str, name: &str) -> Vec<u32> {
    let chars: Vec<char> = line.chars().collect();
    let needle: Vec<char> = name.chars().collect();
    let mut out = Vec::new();
    if needle.is_empty() || needle.len() > chars.len() {
        return out;
    }
    let mut i = 0;
    while i + needle.len() <= chars.len() {
        if chars[i..i + needle.len()] == needle[..] {
            let before_ok = i == 0 || !is_ident_char(chars[i - 1]);
            let after = i + needle.len();
            let after_ok = after >= chars.len() || !is_ident_char(chars[after]);
            if before_ok && after_ok {
                out.push(i as u32);
            }
        }
        i += 1;
    }
    out
}

/// The 0-based char column just past the first `=` at/after `key_col` on `line`
/// (the operator of a `key = value` leaf; also the `=` in `>=`/`?=`/etc.). The
/// value token scan starts here so nothing in the key can be mistaken for the
/// value. `None` when no `=` follows the key.
fn value_start_after_eq(line: &str, key_col: u32) -> Option<u32> {
    line.chars()
        .enumerate()
        .skip(key_col as usize)
        .find(|(_, c)| *c == '=')
        .map(|(i, _)| i as u32 + 1)
}

/// The 0-based char column of the value token `name` on `line`, scanning only
/// the region at/after char column `from` and stopping at an unquoted `#`
/// comment. Takes the FIRST whole-token match so a repeat of the name inside a
/// trailing comment (`x = MY_FOCUS # keep MY_FOCUS`) or a second `key = value`
/// pair later on the line can't be mistaken for the value. Quoted values
/// (`"MY_FOCUS"`) match the inner token. `None` when `name` doesn't occur here.
fn value_col_in_line(line: &str, name: &str, from: u32) -> Option<u32> {
    let chars: Vec<char> = line.chars().collect();
    let needle: Vec<char> = name.chars().collect();
    if needle.is_empty() {
        return None;
    }
    let mut in_string = false;
    let mut i = from as usize;
    while i + needle.len() <= chars.len() {
        match chars[i] {
            '"' => in_string = !in_string,
            '#' if !in_string => break,
            _ => {}
        }
        if chars[i..i + needle.len()] == needle[..] {
            let before_ok = i == 0 || !is_ident_char(chars[i - 1]);
            let after = i + needle.len();
            let after_ok = after >= chars.len() || !is_ident_char(chars[after]);
            if before_ok && after_ok {
                return Some(i as u32);
            }
        }
        i += 1;
    }
    None
}

/// The identifier token the cursor sits in (extended both directions over the
/// identifier charset). `None` when the cursor isn't on an identifier.
fn word_at_position(text: &str, line0: u32, char0: u32) -> Option<String> {
    let line = text.lines().nth(line0 as usize)?;
    let chars: Vec<char> = line.chars().collect();
    let cur = (char0 as usize).min(chars.len());
    let mut start = cur;
    while start > 0 && is_ident_char(chars[start - 1]) {
        start -= 1;
    }
    let mut end = cur;
    while end < chars.len() && is_ident_char(chars[end]) {
        end += 1;
    }
    if start == end {
        return None;
    }
    Some(chars[start..end].iter().collect())
}

/// Region folding ranges for every multi-line `{ … }` block, from a brace-match
/// scan of the text (comments and quoted strings ignored). More accurate than
/// the AST for the closing-brace line, which the parser doesn't retain.
fn brace_folding_ranges(text: &str) -> Vec<FoldingRange> {
    let mut ranges = Vec::new();
    let mut stack: Vec<u32> = Vec::new();
    let mut line: u32 = 0;
    let mut in_string = false;
    let mut in_comment = false;
    for c in text.chars() {
        if c == '\n' {
            line += 1;
            in_comment = false;
            // Quoted strings never span lines in this grammar.
            in_string = false;
            continue;
        }
        if c == '\r' || in_comment {
            continue;
        }
        if in_string {
            if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '#' => in_comment = true,
            '"' => in_string = true,
            '{' => stack.push(line),
            '}' => {
                if let Some(start) = stack.pop()
                    && line > start
                {
                    ranges.push(FoldingRange {
                        start_line: start,
                        start_character: None,
                        end_line: line,
                        end_character: None,
                        kind: Some(FoldingRangeKind::Region),
                        collapsed_text: None,
                    });
                }
            }
            _ => {}
        }
    }
    ranges
}

/// The identity value of a block (`id` / `name` / `tag` child leaf, in that
/// priority), used to give repeated block keys (`focus`, `country_event`, …)
/// distinct outline names. `None` when the block has no such leaf.
fn identity_value(
    children: &[cwtools_parser::ast::Child],
    arena: &cwtools_parser::ast::Arena,
    table: &StringTable,
) -> Option<String> {
    use cwtools_parser::ast::{Child, Value};
    for want in ["id", "name", "tag"] {
        for child in children {
            let Child::Leaf(idx) = child else { continue };
            let leaf = &arena.leaves[*idx as usize];
            let key = table.get_string(leaf.key.normal).unwrap_or_default();
            if key.eq_ignore_ascii_case(want)
                && let Value::String(t) | Value::QString(t) = &leaf.value
                && let Some(raw) = table.get_string(t.normal)
            {
                let v = raw
                    .strip_prefix('"')
                    .and_then(|x| x.strip_suffix('"'))
                    .unwrap_or(&raw);
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// Build a nested `DocumentSymbol` tree from AST children: every keyed clause
/// becomes a STRUCT symbol (named by its identity leaf when present, else its
/// key) whose children are the nested clauses. `range` is the block span,
/// `selection_range` the key token (⊆ range, as LSP requires). Sibling ranges
/// are clamped so the parser's trailing-whitespace overshoot can't nest them.
fn build_doc_symbols(
    children: &[cwtools_parser::ast::Child],
    arena: &cwtools_parser::ast::Arena,
    table: &StringTable,
    text: &str,
    encoding: &PositionEncodingKind,
) -> Vec<DocumentSymbol> {
    let mut syms: Vec<DocumentSymbol> = Vec::new();
    for child in children {
        let Some(kc) = arena.keyed_clause(child) else {
            continue;
        };
        let key = table.get_string(kc.key.normal).unwrap_or_default();
        if key.is_empty() {
            continue;
        }
        let child_syms = build_doc_symbols(kc.children, arena, table, text, encoding);
        let (name, detail) = match identity_value(kc.children, arena, table) {
            Some(v) if v != key => (v, Some(key.clone())),
            _ => (key.clone(), None),
        };
        let start = source_position_to_lsp(
            text,
            kc.pos.start.line.saturating_sub(1),
            kc.pos.start.col as u32,
            encoding,
        );
        let end = source_position_to_lsp(
            text,
            kc.pos.end.line.saturating_sub(1),
            kc.pos.end.col as u32,
            encoding,
        );
        let selection_end = source_range_in_text(
            text,
            kc.pos.start.line.saturating_sub(1),
            kc.pos.start.col as u32,
            &key,
            encoding,
        )
        .end;
        #[allow(deprecated)]
        syms.push(DocumentSymbol {
            name,
            detail,
            kind: SymbolKind::STRUCT,
            tags: None,
            deprecated: None,
            range: Range { start, end },
            selection_range: Range {
                start,
                end: selection_end,
            },
            children: (!child_syms.is_empty()).then_some(child_syms),
        });
    }
    // Clamp each range end to the next sibling's start so the overshoot past
    // `}` (the parser consumes trailing whitespace) can't swallow a sibling.
    for i in 0..syms.len().saturating_sub(1) {
        let next_start = syms[i + 1].range.start;
        let cur_end = syms[i].range.end;
        if (next_start.line, next_start.character) < (cur_end.line, cur_end.character) {
            syms[i].range.end = next_start;
            let sel_end = syms[i].selection_range.end;
            if (sel_end.line, sel_end.character) > (next_start.line, next_start.character) {
                syms[i].selection_range.end = next_start;
            }
        }
    }
    syms
}

/// Strip matching outer double quotes from a token. Quoted string values keep
/// their quotes through the parser/string-table, but indexed instance names and
/// loc keys are unquoted, so references must be unquoted before comparison.
pub(crate) fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .unwrap_or(s)
}

/// Build Locations from `(file_uri, location)` pairs, each highlighting a token
/// of `name`'s length. Callers snapshot indexed data before invoking this helper
/// so range conversion never runs while an index guard is held.
fn locations_at(
    backend: &Backend,
    pairs: impl IntoIterator<Item = (String, cwtools_info::SourceLocation)>,
    name: &str,
    fallback: &Url,
) -> Vec<Location> {
    pairs
        .into_iter()
        .map(|(file_uri, loc)| backend.source_location(&file_uri, loc, name, fallback))
        .collect()
}

fn prepare_rename_range(
    text: Option<&str>,
    pos: Position,
    instance_name: &str,
    encoding: &PositionEncodingKind,
) -> Range {
    let start = text.map_or(pos, |text| {
        current_token_range_with_encoding(text, pos.line, pos.character, encoding).start
    });
    Range {
        start,
        end: Position {
            line: start.line,
            character: start.character + encoded_position_len(instance_name, encoding),
        },
    }
}

fn source_range_in_text(
    text: &str,
    line: u32,
    column: u32,
    token: &str,
    encoding: &PositionEncodingKind,
) -> Range {
    Range {
        start: source_position_to_lsp(text, line, column, encoding),
        end: source_position_to_lsp(text, line, column + token.chars().count() as u32, encoding),
    }
}

fn source_range_without_text(
    line: u32,
    column: u32,
    token: &str,
    encoding: &PositionEncodingKind,
) -> Range {
    let start = Position::new(line, column);
    Range::new(
        start,
        Position::new(line, column + encoded_position_len(token, encoding)),
    )
}

/// Build a `SymbolInformation` (the `deprecated` field is required by the
/// struct but deprecated by the protocol).
fn make_symbol(
    name: String,
    kind: SymbolKind,
    location: Location,
    container_name: Option<String>,
) -> SymbolInformation {
    #[allow(deprecated)]
    SymbolInformation {
        name,
        kind,
        tags: None,
        deprecated: None,
        location,
        container_name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_location(uri: &str, line: u32, ch: u32) -> Location {
        Location {
            uri: uri.parse().unwrap(),
            range: Range {
                start: Position {
                    line,
                    character: ch,
                },
                end: Position {
                    line,
                    character: ch + 5,
                },
            },
        }
    }

    #[test]
    fn prepare_rename_range_uses_negotiated_encoding() {
        let text = "😀 target";
        assert_eq!(
            prepare_rename_range(
                Some(text),
                Position::new(0, 9),
                "target",
                &PositionEncodingKind::UTF16,
            ),
            Range::new(Position::new(0, 3), Position::new(0, 9))
        );
        assert_eq!(
            prepare_rename_range(
                Some(text),
                Position::new(0, 8),
                "target",
                &PositionEncodingKind::UTF32,
            ),
            Range::new(Position::new(0, 2), Position::new(0, 8))
        );
    }

    #[test]
    fn prepare_rename_range_counts_non_bmp_name_units() {
        let text = "name𐐀";
        assert_eq!(
            prepare_rename_range(
                Some(text),
                Position::new(0, 6),
                "name𐐀",
                &PositionEncodingKind::UTF16,
            ),
            Range::new(Position::new(0, 0), Position::new(0, 6))
        );
        assert_eq!(
            prepare_rename_range(
                Some(text),
                Position::new(0, 5),
                "name𐐀",
                &PositionEncodingKind::UTF32,
            ),
            Range::new(Position::new(0, 0), Position::new(0, 5))
        );
    }

    #[test]
    fn source_ranges_use_negotiated_encoding() {
        let text = "😀 name𐐀";
        assert_eq!(
            source_range_in_text(text, 0, 2, "name𐐀", &PositionEncodingKind::UTF16),
            Range::new(Position::new(0, 3), Position::new(0, 9))
        );
        assert_eq!(
            source_range_in_text(text, 0, 2, "name𐐀", &PositionEncodingKind::UTF32),
            Range::new(Position::new(0, 2), Position::new(0, 7))
        );
    }

    #[test]
    fn dedup_locations_collapses_identical() {
        // Issue #62: the same definition reached through two index paths yields
        // two Locations at the same (uri, line, char). They must collapse to one
        // (distinct sites are covered by the tests below).
        let file = "file:///mod/events/a.txt";
        let locs = vec![
            make_location(file, 2, 0),
            make_location(file, 2, 0), // duplicate
        ];
        let deduped = dedup_locations(locs);
        assert_eq!(deduped.len(), 1, "identical locations must collapse to one");
    }

    #[test]
    fn dedup_locations_preserves_distinct_positions() {
        // Mod at line 2, vanilla fallback happens to be at line 6 — two
        // genuinely different definition sites, both must survive.
        let file = "file:///mod/events/a.txt";
        let locs = vec![make_location(file, 2, 0), make_location(file, 6, 0)];
        let deduped = dedup_locations(locs);
        assert_eq!(deduped.len(), 2, "distinct positions must both survive");
    }

    #[test]
    fn dedup_locations_preserves_distinct_uris() {
        // Mod file and a different (real) vanilla file: two separate definitions.
        let locs = vec![
            make_location("file:///mod/events/a.txt", 2, 0),
            make_location("file:///vanilla/events/a.txt", 2, 0),
        ];
        let deduped = dedup_locations(locs);
        assert_eq!(
            deduped.len(),
            2,
            "different URIs at same position must both survive"
        );
    }

    #[test]
    fn dedup_locations_keeps_first_occurrence() {
        // When two are identical the first must be kept (stable ordering).
        let file = "file:///mod/events/a.txt";
        let first = Location {
            uri: file.parse().unwrap(),
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 10,
                },
            },
        };
        let second = Location {
            uri: file.parse().unwrap(),
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 99,
                }, // different end, same start key
            },
        };
        let deduped = dedup_locations(vec![first.clone(), second]);
        assert_eq!(deduped.len(), 1);
        assert_eq!(
            deduped[0].range.end.character, 10,
            "must keep first occurrence"
        );
    }
}
