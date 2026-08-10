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
use crate::{Backend, FileTextSnapshot, ParsedDoc, RuleCursorInfo};
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
    async fn loc_ref_goto(
        &self,
        uri: &str,
        pos: Position,
        fallback: &Url,
    ) -> Option<GotoDefinitionResponse> {
        let (key, _, _) = self.loc_ref_at_cursor_doc(uri, pos)?;
        let key = key.to_lowercase();
        let target = {
            let map = self.state.loc_locations.read();
            map.get(key.as_str()).cloned()
        }?;
        let text = self.file_text_for(target.0.as_ref()).await;
        Some(GotoDefinitionResponse::Array(vec![
            self.source_location_with_text(
                target.0.as_ref(),
                target.1,
                0,
                &key,
                fallback,
                text.as_deref(),
            ),
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
            return Ok(self.loc_ref_goto(&uri, pos, fallback).await);
        }

        // `.cwt` rule file: a `<type>` / `enum[..]` / `single_alias_right[..]`
        // reference jumps to its definition in the loaded rules folder.
        if crate::paths::is_cwt_file(&uri) {
            return Ok(self.cwt_goto(&uri, pos, fallback).await);
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
                    locations_at(self, defs, value, fallback).await
                }
                ReferenceHint::Variable { name, .. } => {
                    let defs = {
                        let svc = self.state.info_service.read();
                        svc.find_variable_definitions(name)
                    };
                    locations_at(self, defs, name, fallback).await
                }
                ReferenceHint::LocRef { key } => {
                    let key = key.to_lowercase();
                    let target = {
                        let map = self.state.loc_locations.read();
                        map.get(key.as_str()).cloned()
                    };
                    if let Some((file_uri, line)) = target {
                        let text = self.file_text_for(file_uri.as_ref()).await;
                        vec![self.source_location_with_text(
                            file_uri.as_ref(),
                            line,
                            0,
                            &key,
                            fallback,
                            text.as_deref(),
                        )]
                    } else {
                        Vec::new()
                    }
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
                let locations = dedup_locations(locations_at(self, pairs, &symbol, fallback).await);
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
        let rel = path.trim_start_matches(['/', '\\']);
        let rel = std::path::Path::new(rel);
        if rel.is_absolute()
            || rel.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            })
        {
            return Vec::new();
        }
        for root in self.search_roots() {
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

    /// The classified `.cwt` reference under the cursor, read from the line
    /// text (rule files aren't game ASTs, so no rule walk).
    pub(crate) async fn cwt_ref_at_cursor(
        &self,
        uri: &str,
        pos: Position,
    ) -> Option<(cwtools_rules::rules_types::CwtDefKind, String)> {
        let text = self.file_text_for(uri).await?;
        let encoding = self.state.config.read().position_encoding.clone();
        let (_, col) = lsp_pos_to_source_in_text(&text, pos, &encoding);
        let line = text.lines().nth(pos.line as usize)?;
        cwt_ref_at(line, col as u32)
    }

    /// Goto inside a `.cwt`: jump to the referenced definition recorded by the
    /// ruleset loader. `None` when the cursor isn't on a resolvable reference.
    async fn cwt_goto(
        &self,
        uri: &str,
        pos: Position,
        fallback: &Url,
    ) -> Option<GotoDefinitionResponse> {
        let (kind, name) = self.cwt_ref_at_cursor(uri, pos).await?;
        let def = {
            let rules = self.state.rules.read();
            let rs = rules.ruleset.as_ref()?;
            rs.def_positions
                .iter()
                .find(|d| d.kind == kind && d.name == name)
                .cloned()
        }?;
        let target_uri = crate::paths::path_to_uri(&def.file);
        let text = self.file_text_for(&target_uri).await;
        Some(GotoDefinitionResponse::Array(vec![
            self.source_location_with_text(
                &target_uri,
                def.line.saturating_sub(1),
                def.col as u32,
                &name,
                fallback,
                text.as_deref(),
            ),
        ]))
    }

    /// The roots a game-relative path resolves against, in probe order: the
    /// workspace, then the configured vanilla install.
    pub(crate) fn search_roots(&self) -> Vec<std::path::PathBuf> {
        let (ws_uri, vanilla_dir) = {
            let cfg = self.state.config.read();
            (cfg.workspace_uri.clone(), cfg.vanilla_dir.clone())
        };
        let mut roots: Vec<std::path::PathBuf> = Vec::new();
        if let Some(ws) = ws_uri
            && let Ok(url) = Url::parse(&ws)
            && url.scheme() == "file"
            && let Ok(path) = url.to_file_path()
        {
            roots.push(path);
        }
        if let Some(v) = vanilla_dir {
            roots.push(v);
        }
        roots
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

        let include_declaration = params.context.include_declaration;

        if let Some((type_name, instance_name)) = type_ref {
            let fallback = &params.text_document_position.text_document.uri;
            let definitions = if include_declaration {
                let info = self.state.info_service.read();
                info.type_index
                    .instances(&type_name)
                    .iter()
                    .filter(|(_, inst)| inst.name == instance_name)
                    .map(|(file_uri, inst)| (file_uri.to_string(), inst.location))
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };

            // 2. Use-sites (open docs via live AST + closed files via index).
            let sites = self.collect_use_sites(&type_name, &instance_name);
            let mut text_uris: Vec<String> = definitions
                .iter()
                .map(|(file_uri, _)| file_uri.clone())
                .collect();
            text_uris.extend(sites.iter().map(|(file_uri, _)| file_uri.clone()));
            let texts = self.file_text_snapshots_for(&text_uris).await;
            let mut all_locs: Vec<Location> =
                locations_at_with_texts(self, definitions, &instance_name, fallback, &texts);
            for (file_uri, line0, col, _) in
                self.resolve_value_sites(&sites, &instance_name, &texts)
            {
                all_locs.push(Location {
                    uri: parse_uri(&file_uri, fallback),
                    range: self.source_range_with_text(
                        texts.get(&file_uri).map(|snapshot| snapshot.text.as_str()),
                        line0,
                        col,
                        &instance_name,
                    ),
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
                    if include_declaration {
                        info.find_definitions(&symbol).cloned().unwrap_or_default()
                    } else {
                        Vec::new()
                    },
                    info.find_references(&symbol).unwrap_or_default(),
                )
            };
            let mut pairs = definitions;
            pairs.extend(references);
            let text_uris: Vec<String> =
                pairs.iter().map(|(file_uri, _)| file_uri.clone()).collect();
            let texts = self.file_text_snapshots_for(&text_uris).await;
            let all_locs = locations_at_with_texts(self, pairs, &symbol, fallback, &texts);
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
    pub(crate) fn collect_use_sites(
        &self,
        type_name: &str,
        instance_name: &str,
    ) -> Vec<(String, cwtools_info::SourceLocation)> {
        let mut sites: Vec<(String, cwtools_info::SourceLocation)> = Vec::new();
        let open_uris: HashSet<String> = {
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
            docs.keys().cloned().collect()
        };
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
        texts: &HashMap<String, FileTextSnapshot>,
    ) -> Vec<(String, u32, u32, bool)> {
        let mut by_file: HashMap<&str, Vec<cwtools_info::SourceLocation>> = HashMap::new();
        for (uri, loc) in sites {
            by_file.entry(uri.as_str()).or_default().push(*loc);
        }
        let mut out = Vec::new();
        for (uri, locs) in by_file {
            let lines: Option<Vec<&str>> = texts
                .get(uri)
                .map(|snapshot| snapshot.text.lines().collect());
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
    /// disk through the access boundary on Tokio's blocking pool.
    pub(crate) async fn file_text_for(&self, uri: &str) -> Option<String> {
        {
            let docs = self.state.documents.lock();
            if let Some(doc) = docs.get(uri) {
                return Some(doc.text.to_string());
            }
        }
        let roots = self.state.config.read().authorized_roots.clone();
        let uri = uri.to_string();
        tokio::task::spawn_blocking(move || {
            crate::access::read_authorized_text(&uri, &roots, crate::access::MAX_URI_READ_BYTES)
        })
        .await
        .ok()
        .flatten()
    }

    pub(crate) async fn file_text_snapshots_for(
        &self,
        uris: &[String],
    ) -> HashMap<String, FileTextSnapshot> {
        let mut snapshots = HashMap::new();
        let mut closed = Vec::new();
        let mut seen_closed = HashSet::new();
        {
            let docs = self.state.documents.lock();
            for uri in uris {
                if let Some(doc) = docs.get(uri) {
                    let text = doc.text.to_string();
                    snapshots.insert(
                        uri.clone(),
                        FileTextSnapshot {
                            content_hash: cwtools_cache::workspace::content_hash(&text),
                            text,
                            version: Some(doc.version),
                        },
                    );
                } else if seen_closed.insert(uri.clone()) {
                    closed.push(uri.clone());
                }
            }
        }
        if closed.is_empty() {
            return snapshots;
        }
        let roots = self.state.config.read().authorized_roots.clone();
        if let Ok(read) = tokio::task::spawn_blocking(move || {
            closed
                .into_iter()
                .filter_map(|uri| {
                    let text = crate::access::read_authorized_text(
                        &uri,
                        &roots,
                        crate::access::MAX_URI_READ_BYTES,
                    )?;
                    Some((
                        uri,
                        FileTextSnapshot {
                            content_hash: cwtools_cache::workspace::content_hash(&text),
                            text,
                            version: None,
                        },
                    ))
                })
                .collect::<HashMap<_, _>>()
        })
        .await
        {
            snapshots.extend(read);
        }
        snapshots
    }

    fn source_range_with_text(
        &self,
        text: Option<&str>,
        line: u32,
        column: u32,
        token: &str,
    ) -> Range {
        let encoding = self.state.config.read().position_encoding.clone();
        text.map_or_else(
            || source_range_without_text(line, column, token, &encoding),
            |text| source_range_in_text(text, line, column, token, &encoding),
        )
    }

    fn source_location_with_text(
        &self,
        uri: &str,
        line: u32,
        column: u32,
        token: &str,
        fallback: &Url,
        text: Option<&str>,
    ) -> Location {
        Location {
            uri: parse_uri(uri, fallback),
            range: self.source_range_with_text(text, line, column, token),
        }
    }

    pub(crate) async fn folding_range_impl(
        &self,
        params: FoldingRangeParams,
    ) -> Result<Option<Vec<FoldingRange>>> {
        let uri = params.text_document.uri.to_string();
        let Some(text) = self.file_text_for(&uri).await else {
            return Ok(None);
        };
        // Brace-matched folding over the text: the parser drops the exact `}`
        // line (it consumes trailing whitespace after a clause), so a direct
        // scan is more accurate than the AST for the closing-brace line.
        let mut ranges = brace_folding_ranges(&text);
        ranges.extend(comment_and_region_folds(&text));
        if ranges.is_empty() {
            Ok(None)
        } else {
            Ok(Some(ranges))
        }
    }

    pub(crate) async fn selection_range_impl(
        &self,
        params: SelectionRangeParams,
    ) -> Result<Option<Vec<SelectionRange>>> {
        let uri = params.text_document.uri.to_string();
        let Some(text) = self.file_text_for(&uri).await else {
            return Ok(None);
        };
        let encoding = self.state.config.read().position_encoding.clone();
        let pairs = brace_pairs(&text);
        // One chain per requested position, in request order (LSP requires the
        // result to line up with `positions`).
        let out: Vec<SelectionRange> = params
            .positions
            .iter()
            .map(|pos| {
                // The conversion returns a 1-based line; only the char column
                // is needed (the request's 0-based line is used directly).
                let (_, col) = lsp_pos_to_source_in_text(&text, *pos, &encoding);
                let spans = selection_spans(&text, &pairs, pos.line, col as u32);
                let mut node: Option<SelectionRange> = None;
                for &((sl, sc), (el, ec)) in spans.iter().rev() {
                    node = Some(SelectionRange {
                        range: Range {
                            start: source_position_to_lsp(&text, sl, sc, &encoding),
                            end: source_position_to_lsp(&text, el, ec, &encoding),
                        },
                        parent: node.map(Box::new),
                    });
                }
                // Outside any token or block: an empty chain is not allowed,
                // so anchor at the cursor itself.
                node.unwrap_or(SelectionRange {
                    range: Range {
                        start: *pos,
                        end: *pos,
                    },
                    parent: None,
                })
            })
            .collect();
        Ok(Some(out))
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
        let Some(text) = self.file_text_for(&uri).await else {
            return Ok(None);
        };
        // The identifier under the cursor: prefer the rule-resolved type-ref
        // instance name, falling back to the raw token in the text.
        let (ws_prefix, position_encoding) = {
            let cfg = self.state.config.read();
            (cfg.workspace_prefix.clone(), cfg.position_encoding.clone())
        };
        let logical_path = logical_path_from_uri(&uri, &ws_prefix);
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
                code_token_cols_in_line(line, symbol)
                    .into_iter()
                    .map(move |col| DocumentHighlight {
                        range: source_range_in_text(
                            text,
                            line0 as u32,
                            col,
                            symbol,
                            position_encoding,
                        ),
                        kind: Some(highlight_kind(line, col, symbol)),
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
            let text = self.file_text_for(&uri).await.unwrap_or_default();
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

        let text = self.file_text_for(&uri).await;

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
                        range: self.source_range_with_text(
                            text.as_deref(),
                            loc.line.saturating_sub(1),
                            loc.col as u32,
                            &name,
                        ),
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
                    range: self.source_range_with_text(
                        text.as_deref(),
                        loc.line.saturating_sub(1),
                        loc.col as u32,
                        &name,
                    ),
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
        // Bounded top-k on the deterministic (rank, name, uri, line, col)
        // order: a max-heap whose root is the worst kept candidate, so a
        // non-improving match is rejected by one borrowed comparison before
        // any string is cloned. The old collect-then-sort materialized every
        // matching symbol — the whole workspace for an empty query (the
        // picker's initial list) — just to keep 500.
        let mut top = TopSymbols::new(WORKSPACE_SYMBOL_LIMIT);
        {
            let info = self.state.info_service.read();
            for (type_name, instances) in &info.type_index.map {
                for (file_uri, inst) in instances {
                    let Some(rank) = symbol_rank(&inst.name, &query) else {
                        continue;
                    };
                    let line0 = inst.location.line.saturating_sub(1);
                    let col = inst.location.col as u32;
                    if top.accepts(rank, &inst.name, file_uri, line0, col) {
                        top.push(SymbolCandidate {
                            rank,
                            name: inst.name.clone(),
                            container: Some(type_name.clone()),
                            kind: SymbolKind::STRUCT,
                            file_uri: file_uri.to_string(),
                            line0,
                            col,
                        });
                    }
                }
            }
            // `@`-constants, still tracked per-file (as in the document outline).
            for (file_uri, fi) in &info.files {
                for (name, loc) in &fi.defined_variables {
                    let Some(rank) = symbol_rank(name, &query) else {
                        continue;
                    };
                    let line0 = loc.line.saturating_sub(1);
                    let col = loc.col as u32;
                    if top.accepts(rank, name, file_uri, line0, col) {
                        top.push(SymbolCandidate {
                            rank,
                            name: name.clone(),
                            container: None,
                            kind: SymbolKind::CONSTANT,
                            file_uri: file_uri.clone(),
                            line0,
                            col,
                        });
                    }
                }
            }
        }
        // Localisation keys (stored lowercased; loc keys are conventionally
        // lowercase, so the display form matches the file).
        {
            let ll = self.state.loc_locations.read();
            for (key, (file_uri, line0)) in ll.iter() {
                let Some(rank) = symbol_rank(key, &query) else {
                    continue;
                };
                if top.accepts(rank, key, file_uri, *line0, 0) {
                    top.push(SymbolCandidate {
                        rank,
                        name: key.to_string(),
                        container: None,
                        kind: SymbolKind::KEY,
                        file_uri: file_uri.to_string(),
                        line0: *line0,
                        col: 0,
                    });
                }
            }
        }
        let cands = top.into_sorted_vec();

        let text_uris: Vec<String> = cands.iter().map(|c| c.file_uri.clone()).collect();
        let texts = self.file_text_snapshots_for(&text_uris).await;
        let mut symbols: Vec<SymbolInformation> = Vec::with_capacity(cands.len());
        // No request document to fall back to for a workspace-wide query.
        let fallback = Url::parse("file:///unknown").expect("static URI");
        for c in cands {
            symbols.push(make_symbol(
                c.name.clone(),
                c.kind,
                self.source_location_with_text(
                    &c.file_uri,
                    c.line0,
                    c.col,
                    &c.name,
                    &fallback,
                    texts
                        .get(&c.file_uri)
                        .map(|snapshot| snapshot.text.as_str()),
                ),
                c.container,
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

/// The rename-cancelled error for a target the edit boundary refused, naming
/// the cause it reported. `-32002` is RequestFailed, the same code the
/// unresolvable-reference refusal uses, so the client shows the reason instead
/// of applying a partial rename.
fn rename_refused(uri: &str, refusal: crate::access::EditRefusal) -> tower_lsp::jsonrpc::Error {
    tower_lsp::jsonrpc::Error {
        code: tower_lsp::jsonrpc::ErrorCode::ServerError(-32002),
        message: format!(
            "Rename cancelled: '{uri}' {}; cwtools only edits files in the workspace folders.",
            refusal.reason()
        )
        .into(),
        data: None,
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
                    end: (leaf.pos.end.line, leaf.pos.end.col),
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
/// Uses the ruleset's depth-one leaf-key lookup when available. Hand-built
/// rulesets that have not been reindexed retain the direct root-rule scan.
pub(crate) fn is_type_ref_leaf(
    ruleset: &RuleSet,
    leaf_key: &str,
    type_name: &str,
    logical_path: &str,
) -> bool {
    if !ruleset.type_reference_rules.is_empty() {
        return ruleset
            .type_reference_rules_for_key(leaf_key)
            .is_some_and(|entries| {
                entries.iter().any(|entry| {
                    if entry.ref_type != type_name {
                        return false;
                    }
                    match &entry.root_type {
                        None => true,
                        Some(root_type) => ruleset
                            .type_by_name
                            .get(root_type)
                            .map(|&idx| {
                                cwtools_info::check_path_dir(
                                    &ruleset.types[idx].path_options,
                                    logical_path,
                                )
                            })
                            // Preserve the legacy scan: a TypeRule without a
                            // matching TypeDefinition has no path gate.
                            .unwrap_or(true),
                    }
                })
            });
    }

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
            RuleType::NodeRule { rules, .. } => rules.as_ref(),
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

/// Case-insensitive substring test for the `workspace/symbol` query, run over
/// every instance in the type index. `query` must already be lowercased.
/// Instance names are ASCII-dominant, so the common case is matched by
/// ASCII-folding bytes with no allocation; a name containing non-ASCII bytes
/// falls back to `to_lowercase().contains(..)` so multi-byte case folding
/// still matches (results are identical to that for every input, just not
/// allocation-free).
fn name_contains_ignore_case(name: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    if !name.is_ascii() {
        return name.to_lowercase().contains(query);
    }
    let (n, q) = (name.as_bytes(), query.as_bytes());
    q.len() <= n.len() && n.windows(q.len()).any(|w| w.eq_ignore_ascii_case(q))
}

/// One `workspace/symbol` match before range conversion: where it lives
/// (0-based line, char col) plus how it sorts (`rank`, then name, then uri,
/// then position — the `Ord` impl below).
struct SymbolCandidate {
    rank: u8,
    name: String,
    container: Option<String>,
    kind: SymbolKind,
    file_uri: String,
    line0: u32,
    col: u32,
}

impl SymbolCandidate {
    fn sort_key(&self) -> (u8, &str, &str, u32, u32) {
        (self.rank, &self.name, &self.file_uri, self.line0, self.col)
    }
}

impl Ord for SymbolCandidate {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.sort_key().cmp(&other.sort_key())
    }
}

impl PartialOrd for SymbolCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for SymbolCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.sort_key() == other.sort_key()
    }
}

impl Eq for SymbolCandidate {}

/// Response cap for `workspace/symbol`, matching what symbol pickers show.
const WORKSPACE_SYMBOL_LIMIT: usize = 500;

/// Bounded top-k accumulator for `workspace/symbol`: a max-heap of at most
/// `limit` candidates whose root is the worst one kept. [`accepts`](Self::accepts)
/// compares an incoming candidate's borrowed sort key against that root, so
/// callers only clone name/uri strings for candidates that make the cut.
struct TopSymbols {
    limit: usize,
    heap: std::collections::BinaryHeap<SymbolCandidate>,
}

impl TopSymbols {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            heap: std::collections::BinaryHeap::with_capacity(limit + 1),
        }
    }

    /// Whether a candidate with this sort key would be kept. Equal-to-worst is
    /// rejected: it could only displace an identically-ordered entry.
    fn accepts(&self, rank: u8, name: &str, file_uri: &str, line0: u32, col: u32) -> bool {
        if self.heap.len() < self.limit {
            return true;
        }
        let Some(worst) = self.heap.peek() else {
            return true;
        };
        (rank, name, file_uri, line0, col) < worst.sort_key()
    }

    fn push(&mut self, cand: SymbolCandidate) {
        self.heap.push(cand);
        if self.heap.len() > self.limit {
            self.heap.pop();
        }
    }

    /// The kept candidates, best first.
    fn into_sorted_vec(self) -> Vec<SymbolCandidate> {
        self.heap.into_sorted_vec()
    }
}

/// Rank of a workspace-symbol candidate against the (already lowercased)
/// query: 0 exact, 1 prefix, 2 substring, `None` when it doesn't match. The
/// empty query admits everything (the picker's initial, unfiltered list).
/// ASCII names (the dominant case) rank with no allocation; non-ASCII names
/// take the same `to_lowercase` fallback as `name_contains_ignore_case`.
fn symbol_rank(name: &str, query: &str) -> Option<u8> {
    if query.is_empty() {
        return Some(2);
    }
    if !name_contains_ignore_case(name, query) {
        return None;
    }
    if name.is_ascii() {
        let (n, q) = (name.as_bytes(), query.as_bytes());
        return if n.eq_ignore_ascii_case(q) {
            Some(0)
        } else if q.len() <= n.len() && n[..q.len()].eq_ignore_ascii_case(q) {
            Some(1)
        } else {
            Some(2)
        };
    }
    let lower = name.to_lowercase();
    if lower == query {
        Some(0)
    } else if lower.starts_with(query) {
        Some(1)
    } else {
        Some(2)
    }
}

/// Whether `c` continues an identifier token (bare id charset plus `.` for
/// dotted event ids). Used to word-bound the token searches below.
fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.'
}

/// Every 0-based char column where `name` appears on `line` as a whole
/// identifier (bounded by non-identifier chars), ignoring anything behind an
/// unquoted `#` comment. Quoted occurrences still match (values may be
/// quoted). Char-based to match the parser's column counting.
fn code_token_cols_in_line(line: &str, name: &str) -> Vec<u32> {
    let chars: Vec<char> = line.chars().collect();
    let needle: Vec<char> = name.chars().collect();
    let mut out = Vec::new();
    if needle.is_empty() || needle.len() > chars.len() {
        return out;
    }
    let mut in_string = false;
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '"' => in_string = !in_string,
            '#' if !in_string => break,
            _ => {}
        }
        if i + needle.len() <= chars.len() && chars[i..i + needle.len()] == needle[..] {
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

/// WRITE when the token at `col` is an assignment key (the next non-space char
/// after it is `=`), READ otherwise. Advisory: clients only use this to tint
/// the highlight.
fn highlight_kind(line: &str, col: u32, name: &str) -> DocumentHighlightKind {
    let after = col as usize + name.chars().count();
    match line.chars().skip(after).find(|c| !c.is_whitespace()) {
        Some('=') => DocumentHighlightKind::WRITE,
        _ => DocumentHighlightKind::READ,
    }
}

/// The 0-based char column just past the first `=` at/after `key_col` on `line`
/// (the operator of a `key = value` leaf; also the `=` in `>=`/`?=`/etc.). The
/// value token scan starts here so nothing in the key can be mistaken for the
/// value. `None` when no `=` follows the key.
pub(crate) fn value_start_after_eq(line: &str, key_col: u32) -> Option<u32> {
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
pub(crate) fn value_col_in_line(line: &str, name: &str, from: u32) -> Option<u32> {
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

/// Classify the `.cwt` construct at char `col` on `line`: a `<type>` /
/// `<!type>` / `<type.subtype>` reference, `enum[..]` / `complex_enum[..]`,
/// or `single_alias_right[..]`. Alias categories and value sets are out —
/// they have no single definition site (consistent with the structural lint).
pub(crate) fn cwt_ref_at(
    line: &str,
    col: u32,
) -> Option<(cwtools_rules::rules_types::CwtDefKind, String)> {
    use cwtools_rules::rules_types::CwtDefKind;
    let chars: Vec<char> = line.chars().collect();
    let col = col as usize;
    // `<...>` spans (angle brackets included in the hit area).
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '<'
            && let Some(close) = chars[i + 1..]
                .iter()
                .position(|&c| c == '>')
                .map(|p| p + i + 1)
        {
            if (i..=close).contains(&col) {
                let inner: String = chars[i + 1..close].iter().collect();
                let name = inner.trim_start_matches('!');
                // `<type.subtype>` is defined by its base type.
                let base = name.split('.').next().unwrap_or(name);
                return (!base.is_empty()).then(|| (CwtDefKind::Type, base.to_string()));
            }
            i = close + 1;
            continue;
        }
        i += 1;
    }
    for (prefix, kind) in [
        ("complex_enum", CwtDefKind::Enum),
        ("enum", CwtDefKind::Enum),
        ("single_alias_right", CwtDefKind::SingleAlias),
    ] {
        if let Some(name) = bracket_ref_at(&chars, col, prefix) {
            return Some((kind, name));
        }
    }
    None
}

/// The bracketed name of a `prefix[NAME]` occurrence whose span covers `col`,
/// word-bounded so `enum[` inside `complex_enum[` doesn't match.
fn bracket_ref_at(chars: &[char], col: usize, prefix: &str) -> Option<String> {
    let p: Vec<char> = prefix.chars().collect();
    let mut i = 0;
    while i + p.len() < chars.len() {
        if chars[i..i + p.len()] == p[..]
            && chars.get(i + p.len()) == Some(&'[')
            && (i == 0 || !is_ident_char(chars[i - 1]))
            && let Some(close) = chars[i + p.len() + 1..]
                .iter()
                .position(|&c| c == ']')
                .map(|q| q + i + p.len() + 1)
        {
            if (i..=close).contains(&col) {
                let name: String = chars[i + p.len() + 1..close].iter().collect();
                return (!name.is_empty()).then_some(name);
            }
            i = close + 1;
            continue;
        }
        i += 1;
    }
    None
}

/// The `@name` script-constant token at (line0, col): the full token including
/// the sigil, and its 0-based start char col. `None` when the cursor isn't on
/// one.
fn at_var_at_cursor(text: &str, line0: u32, col: u32) -> Option<(String, u32)> {
    let line = text.lines().nth(line0 as usize)?;
    let chars: Vec<char> = line.chars().collect();
    let mut cur = (col as usize).min(chars.len());
    // Cursor on the sigil itself: step into the name.
    if cur < chars.len() && chars[cur] == '@' {
        cur += 1;
    }
    let mut start = cur;
    while start > 0 && is_ident_char(chars[start - 1]) {
        start -= 1;
    }
    let mut end = cur;
    while end < chars.len() && is_ident_char(chars[end]) {
        end += 1;
    }
    if start == end || start == 0 || chars[start - 1] != '@' {
        return None;
    }
    let name: String = std::iter::once('@')
        .chain(chars[start..end].iter().copied())
        .collect();
    Some((name, start as u32 - 1))
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

/// A start/end span in source char coordinates: ((line, col), (line, col)),
/// end-exclusive.
type CharSpan = ((u32, u32), (u32, u32));

/// Every matched `{ … }` pair in `text` as ((open_line, open_col),
/// (close_line, close_col)) char positions of the braces themselves, from the
/// same comment- and string-aware scan folding uses.
fn brace_pairs(text: &str) -> Vec<CharSpan> {
    let mut pairs = Vec::new();
    let mut stack: Vec<(u32, u32)> = Vec::new();
    let (mut line, mut col): (u32, u32) = (0, 0);
    let mut in_string = false;
    let mut in_comment = false;
    for c in text.chars() {
        if c == '\n' {
            line += 1;
            col = 0;
            in_comment = false;
            // Quoted strings never span lines in this grammar.
            in_string = false;
            continue;
        }
        let here = (line, col);
        col += 1;
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
            '{' => stack.push(here),
            '}' => {
                if let Some(open) = stack.pop() {
                    pairs.push((open, here));
                }
            }
            _ => {}
        }
    }
    pairs
}

/// The innermost-first selection chain at (line0, col): the identifier token
/// under the cursor, then for each enclosing brace pair its content span
/// (inside the braces) followed by the full span (including them). Every span
/// contains the previous one, as `textDocument/selectionRange` requires.
fn selection_spans(text: &str, pairs: &[CharSpan], line0: u32, col: u32) -> Vec<CharSpan> {
    let mut spans: Vec<CharSpan> = Vec::new();
    if let Some(line) = text.lines().nth(line0 as usize) {
        let chars: Vec<char> = line.chars().collect();
        let cur = (col as usize).min(chars.len());
        let mut start = cur;
        while start > 0 && is_ident_char(chars[start - 1]) {
            start -= 1;
        }
        let mut end = cur;
        while end < chars.len() && is_ident_char(chars[end]) {
            end += 1;
        }
        if start < end {
            spans.push(((line0, start as u32), (line0, end as u32)));
        }
    }
    let pos = (line0, col);
    let mut enclosing: Vec<&CharSpan> = pairs
        .iter()
        .filter(|(open, close)| *open <= pos && pos <= *close)
        .collect();
    // Innermost first: the latest-opening enclosing pair is the tightest.
    enclosing.sort_by_key(|p| std::cmp::Reverse(p.0));
    for &&((ol, oc), (cl, cc)) in &enclosing {
        let inner = ((ol, oc + 1), (cl, cc));
        if inner.0 < inner.1 {
            spans.push(inner);
        }
        spans.push(((ol, oc), (cl, cc + 1)));
    }
    spans
}

/// Folding ranges the brace scan can't produce: runs of two or more full-line
/// `#` comments fold as `Comment`, and `#region` / `#endregion` marker pairs
/// (stack-matched, so they nest) fold as `Region`. Marker lines belong to
/// their region fold and never count toward a comment run; unmatched markers
/// are ignored.
fn comment_and_region_folds(text: &str) -> Vec<FoldingRange> {
    let mut folds = Vec::new();
    let mut region_stack: Vec<u32> = Vec::new();
    let mut run_start: Option<u32> = None;
    let mut prev_line: u32 = 0;
    let close_run = |start: Option<u32>, end_line: u32, folds: &mut Vec<FoldingRange>| {
        if let Some(start) = start
            && end_line > start
        {
            folds.push(FoldingRange {
                start_line: start,
                start_character: None,
                end_line,
                end_character: None,
                kind: Some(FoldingRangeKind::Comment),
                collapsed_text: None,
            });
        }
    };
    for (line0, line) in text.lines().enumerate() {
        let line0 = line0 as u32;
        prev_line = line0;
        let trimmed = line.trim_start();
        let is_marker_start = region_marker(trimmed) == Some(true);
        let is_marker_end = region_marker(trimmed) == Some(false);
        if is_marker_start || is_marker_end || !trimmed.starts_with('#') {
            close_run(run_start.take(), line0.saturating_sub(1), &mut folds);
        } else if run_start.is_none() {
            run_start = Some(line0);
        }
        if is_marker_start {
            region_stack.push(line0);
        } else if is_marker_end
            && let Some(start) = region_stack.pop()
            && line0 > start
        {
            folds.push(FoldingRange {
                start_line: start,
                start_character: None,
                end_line: line0,
                end_character: None,
                kind: Some(FoldingRangeKind::Region),
                collapsed_text: None,
            });
        }
    }
    close_run(run_start.take(), prev_line, &mut folds);
    folds
}

/// `Some(true)` for a `#region` marker, `Some(false)` for `#endregion`, `None`
/// for anything else. `line` must already be left-trimmed. A marker may carry
/// a trailing label (`#region Alpha`).
fn region_marker(line: &str) -> Option<bool> {
    if let Some(rest) = line.strip_prefix("#endregion") {
        (rest.is_empty() || rest.starts_with(char::is_whitespace)).then_some(false)
    } else if let Some(rest) = line.strip_prefix("#region") {
        (rest.is_empty() || rest.starts_with(char::is_whitespace)).then_some(true)
    } else {
        None
    }
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
/// of `name`'s length. Text is fetched in one batch before the pure conversion.
async fn locations_at(
    backend: &Backend,
    pairs: impl IntoIterator<Item = (String, cwtools_info::SourceLocation)>,
    name: &str,
    fallback: &Url,
) -> Vec<Location> {
    let pairs: Vec<_> = pairs.into_iter().collect();
    let uris: Vec<String> = pairs.iter().map(|(file_uri, _)| file_uri.clone()).collect();
    let texts = backend.file_text_snapshots_for(&uris).await;
    locations_at_with_texts(backend, pairs, name, fallback, &texts)
}

fn locations_at_with_texts(
    backend: &Backend,
    pairs: impl IntoIterator<Item = (String, cwtools_info::SourceLocation)>,
    name: &str,
    fallback: &Url,
    texts: &HashMap<String, FileTextSnapshot>,
) -> Vec<Location> {
    pairs
        .into_iter()
        .map(|(file_uri, loc)| {
            backend.source_location_with_text(
                &file_uri,
                loc.line.saturating_sub(1),
                loc.col as u32,
                name,
                fallback,
                texts.get(&file_uri).map(|snapshot| snapshot.text.as_str()),
            )
        })
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
    fn code_token_cols_skip_comment_matches() {
        assert_eq!(
            code_token_cols_in_line("x = FOO # FOO again", "FOO"),
            vec![4]
        );
        assert_eq!(
            code_token_cols_in_line("# only FOO", "FOO"),
            Vec::<u32>::new()
        );
        assert_eq!(code_token_cols_in_line("x = \"FOO\" # FOO", "FOO"), vec![5]);
        // A `#` inside a quoted string does not start a comment.
        assert_eq!(code_token_cols_in_line("x = \"# FOO\"", "FOO"), vec![7]);
        assert_eq!(code_token_cols_in_line("FOO = FOO", "FOO"), vec![0, 6]);
    }

    #[test]
    fn highlight_kind_write_for_assignment_key_read_otherwise() {
        assert_eq!(
            highlight_kind("MY_FOCUS = { }", 0, "MY_FOCUS"),
            DocumentHighlightKind::WRITE
        );
        assert_eq!(
            highlight_kind("    has_focus = MY_FOCUS", 16, "MY_FOCUS"),
            DocumentHighlightKind::READ
        );
        assert_eq!(
            highlight_kind("    var >= MY_FOCUS", 11, "MY_FOCUS"),
            DocumentHighlightKind::READ
        );
    }

    #[test]
    fn name_contains_ignore_case_matches_ascii_case_insensitively() {
        assert!(name_contains_ignore_case("Ship_Hull_Submarine", "hull_sub"));
        assert!(name_contains_ignore_case("Ship_Hull_Submarine", ""));
        assert!(!name_contains_ignore_case("Ship_Hull_Submarine", "cruiser"));
        assert!(!name_contains_ignore_case("abc", "abcd"));
    }

    #[test]
    fn comment_folds_need_two_consecutive_lines() {
        let folds = comment_and_region_folds("# one\n# two\n# three\nx = 1\n# lone\n");
        assert_eq!(folds.len(), 1);
        assert_eq!((folds[0].start_line, folds[0].end_line), (0, 2));
        assert_eq!(folds[0].kind, Some(FoldingRangeKind::Comment));
    }

    #[test]
    fn comment_fold_at_eof_without_newline() {
        let folds = comment_and_region_folds("x = 1\n# a\n# b");
        assert_eq!(folds.len(), 1);
        assert_eq!((folds[0].start_line, folds[0].end_line), (1, 2));
    }

    #[test]
    fn region_markers_fold_and_nest() {
        let text = "#region outer\na = 1\n#region inner\nb = 2\n#endregion\n#endregion\n";
        let folds = comment_and_region_folds(text);
        let regions: Vec<(u32, u32)> = folds
            .iter()
            .filter(|f| f.kind == Some(FoldingRangeKind::Region))
            .map(|f| (f.start_line, f.end_line))
            .collect();
        assert!(regions.contains(&(0, 5)), "outer region, got {:?}", regions);
        assert!(regions.contains(&(2, 4)), "inner region, got {:?}", regions);
    }

    #[test]
    fn unmatched_region_markers_are_ignored() {
        assert!(comment_and_region_folds("#endregion\nx = 1\n").is_empty());
        assert!(comment_and_region_folds("#region only\nx = 1\n").is_empty());
    }

    #[test]
    fn region_markers_break_comment_runs() {
        // The marker line belongs to its region fold, not to a comment run, so
        // a lone comment next to a marker doesn't fold as a comment block.
        let text = "# note\n#region r\nx = 1\n#endregion\n";
        let folds = comment_and_region_folds(text);
        assert!(
            folds
                .iter()
                .all(|f| f.kind != Some(FoldingRangeKind::Comment)),
            "got {:?}",
            folds
        );
    }

    #[test]
    fn selection_spans_token_then_inner_then_full_pair() {
        let text = "a = {\n    foo = bar\n}\n";
        let pairs = brace_pairs(text);
        // Cursor inside `bar` (line 1, col 10).
        let spans = selection_spans(text, &pairs, 1, 10);
        assert_eq!(
            spans,
            vec![
                ((1, 10), (1, 13)), // the token
                ((0, 5), (2, 0)),   // inside the braces
                ((0, 4), (2, 1)),   // including the braces
            ]
        );
    }

    #[test]
    fn selection_spans_nested_pairs_chain_outward() {
        let text = "a = {\n    b = {\n        x = 1\n    }\n}\n";
        let pairs = brace_pairs(text);
        let spans = selection_spans(text, &pairs, 2, 8);
        assert_eq!(
            spans,
            vec![
                ((2, 8), (2, 9)), // `x`
                ((1, 9), (3, 4)), // inside inner braces
                ((1, 8), (3, 5)), // inner pair
                ((0, 5), (4, 0)), // inside outer braces
                ((0, 4), (4, 1)), // outer pair
            ]
        );
    }

    #[test]
    fn brace_pairs_ignore_comments_and_strings() {
        assert!(brace_pairs("# { not a block\n").is_empty());
        assert!(brace_pairs("x = \"{\"\n").is_empty());
    }

    #[test]
    fn selection_spans_on_whitespace_start_at_block() {
        let text = "a = {\n    foo = bar\n}\n";
        let pairs = brace_pairs(text);
        // Cursor on the indent whitespace: no token, chain starts at the block.
        let spans = selection_spans(text, &pairs, 1, 2);
        assert_eq!(spans, vec![((0, 5), (2, 0)), ((0, 4), (2, 1))]);
    }

    #[test]
    fn cwt_ref_at_classifies_rule_references() {
        use cwtools_rules::rules_types::CwtDefKind;
        // `<focus>` anywhere in the span, including the angle brackets.
        let line = "    has_focus = <focus>";
        assert_eq!(
            cwt_ref_at(line, 18),
            Some((CwtDefKind::Type, "focus".to_string()))
        );
        assert_eq!(
            cwt_ref_at(line, 16),
            Some((CwtDefKind::Type, "focus".to_string()))
        );
        // `<!focus>` negation still names the type.
        assert_eq!(
            cwt_ref_at("    a = <!focus>", 12),
            Some((CwtDefKind::Type, "focus".to_string()))
        );
        assert_eq!(
            cwt_ref_at("    stat = enum[stat]", 17),
            Some((CwtDefKind::Enum, "stat".to_string()))
        );
        assert_eq!(
            cwt_ref_at("    b = single_alias_right[block]", 29),
            Some((CwtDefKind::SingleAlias, "block".to_string()))
        );
        // Alias categories and value sets are out of scope.
        assert_eq!(cwt_ref_at("    alias_name[effect] = x", 12), None);
        assert_eq!(cwt_ref_at("    v = value[my_set]", 16), None);
        // Cursor outside any construct.
        assert_eq!(cwt_ref_at("    has_focus = <focus>", 6), None);
    }

    #[test]
    fn at_var_at_cursor_finds_sigil_token() {
        let text = "y = @foo\n";
        assert_eq!(at_var_at_cursor(text, 0, 6), Some(("@foo".to_string(), 4)));
        assert_eq!(at_var_at_cursor(text, 0, 4), Some(("@foo".to_string(), 4)));
        // End-of-token cursor still resolves.
        assert_eq!(at_var_at_cursor(text, 0, 8), Some(("@foo".to_string(), 4)));
        // A plain identifier has no sigil.
        assert_eq!(at_var_at_cursor("y = foo\n", 0, 5), None);
        // A lone `@` is not a constant.
        assert_eq!(at_var_at_cursor("y = @\n", 0, 4), None);
    }

    #[test]
    fn symbol_rank_orders_exact_prefix_substring() {
        assert_eq!(symbol_rank("MY_FOCUS", "my_focus"), Some(0));
        assert_eq!(symbol_rank("my_focus_tooltip", "my_focus"), Some(1));
        assert_eq!(symbol_rank("@my_const", "my"), Some(2));
        assert_eq!(symbol_rank("unrelated", "my_focus"), None);
        // Empty query admits everything (the picker's initial list).
        assert_eq!(symbol_rank("anything", ""), Some(2));
        // Non-ASCII names take the to_lowercase fallback, same tiers.
        assert_eq!(symbol_rank("İstanbul", &"İstanbul".to_lowercase()), Some(0));
        assert_eq!(
            symbol_rank("İstanbul_x", &"İstanbul".to_lowercase()),
            Some(1)
        );
        assert_eq!(
            symbol_rank("x_İstanbul", &"İstanbul".to_lowercase()),
            Some(2)
        );
    }

    fn cand(rank: u8, name: &str, uri: &str, line0: u32, col: u32) -> SymbolCandidate {
        SymbolCandidate {
            rank,
            name: name.to_string(),
            container: None,
            kind: SymbolKind::STRUCT,
            file_uri: uri.to_string(),
            line0,
            col,
        }
    }

    #[test]
    fn top_symbols_matches_sort_and_truncate() {
        // The heap must keep exactly what sort-everything-then-truncate kept.
        let mut all = Vec::new();
        for (i, rank) in [2u8, 0, 1, 2, 0, 1, 2, 2].into_iter().enumerate() {
            all.push(cand(
                rank,
                &format!("name_{}", i % 5),
                "file:///a",
                i as u32,
                0,
            ));
            all.push(cand(
                rank,
                &format!("name_{}", i % 3),
                "file:///b",
                i as u32,
                7,
            ));
        }
        for limit in [1, 3, 5, all.len(), all.len() + 10] {
            let mut top = TopSymbols::new(limit);
            for c in &all {
                if top.accepts(c.rank, &c.name, &c.file_uri, c.line0, c.col) {
                    top.push(cand(c.rank, &c.name, &c.file_uri, c.line0, c.col));
                }
            }
            let mut expected: Vec<_> = all.iter().map(|c| c.sort_key()).collect();
            expected.sort();
            expected.truncate(limit);
            let got: Vec<_> = top.into_sorted_vec();
            assert_eq!(
                got.iter().map(|c| c.sort_key()).collect::<Vec<_>>(),
                expected
            );
        }
    }

    #[test]
    fn top_symbols_accepts_only_improving_candidates_when_full() {
        let mut top = TopSymbols::new(2);
        top.push(cand(0, "aaa", "file:///a", 0, 0));
        top.push(cand(1, "bbb", "file:///a", 1, 0));
        // Worse than the kept worst: rejected without displacing anything.
        assert!(!top.accepts(2, "ccc", "file:///a", 2, 0));
        // Equal to the kept worst: rejected (could only swap an identical key).
        assert!(!top.accepts(1, "bbb", "file:///a", 1, 0));
        // Better than the kept worst: accepted, and pushing evicts it.
        assert!(top.accepts(0, "aab", "file:///a", 3, 0));
        top.push(cand(0, "aab", "file:///a", 3, 0));
        let names: Vec<_> = top.into_sorted_vec().into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["aaa", "aab"]);
    }

    #[test]
    fn name_contains_ignore_case_falls_back_for_non_ascii_names() {
        // Matches the old `to_lowercase().contains(..)` behavior exactly, just
        // via the slow path (Turkish İ lowercases to "i̇", not ASCII "i").
        let name = "İstanbul";
        let query = name.to_lowercase();
        assert!(name_contains_ignore_case(name, &query));
        assert!(!name_contains_ignore_case(name, "nomatch"));
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
