//! The `Backend` struct — the integration point between `tower_lsp`
//! and our domain crates. Also owns the job queue and background
//! indexer task handles.

use std::sync::Arc;

use tfls_state::{JobQueue, StateStore};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tower_lsp::lsp_types::{
    CodeActionParams, CodeActionResponse, CodeLens, CodeLensParams, CompletionParams,
    CompletionResponse, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidChangeConfigurationParams, DidChangeWatchedFilesParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, DocumentFormattingParams, ExecuteCommandParams,
    DocumentHighlight, DocumentHighlightParams, DocumentLink, DocumentLinkParams,
    DocumentOnTypeFormattingParams, DocumentRangeFormattingParams, InlayHint, InlayHintParams,
    DocumentSymbolParams, DocumentSymbolResponse, FoldingRange, FoldingRangeParams,
    GotoDefinitionParams,
    GotoDefinitionResponse, Hover, HoverParams, InitializeParams, InitializeResult,
    InitializedParams, Location, MessageType, ReferenceParams, SemanticTokensParams,
    PrepareRenameResponse, RenameParams, SelectionRange, SelectionRangeParams,
    SemanticTokensRangeParams, SemanticTokensRangeResult,
    SemanticTokensResult, ServerInfo, SignatureHelp, SignatureHelpParams, SymbolInformation,
    TextDocumentPositionParams, TextEdit, WorkspaceEdit, WorkspaceSymbolParams,
    request::{GotoDeclarationParams, GotoDeclarationResponse},
};
use tower_lsp::{Client, LanguageServer, jsonrpc};

use crate::capabilities::server_capabilities;
use crate::handlers;
use crate::indexer;

pub struct Backend {
    pub client: Client,
    pub state: Arc<StateStore>,
    pub jobs: Arc<JobQueue>,
    /// Handles for spawned worker/watcher tasks, aborted on shutdown.
    tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            state: Arc::new(StateStore::new()),
            jobs: Arc::new(JobQueue::new()),
            tasks: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Construct a Backend sharing state with an existing one.
    /// Intended for tests that need an owned `Backend` without a
    /// running tower_lsp service.
    pub fn with_shared_state(client: Client, state: Arc<StateStore>, jobs: Arc<JobQueue>) -> Self {
        Self {
            client,
            state,
            jobs,
            tasks: Arc::new(Mutex::new(Vec::new())),
        }
    }

    async fn spawn_background(&self) {
        let worker = indexer::spawn_worker(Arc::clone(&self.state), Arc::clone(&self.jobs));
        let mut guard = self.tasks.lock().await;
        guard.push(worker);
    }

    async fn spawn_workspace_watcher(&self, root: std::path::PathBuf) {
        indexer::enqueue_workspace_scan(&self.state, &self.jobs, &root);
        indexer::enqueue_schema_fetch(&self.jobs, &root);
        // Functions are workspace-independent — fetch once per session.
        indexer::enqueue_functions_fetch(&self.jobs);
        match indexer::spawn_watcher(
            Arc::clone(&self.state),
            Arc::clone(&self.jobs),
            root.clone(),
        ) {
            Ok(handle) => {
                let mut guard = self.tasks.lock().await;
                guard.push(handle);
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    root = %root.display(),
                    "failed to start workspace watcher"
                );
            }
        }
    }

    async fn abort_tasks(&self) {
        let mut guard = self.tasks.lock().await;
        for h in guard.drain(..) {
            h.abort();
        }
    }

    /// Custom LSP extension: `terraform-ls/searchDocs` — free-text search
    /// across loaded provider schemas. Wired via `LspService::build`
    /// in `tfls-cli`.
    pub async fn search_docs(
        &self,
        params: handlers::search_docs::SearchDocsParams,
    ) -> jsonrpc::Result<handlers::search_docs::SearchDocsResult> {
        handlers::search_docs::search_docs(self, params).await
    }

    /// Custom LSP extension: `terraform-ls/getDoc` — full synthesised
    /// markdown for a resource or data source by name.
    pub async fn get_doc(
        &self,
        params: handlers::search_docs::GetDocParams,
    ) -> jsonrpc::Result<handlers::search_docs::GetDocResult> {
        handlers::search_docs::get_doc(self, params).await
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> jsonrpc::Result<InitializeResult> {
        self.spawn_background().await;

        for folder in params.workspace_folders.unwrap_or_default() {
            if let Ok(path) = folder.uri.to_file_path() {
                self.spawn_workspace_watcher(path).await;
            }
        }
        #[allow(deprecated)]
        if let Some(root_uri) = params.root_uri {
            if let Ok(path) = root_uri.to_file_path() {
                self.spawn_workspace_watcher(path).await;
            }
        }

        Ok(InitializeResult {
            capabilities: server_capabilities(),
            server_info: Some(ServerInfo {
                name: "terraform-ls-rs".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "terraform-ls-rs initialized")
            .await;
    }

    async fn shutdown(&self) -> jsonrpc::Result<()> {
        self.abort_tasks().await;
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        handlers::document::did_open(self, params).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        handlers::document::did_change(self, params).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        handlers::document::did_save(self, params).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        handlers::document::did_close(self, params).await;
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> jsonrpc::Result<Option<GotoDefinitionResponse>> {
        handlers::navigation::goto_definition(self, params).await
    }

    async fn references(
        &self,
        params: ReferenceParams,
    ) -> jsonrpc::Result<Option<Vec<Location>>> {
        handlers::navigation::references(self, params).await
    }

    async fn hover(&self, params: HoverParams) -> jsonrpc::Result<Option<Hover>> {
        handlers::navigation::hover(self, params).await
    }

    async fn completion(
        &self,
        params: CompletionParams,
    ) -> jsonrpc::Result<Option<CompletionResponse>> {
        handlers::completion::completion(self, params).await
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> jsonrpc::Result<Option<SemanticTokensResult>> {
        handlers::semantic_tokens::semantic_tokens_full(self, params).await
    }

    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> jsonrpc::Result<Option<SemanticTokensRangeResult>> {
        handlers::semantic_tokens::semantic_tokens_range(self, params).await
    }

    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> jsonrpc::Result<Option<Vec<TextEdit>>> {
        handlers::formatting::formatting(self, params).await
    }

    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> jsonrpc::Result<Option<Vec<TextEdit>>> {
        handlers::formatting::range_formatting(self, params).await
    }

    async fn on_type_formatting(
        &self,
        params: DocumentOnTypeFormattingParams,
    ) -> jsonrpc::Result<Option<Vec<TextEdit>>> {
        handlers::formatting::on_type_formatting(self, params).await
    }

    async fn document_link(
        &self,
        params: DocumentLinkParams,
    ) -> jsonrpc::Result<Option<Vec<DocumentLink>>> {
        handlers::document_link::document_link(self, params).await
    }

    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> jsonrpc::Result<Option<CodeActionResponse>> {
        handlers::code_action::code_action(self, params).await
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> jsonrpc::Result<Option<DocumentSymbolResponse>> {
        handlers::symbols::document_symbol(self, params).await
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> jsonrpc::Result<Option<Vec<SymbolInformation>>> {
        handlers::symbols::workspace_symbol(self, params).await
    }

    async fn goto_declaration(
        &self,
        params: GotoDeclarationParams,
    ) -> jsonrpc::Result<Option<GotoDeclarationResponse>> {
        handlers::navigation::goto_declaration(self, params).await
    }

    async fn code_lens(
        &self,
        params: CodeLensParams,
    ) -> jsonrpc::Result<Option<Vec<CodeLens>>> {
        handlers::code_lens::code_lens(self, params).await
    }

    async fn signature_help(
        &self,
        params: SignatureHelpParams,
    ) -> jsonrpc::Result<Option<SignatureHelp>> {
        handlers::signature_help::signature_help(self, params).await
    }

    async fn folding_range(
        &self,
        params: FoldingRangeParams,
    ) -> jsonrpc::Result<Option<Vec<FoldingRange>>> {
        handlers::folding::folding_range(self, params).await
    }

    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> jsonrpc::Result<Option<Vec<SelectionRange>>> {
        handlers::folding::selection_range(self, params).await
    }

    async fn inlay_hint(
        &self,
        params: InlayHintParams,
    ) -> jsonrpc::Result<Option<Vec<InlayHint>>> {
        handlers::inlay_hints::inlay_hint(self, params).await
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> jsonrpc::Result<Option<Vec<DocumentHighlight>>> {
        handlers::highlight::document_highlight(self, params).await
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> jsonrpc::Result<Option<PrepareRenameResponse>> {
        handlers::rename::prepare_rename(self, params).await
    }

    async fn rename(
        &self,
        params: RenameParams,
    ) -> jsonrpc::Result<Option<WorkspaceEdit>> {
        handlers::rename::rename(self, params).await
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        handlers::workspace::did_change_configuration(self, params).await
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        handlers::workspace::did_change_watched_files(self, params).await
    }

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> jsonrpc::Result<Option<serde_json::Value>> {
        handlers::commands::execute_command(self, params).await
    }
}
