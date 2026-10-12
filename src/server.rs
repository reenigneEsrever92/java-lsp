//! The LSP shell: the editor-facing [`LanguageServer`] and the hub's first
//! client.
//!
//! The shell is a hub client like any other ([`crate::hub`]): it notifies the
//! editor's input and renders the editor-facing notifications it receives on its
//! own subscription. The engine modules it talks to — the document module, the
//! index, the analysis core, the diagnostics and quick-fix subsystems, and the
//! warm-up drivers — are being reintroduced one at a time, so this is the
//! skeleton: the lifecycle handshake, incremental document sync advertised so
//! editors open and change documents, the workspace root published on the hub,
//! the navigation queries routed to the `navigate` module, and a drain task ready
//! to render what the modules publish next.

use std::sync::Arc;
use std::sync::Mutex;

use tokio::sync::mpsc::UnboundedReceiver;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::request::{
    GotoDeclarationParams, GotoDeclarationResponse, GotoImplementationParams,
    GotoImplementationResponse,
};
use tower_lsp::lsp_types::{
    DeclarationCapability, GotoDefinitionParams, GotoDefinitionResponse,
    ImplementationProviderCapability, InitializeParams, InitializeResult, InitializedParams,
    Location, OneOf, ReferenceParams, ServerCapabilities, TextDocumentPositionParams,
    TextDocumentSyncCapability, TextDocumentSyncKind, Url,
};
use tower_lsp::LanguageServer;

use crate::hub::HubClient;
use crate::messages::{NavigateRequest, NavigateResponse, Notification, Request, Response};

/// The LSP shell: the hub's first client.
pub struct JavaLanguageServer {
    /// The shell's client on the engine hub.
    hub: HubClient,
    /// The workspace root learned in `initialize`, published on the hub in
    /// `initialized` as [`Notification::ServerStarted`].
    workspace_root: Mutex<Option<Url>>,
}

impl std::fmt::Debug for JavaLanguageServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JavaLanguageServer").finish_non_exhaustive()
    }
}

impl JavaLanguageServer {
    /// Builds the shell on `hub` — the client `main` registered first — with the
    /// notification subscription `incoming` it took from that client.
    ///
    /// The shell drains `incoming` on its own task, so the hub never blocks on a
    /// slow client; the editor-facing notifications (diagnostics, progress,
    /// notices) are rendered here as the modules that publish them return.
    pub fn new(hub: HubClient, mut incoming: UnboundedReceiver<Arc<Notification>>) -> Self {
        tokio::spawn(async move {
            while incoming.recv().await.is_some() {
                // Nothing to render yet: the modules that publish the
                // editor-facing notifications are reintroduced one at a time.
            }
        });
        Self {
            hub,
            workspace_root: Mutex::new(None),
        }
    }

    /// Resolves the symbol at `position` in the requested document to its
    /// declaration by asking the `navigate` module; `None` when the cursor names
    /// nothing or is ambiguous. Shared by `goto_definition` and
    /// `goto_declaration`, which land on the same place because Java draws no
    /// declaration/definition split.
    async fn declaration_at(
        &self,
        position: TextDocumentPositionParams,
    ) -> Result<Option<Location>> {
        let response = self
            .hub
            .request(Request::Navigate(NavigateRequest::FindDeclaration {
                uri: position.text_document.uri,
                position: position.position,
            }))
            .await;
        match response {
            Response::Navigate(NavigateResponse::Declaration(location)) => Ok(location),
            _ => Ok(None),
        }
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
        match root {
            Some(root) => {
                tracing::info!(%root, "workspace root");
                if let Ok(mut slot) = self.workspace_root.lock() {
                    *slot = Some(root);
                }
            }
            // No root means no workspace scan: say so, so an empty index is
            // attributable to the handshake instead of a silent failure.
            None => tracing::warn!("no workspace root; workspace indexing is disabled"),
        }
        Ok(InitializeResult {
            // Incremental document sync, so editors open, change, and close
            // documents; the shell forwards those events onto the hub once the
            // document module that consumes them returns. Definition, declaration,
            // implementation, and references are advertised now: the `navigate`
            // module answers all four.
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                definition_provider: Some(OneOf::Left(true)),
                declaration_provider: Some(DeclarationCapability::Simple(true)),
                implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
                references_provider: Some(OneOf::Left(true)),
                ..ServerCapabilities::default()
            },
            ..InitializeResult::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        tracing::info!("java-lsp initialized");
        // The warm-up starts once the handshake completes: the shell publishes
        // the workspace root and the drivers take it from there.
        let root = self
            .workspace_root
            .lock()
            .ok()
            .and_then(|slot| slot.clone());

        if let Some(uri) = root {
            self.hub.notify(Notification::ServerStarted {
                workspace_root: uri,
            });
        }
    }

    async fn shutdown(&self) -> Result<()> {
        tracing::info!("shutdown requested");
        Ok(())
    }

    /// Answers `textDocument/definition` by asking the `navigate` module where
    /// the symbol under the cursor is declared; `None` when the cursor names
    /// nothing or is ambiguous.
    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        Ok(self
            .declaration_at(params.text_document_position_params)
            .await?
            .map(GotoDefinitionResponse::Scalar))
    }

    /// Answers `textDocument/declaration` with the same resolution as
    /// `textDocument/definition`: Java draws no declaration/definition split, so
    /// both requests land on the declaration the cursor names.
    async fn goto_declaration(
        &self,
        params: GotoDeclarationParams,
    ) -> Result<Option<GotoDeclarationResponse>> {
        Ok(self
            .declaration_at(params.text_document_position_params)
            .await?
            .map(GotoDeclarationResponse::Scalar))
    }

    /// Answers `textDocument/references` by asking the `navigate` module for
    /// every occurrence of the symbol under the cursor across the workspace
    /// sources.
    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let TextDocumentPositionParams {
            text_document,
            position,
        } = params.text_document_position;
        let response = self
            .hub
            .request(Request::Navigate(NavigateRequest::FindReferences {
                uri: text_document.uri,
                position,
                include_declaration: params.context.include_declaration,
            }))
            .await;
        match response {
            Response::Navigate(NavigateResponse::References(locations)) => Ok(Some(locations)),
            _ => Ok(Some(Vec::new())),
        }
    }

    /// Answers `textDocument/implementation` by asking the `navigate` module for
    /// the workspace declarations implementing the contract under the cursor;
    /// `None` when the cursor names no contract or nothing implements it.
    async fn goto_implementation(
        &self,
        params: GotoImplementationParams,
    ) -> Result<Option<GotoImplementationResponse>> {
        let TextDocumentPositionParams {
            text_document,
            position,
        } = params.text_document_position_params;
        let response = self
            .hub
            .request(Request::Navigate(NavigateRequest::FindImplementation {
                uri: text_document.uri,
                position,
            }))
            .await;
        match response {
            Response::Navigate(NavigateResponse::Implementations(locations))
                if !locations.is_empty() =>
            {
                Ok(Some(GotoImplementationResponse::Array(locations)))
            }
            _ => Ok(None),
        }
    }
}
