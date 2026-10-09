use std::sync::Arc;

use tower_lsp::lsp_types::{TextDocumentContentChangeEvent, Url};

// The `index` module is out while the engine modules are reintroduced one by
// one; restore this with it.
// use crate::index::SymbolEntry;

#[derive(Debug, Clone)]
pub struct Envelope {
    pub id: u64,
    pub message: Arc<Message>,
}

#[derive(Debug)]
pub enum Message {
    Request(Request),
    Response(Response),
    Notification(Notification),
}

/// A request routed by the hub to the one module that answers it. The owner
/// answers with the [`ReplyHandle`] it carries; the hub sees that reply, logs and
/// times it, and delivers the value to the requester waiting on its own channel.
#[derive(Debug)]
pub enum Request {
    IndexQuery { name: String },
}

#[derive(Debug)]
pub enum Response {
    // `IndexResponse { symbols: SymbolEntry }` — restored with the `index` module.
}

/// A notification the hub broadcasts to every subscriber; each consumes the
/// ones it cares about. Large payloads can be retained cheaply by cloning their
/// shared `Arc`s rather than the value behind them.
#[derive(Debug)]
pub enum Notification {
    /// The workspace root (or a later folder) is known.
    FolderAdded { uri: Url },
    /// The editor opened a document: its full text and version.
    DocumentOpened {
        uri: Url,
        version: i32,
        text: Arc<String>,
    },
    /// The editor changed a document: the incremental changes to apply, in
    /// order, and the new version. The owner of the document's text applies
    /// them.
    DocumentChanged {
        uri: Url,
        version: i32,
        changes: Vec<TextDocumentContentChangeEvent>,
    },
    /// The editor closed a document.
    DocumentClosed { uri: Url },
}
