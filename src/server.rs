//! The LSP shell: editor-facing handlers that delegate all analysis to the
//! `SemanticEngine` behind the trait seam.

use std::sync::{Arc, Mutex};

use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{
    CompletionOptions, CompletionParams, CompletionResponse, Diagnostic,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DocumentSymbolParams, DocumentSymbolResponse, FoldingRange, FoldingRangeParams,
    FoldingRangeProviderCapability, GotoDefinitionParams, GotoDefinitionResponse, Hover,
    HoverParams, HoverProviderCapability, InitializeParams, InitializeResult, InitializedParams,
    OneOf, SemanticTokenModifier, SemanticTokensFullOptions, SemanticTokensLegend,
    SemanticTokensOptions, SemanticTokensParams, SemanticTokensResult,
    SemanticTokensServerCapabilities, ServerCapabilities, SymbolInformation,
    TextDocumentSyncCapability, TextDocumentSyncKind, Url, WorkDoneProgressOptions,
    WorkspaceSymbolParams,
};
use tower_lsp::{Client, LanguageServer};

use crate::document::DocumentStore;
use crate::engine::{SemanticEngine, TreeSitterEngine};

/// The legend for the semantic tokens the engine emits; keep in sync with
/// `engine::syntax::SEMANTIC_TOKEN_TYPES`.
fn semantic_token_legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: crate::engine::syntax::SEMANTIC_TOKEN_TYPES.to_vec(),
        token_modifiers: Vec::<SemanticTokenModifier>::new(),
    }
}

pub struct JavaLanguageServer {
    client: Client,
    documents: Arc<RwLock<DocumentStore>>,
    engine: Arc<RwLock<Box<dyn SemanticEngine>>>,
    workspace_root: Mutex<Option<Url>>,
}

impl std::fmt::Debug for JavaLanguageServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JavaLanguageServer").finish_non_exhaustive()
    }
}

impl JavaLanguageServer {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: Arc::new(RwLock::new(DocumentStore::default())),
            engine: Arc::new(RwLock::new(Box::new(TreeSitterEngine::new()))),
            workspace_root: Mutex::new(None),
        }
    }

    /// The shell's document store, shared with tests (and later, other tasks).
    pub fn documents(&self) -> Arc<RwLock<DocumentStore>> {
        Arc::clone(&self.documents)
    }

    /// The engine behind the seam, shared with tests (and later, other tasks).
    pub fn engine(&self) -> Arc<RwLock<Box<dyn SemanticEngine>>> {
        Arc::clone(&self.engine)
    }

    async fn publish_engine_diagnostics(&self, uri: Url, version: Option<i32>) {
        let diagnostics: Vec<Diagnostic> = {
            let engine = self.engine.read().await;
            engine.diagnostics(&uri)
        };
        self.client
            .publish_diagnostics(uri, diagnostics, version)
            .await;
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for JavaLanguageServer {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        tracing::info!("initializing java-lsp");
        let root = params.root_uri.or_else(|| {
            params
                .workspace_folders
                .as_ref()
                .and_then(|folders| folders.first().map(|folder| folder.uri.clone()))
        });
        if let Some(root) = root {
            tracing::info!(root = %root, "workspace root");
            if let Ok(mut slot) = self.workspace_root.lock() {
                *slot = Some(root);
            }
        }
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_string()]),
                    ..CompletionOptions::default()
                }),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                semantic_tokens_provider: Some(SemanticTokensServerCapabilities::from(
                    SemanticTokensOptions {
                        work_done_progress_options: WorkDoneProgressOptions::default(),
                        legend: semantic_token_legend(),
                        range: Some(false),
                        full: Some(SemanticTokensFullOptions::Bool(true)),
                    },
                )),
                ..ServerCapabilities::default()
            },
            ..InitializeResult::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        tracing::info!("java-lsp initialized");
        // The background workspace scan starts once the handshake completes;
        // it never blocks text sync or request handling (R6).
        let root = self
            .workspace_root
            .lock()
            .ok()
            .and_then(|slot| slot.clone());
        if let Some(root) = root {
            let engine = self.engine.read().await;
            engine.set_workspace_root(&root);
        }
    }

    async fn shutdown(&self) -> Result<()> {
        tracing::info!("shutdown requested");
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let version = params.text_document.version;
        let text = params.text_document.text;
        tracing::info!(uri = %uri, version, "didOpen");
        {
            let mut docs = self.documents.write().await;
            docs.open(uri.clone(), version, &text);
        }
        {
            let engine = self.engine.read().await;
            engine.open(&uri, &text);
        }
        self.publish_engine_diagnostics(uri, Some(version)).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let version = params.text_document.version;
        tracing::debug!(uri = %uri, version, changes = params.content_changes.len(), "didChange");
        let text = {
            let mut docs = self.documents.write().await;
            if !docs.change(&uri, version, &params.content_changes) {
                tracing::warn!(uri = %uri, "didChange for unopened document");
                return;
            }
            docs.get(&uri)
                .map(|doc| String::from_utf8_lossy(&doc.bytes).into_owned())
        };
        let Some(text) = text else { return };
        {
            let engine = self.engine.read().await;
            engine.change(&uri, &text);
        }
        self.publish_engine_diagnostics(uri, Some(version)).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        tracing::info!(uri = %uri, "didClose");
        {
            let mut docs = self.documents.write().await;
            docs.close(&uri);
        }
        {
            let engine = self.engine.read().await;
            engine.close(&uri);
        }
        // The document is gone; clear any published diagnostics.
        self.client.publish_diagnostics(uri, Vec::new(), None).await;
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let hover = {
            let engine = self.engine.read().await;
            engine.hover(&uri, position)
        };
        Ok(hover)
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let definition = {
            let engine = self.engine.read().await;
            engine.definition(&uri, position)
        };
        Ok(definition.map(GotoDefinitionResponse::Scalar))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let completions = {
            let engine = self.engine.read().await;
            engine.completions(&uri, position)
        };
        Ok(completions)
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri;
        let symbols = {
            let engine = self.engine.read().await;
            engine.document_symbols(&uri)
        };
        Ok(symbols.map(DocumentSymbolResponse::Nested))
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let symbols = {
            let engine = self.engine.read().await;
            engine.workspace_symbols(&params.query)
        };
        Ok(Some(symbols))
    }

    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        let uri = params.text_document.uri;
        let ranges = {
            let engine = self.engine.read().await;
            engine.folding_ranges(&uri)
        };
        Ok(ranges)
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri;
        let tokens = {
            let engine = self.engine.read().await;
            engine.semantic_tokens(&uri)
        };
        Ok(tokens.map(SemanticTokensResult::Tokens))
    }
}
