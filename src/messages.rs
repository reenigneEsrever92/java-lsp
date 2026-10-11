use std::sync::Arc;

use tower_lsp::lsp_types::{TextDocumentContentChangeEvent, Url};

use crate::index::SymbolEntry;

#[derive(Debug, Clone)]
pub struct Envelope {
    pub id: u64,
    pub request_id: Option<u64>,
    pub sender: &'static str,
    pub message: Arc<Message>,
}

#[derive(Debug)]
pub enum Message {
    Request(Request),
    Response(Response),
    Notification(Arc<Notification>),
}

/// A request routed by the hub to the one module that answers it. The owner
/// answers by calling [`crate::hub::HubClient::respond`] with the request's id;
/// the hub sees that reply, logs it, and routes it back to the requester waiting
/// on its own [`crate::hub::Reply`].
#[derive(Debug)]
pub enum Request {
    Index(IndexRequest),
}

/// A read from the index.
#[derive(Debug)]
pub enum IndexRequest {
    /// All entries declared with exactly `name`.
    Name { name: String },
}

#[derive(Debug, Clone)]
pub enum Response {
    Index(IndexResponse),
}

/// The empty reply, so a request whose owner is gone resolves to it rather than
/// leaving the requester waiting.
impl Default for Response {
    fn default() -> Self {
        Response::Index(IndexResponse::default())
    }
}

/// The index's reply.
#[derive(Debug, Clone)]
pub enum IndexResponse {
    /// The entries for an [`IndexRequest::Name`].
    Symbols(Vec<Arc<SymbolEntry>>),
}

impl Default for IndexResponse {
    fn default() -> Self {
        IndexResponse::Symbols(Vec::new())
    }
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
