use std::sync::Arc;

use tower_lsp::lsp_types::Url;

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

#[derive(Debug)]
pub enum Notification {
    FolderAdded { uri: Url },
}
