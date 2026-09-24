//! The engine's message vocabulary: every message in or out, in one place.
//!
//! The shell talks to the engine with [`Command`] and hears back through
//! [`EngineEvent`]. Every subsystem — the filesystem, the project walker, the
//! dependency resolver, the source scanner, the jar/JDK indexers, the source
//! downloader — is a driver that speaks only [`DriverMessage`], which the engine
//! applies to the index, relays to every driver, and turns into editor
//! reporting. No subsystem emits an [`EngineEvent`] itself.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;

use tokio::sync::mpsc as tokio_mpsc;
use tokio::sync::oneshot;
use tower_lsp::lsp_types::{
    CodeAction, CompletionResponse, Diagnostic, DocumentSymbol, FoldingRange, Hover, InlayHint,
    Location, Position, Range, SemanticTokens, SignatureHelp, SymbolInformation, Url,
    WorkspaceEdit,
};

use crate::index::SymbolEntry;
use crate::project::ProjectModel;
use crate::resolve::Artifact;
use crate::types::{ModelLayers, SourceLayerIndex, TypeModel};

type Reply<T> = oneshot::Sender<T>;

/// Commands the shell sends to the engine. Queries carry a `oneshot` reply.
pub enum Command {
    /// The workspace root is known; the filesystem driver is told about it.
    SetWorkspaceRoot(Url),
    /// Records whether the client supports the `CreateFile` resource operation.
    SetClientCapabilities {
        resource_operations: bool,
    },
    Open {
        uri: Url,
        text: String,
        version: i32,
    },
    Change {
        uri: Url,
        text: String,
        version: i32,
    },
    Close(Url),
    /// Watched filesystem events (created/changed/deleted `.java` files) the
    /// editor reported but never opened.
    WatchedFiles {
        changes: Vec<(Url, WatchedChange)>,
    },
    Hover {
        uri: Url,
        position: Position,
        reply: Reply<Option<Hover>>,
    },
    Definition {
        uri: Url,
        position: Position,
        reply: Reply<Option<Location>>,
    },
    Implementation {
        uri: Url,
        position: Position,
        reply: Reply<Vec<Location>>,
    },
    Completions {
        uri: Url,
        position: Position,
        reply: Reply<Option<CompletionResponse>>,
    },
    DocumentSymbols {
        uri: Url,
        reply: Reply<Option<Vec<DocumentSymbol>>>,
    },
    FoldingRanges {
        uri: Url,
        reply: Reply<Option<Vec<FoldingRange>>>,
    },
    SemanticTokens {
        uri: Url,
        reply: Reply<Option<SemanticTokens>>,
    },
    InlayHints {
        uri: Url,
        range: Range,
        reply: Reply<Vec<InlayHint>>,
    },
    SignatureHelp {
        uri: Url,
        position: Position,
        reply: Reply<Option<SignatureHelp>>,
    },
    References {
        uri: Url,
        position: Position,
        include_declaration: bool,
        reply: Reply<Vec<Location>>,
    },
    Rename {
        uri: Url,
        position: Position,
        new_name: String,
        reply: Reply<Option<WorkspaceEdit>>,
    },
    WorkspaceSymbols {
        query: String,
        reply: Reply<Vec<SymbolInformation>>,
    },
    CodeActions {
        uri: Url,
        diagnostics: Vec<Diagnostic>,
        reply: Reply<Vec<CodeAction>>,
    },
    /// A flat snapshot of the index; a verification hook for tests.
    IndexedSymbols {
        reply: Reply<Vec<SymbolEntry>>,
    },
    /// True once the initial scan finished; a verification hook for tests.
    IndexReady {
        reply: Reply<bool>,
    },
}

/// How a watched file changed, as reported through `workspace/didChangeWatchedFiles`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchedChange {
    Created,
    Changed,
    Deleted,
}

/// Events the engine pushes to the shell.
#[derive(Debug)]
pub enum EngineEvent {
    /// Diagnostics for `uri`, replacing whatever was published for it before.
    /// A `None` version clears them (the document closed).
    Diagnostics {
        uri: Url,
        version: Option<i32>,
        diagnostics: Vec<Diagnostic>,
    },
    /// Progress for the single background job (warm-up and source fetch).
    Progress(ProgressUpdate),
    /// A discrete, notable message for the user.
    Message { level: MessageLevel, text: String },
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

/// A message a driver sends to the engine, which the engine applies, relays to
/// every driver, and translates to editor reporting. Large payloads are shared
/// behind an `Arc`, so relaying to every driver is a pointer copy.
#[derive(Clone)]
pub enum DriverMessage {
    // -- filesystem driver --
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
    /// and translated by the hub into an editor event.
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
    // -- reporting --
    Progress(ProgressUpdate),
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
}

/// A message on the engine bus: a notification every module may consume, or a
/// request the hub routes to the one module that answers it. This is the value a
/// module or driver receives; the sender's name and a request's correlation id
/// ride the hub's own [`Inbound`] envelope, not this.
pub enum Bus {
    Notify(DriverMessage),
    Request(Request),
}

/// A message a client posts to the hub. It carries the sender's name (and a
/// request's correlation id) so the hub can attribute and time every line it
/// logs; the value the modules and drivers receive (`Bus`) carries neither.
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
}

/// A request's reply handle. The owner answers with [`ReplyHandle::send`], which
/// posts the value back through the hub (so the hub sees and times the reply)
/// rather than straight to the caller. Dropping it unanswered tells the hub to
/// forget the request.
pub struct ReplyHandle<R> {
    inbound: tokio_mpsc::UnboundedSender<Inbound>,
    id: u64,
    tx: Option<std_mpsc::Sender<R>>,
}

impl<R> ReplyHandle<R> {
    /// Builds the handle for a request; the bus allocates the id.
    pub(crate) fn new(
        inbound: tokio_mpsc::UnboundedSender<Inbound>,
        id: u64,
        tx: std_mpsc::Sender<R>,
    ) -> Self {
        Self {
            inbound,
            id,
            tx: Some(tx),
        }
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

/// Turns a message's reporting into an editor event or a `tracing` line. The
/// engine is the only component that talks to the editor.
pub fn translate(
    message: &DriverMessage,
    events: &tokio::sync::mpsc::UnboundedSender<EngineEvent>,
) {
    match message {
        DriverMessage::Diagnostics {
            uri,
            version,
            diagnostics,
        } => {
            let _ = events.send(EngineEvent::Diagnostics {
                uri: uri.clone(),
                version: *version,
                diagnostics: diagnostics.clone(),
            });
        }
        DriverMessage::Progress(update) => {
            let _ = events.send(EngineEvent::Progress(update.clone()));
        }
        DriverMessage::Notice { level, text } => {
            let _ = events.send(EngineEvent::Message {
                level: *level,
                text: text.clone(),
            });
        }
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
