use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use serde_json::Value;

#[derive(Debug)]
struct Backend {
    client: Client,
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self,
        params: InitializeParams,
    ) -> Result<InitializeResult> {
        // Extract initialization options from VS Code extension
        let _opts = params.initialization_options;

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        ..Default::default()
                    },
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec!["=".to_string(), "<".to_string()]),
                    ..Default::default()
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

    // Text document sync
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        self.client
            .log_message(MessageType::INFO, format!("Opened: {}", uri))
            .await;
    }

    async fn did_change(&self, _params: DidChangeTextDocumentParams) {
        // TODO: re-parse and validate
    }

    async fn did_close(&self, _params: DidCloseTextDocumentParams) {
        // TODO: cleanup
    }

    // Hover
    async fn hover(&self,
        _params: HoverParams,
    ) -> Result<Option<Hover>> {
        Ok(None)
    }

    // Completion
    async fn completion(&self,
        _params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        Ok(None)
    }

    // Definition
    async fn goto_definition(&self,
        _params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        Ok(None)
    }

    // References
    async fn references(&self,
        _params: ReferenceParams,
    ) -> Result<Option<Vec<Location>>> {
        Ok(None)
    }

    // Execute command
    async fn execute_command(&self,
        params: ExecuteCommandParams,
    ) -> Result<Option<Value>> {
        match params.command.as_str() {
            "getFileTypes" => {
                // TODO: return file type mappings
                Ok(Some(Value::Array(vec![])))
            }
            _ => Ok(None),
        }
    }
}

fn main() {
    // Initialize tokio runtime
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let (stdin, stdout) = (tokio::io::stdin(), tokio::io::stdout());
            let (service, socket) = LspService::new(|client| Backend { client });
            Server::new(stdin, stdout, socket).serve(service).await;
        });
}
