use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use serde_json::Value;

use cwtools_parser::parser::parse_string;
use cwtools_parser::ast::ParsedFile;
use cwtools_rules::rules_types::RuleSet;
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
        let ruleset = self.state.ruleset.lock().unwrap();
        if let Some(rules) = ruleset.as_ref() {
            let mut items = Vec::new();
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
            return Ok(Some(CompletionResponse::Array(items)));
        }
        Ok(None)
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
                    let index = self.state.symbol_index.lock().unwrap();
                    if let Some(locs) = index.find_definitions(&symbol) {
                        let locations: Vec<Location> = locs.iter().map(|l| Location {
                            uri: l.uri.parse().unwrap_or_else(|_| params.text_document_position_params.text_document.uri.clone()),
                            range: Range {
                                start: Position { line: l.line.saturating_sub(1), character: l.col as u32 },
                                end: Position { line: l.line.saturating_sub(1), character: (l.col + symbol.len() as u16) as u32 },
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
                    let index = self.state.symbol_index.lock().unwrap();
                    let mut all_locs = Vec::new();
                    if let Some(defs) = index.find_definitions(&symbol) {
                        all_locs.extend(defs.iter().map(|l| Location {
                            uri: l.uri.parse().unwrap_or_else(|_| params.text_document_position.text_document.uri.clone()),
                            range: Range {
                                start: Position { line: l.line.saturating_sub(1), character: l.col as u32 },
                                end: Position { line: l.line.saturating_sub(1), character: (l.col + symbol.len() as u16) as u32 },
                            },
                        }));
                    }
                    if let Some(refs) = index.find_references(&symbol) {
                        all_locs.extend(refs.iter().map(|l| Location {
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
    /// Parse and validate a single document.
    async fn parse_and_validate(
        &self,
        uri: &str,
        text: &str,
    ) -> (Vec<Diagnostic>, Option<ParsedFile>) {
        let mut diagnostics = Vec::new();

        match parse_string(text, &self.state.string_table) {
            Ok(parsed) => {
                // Update symbol index
                {
                    let mut index = self.state.symbol_index.lock().unwrap();
                    index.clear_document(uri);
                    index.index_document(uri, &parsed, &self.state.string_table);
                }

                // If we have rules loaded, run validation
                let ruleset_guard = self.state.ruleset.lock().unwrap();
                if let Some(ruleset) = ruleset_guard.as_ref() {
                    let errors = validate_ast(&parsed, ruleset, &self.state.string_table);
                    for err in errors {
                        diagnostics.push(validation_error_to_diagnostic(&err));
                    }
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
