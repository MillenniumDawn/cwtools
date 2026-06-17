use std::collections::{HashMap, HashSet};

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

use cwtools_info::{PositionElement, element_at_position};
use cwtools_rules::rules_types::{NewField, RootRule, RuleSet, RuleType};
use cwtools_string_table::string_table::StringTable;

use crate::paths::{logical_path_from_uri, parse_uri};
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

        let ws_uri = self.state.config.read().workspace_uri.clone();
        let logical_path = logical_path_from_uri(&uri, &ws_uri);
        let fallback = &params.text_document_position_params.text_document.uri;

        // Rule-aware lookup via the position resolver. The classified hint tells
        // us how to find the definition; mirror the kinds hover handles.
        if let Some(info) = self.rule_info_at_cursor(&uri, pos, &logical_path) {
            let locations = match &info.hint {
                ReferenceHint::TypeRef { type_name, value } => {
                    let value = unquote(value);
                    let svc = self.state.info_service.read();
                    type_instance_locations(&svc, type_name, value, fallback)
                }
                ReferenceHint::Variable { name, .. } => {
                    let svc = self.state.info_service.read();
                    let defs = svc.find_variable_definitions(name);
                    locations_at(defs.iter().map(|(u, l)| (u.as_str(), *l)), name, fallback)
                }
                ReferenceHint::LocRef { key } => {
                    let map = self.state.loc_locations.read();
                    map.get(&key.to_lowercase())
                        .map(|(file_uri, line)| {
                            vec![line_location(file_uri, *line, 0, key.len(), fallback)]
                        })
                        .unwrap_or_default()
                }
                ReferenceHint::FileRef { path } => self.file_ref_locations(path, fallback),
                _ => Vec::new(),
            };
            if !locations.is_empty() {
                return Ok(Some(GotoDefinitionResponse::Array(locations)));
            }
        }

        // Fallback: heuristic symbol-based lookup
        let docs = self.state.documents.lock();
        if let Some(doc) = docs.get(&uri)
            && let Some(ast) = &doc.ast
            && let Some(element) = element_at_position(
                ast,
                pos.line + 1,
                pos.character as u16,
                &self.state.string_table,
            )
        {
            let symbol = match &element {
                PositionElement::Leaf { key, .. } => key.clone(),
                PositionElement::LeafValue { value } => value.clone(),
            };
            drop(docs);
            let info = self.state.info_service.read();
            if let Some(defs) = info.find_definitions(&symbol) {
                let locations: Vec<Location> = defs
                    .iter()
                    .map(|(file_uri, loc)| Location {
                        uri: parse_uri(
                            file_uri,
                            &params.text_document_position_params.text_document.uri,
                        ),
                        range: Range {
                            start: Position {
                                line: loc.line.saturating_sub(1),
                                character: loc.col as u32,
                            },
                            end: Position {
                                line: loc.line.saturating_sub(1),
                                character: (loc.col + symbol.len() as u16) as u32,
                            },
                        },
                    })
                    .collect();
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
    fn file_ref_locations(&self, path: &str, fallback: &Url) -> Vec<Location> {
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
            if std::fs::metadata(&candidate).is_ok() {
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

        let ws_uri = self.state.config.read().workspace_uri.clone();
        let logical_path = logical_path_from_uri(&uri, &ws_uri);

        // Try rule-aware: identify a TypeRef at cursor then scan type_index for
        // all locations where that type's instances are referenced.
        //
        // Limitation: reference scanning walks the TypeIndex for definition
        // locations only.  Tracking every *use* of a type instance across the
        // workspace would require an additional references index that is not yet
        // built.  Full cross-file reference tracking is left as future work.
        let type_ref = self.type_ref_at_cursor(&uri, pos, &logical_path);

        if let Some((type_name, instance_name)) = type_ref {
            let mut all_locs: Vec<Location> = Vec::new();

            // 1. Definition location(s) from TypeIndex.
            {
                let info = self.state.info_service.read();
                let instances = info.type_index.instances(&type_name);
                for (file_uri, inst) in instances
                    .iter()
                    .filter(|(_, inst)| inst.name == instance_name)
                {
                    all_locs.push(Location {
                        uri: file_uri.as_ref().parse().unwrap_or_else(|_| {
                            params.text_document_position.text_document.uri.clone()
                        }),
                        range: Range {
                            start: Position {
                                line: inst.location.line.saturating_sub(1),
                                character: inst.location.col as u32,
                            },
                            end: Position {
                                line: inst.location.line.saturating_sub(1),
                                character: inst.location.col as u32 + instance_name.len() as u32,
                            },
                        },
                    });
                }
            }

            // 2. Use-sites: scan all docs for TypeField leaves with the same value.
            {
                let docs = self.state.documents.lock();
                let rules_guard = self.state.rules.read();
                let ws_uri = self.state.config.read().workspace_uri.clone();
                if let Some(rs) = rules_guard.ruleset.as_ref() {
                    let use_sites = scan_use_sites(
                        &type_name,
                        &instance_name,
                        &docs,
                        rs,
                        &ws_uri,
                        &self.state.string_table,
                    );
                    for (file_uri, loc) in use_sites {
                        all_locs.push(Location {
                            uri: parse_uri(
                                file_uri,
                                &params.text_document_position.text_document.uri,
                            ),
                            range: Range {
                                start: Position {
                                    line: loc.line.saturating_sub(1),
                                    character: loc.col as u32,
                                },
                                end: Position {
                                    line: loc.line.saturating_sub(1),
                                    character: loc.col as u32 + instance_name.len() as u32,
                                },
                            },
                        });
                    }
                }
            }

            if !all_locs.is_empty() {
                return Ok(Some(all_locs));
            }
        }

        // Fallback: heuristic-based approach
        let docs = self.state.documents.lock();
        if let Some(doc) = docs.get(&uri)
            && let Some(ast) = &doc.ast
            && let Some(element) = element_at_position(
                ast,
                pos.line + 1,
                pos.character as u16,
                &self.state.string_table,
            )
        {
            let symbol = match &element {
                PositionElement::Leaf { key, .. } => key.clone(),
                PositionElement::LeafValue { value } => value.clone(),
            };
            drop(docs);
            let info = self.state.info_service.read();
            let mut all_locs = Vec::new();
            if let Some(defs) = info.find_definitions(&symbol) {
                all_locs.extend(defs.iter().map(|(file_uri, loc)| Location {
                    uri: parse_uri(file_uri, &params.text_document_position.text_document.uri),
                    range: Range {
                        start: Position {
                            line: loc.line.saturating_sub(1),
                            character: loc.col as u32,
                        },
                        end: Position {
                            line: loc.line.saturating_sub(1),
                            character: (loc.col + symbol.len() as u16) as u32,
                        },
                    },
                }));
            }
            if let Some(refs) = info.find_references(&symbol) {
                all_locs.extend(refs.iter().map(|(file_uri, loc)| Location {
                    uri: parse_uri(file_uri, &params.text_document_position.text_document.uri),
                    range: Range {
                        start: Position {
                            line: loc.line.saturating_sub(1),
                            character: loc.col as u32,
                        },
                        end: Position {
                            line: loc.line.saturating_sub(1),
                            character: (loc.col + symbol.len() as u16) as u32,
                        },
                    },
                }));
            }
            let index = self.state.symbol_index.lock();
            if let Some(locs) = index.find_references(&symbol) {
                all_locs.extend(locs.iter().map(|l| Location {
                    uri: l.uri.parse().unwrap_or_else(|_| {
                        params.text_document_position.text_document.uri.clone()
                    }),
                    range: Range {
                        start: Position {
                            line: l.line.saturating_sub(1),
                            character: l.col as u32,
                        },
                        end: Position {
                            line: l.line.saturating_sub(1),
                            character: (l.col + symbol.len() as u16) as u32,
                        },
                    },
                }));
            }
            if !all_locs.is_empty() {
                return Ok(Some(all_locs));
            }
        }
        Ok(None)
    }

    pub(crate) async fn document_symbol_impl(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri.to_string();
        let info = self.state.info_service.read();

        // Emit type instances as document symbols (one per named instance),
        // derived from the cross-file index — `FileInfo` no longer keeps a
        // per-file copy of these.
        let mut symbols: Vec<SymbolInformation> = Vec::new();
        for (type_name, inst) in info.type_index.instances_in_file(&uri) {
            #[allow(deprecated)]
            symbols.push(SymbolInformation {
                name: inst.name.clone(),
                kind: SymbolKind::STRUCT,
                tags: None,
                deprecated: None,
                location: Location {
                    uri: params.text_document.uri.clone(),
                    range: Range {
                        start: Position {
                            line: inst.location.line.saturating_sub(1),
                            character: inst.location.col as u32,
                        },
                        end: Position {
                            line: inst.location.line.saturating_sub(1),
                            character: inst.location.col as u32 + inst.name.len() as u32,
                        },
                    },
                },
                container_name: Some(type_name.to_string()),
            });
        }

        // Also include @-variables as symbols (still tracked per-file).
        let Some(file_info) = info.files.get(&uri) else {
            return Ok(if symbols.is_empty() {
                None
            } else {
                Some(DocumentSymbolResponse::Flat(symbols))
            });
        };
        for (name, loc) in &file_info.defined_variables {
            #[allow(deprecated)]
            symbols.push(SymbolInformation {
                name: name.clone(),
                kind: SymbolKind::CONSTANT,
                tags: None,
                deprecated: None,
                location: Location {
                    uri: params.text_document.uri.clone(),
                    range: Range {
                        start: Position {
                            line: loc.line.saturating_sub(1),
                            character: loc.col as u32,
                        },
                        end: Position {
                            line: loc.line.saturating_sub(1),
                            character: loc.col as u32 + name.len() as u32,
                        },
                    },
                },
                container_name: None,
            });
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
        let info = self.state.info_service.read();
        let mut symbols: Vec<SymbolInformation> = Vec::new();

        for (type_name, instances) in &info.type_index.map {
            for (file_uri, inst) in instances {
                if query.is_empty() || inst.name.to_lowercase().contains(&query) {
                    #[allow(deprecated)]
                    symbols.push(SymbolInformation {
                        name: inst.name.clone(),
                        kind: SymbolKind::STRUCT,
                        tags: None,
                        deprecated: None,
                        location: Location {
                            uri: file_uri
                                .as_ref()
                                .parse()
                                .unwrap_or_else(|_| Url::parse("file:///unknown").unwrap()),
                            range: Range {
                                start: Position {
                                    line: inst.location.line.saturating_sub(1),
                                    character: inst.location.col as u32,
                                },
                                end: Position {
                                    line: inst.location.line.saturating_sub(1),
                                    character: inst.location.col as u32 + inst.name.len() as u32,
                                },
                            },
                        },
                        container_name: Some(type_name.clone()),
                    });
                }
                // Cap at 500 to avoid flooding the client.
                if symbols.len() >= 500 {
                    break;
                }
            }
            if symbols.len() >= 500 {
                break;
            }
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
        let ws_uri = self.state.config.read().workspace_uri.clone();
        let logical_path = logical_path_from_uri(&uri, &ws_uri);

        let type_ref = self.type_ref_at_cursor(&uri, pos, &logical_path);

        if let Some((_, instance_name)) = type_ref {
            // Return a range covering the whole instance name token. The range
            // start is computed by finding where the token begins relative to
            // pos.character; for now we start at pos.character and extend right
            // (the cursor is somewhere within the token).
            // TODO: compute the true token-start position for mid-token cursors.
            let range = Range {
                start: Position {
                    line: pos.line,
                    character: pos.character,
                },
                end: Position {
                    line: pos.line,
                    character: pos.character + instance_name.len() as u32,
                },
            };
            return Ok(Some(PrepareRenameResponse::Range(range)));
        }
        Ok(None)
    }

    pub(crate) async fn rename_impl(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = params.text_document_position.text_document.uri.to_string();
        let pos = params.text_document_position.position;
        let new_name = params.new_name.clone();
        let ws_uri = self.state.config.read().workspace_uri.clone();
        let logical_path = logical_path_from_uri(&uri, &ws_uri);

        // Identify what's under the cursor
        let type_ref = self.type_ref_at_cursor(&uri, pos, &logical_path);

        let (type_name, instance_name) = match type_ref {
            Some(r) => r,
            None => return Ok(None),
        };

        // Collect definition + use-site locations (reuse references logic)
        let mut all_locs: Vec<(String, cwtools_info::SourceLocation, usize)> = Vec::new();

        // Snapshot open URIs so we can detect closed-file appearances below.
        let open_uris_snap: HashSet<String> = {
            let docs = self.state.documents.lock();
            docs.keys().cloned().collect()
        };

        {
            let info = self.state.info_service.read();
            let instances = info.type_index.instances(&type_name);
            for (file_uri, inst) in instances.iter().filter(|(_, i)| i.name == instance_name) {
                all_locs.push((file_uri.to_string(), inst.location, instance_name.len()));
            }
        }

        {
            let docs = self.state.documents.lock();
            let rules_guard = self.state.rules.read();
            let ws_uri2 = self.state.config.read().workspace_uri.clone();
            if let Some(rs) = rules_guard.ruleset.as_ref() {
                let use_sites = scan_use_sites(
                    &type_name,
                    &instance_name,
                    &docs,
                    rs,
                    &ws_uri2,
                    &self.state.string_table,
                );
                for (file_uri, loc) in use_sites {
                    all_locs.push((file_uri, loc, instance_name.len()));
                }
            }
        }

        if all_locs.is_empty() {
            return Ok(None);
        }

        // Refuse if the symbol appears in closed files. Producing a partial
        // WorkspaceEdit for open-only files would silently leave dangling
        // references in closed files; better to tell the user up front.
        let closed_file = all_locs
            .iter()
            .find(|(file_uri, _, _)| !open_uris_snap.contains(file_uri));
        if let Some((file_uri, _, _)) = closed_file {
            return Err(tower_lsp::jsonrpc::Error {
                // -32002 = RequestFailed (LSP extension to JSON-RPC)
                code: tower_lsp::jsonrpc::ErrorCode::ServerError(-32002),
                message: format!(
                    "Rename cancelled: '{}' appears in closed file {}. \
                     Open all files that reference this symbol and retry.",
                    instance_name, file_uri
                )
                .into(),
                data: None,
            });
        }

        // Build WorkspaceEdit: group text edits by file URI
        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        for (file_uri, loc, name_len) in all_locs {
            let url = match file_uri.parse::<Url>() {
                Ok(u) => u,
                Err(_) => continue,
            };
            let edit = TextEdit {
                range: Range {
                    start: Position {
                        line: loc.line.saturating_sub(1),
                        character: loc.col as u32,
                    },
                    end: Position {
                        line: loc.line.saturating_sub(1),
                        character: loc.col as u32 + name_len as u32,
                    },
                },
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
    workspace_uri: &Option<String>,
    string_table: &cwtools_string_table::string_table::StringTable,
) -> Vec<(String, cwtools_info::SourceLocation)> {
    let mut results = Vec::new();

    for (file_uri, parsed_doc) in docs {
        let ast = match &parsed_doc.ast {
            Some(a) => a,
            None => continue,
        };
        let logical_path = logical_path_from_uri(file_uri, workspace_uri);

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

/// Strip matching outer double quotes from a token. Quoted string values keep
/// their quotes through the parser/string-table, but indexed instance names and
/// loc keys are unquoted, so references must be unquoted before comparison.
pub(crate) fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .unwrap_or(s)
}

/// Build goto Locations for every definition of `instance_name` of `type_name`
/// in the type index.
fn type_instance_locations(
    svc: &cwtools_info::InfoService,
    type_name: &str,
    instance_name: &str,
    fallback: &Url,
) -> Vec<Location> {
    let instances = svc.type_index.instances(type_name);
    instances
        .iter()
        .filter(|(_, inst)| inst.name == instance_name)
        .map(|(file_uri, inst)| {
            line_location(
                file_uri,
                inst.location.line.saturating_sub(1),
                inst.location.col as u32,
                instance_name.len(),
                fallback,
            )
        })
        .collect()
}

/// Build Locations from `(file_uri, location)` pairs, each highlighting a token
/// of `name`'s length.
fn locations_at<'a>(
    pairs: impl Iterator<Item = (&'a str, cwtools_info::SourceLocation)>,
    name: &str,
    fallback: &Url,
) -> Vec<Location> {
    pairs
        .map(|(file_uri, loc)| {
            line_location(
                file_uri,
                loc.line.saturating_sub(1),
                loc.col as u32,
                name.len(),
                fallback,
            )
        })
        .collect()
}

/// A single-line Location at `(line0, col)` spanning `len` characters.
fn line_location(file_uri: &str, line0: u32, col: u32, len: usize, fallback: &Url) -> Location {
    Location {
        uri: parse_uri(file_uri, fallback),
        range: Range {
            start: Position {
                line: line0,
                character: col,
            },
            end: Position {
                line: line0,
                character: col + len as u32,
            },
        },
    }
}
