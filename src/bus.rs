//! The engine bus: one mechanism every module uses.
//!
//! Modules never call each other directly and never share state. Each module
//! owns its own state and holds a [`BusClient`]; it sends **notifications**
//! (which the hub broadcasts to every module — each consumes the ones it cares
//! about) and **requests** (which the hub routes to the one module that answers
//! them, the reply riding the same bus). The hub itself is a thread, so a module
//! may block on a request from any thread — including a runtime worker — without
//! deadlocking.
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
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use tokio::sync::mpsc as tokio_mpsc;
use tower_lsp::lsp_types::{CodeAction, Diagnostic, Url};

use crate::index::SymbolEntry;
use crate::messages::{self, Bus, DriverMessage, EngineEvent, Inbound, ReplyHandle, Request};
use crate::project::ProjectModel;
use crate::types::{ModelLayers, SourceLayerIndex, TypeModel};

/// Which module owns a request.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Module {
    Index,
    Diagnostics,
    QuickFix,
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

    /// Sends a request to the hub, which routes it to the owning module, and
    /// waits for the reply. Degrades to the type's default if the bus is gone.
    fn request<R, F>(&self, build: F) -> R
    where
        R: Default + Send + 'static,
        F: FnOnce(ReplyHandle<R>) -> Request,
    {
        let (tx, rx) = std_mpsc::channel();
        let id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let reply = ReplyHandle::new(self.inbound.clone(), id, tx);
        if self
            .inbound
            .send(Inbound::Request {
                sender: self.sender.clone(),
                id,
                request: build(reply),
            })
            .is_err()
        {
            return R::default();
        }
        rx.recv().unwrap_or_default()
    }

    // -- index subsystem ---------------------------------------------------

    pub fn query_name(&self, name: &str) -> Vec<Arc<SymbolEntry>> {
        let name = name.to_string();
        self.request(move |reply| Request::IndexQueryName { name, reply })
    }

    /// Exact-name lookups for many names in one round trip; every requested name
    /// has an entry in the result.
    pub fn query_names(&self, names: Vec<String>) -> HashMap<String, Vec<Arc<SymbolEntry>>> {
        self.request(move |reply| Request::IndexQueryNames { names, reply })
    }

    pub fn query_prefix(&self, prefix: &str) -> Vec<Arc<SymbolEntry>> {
        let prefix = prefix.to_string();
        self.request(move |reply| Request::IndexQueryPrefix { prefix, reply })
    }

    pub fn all_symbols(&self) -> Vec<SymbolEntry> {
        self.request(|reply| Request::IndexAllSymbols { reply })
    }

    pub fn file_count(&self) -> usize {
        self.request(|reply| Request::IndexFileCount { reply })
    }

    pub fn ready(&self) -> bool {
        self.request(|reply| Request::IndexReady { reply })
    }

    pub fn has_package(&self, package: &str) -> bool {
        let package = package.to_string();
        self.request(move |reply| Request::IndexHasPackage { package, reply })
    }

    /// The packages among `packages` the index knows, in one round trip.
    pub fn has_packages(&self, packages: Vec<String>) -> HashSet<String> {
        self.request(move |reply| Request::IndexHasPackages { packages, reply })
    }

    pub fn type_model(&self) -> Option<Arc<SourceLayerIndex>> {
        self.request(|reply| Request::IndexTypeModel { reply })
    }

    pub fn type_layers(&self) -> ModelLayers {
        self.request(|reply| Request::IndexTypeLayers { reply })
    }

    pub fn source_files(&self) -> Vec<Url> {
        self.request(|reply| Request::IndexSourceFiles { reply })
    }

    pub fn source_roots(&self) -> Vec<PathBuf> {
        self.request(|reply| Request::IndexSourceRoots { reply })
    }

    pub fn source_layer_index(&self) -> Arc<SourceLayerIndex> {
        self.request(|reply| Request::IndexSourceLayerIndex { reply })
    }

    pub fn source_models(&self) -> Vec<(Url, Arc<TypeModel>)> {
        self.request(|reply| Request::IndexSourceModels { reply })
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
    pub fn diagnostics(&self, uri: &Url) -> Option<(i32, Arc<Vec<Diagnostic>>)> {
        let uri = uri.clone();
        self.request(move |reply| Request::DiagnosticsForDocument { uri, reply })
    }

    // -- quick-fix subsystem -----------------------------------------------

    /// The quick fixes for `diagnostics` in `uri`.
    pub fn code_actions(&self, uri: &Url, diagnostics: Vec<Diagnostic>) -> Vec<CodeAction> {
        let uri = uri.clone();
        self.request(move |reply| Request::QuickFixForDocument {
            uri,
            diagnostics,
            reply,
        })
    }
}

/// Starts the hub thread. Every module registered in `modules` receives every
/// notification as a [`Bus`] value; every sink in `notifications` receives the
/// bare [`DriverMessage`] (the drivers). A request is routed to the module
/// registered in `owners`. The returned client is how the shell, the core, and
/// the modules reach the bus.
pub fn spawn_router(
    events: tokio_mpsc::UnboundedSender<EngineEvent>,
    modules: Vec<tokio_mpsc::UnboundedSender<Bus>>,
    notifications: Vec<tokio_mpsc::UnboundedSender<DriverMessage>>,
    owners: HashMap<Module, tokio_mpsc::UnboundedSender<Bus>>,
) -> BusClient {
    let (inbound, mut rx) = tokio_mpsc::unbounded_channel::<Inbound>();
    thread::Builder::new()
        .name("java-lsp-hub".to_string())
        .spawn(move || {
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
                        // The hub is the sole translator to the editor.
                        messages::translate(&message, &events);
                        for sink in &modules {
                            let _ = sink.send(Bus::Notify(message.clone()));
                        }
                        for sink in &notifications {
                            let _ = sink.send(message.clone());
                        }
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
                        if let Some(sink) = owners.get(&owner) {
                            let _ = sink.send(Bus::Request(request));
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
    let (events, _events_rx) = tokio_mpsc::unbounded_channel();
    let (index, index_rx) = channel();
    crate::index::spawn_index_module(index_rx);
    let mut owners = HashMap::new();
    owners.insert(Module::Index, index.clone());
    spawn_router(events, vec![index], Vec::new(), owners).labeled("core")
}

/// A bus channel: the sink a module registers with the hub, and its receiver.
pub fn channel() -> (
    tokio_mpsc::UnboundedSender<Bus>,
    tokio_mpsc::UnboundedReceiver<Bus>,
) {
    tokio_mpsc::unbounded_channel()
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

    #[test]
    fn a_reply_is_not_logged_when_no_description_was_captured() {
        let mut done = pending_reply();
        done.desc = None;
        assert!(reply_log(&done, true).is_none());
    }
}
