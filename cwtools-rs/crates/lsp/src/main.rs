use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use serde_json::Value;

use cwtools_parser::parser::parse_string;
use cwtools_string_table::string_table::StringTable;

/// Server state: parsed documents.
struct DocumentState {
    /// file URI -> parsed result
    documents: Mutex<HashMap<String, ParsedDoc>>,
    /// shared string table
    string_table: StringTable,
}

#[derive(Debug)]
struct ParsedDoc {
    version: i32,
    text: String,
    // TODO: store parsed AST
}

impl DocumentState {
    fn new() -> Self {
        Self {
            documents: Mutex::new(HashMap::new()),
            string_table: StringTable::new(),
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
        // Log initialization options for debugging
        if let Some(opts) = &params.initialization_options {
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
    async fn did_open(&self, params: DidOpenTextDocumentParams,
    ) {
        let uri = params.text_document.uri.to_string();
        let text = params.text_document.text;
        let version = params.text_document.version;

        // Parse the document
        let diagnostics = self.parse_and_validate(&uri, &text).await;

        // Store in state
        {
            let mut docs = self.state.documents.lock().unwrap();
            docs.insert(
                uri.clone(),
                ParsedDoc {
                    version,
                    text: text.clone(),
                },
            );
        }

        // Publish diagnostics
        self.client
            .publish_diagnostics(params.text_document.uri, diagnostics, Some(version))
            .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams,
    ) {
        let uri = params.text_document.uri.to_string();
        let version = params.text_document.version;

        // With Full sync, we get the entire new document
        if let Some(change) = params.content_changes.into_iter().next() {
            let text = change.text;

            let diagnostics = self.parse_and_validate(&uri, &text).await;

            // Update state
            {
                let mut docs = self.state.documents.lock().unwrap();
                docs.insert(
                    uri.clone(),
                    ParsedDoc {
                        version,
                        text: text.clone(),
                    },
                );
            }

            self.client
                .publish_diagnostics(params.text_document.uri, diagnostics, Some(version))
                .await;
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams,
    ) {
        let uri = params.text_document.uri.to_string();
        // Re-validate on save
        if let Some(doc) = {
            let docs = self.state.documents.lock().unwrap();
            docs.get(&uri).map(|d| d.text.clone())
        } {
            let diagnostics = self.parse_and_validate(&uri, &doc).await;
            self.client
                .publish_diagnostics(params.text_document.uri, diagnostics, None)
                .await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams,
    ) {
        let uri = params.text_document.uri.to_string();
        {
            let mut docs = self.state.documents.lock().unwrap();
            docs.remove(&uri);
        }
        // Clear diagnostics
        self.client
            .publish_diagnostics(params.text_document.uri, vec![], None)
            .await;
    }

    // --- Language features (stubs) ---
    async fn hover(&self,
        _params: HoverParams,
    ) -> Result<Option<Hover>> {
        Ok(None)
    }

    async fn completion(&self,
        _params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        Ok(None)
    }

    async fn goto_definition(&self,
        _params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        Ok(None)
    }

    async fn references(&self,
        _params: ReferenceParams,
    ) -> Result<Option<Vec<Location>>> {
        Ok(None)
    }

    async fn execute_command(&self,
        params: ExecuteCommandParams,
    ) -> Result<Option<Value>> {
        match params.command.as_str() {
            "getFileTypes" => {
                // Return a simple mapping for now
                let mut map = serde_json::Map::new();
                map.insert("txt".to_string(), Value::String("script".to_string()));
                Ok(Some(Value::Object(map)))
            }
            _ => Ok(None),
        }
    }
}

impl Backend {
    /// Parse a document and return any syntax diagnostics.
    async fn parse_and_validate(&self,
        _uri: &str,
        text: &str,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        match parse_string(text, &self.state.string_table) {
            Ok(_parsed) => {
                // TODO: run rule validation and produce diagnostics
            }
            Err(e) => {
                // Convert parse error to diagnostic
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
            }
        }

        diagnostics
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
