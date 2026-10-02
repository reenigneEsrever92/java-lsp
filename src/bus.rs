//! The engine bus: one mechanism every module uses.
//!
//! Modules never call each other directly and never share state. Each module
//! owns its own state and holds a [`BusClient`], which is all it needs: it
//! registers with the hub through it ([`BusClient::subscribe`], or
//! [`BusClient::serve`] to also own a module's requests), sends
//! **notifications** (which the hub broadcasts to every subscriber — each
//! consumes the ones it cares about) and **requests** (which the hub routes to
//! the one module that serves them, the reply riding the same bus). Every request returns a [`Reply`]: a
//! thin wrapper over a tokio oneshot receiver. Code on the runtime (the shell,
//! the drivers) awaits it; synchronous module code — the analysis core, the
//! diagnostics and quick-fix modules, which run on their own threads or the
//! blocking pool — calls [`Reply::blocking_recv`]. The hub is a thread, so a
//! blocked module never stalls it.
//!
//! `BusClient` mirrors the query surface its callers need, so a module that used
//! to hold a neighbour's handle now holds a client instead.
//!
//! The hub **logs every message passing through** at `debug`
//! (`RUST_LOG=java_lsp::bus=debug`): one line per message, each prefixed with the
//! sender's name, with the identifiers and counts that matter but never a payload
//! (an open document's text is logged as its size). The high-cardinality, per-item
//! notifications (a source file, an artifact, a progress tick) are logged at
//! `trace` instead, so `debug` shows the flow rather than thousands of per-item
//! lines. A request's reply is logged too, naming the module that answered it and
//! the time it took. See [`describe_notification`] and [`describe_request`]. The
//! batched lookups (`IndexQueryNames`, `IndexHasPackages`) are logged by count,
//! never by their key lists.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::thread;
use std::time::Instant;

use tokio::sync::mpsc as tokio_mpsc;
use tokio::sync::oneshot;
use tower_lsp::lsp_types::{
    CodeAction, CompletionResponse, Diagnostic, DocumentSymbol, FoldingRange, Hover, InlayHint,
    Location, Position, Range, SemanticTokens, SignatureHelp, SymbolInformation, Url,
    WorkspaceEdit,
};

use crate::index::SymbolEntry;
use crate::messages::{self, AnalysisRequest, Bus, DriverMessage, Inbound, ReplyHandle, Request};
use crate::project::ProjectModel;
use crate::types::{ModelLayers, SourceLayerIndex, TypeModel};

/// Which module owns a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Module {
    Index,
    Diagnostics,
    QuickFix,
    Analysis,
}

/// A cheap, cloneable client for talking on the bus. Everything — the core, the
/// subsystems, and the shell — uses this; none of them reaches into another
/// module. The client carries the name it logs under, so a clone may be relabeled
/// with [`BusClient::labeled`].
#[derive(Clone)]
pub struct BusClient {
    inbound: tokio_mpsc::UnboundedSender<Inbound>,
    sender: String,
}

/// The next request id: unique for the process, so the hub can pair a reply with
/// its request.
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

impl BusClient {
    /// A clone of this client that logs under `label`.
    pub fn labeled(&self, label: impl Into<String>) -> Self {
        Self {
            inbound: self.inbound.clone(),
            sender: label.into(),
        }
    }

    /// Sends a notification; every module may consume it.
    pub fn notify(&self, message: DriverMessage) {
        let _ = self.inbound.send(Inbound::Notify {
            sender: self.sender.clone(),
            message,
        });
    }

    /// Registers with the hub for every notification. Call it before starting
    /// the thread or task that drains the receiver: the subscription rides the
    /// hub's FIFO inbound channel, so it is in place before any message sent
    /// afterwards. Dropping the receiver unsubscribes.
    pub fn subscribe(&self) -> tokio_mpsc::UnboundedReceiver<Bus> {
        self.register(None)
    }

    /// [`Self::subscribe`], and also makes this receiver the owner of
    /// `module`'s requests. A module already served keeps its first owner (the
    /// hub logs an error); dropping the receiver releases the ownership.
    pub fn serve(&self, module: Module) -> tokio_mpsc::UnboundedReceiver<Bus> {
        self.register(Some(module))
    }

    fn register(&self, serves: Option<Module>) -> tokio_mpsc::UnboundedReceiver<Bus> {
        let (sink, rx) = tokio_mpsc::unbounded_channel();
        let _ = self.inbound.send(Inbound::Subscribe {
            sender: self.sender.clone(),
            sink,
            serves,
        });
        rx
    }

    /// A fresh request id with its reply pair: the handle travels in the request,
    /// the [`Reply`] stays with the caller.
    fn reply<R>(&self) -> (ReplyHandle<R>, Reply<R>) {
        let id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        (ReplyHandle::new(self.inbound.clone(), id, tx), Reply(rx))
    }

    /// Posts request `id` to the hub, which routes it to the owning module. If
    /// the bus is gone the request (and its handle) is dropped, so the caller's
    /// [`Reply`] resolves to the default.
    fn post(&self, id: u64, request: Request) {
        let _ = self.inbound.send(Inbound::Request {
            sender: self.sender.clone(),
            id,
            request,
        });
    }

    // -- index subsystem ---------------------------------------------------

    pub fn query_name(&self, name: &str) -> Reply<Vec<Arc<SymbolEntry>>> {
        let (reply, answer) = self.reply();
        let name = name.to_string();
        self.post(reply.id(), Request::IndexQueryName { name, reply });
        answer
    }

    /// Exact-name lookups for many names in one round trip; every requested name
    /// has an entry in the result.
    pub fn query_names(&self, names: Vec<String>) -> Reply<HashMap<String, Vec<Arc<SymbolEntry>>>> {
        let (reply, answer) = self.reply();
        self.post(reply.id(), Request::IndexQueryNames { names, reply });
        answer
    }

    pub fn query_prefix(&self, prefix: &str) -> Reply<Vec<Arc<SymbolEntry>>> {
        let (reply, answer) = self.reply();
        let prefix = prefix.to_string();
        self.post(reply.id(), Request::IndexQueryPrefix { prefix, reply });
        answer
    }

    pub fn all_symbols(&self) -> Reply<Vec<SymbolEntry>> {
        let (reply, answer) = self.reply();
        self.post(reply.id(), Request::IndexAllSymbols { reply });
        answer
    }

    pub fn file_count(&self) -> Reply<usize> {
        let (reply, answer) = self.reply();
        self.post(reply.id(), Request::IndexFileCount { reply });
        answer
    }

    /// True once the warm-up's core stages finished.
    pub fn ready(&self) -> Reply<bool> {
        let (reply, answer) = self.reply();
        self.post(reply.id(), Request::IndexReady { reply });
        answer
    }

    pub fn has_package(&self, package: &str) -> Reply<bool> {
        let (reply, answer) = self.reply();
        let package = package.to_string();
        self.post(reply.id(), Request::IndexHasPackage { package, reply });
        answer
    }

    /// The packages among `packages` the index knows, in one round trip.
    pub fn has_packages(&self, packages: Vec<String>) -> Reply<HashSet<String>> {
        let (reply, answer) = self.reply();
        self.post(reply.id(), Request::IndexHasPackages { packages, reply });
        answer
    }

    pub fn type_model(&self) -> Reply<Option<Arc<SourceLayerIndex>>> {
        let (reply, answer) = self.reply();
        self.post(reply.id(), Request::IndexTypeModel { reply });
        answer
    }

    pub fn type_layers(&self) -> Reply<ModelLayers> {
        let (reply, answer) = self.reply();
        self.post(reply.id(), Request::IndexTypeLayers { reply });
        answer
    }

    pub fn source_files(&self) -> Reply<Vec<Url>> {
        let (reply, answer) = self.reply();
        self.post(reply.id(), Request::IndexSourceFiles { reply });
        answer
    }

    pub fn source_roots(&self) -> Reply<Vec<PathBuf>> {
        let (reply, answer) = self.reply();
        self.post(reply.id(), Request::IndexSourceRoots { reply });
        answer
    }

    pub fn source_layer_index(&self) -> Reply<Arc<SourceLayerIndex>> {
        let (reply, answer) = self.reply();
        self.post(reply.id(), Request::IndexSourceLayerIndex { reply });
        answer
    }

    pub fn source_models(&self) -> Reply<Vec<(Url, Arc<TypeModel>)>> {
        let (reply, answer) = self.reply();
        self.post(reply.id(), Request::IndexSourceModels { reply });
        answer
    }

    // -- index mutations (notifications) -----------------------------------

    /// Notifies the index that a file is gone.
    /// Notifies the index of one file's symbol entries (an open buffer or a
    /// watched/close re-read).
    pub fn upsert_file(&self, uri: &Url, entries: Vec<SymbolEntry>) {
        self.notify(DriverMessage::SourceEntries {
            uri: uri.clone(),
            entries: Arc::new(entries),
        });
    }

    /// Notifies the index of a warm-up per-file declared-type model.
    pub fn set_source_types(&self, uri: &Url, model: Arc<TypeModel>) {
        self.notify(DriverMessage::SourceTypes {
            uri: uri.clone(),
            model,
        });
    }

    /// Notifies the index that the whole non-source base is replaced.
    pub fn set_types(&self, model: Arc<TypeModel>) {
        self.notify(DriverMessage::BaseTypes { model });
    }

    /// Notifies the index of the project model.
    pub fn set_model(&self, model: ProjectModel) {
        self.notify(DriverMessage::ProjectModel {
            model: Arc::new(model),
        });
    }

    /// Notifies the index of a non-source base artifact layer.
    pub fn add_base_layer(&self, uri: &Url, entries: Vec<SymbolEntry>, types: Arc<TypeModel>) {
        self.notify(DriverMessage::BaseArtifact {
            uri: uri.clone(),
            entries: Arc::new(entries),
            types,
        });
    }

    /// Notifies the index that a base artifact layer is dropped.
    pub fn remove_base_layer(&self, uri: &Url) {
        self.notify(DriverMessage::RemoveBase { uri: uri.clone() });
    }

    /// Notifies the index that the warm-up finished.
    pub fn set_ready(&self) {
        self.notify(DriverMessage::Ready);
    }

    /// Applies a driver's contribution by notification (a convenience for the
    /// warm-up and for tests).
    pub fn apply(&self, message: DriverMessage) {
        self.notify(message);
    }

    /// Notifies the index that a file is gone.
    pub fn remove_file(&self, uri: &Url) {
        self.notify(DriverMessage::SourceRemoved { uri: uri.clone() });
    }

    /// Notifies the index of a file's current declared-type model.
    pub fn record_dirty_type(&self, uri: &Url, model: Arc<TypeModel>) {
        self.notify(DriverMessage::DirtyTypes {
            uri: uri.clone(),
            model,
        });
    }

    /// Notifies the index that a file's dirty type overlay is dropped.
    pub fn drop_dirty_type(&self, uri: &Url) {
        self.notify(DriverMessage::DirtyTypesDropped { uri: uri.clone() });
    }

    // -- diagnostics subsystem ---------------------------------------------

    /// The diagnostics subsystem's cached pass for `uri`.
    pub fn diagnostics(&self, uri: &Url) -> Reply<Option<(i32, Arc<Vec<Diagnostic>>)>> {
        let (reply, answer) = self.reply();
        let uri = uri.clone();
        self.post(reply.id(), Request::DiagnosticsForDocument { uri, reply });
        answer
    }

    // -- quick-fix subsystem -----------------------------------------------

    /// The quick fixes for `diagnostics` in `uri`.
    pub fn code_actions(&self, uri: &Url, diagnostics: Vec<Diagnostic>) -> Reply<Vec<CodeAction>> {
        let (reply, answer) = self.reply();
        let uri = uri.clone();
        self.post(
            reply.id(),
            Request::QuickFixForDocument {
                uri,
                diagnostics,
                reply,
            },
        );
        answer
    }

    // -- analysis module ---------------------------------------------------

    pub fn hover(&self, uri: Url, position: Position) -> Reply<Option<Hover>> {
        let (reply, answer) = self.reply();
        let request = AnalysisRequest::Hover {
            uri,
            position,
            reply,
        };
        self.post_analysis(request);
        answer
    }

    pub fn definition(&self, uri: Url, position: Position) -> Reply<Option<Location>> {
        let (reply, answer) = self.reply();
        let request = AnalysisRequest::Definition {
            uri,
            position,
            reply,
        };
        self.post_analysis(request);
        answer
    }

    pub fn implementation(&self, uri: Url, position: Position) -> Reply<Vec<Location>> {
        let (reply, answer) = self.reply();
        let request = AnalysisRequest::Implementation {
            uri,
            position,
            reply,
        };
        self.post_analysis(request);
        answer
    }

    pub fn completions(&self, uri: Url, position: Position) -> Reply<Option<CompletionResponse>> {
        let (reply, answer) = self.reply();
        let request = AnalysisRequest::Completions {
            uri,
            position,
            reply,
        };
        self.post_analysis(request);
        answer
    }

    pub fn document_symbols(&self, uri: Url) -> Reply<Option<Vec<DocumentSymbol>>> {
        let (reply, answer) = self.reply();
        self.post_analysis(AnalysisRequest::DocumentSymbols { uri, reply });
        answer
    }

    pub fn folding_ranges(&self, uri: Url) -> Reply<Option<Vec<FoldingRange>>> {
        let (reply, answer) = self.reply();
        self.post_analysis(AnalysisRequest::FoldingRanges { uri, reply });
        answer
    }

    pub fn semantic_tokens(&self, uri: Url) -> Reply<Option<SemanticTokens>> {
        let (reply, answer) = self.reply();
        self.post_analysis(AnalysisRequest::SemanticTokens { uri, reply });
        answer
    }

    pub fn inlay_hints(&self, uri: Url, range: Range) -> Reply<Vec<InlayHint>> {
        let (reply, answer) = self.reply();
        self.post_analysis(AnalysisRequest::InlayHints { uri, range, reply });
        answer
    }

    pub fn signature_help(&self, uri: Url, position: Position) -> Reply<Option<SignatureHelp>> {
        let (reply, answer) = self.reply();
        let request = AnalysisRequest::SignatureHelp {
            uri,
            position,
            reply,
        };
        self.post_analysis(request);
        answer
    }

    pub fn references(
        &self,
        uri: Url,
        position: Position,
        include_declaration: bool,
    ) -> Reply<Vec<Location>> {
        let (reply, answer) = self.reply();
        let request = AnalysisRequest::References {
            uri,
            position,
            include_declaration,
            reply,
        };
        self.post_analysis(request);
        answer
    }

    pub fn rename(
        &self,
        uri: Url,
        position: Position,
        new_name: String,
    ) -> Reply<Option<WorkspaceEdit>> {
        let (reply, answer) = self.reply();
        let request = AnalysisRequest::Rename {
            uri,
            position,
            new_name,
            reply,
        };
        self.post_analysis(request);
        answer
    }

    pub fn workspace_symbols(&self, query: String) -> Reply<Vec<SymbolInformation>> {
        let (reply, answer) = self.reply();
        self.post_analysis(AnalysisRequest::WorkspaceSymbols { query, reply });
        answer
    }

    fn post_analysis(&self, request: AnalysisRequest) {
        self.post(request.id(), Request::Analysis(request));
    }
}

/// The pending answer to a bus request: a thin wrapper over the request's tokio
/// oneshot receiver. Await it on the runtime; synchronous module code (a module
/// thread, the blocking pool, a sync test) calls [`Reply::blocking_recv`]. If
/// the owner drops the request unanswered or the bus is gone, it resolves to
/// `R::default()`.
#[must_use = "a request's answer arrives only through its Reply"]
pub struct Reply<R>(oneshot::Receiver<R>);

impl<R: Default> Reply<R> {
    /// Blocks the current thread for the answer. Panics inside an async
    /// context: await the reply there instead.
    pub fn blocking_recv(self) -> R {
        self.0.blocking_recv().unwrap_or_default()
    }
}

impl<R: Default> Future for Reply<R> {
    type Output = R;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<R> {
        Pin::new(&mut self.0)
            .poll(cx)
            .map(Result::unwrap_or_default)
    }
}

/// The next notification on a subscriber's receiver, or `None` once the bus is
/// gone. A driver serves no module, so a request on the receiver is skipped.
pub(crate) async fn next_notification(
    rx: &mut tokio_mpsc::UnboundedReceiver<Bus>,
) -> Option<DriverMessage> {
    loop {
        match rx.recv().await? {
            Bus::Notify(message) => return Some(message),
            Bus::Request(_) => continue,
        }
    }
}

/// Starts the hub thread, with no participants yet: every module, driver, and
/// the shell registers through its client ([`BusClient::subscribe`],
/// [`BusClient::serve`]). Every subscriber receives every notification; a
/// request is routed to the subscriber serving its module. The hub renders the
/// log reporting (`Log`, `Summary`) itself and nothing else: the editor-facing
/// notifications are the shell's to render.
pub fn spawn_hub() -> BusClient {
    let (inbound, mut rx) = tokio_mpsc::unbounded_channel::<Inbound>();
    thread::Builder::new()
        .name("java-lsp-hub".to_string())
        .spawn(move || {
            let mut subscribers: Vec<tokio_mpsc::UnboundedSender<Bus>> = Vec::new();
            let mut owners: HashMap<Module, tokio_mpsc::UnboundedSender<Bus>> = HashMap::new();
            // The requests the hub is still waiting on, so each reply can be
            // timed and attributed to the module that owned its request.
            let mut pending: HashMap<u64, Pending> = HashMap::new();
            while let Some(message) = rx.blocking_recv() {
                // The hub logs every message passing through, so the whole
                // message flow is observable from one place: each line names its
                // sender, and a reply names its requester and latency. Detail is
                // at `debug` (enable with `RUST_LOG=java_lsp::bus=debug`).
                match message {
                    Inbound::Notify { sender, message } => {
                        // Bulk, per-item notifications stay at `trace`; the rest of the
                        // flow is `debug`, so `debug` is readable on a large workspace.
                        if is_bulk(&message) {
                            if tracing::enabled!(tracing::Level::TRACE) {
                                tracing::trace!(
                                    target: "java_lsp::bus",
                                    "sender={sender} notify {}",
                                    describe_notification(&message)
                                );
                            }
                        } else if tracing::enabled!(tracing::Level::DEBUG) {
                            tracing::debug!(
                                target: "java_lsp::bus",
                                "sender={sender} notify {}",
                                describe_notification(&message)
                            );
                        }
                        messages::log_reporting(&message);
                        // A failed send is a dropped receiver: unsubscribe it.
                        subscribers.retain(|sink| sink.send(Bus::Notify(message.clone())).is_ok());
                    }
                    Inbound::Request {
                        sender,
                        id,
                        request,
                    } => {
                        let owner = owner_of(&request);
                        let desc = tracing::enabled!(tracing::Level::DEBUG)
                            .then(|| describe_request(&request));
                        if let Some(desc) = &desc {
                            tracing::debug!(
                                target: "java_lsp::bus",
                                "sender={sender} request {desc}"
                            );
                        }
                        pending.insert(
                            id,
                            Pending {
                                requester: sender,
                                desc,
                                owner,
                                started: Instant::now(),
                            },
                        );
                        // An unowned request, or one whose owner is gone, is
                        // dropped here: its reply resolves to the default.
                        let delivered = owners
                            .get(&owner)
                            .is_some_and(|sink| sink.send(Bus::Request(request)).is_ok());
                        if !delivered {
                            owners.remove(&owner);
                        }
                    }
                    Inbound::Reply { id, deliver } => {
                        // A dropped reply (`deliver: None`) is a request the owner
                        // never answered: forget it, but do not log a reply that
                        // did not happen.
                        let answered = deliver.is_some();
                        if let Some(done) = pending.remove(&id) {
                            if let Some(line) = reply_log(&done, answered) {
                                tracing::debug!(target: "java_lsp::bus", "{line}");
                            }
                        }
                        if let Some(deliver) = deliver {
                            deliver();
                        }
                    }
                    Inbound::Subscribe {
                        sender,
                        sink,
                        serves,
                    } => {
                        match serves {
                            Some(module) => {
                                tracing::debug!(
                                    target: "java_lsp::bus",
                                    "sender={sender} serve {module:?}"
                                );
                                let taken = owners
                                    .get(&module)
                                    .is_some_and(|owner| !owner.is_closed());
                                if taken {
                                    tracing::error!(
                                        target: "java_lsp::bus",
                                        "sender={sender} cannot serve {module:?}: it already has an owner; keeping the first"
                                    );
                                } else {
                                    owners.insert(module, sink.clone());
                                }
                            }
                            None => tracing::debug!(
                                target: "java_lsp::bus",
                                "sender={sender} subscribe"
                            ),
                        }
                        subscribers.push(sink);
                    }
                }
            }
        })
        .expect("spawn the hub thread");
    BusClient {
        inbound,
        sender: "engine".to_string(),
    }
}

impl BusClient {
    /// A standalone bus with only the index module: for a core used on its own
    /// (the unit tests, and `TreeSitterEngine::new`).
    pub fn standalone() -> Self {
        standalone_client()
    }

    /// Wraps an existing hub's client (production, where the engine owns the
    /// hub).
    pub fn from_client(client: BusClient) -> Self {
        client
    }
}

/// A standalone bus with only the index module.
fn standalone_client() -> BusClient {
    let client = spawn_hub();
    crate::index::spawn(&client);
    client.labeled("core")
}

/// A module that owns a request.
fn owner_of(request: &Request) -> Module {
    match request {
        Request::IndexQueryName { .. }
        | Request::IndexQueryNames { .. }
        | Request::IndexQueryPrefix { .. }
        | Request::IndexAllSymbols { .. }
        | Request::IndexFileCount { .. }
        | Request::IndexReady { .. }
        | Request::IndexHasPackage { .. }
        | Request::IndexHasPackages { .. }
        | Request::IndexTypeModel { .. }
        | Request::IndexTypeLayers { .. }
        | Request::IndexSourceFiles { .. }
        | Request::IndexSourceRoots { .. }
        | Request::IndexSourceLayerIndex { .. }
        | Request::IndexSourceModels { .. } => Module::Index,
        Request::DiagnosticsForDocument { .. } => Module::Diagnostics,
        Request::QuickFixForDocument { .. } => Module::QuickFix,
        Request::Analysis(_) => Module::Analysis,
    }
}

/// A request the hub is still waiting on: enough to name and time the reply.
struct Pending {
    requester: String,
    desc: Option<String>,
    owner: Module,
    started: Instant,
}

/// The hub-log name of the module that owns a request.
fn module_label(module: Module) -> &'static str {
    match module {
        Module::Index => "index",
        Module::Diagnostics => "diagnostics",
        Module::QuickFix => "quickfix",
        Module::Analysis => "analysis",
    }
}

/// The hub-log line for a reply, or `None` when there is nothing to log: an
/// unanswered request (`answered` false), or no description captured because
/// logging was off when the request arrived.
fn reply_log(done: &Pending, answered: bool) -> Option<String> {
    if !answered {
        return None;
    }
    let desc = done.desc.as_deref()?;
    Some(format!(
        "sender={} reply to={} {} elapsed={}ms",
        module_label(done.owner),
        done.requester,
        desc,
        done.started.elapsed().as_millis()
    ))
}

/// True for the high-cardinality, per-item notifications — one per source file,
/// artifact, or progress tick — that would drown the `debug` flow. The hub logs
/// these at `trace`; everything else stays at `debug`.
fn is_bulk(message: &DriverMessage) -> bool {
    matches!(
        message,
        DriverMessage::SourceFile { .. }
            | DriverMessage::BaseArtifact { .. }
            | DriverMessage::RemoveBase { .. }
            | DriverMessage::SourceEntries { .. }
            | DriverMessage::SourceRemoved { .. }
            | DriverMessage::SourceTypes { .. }
            | DriverMessage::DirtyTypes { .. }
            | DriverMessage::DirtyTypesDropped { .. }
            | DriverMessage::Progress(_)
    )
}

/// A concise description of a notification for the hub log: the variant and the
/// identifiers/counts that matter, never a payload — an open document's text is
/// logged as its size, so the log can never flood with source.
fn describe_notification(message: &DriverMessage) -> String {
    match message {
        DriverMessage::FolderAdded { uri } => format!("FolderAdded {uri}"),
        DriverMessage::FileEvent { uri, change } => format!("FileEvent {uri} {change:?}"),
        DriverMessage::ProjectModel { model } => format!("ProjectModel maven={}", model.maven),
        DriverMessage::SourceInventory { files } => {
            format!("SourceInventory files={}", files.len())
        }
        DriverMessage::Artifacts { artifacts } => {
            format!("Artifacts count={}", artifacts.len())
        }
        DriverMessage::SourceFile { uri, entries, .. } => {
            format!("SourceFile {uri} entries={}", entries.len())
        }
        DriverMessage::BaseArtifact { uri, entries, .. } => {
            format!("BaseArtifact {uri} entries={}", entries.len())
        }
        DriverMessage::RemoveBase { uri } => format!("RemoveBase {uri}"),
        DriverMessage::StageDone { stage, count } => format!("StageDone {stage:?} count={count}"),
        DriverMessage::Ready => "Ready".to_string(),
        DriverMessage::Diagnostics {
            uri,
            version,
            diagnostics,
        } => format!(
            "Diagnostics {uri} version={version:?} count={}",
            diagnostics.len()
        ),
        DriverMessage::DocumentOpened { uri, text, version } => {
            format!(
                "DocumentOpened {uri} version={version} chars={}",
                text.len()
            )
        }
        DriverMessage::DocumentChanged { uri, text, version } => {
            format!(
                "DocumentChanged {uri} version={version} chars={}",
                text.len()
            )
        }
        DriverMessage::DocumentClosed { uri } => format!("DocumentClosed {uri}"),
        DriverMessage::AnalysisUpdated => "AnalysisUpdated".to_string(),
        DriverMessage::ClientCapabilities {
            resource_operations,
        } => format!("ClientCapabilities resource_operations={resource_operations}"),
        DriverMessage::SourceEntries { uri, entries } => {
            format!("SourceEntries {uri} entries={}", entries.len())
        }
        DriverMessage::SourceRemoved { uri } => format!("SourceRemoved {uri}"),
        DriverMessage::DirtyTypes { uri, .. } => format!("DirtyTypes {uri}"),
        DriverMessage::DirtyTypesDropped { uri } => format!("DirtyTypesDropped {uri}"),
        DriverMessage::SourceTypes { uri, .. } => format!("SourceTypes {uri}"),
        DriverMessage::BaseTypes { .. } => "BaseTypes".to_string(),
        DriverMessage::Progress(update) => match update {
            crate::messages::ProgressUpdate::Begin { title, .. } => {
                format!("Progress begin {title}")
            }
            crate::messages::ProgressUpdate::Update { message, .. } => {
                format!("Progress update {message}")
            }
            crate::messages::ProgressUpdate::End { .. } => "Progress end".to_string(),
        },
        DriverMessage::Notice { level, text } => format!("Notice {level:?} {text}"),
        DriverMessage::Log { level, message } => format!("Log {level:?} {message}"),
        DriverMessage::Summary {
            files,
            jars,
            jdk_classes,
            maven,
            ..
        } => format!("Summary files={files} jars={jars} jdk_classes={jdk_classes} maven={maven}"),
    }
}

/// The same, for a request.
fn describe_request(request: &Request) -> String {
    match request {
        Request::IndexQueryName { name, .. } => format!("IndexQueryName {name}"),
        Request::IndexQueryNames { names, .. } => format!("IndexQueryNames count={}", names.len()),
        Request::IndexQueryPrefix { prefix, .. } => format!("IndexQueryPrefix {prefix}"),
        Request::IndexAllSymbols { .. } => "IndexAllSymbols".to_string(),
        Request::IndexFileCount { .. } => "IndexFileCount".to_string(),
        Request::IndexReady { .. } => "IndexReady".to_string(),
        Request::IndexHasPackage { package, .. } => format!("IndexHasPackage {package}"),
        Request::IndexHasPackages { packages, .. } => {
            format!("IndexHasPackages count={}", packages.len())
        }
        Request::IndexTypeModel { .. } => "IndexTypeModel".to_string(),
        Request::IndexTypeLayers { .. } => "IndexTypeLayers".to_string(),
        Request::IndexSourceFiles { .. } => "IndexSourceFiles".to_string(),
        Request::IndexSourceRoots { .. } => "IndexSourceRoots".to_string(),
        Request::IndexSourceLayerIndex { .. } => "IndexSourceLayerIndex".to_string(),
        Request::IndexSourceModels { .. } => "IndexSourceModels".to_string(),
        Request::DiagnosticsForDocument { uri, .. } => format!("DiagnosticsForDocument {uri}"),
        Request::QuickFixForDocument {
            uri, diagnostics, ..
        } => format!(
            "QuickFixForDocument {uri} diagnostics={}",
            diagnostics.len()
        ),
        Request::Analysis(request) => describe_analysis(request),
    }
}

/// The same, for an analysis query: the feature, the document, and the cursor.
fn describe_analysis(request: &AnalysisRequest) -> String {
    let at = |position: &Position| format!("{}:{}", position.line, position.character);
    match request {
        AnalysisRequest::Hover { uri, position, .. } => {
            format!("AnalysisHover {uri} {}", at(position))
        }
        AnalysisRequest::Definition { uri, position, .. } => {
            format!("AnalysisDefinition {uri} {}", at(position))
        }
        AnalysisRequest::Implementation { uri, position, .. } => {
            format!("AnalysisImplementation {uri} {}", at(position))
        }
        AnalysisRequest::Completions { uri, position, .. } => {
            format!("AnalysisCompletions {uri} {}", at(position))
        }
        AnalysisRequest::DocumentSymbols { uri, .. } => format!("AnalysisDocumentSymbols {uri}"),
        AnalysisRequest::FoldingRanges { uri, .. } => format!("AnalysisFoldingRanges {uri}"),
        AnalysisRequest::SemanticTokens { uri, .. } => format!("AnalysisSemanticTokens {uri}"),
        AnalysisRequest::InlayHints { uri, range, .. } => format!(
            "AnalysisInlayHints {uri} {}-{}",
            at(&range.start),
            at(&range.end)
        ),
        AnalysisRequest::SignatureHelp { uri, position, .. } => {
            format!("AnalysisSignatureHelp {uri} {}", at(position))
        }
        AnalysisRequest::References {
            uri,
            position,
            include_declaration,
            ..
        } => format!(
            "AnalysisReferences {uri} {} include_declaration={include_declaration}",
            at(position)
        ),
        AnalysisRequest::Rename {
            uri,
            position,
            new_name,
            ..
        } => format!("AnalysisRename {uri} {} to={new_name}", at(position)),
        AnalysisRequest::WorkspaceSymbols { query, .. } => {
            format!("AnalysisWorkspaceSymbols {query}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending_reply() -> Pending {
        Pending {
            requester: "core".to_string(),
            desc: Some("IndexQueryName sum".to_string()),
            owner: Module::Index,
            started: Instant::now(),
        }
    }

    #[test]
    fn an_answered_request_logs_a_reply_with_its_responder_and_requester() {
        let line = reply_log(&pending_reply(), true).expect("an answered request logs a reply");
        assert!(
            line.starts_with("sender=index reply to=core IndexQueryName sum elapsed="),
            "{line}"
        );
    }

    #[test]
    fn an_unanswered_request_is_not_logged_as_a_reply() {
        assert!(reply_log(&pending_reply(), false).is_none());
    }

    /// Answers one `IndexFileCount` request arriving on `rx` with `count`.
    fn answer_file_count(rx: &mut tokio_mpsc::UnboundedReceiver<Bus>, count: usize) {
        while let Some(message) = rx.blocking_recv() {
            if let Bus::Request(Request::IndexFileCount { reply }) = message {
                reply.send(count);
                return;
            }
        }
        panic!("the subscription closed before the request arrived");
    }

    #[test]
    fn a_request_reaches_the_first_module_serving_it() {
        let client = spawn_hub();
        let mut first = client.labeled("first").serve(Module::Index);
        // A second owner is a wiring bug: the hub keeps the first.
        let _second = client.labeled("second").serve(Module::Index);
        let reply = client.file_count();
        answer_file_count(&mut first, 7);
        assert_eq!(reply.blocking_recv(), 7);
    }

    #[test]
    fn a_dropped_owner_resolves_its_requests_to_the_default() {
        let client = spawn_hub();
        drop(client.serve(Module::Index));
        assert_eq!(client.file_count().blocking_recv(), 0);
        // The module is free again, so a new owner can serve it.
        let mut owner = client.serve(Module::Index);
        let reply = client.file_count();
        answer_file_count(&mut owner, 3);
        assert_eq!(reply.blocking_recv(), 3);
    }

    #[test]
    fn a_reply_is_not_logged_when_no_description_was_captured() {
        let mut done = pending_reply();
        done.desc = None;
        assert!(reply_log(&done, true).is_none());
    }
}
