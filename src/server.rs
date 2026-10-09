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
//! and a drain task ready to render what the modules publish next.

use std::sync::Arc;
use std::sync::Mutex;

use tokio::sync::mpsc::UnboundedReceiver;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{
    InitializeParams, InitializeResult, InitializedParams, ServerCapabilities,
    TextDocumentSyncCapability, TextDocumentSyncKind, Url,
};
use tower_lsp::LanguageServer;

use crate::hub::HubClient;
use crate::messages::{Message, Notification};

/// The LSP shell: the hub's first client.
pub struct JavaLanguageServer {
    /// The shell's client on the engine hub.
    hub: HubClient,
    /// The workspace root learned in `initialize`, published on the hub in
    /// `initialized` as [`Notification::FolderAdded`].
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
    pub fn new(hub: HubClient, mut incoming: UnboundedReceiver<Arc<Message>>) -> Self {
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
            // document module that consumes them returns.
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
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
            self.hub
                .notify(Message::Notification(Notification::FolderAdded { uri }));
        }
    }

    async fn shutdown(&self) -> Result<()> {
        tracing::info!("shutdown requested");
        Ok(())
    }
}
