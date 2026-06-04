use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use serde_json::Value;

use cwtools_parser::parser::parse_string;
use cwtools_parser::ast::{ParsedFile, ParseError};
use cwtools_rules::rules_types::RuleSet;
use cwtools_rules::ruleset_loader::load_ruleset_from_dir;
use cwtools_string_table::string_table::StringTable;
use cwtools_validation::{validate_ast, ValidationError};

mod position;
mod symbols;

/// Server state.
struct DocumentState {
    /// file URI -> parsed document
    documents: Mutex<HashMap<String, ParsedDoc>>,
    /// loaded .cwt ruleset
    ruleset: Mutex<Option<RuleSet>>,
    /// shared string table
    string_table: StringTable,
    /// game language from init options
    language: Mutex<String>,
    /// symbol index for goto-definition and references
    symbol_index: Mutex<symbols::SymbolIndex>,
    /// computed info service for type/references/definitions
    info_service: Mutex<cwtools_info::InfoService>,
    /// workspace folder URI captured from initialize params
    workspace_uri: Mutex<Option<String>>,
}

struct ParsedDoc {
    version: i32,
    text: String,
    ast: Option<ParsedFile>,
}

impl DocumentState {
    fn new() -> Self {
        Self {
            documents: Mutex::new(HashMap::new()),
            ruleset: Mutex::new(None),
            string_table: StringTable::new(),
            language: Mutex::new("paradox".to_string()),
            symbol_index: Mutex::new(symbols::SymbolIndex::new()),
            info_service: Mutex::new(cwtools_info::InfoService::new()),
            workspace_uri: Mutex::new(None),
        }
    }
}

struct Backend {
    client: Client,
    state: Arc<DocumentState>,
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(
        &self,
        params: InitializeParams,
    ) -> Result<InitializeResult> {
        // Store language from init options
        if let Some(opts) = &params.initialization_options {
            if let Some(lang) = opts.get("language").and_then(|v| v.as_str()) {
                *self.state.language.lock().unwrap() = lang.to_string();
                self.client
                    .log_message(MessageType::INFO, format!("language: {}", lang))
                    .await;
            }
            self.client
                .log_message(MessageType::INFO, format!("init options: {:?}", opts))
                .await;

            // Load .cwt rules from rulesCache if provided
            if let Some(cache) = opts.get("rulesCache").and_then(|v| v.as_str()) {
                let cache_path = std::path::Path::new(cache);
                let (combined_ruleset, parse_errors) =
                    load_ruleset_from_dir(cache_path, &self.state.string_table);

                for err in &parse_errors {
                    self.client
                        .log_message(MessageType::WARNING, err.clone())
                        .await;
                }

                let loaded = !combined_ruleset.types.is_empty()
                    || !combined_ruleset.enums.is_empty()
                    || !combined_ruleset.aliases.is_empty()
                    || !combined_ruleset.root_rules.is_empty();

                if loaded {
                    self.client
                        .log_message(
                            MessageType::INFO,
                            format!(
                                "Loaded rules from {} ({} types, {} enums, {} aliases, {} errors)",
                                cache,
                                combined_ruleset.types.len(),
                                combined_ruleset.enums.len(),
                                combined_ruleset.aliases.len(),
                                parse_errors.len(),
                            ),
                        )
                        .await;
                    *self.state.ruleset.lock().unwrap() = Some(combined_ruleset);
                } else {
                    self.client
                        .log_message(
                            MessageType::WARNING,
                            format!(
                                "No rules loaded from {}. Errors: {:?}",
                                cache, parse_errors
                            ),
                        )
                        .await;
                }
            }
        }

        // Store workspace URI if provided
        if let Some(folders) = &params.workspace_folders {
            if let Some(first) = folders.first() {
                *self.state.workspace_uri.lock().unwrap() = Some(first.uri.to_string());
            }
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        will_save: None,
                        will_save_wait_until: None,
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(false),
                        })),
                    },
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(false),
                    trigger_characters: Some(vec!["=".to_string(), "<".to_string()]),
                    work_done_progress_options: Default::default(),
                    all_commit_characters: None,
                    completion_item: None,
                }),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec!["getFileTypes".to_string()],
                    work_done_progress_options: Default::default(),
                }),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "cwtools-server".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "CWTools server initialized!")
            .await;

        // --- Workspace-wide initial validation (mirrors F# server behavior) ---
        // Spawned onto a background task so `initialized` returns promptly and
        // does not block LSP handshake on large workspaces.
        let client = self.client.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            let backend = Backend { client, state };
            backend.validate_entire_workspace().await;
        });
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    // --- Text document sync ---
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        let text = params.text_document.text;
        let version = params.text_document.version;

        let (diagnostics, parsed) = self.parse_and_validate(&uri, &text).await;

        {
            let mut docs = self.state.documents.lock().unwrap();
            docs.insert(
                uri.clone(),
                ParsedDoc {
                    version,
                    text: text.clone(),
                    ast: parsed,
                },
            );
        }

        self.client
            .publish_diagnostics(params.text_document.uri, diagnostics, Some(version))
            .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        let version = params.text_document.version;

        if let Some(change) = params.content_changes.into_iter().next() {
            let text = change.text;

            let (diagnostics, parsed) = self.parse_and_validate(&uri, &text).await;

            {
                let mut docs = self.state.documents.lock().unwrap();
                docs.insert(
                    uri.clone(),
                    ParsedDoc {
                        version,
                        text: text.clone(),
                        ast: parsed,
                    },
                );
            }

            self.client
                .publish_diagnostics(params.text_document.uri, diagnostics, Some(version))
                .await;
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        if let Some(text) = {
            let docs = self.state.documents.lock().unwrap();
            docs.get(&uri).map(|d| d.text.clone())
        } {
            let (diagnostics, _) = self.parse_and_validate(&uri, &text).await;
            self.client
                .publish_diagnostics(params.text_document.uri, diagnostics, None)
                .await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        {
            let mut docs = self.state.documents.lock().unwrap();
            docs.remove(&uri);
        }
        self.client
            .publish_diagnostics(params.text_document.uri, vec![], None)
            .await;
    }

    // --- Language features (stubs) ---
    async fn hover(
        &self,
        params: HoverParams,
    ) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri.to_string();
        let pos = params.text_document_position_params.position;
        
        let docs = self.state.documents.lock().unwrap();
        if let Some(doc) = docs.get(&uri) {
            if let Some(ast) = &doc.ast {
                let source_pos = cwtools_parser::ast::SourcePos {
                    line: pos.line + 1,
                    col: pos.character as u16,
                };
                if let Some(element) = position::find_at_position(ast, &source_pos, &self.state.string_table) {
                    let contents = match element {
                        position::AstElement::Node { key, .. } => {
                            format!("**Node**: `{}`", key)
                        }
                        position::AstElement::Leaf { key, value, .. } => {
                            format!("**Field**: `{} = {}`", key, value)
                        }
                        position::AstElement::LeafValue { value, .. } => {
                            format!("**Value**: `{}`", value)
                        }
                    };
                    return Ok(Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: contents,
                        }),
                        range: None,
                    }));
                }
            }
        }
        Ok(None)
    }

    async fn completion(
        &self,
        _params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        let mut items = Vec::new();

        // Type definitions and enums from ruleset
        let ruleset = self.state.ruleset.lock().unwrap();
        if let Some(rules) = ruleset.as_ref() {
            for t in &rules.types {
                items.push(CompletionItem {
                    label: t.name.clone(),
                    kind: Some(CompletionItemKind::STRUCT),
                    detail: Some("Type definition".to_string()),
                    ..Default::default()
                });
            }
            for e in &rules.enums {
                items.push(CompletionItem {
                    label: e.key.clone(),
                    kind: Some(CompletionItemKind::ENUM),
                    detail: Some(format!("Enum ({} values)", e.values.len())),
                    ..Default::default()
                });
            }
        }
        drop(ruleset);

        // Defined variables from info service
        let info = self.state.info_service.lock().unwrap();
        for var in &info.all_variables {
            items.push(CompletionItem {
                label: var.clone(),
                kind: Some(CompletionItemKind::CONSTANT),
                detail: Some("Variable".to_string()),
                ..Default::default()
            });
        }
        // Saved event targets
        for et in &info.all_event_targets {
            items.push(CompletionItem {
                label: format!("event_target:{}", et),
                kind: Some(CompletionItemKind::VARIABLE),
                detail: Some("Event target".to_string()),
                ..Default::default()
            });
        }
        // Top-level keys from all files
        for (uri, file_info) in &info.files {
            for (key, _loc) in &file_info.top_level_keys {
                items.push(CompletionItem {
                    label: key.clone(),
                    kind: Some(CompletionItemKind::KEYWORD),
                    detail: Some(format!("Key in {}", uri)),
                    ..Default::default()
                });
            }
        }

        if items.is_empty() {
            Ok(None)
        } else {
            Ok(Some(CompletionResponse::Array(items)))
        }
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let pos = params.text_document_position_params.position;
        let uri = params.text_document_position_params.text_document.uri.to_string();
        
        let docs = self.state.documents.lock().unwrap();
        if let Some(doc) = docs.get(&uri) {
            if let Some(ast) = &doc.ast {
                let source_pos = cwtools_parser::ast::SourcePos {
                    line: pos.line + 1,
                    col: pos.character as u16,
                };
                if let Some(element) = position::find_at_position(ast, &source_pos, &self.state.string_table) {
                    let symbol = match &element {
                        position::AstElement::Node { key, .. } => key.clone(),
                        position::AstElement::Leaf { key, .. } => key.clone(),
                        position::AstElement::LeafValue { value, .. } => value.clone(),
                    };
                    drop(docs); // release lock
                    let info = self.state.info_service.lock().unwrap();
                    if let Some(defs) = info.find_definitions(&symbol) {
                        let locations: Vec<Location> = defs.iter().map(|(file_uri, loc)| Location {
                            uri: file_uri.parse().unwrap_or_else(|_| params.text_document_position_params.text_document.uri.clone()),
                            range: Range {
                                start: Position { line: loc.line.saturating_sub(1), character: loc.col as u32 },
                                end: Position { line: loc.line.saturating_sub(1), character: (loc.col + symbol.len() as u16) as u32 },
                            },
                        }).collect();
                        if !locations.is_empty() {
                            return Ok(Some(GotoDefinitionResponse::Array(locations)));
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    async fn references(
        &self,
        params: ReferenceParams,
    ) -> Result<Option<Vec<Location>>> {
        let pos = params.text_document_position.position;
        let uri = params.text_document_position.text_document.uri.to_string();
        
        let docs = self.state.documents.lock().unwrap();
        if let Some(doc) = docs.get(&uri) {
            if let Some(ast) = &doc.ast {
                let source_pos = cwtools_parser::ast::SourcePos {
                    line: pos.line + 1,
                    col: pos.character as u16,
                };
                if let Some(element) = position::find_at_position(ast, &source_pos, &self.state.string_table) {
                    let symbol = match &element {
                        position::AstElement::Node { key, .. } => key.clone(),
                        position::AstElement::Leaf { key, .. } => key.clone(),
                        position::AstElement::LeafValue { value, .. } => value.clone(),
                    };
                    drop(docs); // release lock
                    let info = self.state.info_service.lock().unwrap();
                    let mut all_locs = Vec::new();
                    // Definitions
                    if let Some(defs) = info.find_definitions(&symbol) {
                        all_locs.extend(defs.iter().map(|(file_uri, loc)| Location {
                            uri: file_uri.parse().unwrap_or_else(|_| params.text_document_position.text_document.uri.clone()),
                            range: Range {
                                start: Position { line: loc.line.saturating_sub(1), character: loc.col as u32 },
                                end: Position { line: loc.line.saturating_sub(1), character: (loc.col + symbol.len() as u16) as u32 },
                            },
                        }));
                    }
                    // References from other files
                    if let Some(refs) = info.find_references(&symbol) {
                        all_locs.extend(refs.iter().map(|(file_uri, loc)| Location {
                            uri: file_uri.parse().unwrap_or_else(|_| params.text_document_position.text_document.uri.clone()),
                            range: Range {
                                start: Position { line: loc.line.saturating_sub(1), character: loc.col as u32 },
                                end: Position { line: loc.line.saturating_sub(1), character: (loc.col + symbol.len() as u16) as u32 },
                            },
                        }));
                    }
                    // Fallback: also include symbol index references from current document
                    let index = self.state.symbol_index.lock().unwrap();
                    if let Some(locs) = index.find_references(&symbol) {
                        all_locs.extend(locs.iter().map(|l| Location {
                            uri: l.uri.parse().unwrap_or_else(|_| params.text_document_position.text_document.uri.clone()),
                            range: Range {
                                start: Position { line: l.line.saturating_sub(1), character: l.col as u32 },
                                end: Position { line: l.line.saturating_sub(1), character: (l.col + symbol.len() as u16) as u32 },
                            },
                        }));
                    }
                    if !all_locs.is_empty() {
                        return Ok(Some(all_locs));
                    }
                }
            }
        }
        Ok(None)
    }

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<Value>> {
        match params.command.as_str() {
            "getFileTypes" => {
                if let Some(uri_val) = params.arguments.first() {
                    let uri = uri_val.as_str().unwrap_or("");
                    let types = self.determine_file_types(uri).await;
                    let arr: Vec<Value> = types.into_iter().map(Value::String).collect();
                    return Ok(Some(Value::Array(arr)));
                }
                Ok(Some(Value::Array(vec![])))
            }
            _ => Ok(None),
        }
    }
}

impl Backend {
    /// Scan the entire workspace for relevant game files and validate them all.
    /// This matches the F# server's startup behavior of pre-loading and validating
    /// every mod file, rather than waiting for the user to open each one.
    async fn validate_entire_workspace(&self) {
        // 1. Discover the workspace root
        let workspace_uri = {
            let guard = self.state.workspace_uri.lock().unwrap();
            guard.clone()
        };

        let root_path = match workspace_uri {
            Some(uri) => {
                // Strip file:// prefix if present
                let p = uri.strip_prefix("file://").unwrap_or(&uri);
                std::path::PathBuf::from(p)
            }
            None => {
                self.client
                    .log_message(MessageType::WARNING, "No workspace folder; skipping full-workspace validation.")
                    .await;
                return;
            }
        };

        // 2. Build the list of extensions to scan based on language
        let language = {
            let guard = self.state.language.lock().unwrap();
            guard.clone()
        };
        let extensions: Vec<&str> = match language.as_str() {
            "hoi4" => vec!["txt"],
            "stellaris" => vec!["txt"],
            "eu4" => vec!["txt"],
            "ck2" => vec!["txt"],
            "ck3" => vec!["txt"],
            "vic2" => vec!["txt"],
            "vic3" => vec!["txt"],
            "imperator" => vec!["txt"],
            "eu5" => vec!["txt"],
            _ => vec!["txt", "gfx", "gui"],
        };

        // 3. Walk the directory tree
        let mut files_to_validate = Vec::new();
        fn walk_dir(
            path: &std::path::Path,
            extensions: &[&str],
            out: &mut Vec<std::path::PathBuf>,
        ) {
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        // Skip common non-game directories
                        let name = path.file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("")
                            .to_lowercase();
                        let skip = matches!(
                            name.as_str(),
                            ".git" | "node_modules" | "out" | "dist" | "target" | "bin" | "obj"
                        );
                        if !skip {
                            walk_dir(&path, extensions, out);
                        }
                    } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                        if extensions.contains(&ext) {
                            out.push(path);
                        }
                    }
                }
            }
        }
        let ext_slice: &[&str] = &extensions;
        walk_dir(&root_path, ext_slice, &mut files_to_validate);

        if files_to_validate.is_empty() {
            self.client
                .log_message(MessageType::INFO, "No workspace files found to validate.")
                .await;
            return;
        }

        self.client
            .log_message(MessageType::INFO, format!(
                "Validating {} workspace files under {:?} ...",
                files_to_validate.len(),
                root_path
            ))
            .await;

        // 4. Validate each file
        let mut total_errors = 0usize;
        let mut total_files = 0usize;
        for file_path in &files_to_validate {
            let uri = format!("file://{}", file_path.display());
            let text = match std::fs::read_to_string(file_path) {
                Ok(t) => t,
                Err(e) => {
                    self.client
                        .log_message(MessageType::WARNING, format!(
                            "Could not read {}: {}",
                            file_path.display(),
                            e
                        ))
                        .await;
                    continue;
                }
            };

            let (diagnostics, parsed) = self.parse_and_validate(&uri, &text).await;
            total_errors += diagnostics.iter()
                .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
                .count();
            total_files += 1;

            // Store parsed result so future incremental changes can re-use it
            {
                let mut docs = self.state.documents.lock().unwrap();
                docs.insert(
                    uri.clone(),
                    ParsedDoc {
                        version: 0,
                        text: text.clone(),
                        ast: parsed,
                    },
                );
            }

            // Publish diagnostics for every file (not just open ones)
            if let Ok(uri_obj) = Url::parse(&uri) {
                self.client
                    .publish_diagnostics(uri_obj, diagnostics, None)
                    .await;
            }
        }

        self.client
            .log_message(MessageType::INFO, format!(
                "Workspace validation complete: {} errors across {} files",
                total_errors,
                total_files
            ))
            .await;
    }

    /// Parse and validate a single document.
    async fn parse_and_validate(
        &self,
        uri: &str,
        text: &str,
    ) -> (Vec<Diagnostic>, Option<ParsedFile>) {
        let mut diagnostics = Vec::new();

        self.client
            .log_message(MessageType::INFO, format!("[validate] parsing: {}", uri))
            .await;

        match parse_string(text, &self.state.string_table) {
            Ok(parsed) => {
                // Surface any syntax errors recorded by the parser (unclosed braces, etc.)
                for parse_err in &parsed.errors {
                    let diag = match parse_err {
                        ParseError::Pos(_file, line, col, msg) => Diagnostic {
                            range: Range {
                                start: Position {
                                    line: line.saturating_sub(1),
                                    character: *col as u32,
                                },
                                end: Position {
                                    line: line.saturating_sub(1),
                                    character: *col as u32 + 1,
                                },
                            },
                            severity: Some(DiagnosticSeverity::ERROR),
                            code: None,
                            code_description: None,
                            source: Some("cwtools".to_string()),
                            message: msg.clone(),
                            related_information: None,
                            tags: None,
                            data: None,
                        },
                        ParseError::General(msg) => Diagnostic {
                            range: Range {
                                start: Position::default(),
                                end: Position::default(),
                            },
                            severity: Some(DiagnosticSeverity::ERROR),
                            code: None,
                            code_description: None,
                            source: Some("cwtools".to_string()),
                            message: msg.clone(),
                            related_information: None,
                            tags: None,
                            data: None,
                        },
                    };
                    diagnostics.push(diag);
                }

                // Update symbol index
                {
                    let mut index = self.state.symbol_index.lock().unwrap();
                    index.clear_document(uri);
                    index.index_document(uri, &parsed, &self.state.string_table);
                }

                // Update info service (for definitions/references/completion)
                {
                    let ruleset_guard = self.state.ruleset.lock().unwrap();
                    let mut info = self.state.info_service.lock().unwrap();
                    info.clear_file(uri);
                    if let Some(ruleset) = ruleset_guard.as_ref() {
                        info.index_file(uri, &parsed, &self.state.string_table, ruleset);
                    }
                }

                // If we have rules loaded, run validation
                let (errors, log_msg) = {
                    let ruleset_guard = self.state.ruleset.lock().unwrap();
                    if let Some(ruleset) = ruleset_guard.as_ref() {
                        let language = self.state.language.lock().unwrap().clone();
                        let game = cwtools_game::constants::Game::from_str(&language);
                        let start = std::time::Instant::now();
                        let mut errs = validate_ast(&parsed, ruleset, &self.state.string_table, uri, game
                        );
                        let elapsed = start.elapsed();
                        const MAX_ERRORS: usize = 100;
                        let total = errs.len();
                        if total > MAX_ERRORS {
                            errs.truncate(MAX_ERRORS);
                            errs.push(cwtools_validation::ValidationError {
                                message: format!("... {} additional errors truncated", total - MAX_ERRORS),
                                severity: cwtools_validation::ErrorSeverity::Information,
                                line: 0,
                                col: 0,
                                file: uri.to_string(),
                            });
                        }
                        let msg = format!(
                            "[validate] {} errors in {:?} ({} types, {} enums, {} aliases)",
                            total, elapsed, ruleset.types.len(), ruleset.enums.len(), ruleset.aliases.len()
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

    /// Determine file types for a given URI.
    async fn determine_file_types(
        &self, uri: &str) -> Vec<String> {
        let path = uri.to_lowercase();
        let mut types = Vec::new();

        if path.contains("/events/") {
            types.push("event".to_string());
        }
        if path.contains("/common/") {
            types.push("script".to_string());
        }
        if path.contains("/common/scripted_effects") {
            types.push("scripted_effect".to_string());
        }
        if path.contains("/common/scripted_triggers") {
            types.push("scripted_trigger".to_string());
        }
        if path.ends_with(".txt") {
            types.push("txt".to_string());
        }

        types
    }
}

fn validation_error_to_diagnostic(err: &ValidationError) -> Diagnostic {
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
        code: None,
        code_description: None,
        source: Some("cwtools".to_string()),
        message: err.message.clone(),
        related_information: None,
        tags: None,
        data: None,
    }
}

fn main() {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let state = Arc::new(DocumentState::new());
            let (stdin, stdout) = (tokio::io::stdin(), tokio::io::stdout());
            let (service, socket) = LspService::new(|client| Backend {
                client,
                state: state.clone(),
            });
            Server::new(stdin, stdout, socket).serve(service).await;
        });
}
