use std::sync::Arc;

use tower_lsp::lsp_types::{
    CompletionItem, Location, Position, SignatureHelp, SymbolInformation,
    TextDocumentContentChangeEvent, Url,
};

use crate::index::SymbolEntry;
use crate::model::{Member, Ty};

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
/// queries go to [`Request::Navigate`], completion's to [`Request::Completion`],
/// the open buffers' text to [`Request::Buffers`], workspace symbol searches to
/// [`Request::Symbols`], and the index's coarse reads to [`Request::Index`] and
/// [`Request::IndexBatch`].
#[derive(Debug)]
pub enum Request {
    Navigate(NavigateRequest),
    Completion(CompletionRequest),
    Buffers(BuffersRequest),
    Symbols(SymbolRequest),
    Index(IndexRequest),
    IndexBatch(IndexBatchRequest),
}

/// A completion query the shell routes to the `completion` module: what to offer
/// at `position` in `uri`. The module alone knows the word being typed and the
/// names in scope, resolving the rest over the index it reads through the hub.
#[derive(Debug, Clone)]
pub enum CompletionRequest {
    /// The completions offerable at `position` in `uri`.
    Complete { uri: Url, position: Position },
    /// The signature help for the call the cursor at `position` in `uri` sits
    /// in: the callee's overloads and the active parameter, or the created
    /// type's constructor overloads for a `new T(…)`.
    SignatureHelp { uri: Url, position: Position },
}

/// The `completion` module's reply. The requester knows which request it sent,
/// so it reads the variant that request expects.
#[derive(Debug, Clone)]
pub enum CompletionResponse {
    /// The answer to a [`CompletionRequest::Complete`]: the items to offer.
    Items(Vec<CompletionItem>),
    /// The answer to a [`CompletionRequest::SignatureHelp`]: the call's
    /// overloads and active parameter, or `None` when the callee or its
    /// receiver cannot be resolved.
    SignatureHelp(Option<SignatureHelp>),
}

impl Default for CompletionResponse {
    fn default() -> Self {
        CompletionResponse::Items(Vec::new())
    }
}

/// A read of the open buffers' text, which the shell routes to the `buffers`
/// module — the one owner of the editor's unsaved text. A consumer that needs a
/// file's current text reads it here rather than from disk, so an unsaved edit
/// is visible.
#[derive(Debug, Clone)]
pub enum BuffersRequest {
    /// The current text of the open buffer `uri`, or `None` when it is not open.
    Text { uri: Url },
}

/// The `buffers` module's reply. The requester knows which request it sent, so it
/// reads the variant that request expects.
#[derive(Debug, Clone)]
pub enum BuffersResponse {
    /// The answer to a [`BuffersRequest::Text`]: the buffer's text, or `None`
    /// when the document is not open.
    Text(Option<Arc<String>>),
}

impl Default for BuffersResponse {
    fn default() -> Self {
        BuffersResponse::Text(None)
    }
}

/// A workspace symbol search the shell routes to the `workspace` module: the
/// workspace symbols whose simple name matches a query, for the editor's
/// `workspace/symbol`.
#[derive(Debug, Clone)]
pub enum SymbolRequest {
    /// Every workspace symbol whose simple name starts with `query`; an empty
    /// query matches every symbol.
    Search { query: String },
}

/// The `workspace` module's reply. The requester knows which request it sent, so
/// it reads the variant that request expects.
#[derive(Debug, Clone)]
pub enum SymbolResponse {
    /// The answer to a [`SymbolRequest::Search`].
    Symbols(Vec<SymbolInformation>),
}

impl Default for SymbolResponse {
    fn default() -> Self {
        SymbolResponse::Symbols(Vec::new())
    }
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
    /// Every entry whose simple name starts with this prefix, in name order.
    EntriesByPrefix { prefix: String },
    /// One file's entries, or `None` when the index holds no such file.
    FileSymbols { uri: Url },
    /// Every workspace source file the index holds.
    WorkspaceFiles,
    /// The `(simple name, package)` identities of a type and its supertype
    /// closure.
    TypeClosure { ty: Ty, package: Option<String> },
    /// A type's members, inherited ones included, with same-named overloads kept
    /// separate.
    MembersOf { ty: Ty, package: Option<String> },
    /// A type's constructor overloads, the implicit no-arg one included.
    ConstructorsOf { ty: Ty, package: Option<String> },
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
    /// The answer to an [`IndexRequest::ConstructorsOf`].
    Constructors(Vec<Member>),
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
    Completion(CompletionResponse),
    Buffers(BuffersResponse),
    Symbols(SymbolResponse),
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

/// Where the folders in a [`Notification::FoldersDiscoverred`] batch come from;
/// the index maps each variant onto its own [`crate::index::FileOrigin`] so
/// scanned files are tagged accordingly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderOrigin {
    /// The workspace's own source roots: navigable, searchable, renameable.
    Workspace,
    /// Extracted dependency sources in the java-lsp cache: completable and
    /// openable, but never a references/rename candidate.
    LibrarySources,
}

/// A notification the hub broadcasts to every subscriber; each consumes the
/// ones it cares about. Large payloads can be retained cheaply by cloning their
/// shared `Arc`s rather than the value behind them.
#[derive(Debug)]
pub enum Notification {
    /// Lsp server started in folder "workspace_root"
    ServerStarted { workspace_root: Url },
    /// Folders discovered that should be scanned, with the origin the index
    /// should tag their files with.
    FoldersDiscoverred { uri: Vec<Url>, origin: FolderOrigin },
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

/// The compact shapes the hub's per-message log line renders. `Debug` keeps the
/// full payloads (test-failure output and ad-hoc debugging stay verbatim); the
/// summaries are what a running server prints — variant, `uri`, counts, and
/// text payloads as byte lengths, never the bodies themselves.
impl Envelope {
    /// One line for the hub's log: the envelope's id and sender, the request id
    /// a reply answers, and the message's shape.
    pub fn summary(&self) -> String {
        format!(
            "id={} sender={} request_id={:?} {}",
            self.id,
            self.sender,
            self.request_id,
            self.message.summary()
        )
    }
}

impl Message {
    /// The message's kind and shape for the hub's log.
    pub fn summary(&self) -> String {
        match self {
            Message::Request(request) => format!("request {}", request.summary()),
            Message::Response(response) => format!("response {}", response.summary()),
            Message::Notification(notification) => {
                format!("notification {}", notification.summary())
            }
        }
    }
}

impl Request {
    /// The request's shape for the hub's log: family, variant, and the fields
    /// that identify it — `uri`, `position`, name, counts — without payload
    /// bodies.
    pub fn summary(&self) -> String {
        match self {
            Request::Navigate(request) => match request {
                NavigateRequest::FindDeclaration { uri, position } => format!(
                    "Navigate(FindDeclaration uri={uri} position={}:{})",
                    position.line, position.character
                ),
                NavigateRequest::FindReferences { uri, position, .. } => format!(
                    "Navigate(FindReferences uri={uri} position={}:{})",
                    position.line, position.character
                ),
                NavigateRequest::FindImplementation { uri, position } => format!(
                    "Navigate(FindImplementation uri={uri} position={}:{})",
                    position.line, position.character
                ),
            },
            Request::Completion(request) => match request {
                CompletionRequest::Complete { uri, position } => format!(
                    "Completion(Complete uri={uri} position={}:{})",
                    position.line, position.character
                ),
                CompletionRequest::SignatureHelp { uri, position } => format!(
                    "Completion(SignatureHelp uri={uri} position={}:{})",
                    position.line, position.character
                ),
            },
            Request::Buffers(request) => match request {
                BuffersRequest::Text { uri } => format!("Buffers(Text uri={uri})"),
            },
            Request::Symbols(request) => match request {
                SymbolRequest::Search { query } => format!("Symbols(Search query={query})"),
            },
            Request::Index(request) => match request {
                IndexRequest::EntriesByName { name } => {
                    format!("Index(EntriesByName name={name})")
                }
                IndexRequest::EntriesByPrefix { prefix } => {
                    format!("Index(EntriesByPrefix prefix={prefix})")
                }
                IndexRequest::FileSymbols { uri } => format!("Index(FileSymbols uri={uri})"),
                IndexRequest::WorkspaceFiles => "Index(WorkspaceFiles)".into(),
                IndexRequest::TypeClosure { ty, package } => {
                    format!("Index(TypeClosure ty={ty:?} package={package:?})")
                }
                IndexRequest::MembersOf { ty, package } => {
                    format!("Index(MembersOf ty={ty:?} package={package:?})")
                }
                IndexRequest::ConstructorsOf { ty, package } => {
                    format!("Index(ConstructorsOf ty={ty:?} package={package:?})")
                }
                IndexRequest::ResolvesType { ty, package } => {
                    format!("Index(ResolvesType ty={ty:?} package={package:?})")
                }
                IndexRequest::ImplementationsOf {
                    contract,
                    package,
                    member,
                } => format!(
                    "Index(ImplementationsOf contract={contract:?} package={package:?} member={member:?})"
                ),
            },
            Request::IndexBatch(batch) => {
                format!("IndexBatch({} requests)", batch.requests.len())
            }
        }
    }
}

impl Response {
    /// The response's shape for the hub's log: family, variant, and counts —
    /// a text payload as `Some(<n> bytes)` / `None` rather than its body.
    pub fn summary(&self) -> String {
        match self {
            Response::Navigate(response) => match response {
                NavigateResponse::Declaration(Some(location)) => {
                    format!("Navigate(Declaration Some({}))", location.uri)
                }
                NavigateResponse::Declaration(None) => "Navigate(Declaration None)".into(),
                NavigateResponse::References(locations) => {
                    format!("Navigate(References {} locations)", locations.len())
                }
                NavigateResponse::Implementations(locations) => {
                    format!("Navigate(Implementations {} locations)", locations.len())
                }
            },
            Response::Completion(response) => match response {
                CompletionResponse::Items(items) => {
                    format!("Completion(Items {} items)", items.len())
                }
                CompletionResponse::SignatureHelp(help) => match help {
                    Some(_) => "Completion(SignatureHelp Some)".into(),
                    None => "Completion(SignatureHelp None)".into(),
                },
            },
            Response::Buffers(response) => match response {
                BuffersResponse::Text(text) => match text {
                    Some(text) => format!("Buffers(Text Some({} bytes))", text.len()),
                    None => "Buffers(Text None)".into(),
                },
            },
            Response::Symbols(response) => match response {
                SymbolResponse::Symbols(symbols) => {
                    format!("Symbols(Symbols {} symbols)", symbols.len())
                }
            },
            Response::Index(response) => match response {
                IndexResponse::Entries(entries) => {
                    format!("Index(Entries {} entries)", entries.len())
                }
                IndexResponse::File(file) => match file {
                    Some(entries) => format!("Index(File Some({} entries))", entries.len()),
                    None => "Index(File None)".into(),
                },
                IndexResponse::Files(files) => format!("Index(Files {} files)", files.len()),
                IndexResponse::Closure(closure) => {
                    format!("Index(Closure {} types)", closure.len())
                }
                IndexResponse::Members(members) => {
                    format!("Index(Members {} members)", members.len())
                }
                IndexResponse::Constructors(constructors) => {
                    format!("Index(Constructors {} constructors)", constructors.len())
                }
                IndexResponse::Resolves(resolves) => format!("Index(Resolves {resolves})"),
            },
            Response::IndexBatch(batch) => {
                format!("IndexBatch({} responses)", batch.0.len())
            }
            Response::Empty => "Empty".into(),
        }
    }
}

impl Notification {
    /// The notification's shape for the hub's log: variant, `uri`, and text
    /// payloads as byte lengths rather than their bodies.
    pub fn summary(&self) -> String {
        match self {
            Notification::ServerStarted { workspace_root } => {
                format!("ServerStarted root={workspace_root}")
            }
            Notification::FoldersDiscoverred { uri, .. } => {
                format!("FoldersDiscoverred {} folders", uri.len())
            }
            Notification::DocumentOpened { uri, version, text } => format!(
                "DocumentOpened uri={uri} version={version} text={} bytes",
                text.len()
            ),
            Notification::DocumentChanged {
                uri,
                version,
                changes,
            } => {
                let text_bytes = changes
                    .iter()
                    .map(|change| change.text.len())
                    .sum::<usize>();
                format!(
                    "DocumentChanged uri={uri} version={version} changes={} text={text_bytes} bytes",
                    changes.len()
                )
            }
            Notification::DocumentClosed { uri } => format!("DocumentClosed uri={uri}"),
        }
    }
}
