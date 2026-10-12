use std::sync::Arc;

use tower_lsp::lsp_types::{Location, Position, TextDocumentContentChangeEvent, Url};

use crate::index::types::{Member, SymbolEntry, Ty};

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
/// on its own [`crate::hub::Reply`]. The vocabulary splits by owner: navigation's
/// queries go to [`Request::Navigate`] and the index's coarse reads to
/// [`Request::Index`] and [`Request::IndexBatch`].
#[derive(Debug)]
pub enum Request {
    Navigate(NavigateRequest),
    Index(IndexRequest),
    IndexBatch(IndexBatchRequest),
}

/// A navigation query the shell routes to the `navigate` module. The intent is
/// the symbol under the cursor, not a resolved name: navigation alone knows which
/// symbol a position names, resolving it over the index it reads through the hub.
#[derive(Debug, Clone)]
pub enum NavigateRequest {
    /// The declaration the symbol at `position` in `uri` names.
    FindDeclaration { uri: Url, position: Position },
    /// Every occurrence of the symbol at `position` in `uri` across the
    /// workspace sources; `include_declaration` also reports the declaration
    /// sites.
    FindReferences {
        uri: Url,
        position: Position,
        include_declaration: bool,
    },
    /// The workspace declarations implementing the contract at `position` in
    /// `uri`: the subtypes of a type, or the overrides of a member.
    FindImplementation { uri: Url, position: Position },
}

/// The `navigate` module's reply. The requester knows which request it sent, so
/// it reads the variant that request expects and treats any other — including the
/// empty default — as no answer.
#[derive(Debug, Clone)]
pub enum NavigateResponse {
    /// The answer to a [`NavigateRequest::FindDeclaration`].
    Declaration(Option<Location>),
    /// The answer to a [`NavigateRequest::FindReferences`].
    References(Vec<Location>),
    /// The answer to a [`NavigateRequest::FindImplementation`].
    Implementations(Vec<Location>),
}

impl Default for NavigateResponse {
    fn default() -> Self {
        NavigateResponse::Declaration(None)
    }
}

/// The member of a contract whose implementations a query is about: the simple
/// name, and — when the overload is pinned — its parameter types, so an override
/// is matched by name and parameter types rather than by name alone.
#[derive(Debug, Clone)]
pub struct MemberSelector {
    pub name: String,
    pub params: Option<Vec<Ty>>,
}

/// A single coarse, navigation-shaped read of the index. Navigation never needs
/// a whole declared-type model, so each read answers with a light, owned value:
/// the model type itself never crosses the hub.
#[derive(Debug, Clone)]
pub enum IndexRequest {
    /// Every entry declared with this simple name.
    EntriesByName { name: String },
    /// One file's entries, or `None` when the index holds no such file.
    FileSymbols { uri: Url },
    /// Every workspace source file the index holds.
    WorkspaceFiles,
    /// The `(simple name, package)` identities of a type and its supertype
    /// closure.
    TypeClosure { ty: Ty, package: Option<String> },
    /// A type's members, inherited ones included.
    MembersOf { ty: Ty, package: Option<String> },
    /// Whether a type name resolves.
    ResolvesType { ty: Ty, package: Option<String> },
    /// The workspace type entries whose supertype closure reaches the contract
    /// `ty`; or, when `member` names a member, the entries of those subtypes'
    /// own declarations of it.
    ImplementationsOf {
        contract: Ty,
        package: Option<String>,
        member: Option<MemberSelector>,
    },
}

/// Several reads in one round-trip, so a resolution that knows its reads in
/// advance fetches them as a set rather than one request at a time.
#[derive(Debug, Clone)]
pub struct IndexBatchRequest {
    pub requests: Vec<IndexRequest>,
}

/// The index's reply to a single read. The requester reads the variant its
/// request expects.
#[derive(Debug, Clone)]
pub enum IndexResponse {
    /// The answer to an [`IndexRequest::EntriesByName`].
    Entries(Vec<Arc<SymbolEntry>>),
    /// The answer to an [`IndexRequest::FileSymbols`].
    File(Option<Vec<Arc<SymbolEntry>>>),
    /// The answer to an [`IndexRequest::WorkspaceFiles`].
    Files(Vec<Url>),
    /// The answer to an [`IndexRequest::TypeClosure`]: `(simple name, package)`
    /// identities.
    Closure(Vec<(String, Option<String>)>),
    /// The answer to an [`IndexRequest::MembersOf`].
    Members(Vec<Member>),
    /// The answer to an [`IndexRequest::ResolvesType`].
    Resolves(bool),
}

/// The index's reply to a batch: one [`IndexResponse`] per request, in order.
#[derive(Debug, Clone)]
pub struct IndexBatchResponse(pub Vec<IndexResponse>);

/// A module's reply. The requester knows which request it sent, so it reads the
/// variant that request expects; [`Response::Empty`] — the default — is the
/// answer a request with no serving owner resolves to, which no requester reads
/// as an answer.
#[derive(Debug, Clone)]
pub enum Response {
    Navigate(NavigateResponse),
    Index(IndexResponse),
    IndexBatch(IndexBatchResponse),
    Empty,
}

/// The empty reply, so a request whose owner is gone resolves to it rather than
/// leaving the requester waiting.
impl Default for Response {
    fn default() -> Self {
        Response::Empty
    }
}

/// A notification the hub broadcasts to every subscriber; each consumes the
/// ones it cares about. Large payloads can be retained cheaply by cloning their
/// shared `Arc`s rather than the value behind them.
#[derive(Debug)]
pub enum Notification {
    /// Lsp server started in folder "workspace_root"
    ServerStarted { workspace_root: Url },
    /// Folders discovered that should be scanned
    FoldersDiscoverred { uri: Vec<Url> },
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
