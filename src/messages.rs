//! The engine's message vocabulary: every message on the hub, in one place.
//!
//! Every component — the LSP shell, the analysis core, the index, the
//! diagnostics and quick-fix modules, and the drivers (project walker,
//! dependency resolver, source scanner, jar/JDK indexers, source downloader) —
//! speaks only [`DriverMessage`] notifications, which the hub broadcasts, and
//! [`Request`]s, which the hub routes to the one module that answers them. The
//! shell is a module like any other: it consumes the editor-facing notifications
//! (diagnostics, progress, notices) and renders them to the client.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc as tokio_mpsc;
use tokio::sync::oneshot;
use tower_lsp::lsp_types::{
    CodeAction, CompletionResponse, Diagnostic, DocumentSymbol, FoldingRange, Hover, InlayHint,
    Location, Position, Range, SemanticTokens, SignatureHelp, SymbolInformation,
    TextDocumentContentChangeEvent, Url, WorkspaceEdit,
};

use crate::index::SymbolEntry;
use crate::project::ProjectModel;
use crate::resolve::Artifact;
use crate::types::{ModelLayers, SourceLayerIndex, TypeModel};

/// How a watched file changed, as reported through `workspace/didChangeWatchedFiles`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchedChange {
    Created,
    Changed,
    Deleted,
}

/// One step of the background job's progress. The shell owns the progress token
/// and the client-facing shape; the engine only says what happened.
#[derive(Debug, Clone)]
pub enum ProgressUpdate {
    /// The job started: create (or reuse) the item under this title.
    Begin { title: String, message: String },
    /// A phase or count within the same item.
    Update {
        message: String,
        percentage: Option<u32>,
    },
    /// The job finished.
    End { message: Option<String> },
}

/// How a notice is presented to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageLevel {
    Info,
    Warning,
}

/// The severity of a [`DriverMessage::Log`] line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Debug,
    Warn,
}

/// A warm-up stage whose completion the coordinator waits for. `Sources`, `Jars`,
/// and `Jdk` gate `ready`; `Downloads` only gates the closing summary, exactly as
/// today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    Sources,
    Jars,
    Jdk,
    Downloads,
}

/// A notification on the hub, broadcast by the hub to every module and driver;
/// each consumes the ones it cares about. Large payloads are shared behind an
/// `Arc`, so broadcasting is a pointer copy.
#[derive(Clone)]
pub enum DriverMessage {
    // -- the shell (editor input) --
    /// The workspace root (or a later folder) is known.
    FolderAdded {
        uri: Url,
    },
    /// A watched file was created, changed, or deleted.
    FileEvent {
        uri: Url,
        change: WatchedChange,
    },
    // -- project driver --
    /// The Maven project model (modules, source roots).
    ProjectModel {
        model: Arc<ProjectModel>,
    },
    /// The workspace `.java` files under the model's source roots.
    SourceInventory {
        files: Arc<Vec<PathBuf>>,
    },
    // -- dependency driver --
    /// The resolved dependency jars.
    Artifacts {
        artifacts: Arc<Vec<Artifact>>,
    },
    // -- source scanner --
    /// One workspace source file: its index entries and declared-type model.
    SourceFile {
        uri: Url,
        entries: Arc<Vec<SymbolEntry>>,
        types: Arc<TypeModel>,
    },
    // -- jar / JDK indexers, source downloader --
    /// A non-source base artifact: its entries and declared types as one layer.
    BaseArtifact {
        uri: Url,
        entries: Arc<Vec<SymbolEntry>>,
        types: Arc<TypeModel>,
    },
    /// A base artifact superseded by extracted sources: drop its layer.
    RemoveBase {
        uri: Url,
    },
    // -- coordinator --
    /// A producer finished a stage, with the count it contributed.
    StageDone {
        stage: Stage,
        count: usize,
    },
    /// The core stages finished; the engine flips `ready`.
    Ready,
    // -- diagnostics subsystem --
    /// Diagnostics for one open document, computed by the diagnostics subsystem
    /// and published by the shell. A `None` version clears them (the document
    /// closed).
    Diagnostics {
        uri: Url,
        version: Option<i32>,
        diagnostics: Vec<Diagnostic>,
    },
    // -- document lifecycle (consumed by the diagnostics and quick-fix
    // subsystems; the text is a shared `Arc`, so it is not copied per module) --
    DocumentOpened {
        uri: Url,
        text: Arc<String>,
        version: i32,
    },
    DocumentChanged {
        uri: Url,
        text: Arc<String>,
        version: i32,
    },
    DocumentClosed {
        uri: Url,
    },
    /// The client's capabilities (the `CreateFile` resource operation), for the
    /// quick-fix subsystem's create-type fix.
    ClientCapabilities {
        resource_operations: bool,
    },
    /// The analysis core applied a document or file event (and sent its index
    /// updates before this), so a diagnostics sweep now sees the new state.
    AnalysisUpdated,
    // -- index mutations from the core (open buffers) --
    /// The core's symbol entries for one file (an open buffer, a watched
    /// re-read, or a close re-read).
    SourceEntries {
        uri: Url,
        entries: Arc<Vec<SymbolEntry>>,
    },
    /// A file the core forgot (closed and gone, or deleted).
    SourceRemoved {
        uri: Url,
    },
    /// The core's current declared-type model of one file, layered over the base.
    DirtyTypes {
        uri: Url,
        model: Arc<TypeModel>,
    },
    /// The core dropped a file's dirty type overlay.
    DirtyTypesDropped {
        uri: Url,
    },
    /// Replaces one file's warm-up declared-type model (a fixture/test hook).
    SourceTypes {
        uri: Url,
        model: Arc<TypeModel>,
    },
    /// Replaces the whole non-source base with a single layer (a fixture/test
    /// hook).
    BaseTypes {
        model: Arc<TypeModel>,
    },
    // -- reporting (progress and notices are rendered by the shell; logs and
    // the summary by the hub) --
    /// Progress for the single background job (warm-up and source fetch).
    Progress(ProgressUpdate),
    /// A discrete, notable message for the user.
    Notice {
        level: MessageLevel,
        text: String,
    },
    Log {
        level: LogLevel,
        message: String,
    },
    /// The final warm-up summary, rendered as the `workspace index warm-up
    /// complete` line the bench parses.
    Summary {
        root: String,
        files: usize,
        jars: usize,
        jdk_classes: usize,
        maven: bool,
        elapsed_ms: u64,
    },
}

/// A request routed by the hub to the one module that answers it. The owner
/// answers with the [`ReplyHandle`] it carries; the hub sees that reply, logs and
/// times it, and delivers the value to the requester waiting on its own channel.
pub enum Request {
    // -- document module --
    /// Applies the editor's incremental changes to an open document and returns
    /// its new text; `None` when the document is not open.
    DocumentChange {
        uri: Url,
        version: i32,
        changes: Vec<TextDocumentContentChangeEvent>,
        reply: ReplyHandle<Option<Arc<String>>>,
    },
    /// The current text and version of an open document, or `None` when the
    /// document is not open.
    DocumentText {
        uri: Url,
        reply: ReplyHandle<Option<(i32, Arc<String>)>>,
    },
    // -- index subsystem --
    IndexQueryName {
        name: String,
        reply: ReplyHandle<Vec<Arc<SymbolEntry>>>,
    },
    /// Many exact-name lookups in one round trip: one map entry per requested
    /// name, an empty vec for a name with no entries.
    IndexQueryNames {
        names: Vec<String>,
        reply: ReplyHandle<HashMap<String, Vec<Arc<SymbolEntry>>>>,
    },
    IndexQueryPrefix {
        prefix: String,
        reply: ReplyHandle<Vec<Arc<SymbolEntry>>>,
    },
    IndexAllSymbols {
        reply: ReplyHandle<Vec<SymbolEntry>>,
    },
    IndexFileCount {
        reply: ReplyHandle<usize>,
    },
    IndexReady {
        reply: ReplyHandle<bool>,
    },
    IndexHasPackage {
        package: String,
        reply: ReplyHandle<bool>,
    },
    /// Many package-existence checks in one round trip: the requested packages
    /// that exist.
    IndexHasPackages {
        packages: Vec<String>,
        reply: ReplyHandle<HashSet<String>>,
    },
    IndexTypeModel {
        reply: ReplyHandle<Option<Arc<SourceLayerIndex>>>,
    },
    IndexTypeLayers {
        reply: ReplyHandle<ModelLayers>,
    },
    IndexSourceFiles {
        reply: ReplyHandle<Vec<Url>>,
    },
    IndexSourceRoots {
        reply: ReplyHandle<Vec<PathBuf>>,
    },
    IndexSourceLayerIndex {
        reply: ReplyHandle<Arc<SourceLayerIndex>>,
    },
    IndexSourceModels {
        reply: ReplyHandle<Vec<(Url, Arc<TypeModel>)>>,
    },
    // -- diagnostics subsystem --
    DiagnosticsForDocument {
        uri: Url,
        reply: ReplyHandle<Option<(i32, Arc<Vec<Diagnostic>>)>>,
    },
    // -- quick-fix subsystem --
    QuickFixForDocument {
        uri: Url,
        diagnostics: Vec<Diagnostic>,
        reply: ReplyHandle<Vec<CodeAction>>,
    },
    // -- analysis core --
    Analysis(AnalysisRequest),
}

/// A query the analysis module answers from the core: one per editor feature.
pub enum AnalysisRequest {
    Hover {
        uri: Url,
        position: Position,
        reply: ReplyHandle<Option<Hover>>,
    },
    Definition {
        uri: Url,
        position: Position,
        reply: ReplyHandle<Option<Location>>,
    },
    Implementation {
        uri: Url,
        position: Position,
        reply: ReplyHandle<Vec<Location>>,
    },
    Completions {
        uri: Url,
        position: Position,
        reply: ReplyHandle<Option<CompletionResponse>>,
    },
    DocumentSymbols {
        uri: Url,
        reply: ReplyHandle<Option<Vec<DocumentSymbol>>>,
    },
    FoldingRanges {
        uri: Url,
        reply: ReplyHandle<Option<Vec<FoldingRange>>>,
    },
    SemanticTokens {
        uri: Url,
        reply: ReplyHandle<Option<SemanticTokens>>,
    },
    InlayHints {
        uri: Url,
        range: Range,
        reply: ReplyHandle<Vec<InlayHint>>,
    },
    SignatureHelp {
        uri: Url,
        position: Position,
        reply: ReplyHandle<Option<SignatureHelp>>,
    },
    References {
        uri: Url,
        position: Position,
        include_declaration: bool,
        reply: ReplyHandle<Vec<Location>>,
    },
    Rename {
        uri: Url,
        position: Position,
        new_name: String,
        reply: ReplyHandle<Option<WorkspaceEdit>>,
    },
    WorkspaceSymbols {
        query: String,
        reply: ReplyHandle<Vec<SymbolInformation>>,
    },
}

impl AnalysisRequest {
    /// The request's correlation id, from its reply handle.
    pub(crate) fn id(&self) -> u64 {
        match self {
            Self::Hover { reply, .. } => reply.id(),
            Self::Definition { reply, .. } => reply.id(),
            Self::Implementation { reply, .. } => reply.id(),
            Self::Completions { reply, .. } => reply.id(),
            Self::DocumentSymbols { reply, .. } => reply.id(),
            Self::FoldingRanges { reply, .. } => reply.id(),
            Self::SemanticTokens { reply, .. } => reply.id(),
            Self::InlayHints { reply, .. } => reply.id(),
            Self::SignatureHelp { reply, .. } => reply.id(),
            Self::References { reply, .. } => reply.id(),
            Self::Rename { reply, .. } => reply.id(),
            Self::WorkspaceSymbols { reply, .. } => reply.id(),
        }
    }
}

/// A message on the engine hub: a notification every module may consume, or a
/// request the hub routes to the one module that answers it. This is the value a
/// module or driver receives; the sender's name and a request's correlation id
/// ride the hub's own [`Inbound`] envelope, not this.
pub enum Hub {
    Notify(DriverMessage),
    Request(Request),
}

/// A message a client posts to the hub. It carries the sender's name (and a
/// request's correlation id) so the hub can attribute and time every line it
/// logs; the value the modules and drivers receive (`Hub`) carries neither.
pub enum Inbound {
    /// A notification from `sender`; the hub broadcasts it to every module and
    /// every driver.
    Notify {
        sender: String,
        message: DriverMessage,
    },
    /// A request from `sender`, routed to the module that owns it. `id` matches
    /// the request's [`ReplyHandle`], so the hub can pair the answer with it.
    Request {
        sender: String,
        id: u64,
        request: Request,
    },
    /// The answer to request `id`. `deliver` hands the value to the waiting
    /// caller; `None` means the request was dropped unanswered, so the hub only
    /// forgets it.
    Reply {
        id: u64,
        deliver: Option<Box<dyn FnOnce() + Send>>,
    },
    /// `sender` registers `sink` for every notification and, with `serves`, as
    /// the owner of that module's requests.
    Subscribe {
        sender: String,
        sink: tokio_mpsc::UnboundedSender<Hub>,
        serves: Option<crate::hub::Module>,
    },
}

/// A request's reply handle. The owner answers with [`ReplyHandle::send`], which
/// posts the value back through the hub (so the hub sees and times the reply)
/// rather than straight to the caller. Dropping it unanswered tells the hub to
/// forget the request.
///
/// The requester holds the other end of the handle's oneshot channel (a
/// [`crate::hub::Reply`]), which it awaits or, from synchronous module code,
/// receives blocking.
pub struct ReplyHandle<R> {
    inbound: tokio_mpsc::UnboundedSender<Inbound>,
    id: u64,
    tx: Option<oneshot::Sender<R>>,
}

impl<R> ReplyHandle<R> {
    /// Builds the handle for request `id`; `tx` reaches the waiting caller.
    pub(crate) fn new(
        inbound: tokio_mpsc::UnboundedSender<Inbound>,
        id: u64,
        tx: oneshot::Sender<R>,
    ) -> Self {
        Self {
            inbound,
            id,
            tx: Some(tx),
        }
    }

    /// The request's correlation id.
    pub(crate) fn id(&self) -> u64 {
        self.id
    }
}

impl<R: Send + 'static> ReplyHandle<R> {
    /// Answers the request. The value reaches the caller through the hub.
    pub fn send(mut self, value: R) {
        if let Some(tx) = self.tx.take() {
            let _ = self.inbound.send(Inbound::Reply {
                id: self.id,
                deliver: Some(Box::new(move || {
                    let _ = tx.send(value);
                })),
            });
        }
    }
}

impl<R> Drop for ReplyHandle<R> {
    fn drop(&mut self) {
        // Dropped without answering (e.g. a request the owner does not handle):
        // let the hub drop the pending entry rather than leak it.
        if self.tx.take().is_some() {
            let _ = self.inbound.send(Inbound::Reply {
                id: self.id,
                deliver: None,
            });
        }
    }
}

/// Renders a notification's log reporting — a driver's [`DriverMessage::Log`]
/// line or the warm-up [`DriverMessage::Summary`] — as a `tracing` line. The hub
/// calls this for every notification; the editor-facing reporting (diagnostics,
/// progress, notices) is the shell's to render.
pub fn log_reporting(message: &DriverMessage) {
    match message {
        DriverMessage::Log { level, message } => match level {
            LogLevel::Info => tracing::info!("{message}"),
            LogLevel::Debug => tracing::debug!("{message}"),
            LogLevel::Warn => tracing::warn!("{message}"),
        },
        DriverMessage::Summary {
            root,
            files,
            jars,
            jdk_classes,
            maven,
            elapsed_ms,
        } => tracing::info!(
            root = %root,
            files,
            jars,
            jdk_classes,
            maven,
            elapsed_ms,
            "workspace index warm-up complete"
        ),
        _ => {}
    }
}
