//! The engine: the shell boundary, the message hub, and the drivers.
//!
//! The shell holds an [`EngineHandle`], sends [`Command`]s (or awaits a reply),
//! and drains [`EngineEvent`]s. The engine owns the analysis core and a message
//! bus: every subsystem is a driver that emits [`DriverMessage`]s onto the bus;
//! the hub task hands the index-affecting ones to the index subsystem, relays
//! every message to every driver, and is the sole place that turns a message
//! into an [`EngineEvent`] for the shell. Diagnostics are not computed on this
//! path: a mutation only forwards the document to the diagnostics subsystem
//! ([`crate::diagnostics`]), which sweeps on a blocking task, coalesced.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tower_lsp::lsp_types::{
    CodeAction, CompletionResponse, Diagnostic, DocumentSymbol, FoldingRange, Hover, InlayHint,
    Location, Position, Range, SemanticTokens, SignatureHelp, SymbolInformation, Url,
    WorkspaceEdit,
};

use crate::analysis::TreeSitterEngine;
use crate::index::SymbolEntry;
use crate::messages::{
    Command, DriverMessage, EngineEvent, MessageLevel, ProgressUpdate, Stage, WatchedChange,
};

/// How many commands may queue before a sender waits. Generous: commands are
/// small and only mutations hold the dispatcher.
const COMMANDS_IN_FLIGHT: usize = 64;

type Reply<T> = oneshot::Sender<T>;

impl EngineHandle {
    pub async fn set_workspace_root(&self, root: Url) {
        let _ = self.commands.send(Command::SetWorkspaceRoot(root)).await;
    }

    /// Tells the engine whether the client advertised `CreateFile`, so it may
    /// offer the create-type quick fix.
    pub async fn set_resource_operations(&self, supported: bool) {
        let _ = self
            .commands
            .send(Command::SetClientCapabilities {
                resource_operations: supported,
            })
            .await;
    }

    pub async fn open(&self, uri: Url, text: String, version: i32) {
        let _ = self
            .commands
            .send(Command::Open { uri, text, version })
            .await;
    }

    pub async fn change(&self, uri: Url, text: String, version: i32) {
        let _ = self
            .commands
            .send(Command::Change { uri, text, version })
            .await;
    }

    pub async fn close(&self, uri: Url) {
        let _ = self.commands.send(Command::Close(uri)).await;
    }

    /// Reports watched filesystem events for the engine to index or drop.
    pub async fn watched_files(&self, changes: Vec<(Url, WatchedChange)>) {
        let _ = self.commands.send(Command::WatchedFiles { changes }).await;
    }

    pub async fn hover(&self, uri: Url, position: Position) -> Option<Hover> {
        self.request(|reply| Command::Hover {
            uri,
            position,
            reply,
        })
        .await
        .flatten()
    }

    pub async fn definition(&self, uri: Url, position: Position) -> Option<Location> {
        self.request(|reply| Command::Definition {
            uri,
            position,
            reply,
        })
        .await
        .flatten()
    }

    pub async fn implementation(&self, uri: Url, position: Position) -> Vec<Location> {
        self.request(|reply| Command::Implementation {
            uri,
            position,
            reply,
        })
        .await
        .unwrap_or_default()
    }

    pub async fn completions(&self, uri: Url, position: Position) -> Option<CompletionResponse> {
        self.request(|reply| Command::Completions {
            uri,
            position,
            reply,
        })
        .await
        .flatten()
    }

    pub async fn document_symbols(&self, uri: Url) -> Option<Vec<DocumentSymbol>> {
        self.request(|reply| Command::DocumentSymbols { uri, reply })
            .await
            .flatten()
    }

    pub async fn folding_ranges(&self, uri: Url) -> Option<Vec<FoldingRange>> {
        self.request(|reply| Command::FoldingRanges { uri, reply })
            .await
            .flatten()
    }

    pub async fn semantic_tokens(&self, uri: Url) -> Option<SemanticTokens> {
        self.request(|reply| Command::SemanticTokens { uri, reply })
            .await
            .flatten()
    }

    pub async fn inlay_hints(&self, uri: Url, range: Range) -> Vec<InlayHint> {
        self.request(|reply| Command::InlayHints { uri, range, reply })
            .await
            .unwrap_or_default()
    }

    pub async fn signature_help(&self, uri: Url, position: Position) -> Option<SignatureHelp> {
        self.request(|reply| Command::SignatureHelp {
            uri,
            position,
            reply,
        })
        .await
        .flatten()
    }

    pub async fn references(
        &self,
        uri: Url,
        position: Position,
        include_declaration: bool,
    ) -> Vec<Location> {
        self.request(|reply| Command::References {
            uri,
            position,
            include_declaration,
            reply,
        })
        .await
        .unwrap_or_default()
    }

    pub async fn rename(
        &self,
        uri: Url,
        position: Position,
        new_name: String,
    ) -> Option<WorkspaceEdit> {
        self.request(|reply| Command::Rename {
            uri,
            position,
            new_name,
            reply,
        })
        .await
        .flatten()
    }

    pub async fn workspace_symbols(&self, query: String) -> Vec<SymbolInformation> {
        self.request(|reply| Command::WorkspaceSymbols { query, reply })
            .await
            .unwrap_or_default()
    }

    pub async fn code_actions(&self, uri: Url, diagnostics: Vec<Diagnostic>) -> Vec<CodeAction> {
        self.request(|reply| Command::CodeActions {
            uri,
            diagnostics,
            reply,
        })
        .await
        .unwrap_or_default()
    }

    pub async fn indexed_symbols(&self) -> Vec<SymbolEntry> {
        self.request(|reply| Command::IndexedSymbols { reply })
            .await
            .unwrap_or_default()
    }

    pub async fn index_ready(&self) -> bool {
        self.request(|reply| Command::IndexReady { reply })
            .await
            .unwrap_or(true)
    }

    /// Sends a command and awaits its reply, or `None` when the engine is gone
    /// (its task exited) — the shell then falls back to the empty result.
    async fn request<T>(&self, build: impl FnOnce(Reply<T>) -> Command) -> Option<T> {
        let (reply, response) = oneshot::channel();
        self.commands.send(build(reply)).await.ok()?;
        response.await.ok()
    }
}

/// The filesystem driver's input: the root the shell announced, and the client's
/// watched-file events.

/// Spawns the engine task over a fresh core, the hub, every module, and the
/// drivers. `events` is the reverse channel; the caller drains it into client
/// notifications.
pub fn spawn(events: mpsc::UnboundedSender<EngineEvent>) -> EngineHandle {
    let (commands, mut incoming) = mpsc::channel(COMMANDS_IN_FLIGHT);

    // The index module owns the symbol index: it answers the requests the hub
    // routes to it and applies the index-affecting notifications it is
    // broadcast.
    let (index, index_rx) = crate::bus::channel();
    crate::index::spawn_index_module(index_rx);

    // Drivers receive notifications on their own channel.
    let (project_tx, project_rx) = mpsc::unbounded_channel();
    let (dependency_tx, dependency_rx) = mpsc::unbounded_channel();
    let (source_tx, source_rx) = mpsc::unbounded_channel();
    let (jar_tx, jar_rx) = mpsc::unbounded_channel();
    let (jdk_tx, jdk_rx) = mpsc::unbounded_channel();
    let (download_tx, download_rx) = mpsc::unbounded_channel();

    // The hub: broadcasts every notification to every module and driver, routes
    // each request to the module that owns it, and is the sole translator to the
    // editor.
    let (diagnostics, diagnostics_rx) = crate::bus::channel();
    let (quickfix, quickfix_rx) = crate::bus::channel();
    let mut owners = HashMap::new();
    owners.insert(crate::bus::Module::Index, index.clone());
    owners.insert(crate::bus::Module::Diagnostics, diagnostics.clone());
    owners.insert(crate::bus::Module::QuickFix, quickfix.clone());
    let client = crate::bus::spawn_router(
        events.clone(),
        vec![index, diagnostics, quickfix],
        vec![
            project_tx,
            dependency_tx,
            source_tx,
            jar_tx,
            jdk_tx,
            download_tx,
        ],
        owners,
    );

    let engine = Arc::new(TreeSitterEngine::with_index(client.labeled("core")));
    engine.set_events(events.clone());

    // Each sender logs under its own name: the core, the dispatcher task, the six
    // drivers, and the two modules are distinct in the hub log.
    tokio::spawn(project_driver(project_rx, client.labeled("project")));
    tokio::spawn(dependency_driver(
        dependency_rx,
        client.labeled("dependency"),
    ));
    tokio::spawn(source_driver(source_rx, client.labeled("source")));
    tokio::spawn(jar_driver(jar_rx, client.labeled("jar")));
    tokio::spawn(jdk_driver(jdk_rx, client.labeled("jdk")));
    tokio::spawn(download_driver(download_rx, client.labeled("download")));

    crate::diagnostics::spawn_module(diagnostics_rx, client.labeled("diagnostics"));
    crate::quickfix::spawn_module(quickfix_rx, client.labeled("quickfix"));

    // The dispatcher: mutations inline in arrival order, queries spawned.
    let dispatcher_client = client.labeled("dispatch");
    let dispatcher_events = events.clone();
    tokio::spawn(async move {
        while let Some(command) = incoming.recv().await {
            dispatch(command, &engine, &dispatcher_events, &dispatcher_client);
        }
    });
    EngineHandle { commands }
}

/// The project driver and the warm-up coordinator: on an added folder it walks
/// for the Maven model and the source inventory, and once the core stages and the
/// downloader have reported it flips `ready` and closes the progress item.
async fn project_driver(
    mut rx: mpsc::UnboundedReceiver<DriverMessage>,
    client: crate::bus::BusClient,
) {
    let mut root: Option<Url> = None;
    let mut started: Option<std::time::Instant> = None;
    let mut maven = false;
    let mut counts: HashMap<Stage, usize> = HashMap::new();
    let mut ready_sent = false;
    let mut summary_sent = false;
    while let Some(message) = rx.recv().await {
        match message {
            DriverMessage::FolderAdded { uri } if root.is_none() => {
                root = Some(uri.clone());
                started = Some(std::time::Instant::now());
                let _ = client.notify(DriverMessage::Progress(ProgressUpdate::Begin {
                    title: "java-lsp".to_string(),
                    message: "Indexing workspace".to_string(),
                }));
                let client = client.clone();
                let _ = tokio::task::spawn_blocking(move || match uri.to_file_path() {
                    Ok(path) => {
                        let (model, files) = crate::index::walk_project(&path);
                        let _ = client.notify(DriverMessage::ProjectModel {
                            model: Arc::new(model),
                        });
                        let _ = client.notify(DriverMessage::SourceInventory {
                            files: Arc::new(files),
                        });
                    }
                    Err(_) => {
                        let _ = client.notify(DriverMessage::ProjectModel {
                            model: Arc::new(crate::project::ProjectModel::default()),
                        });
                        let _ = client.notify(DriverMessage::SourceInventory {
                            files: Arc::new(Vec::new()),
                        });
                    }
                })
                .await;
            }
            DriverMessage::ProjectModel { model } => {
                maven = model.maven;
            }
            DriverMessage::StageDone { stage, count } => {
                counts.insert(stage, count);
                if !ready_sent
                    && counts.contains_key(&Stage::Sources)
                    && counts.contains_key(&Stage::Jars)
                    && counts.contains_key(&Stage::Jdk)
                {
                    ready_sent = true;
                    let _ = client.notify(DriverMessage::Ready);
                }
                if ready_sent && !summary_sent && counts.contains_key(&Stage::Downloads) {
                    summary_sent = true;
                    let files = counts.get(&Stage::Sources).copied().unwrap_or(0);
                    let jars = counts.get(&Stage::Jars).copied().unwrap_or(0);
                    let jdk_classes = counts.get(&Stage::Jdk).copied().unwrap_or(0);
                    let elapsed_ms = started
                        .map(|at| at.elapsed().as_millis() as u64)
                        .unwrap_or(0);
                    let root = root.as_ref().map(Url::to_string).unwrap_or_default();
                    let _ = client.notify(DriverMessage::Summary {
                        root,
                        files,
                        jars,
                        jdk_classes,
                        maven,
                        elapsed_ms,
                    });
                    let _ = client.notify(DriverMessage::Progress(ProgressUpdate::End {
                        message: Some(format!(
                            "Indexed {files} files, {jars} dependency jars, {jdk_classes} JDK classes"
                        )),
                    }));
                }
            }
            _ => {}
        }
    }
}

/// The dependency driver: on the project model, resolves each module's closure
/// against the local repository and emits the jar list (and the offline notice).
async fn dependency_driver(
    mut rx: mpsc::UnboundedReceiver<DriverMessage>,
    client: crate::bus::BusClient,
) {
    while let Some(message) = rx.recv().await {
        if let DriverMessage::ProjectModel { model } = message {
            let client = client.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let artifacts = crate::index::resolve_artifacts(&model);
                if let Some(text) = crate::index::offline_notice(artifacts.len()) {
                    let _ = client.notify(DriverMessage::Notice {
                        level: MessageLevel::Info,
                        text,
                    });
                }
                let _ = client.notify(DriverMessage::Artifacts {
                    artifacts: Arc::new(artifacts),
                });
            })
            .await;
        }
    }
}

/// The source scanner: on the source inventory, parses each file and emits its
/// entries and model, then the `Sources` stage-done.
async fn source_driver(
    mut rx: mpsc::UnboundedReceiver<DriverMessage>,
    client: crate::bus::BusClient,
) {
    while let Some(message) = rx.recv().await {
        if let DriverMessage::SourceInventory { files } = message {
            let client = client.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let count = {
                    let mut sink = |message| {
                        let _ = client.notify(message);
                    };
                    crate::index::scan_sources(&files, &mut sink)
                };
                let _ = client.notify(DriverMessage::StageDone {
                    stage: Stage::Sources,
                    count,
                });
            })
            .await;
        }
    }
}

/// The jar indexer: on the artifact list, reads each jar and emits its entries
/// and model, then the `Jars` stage-done.
async fn jar_driver(mut rx: mpsc::UnboundedReceiver<DriverMessage>, client: crate::bus::BusClient) {
    while let Some(message) = rx.recv().await {
        if let DriverMessage::Artifacts { artifacts } = message {
            let client = client.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let count = {
                    let mut sink = |message| {
                        let _ = client.notify(message);
                    };
                    crate::index::index_jars(&artifacts, &mut sink)
                };
                let _ = client.notify(DriverMessage::StageDone {
                    stage: Stage::Jars,
                    count,
                });
            })
            .await;
        }
    }
}

/// The JDK indexer: runs once at start, independent of the workspace, and emits
/// its archives' entries and models, then the `Jdk` stage-done.
async fn jdk_driver(_rx: mpsc::UnboundedReceiver<DriverMessage>, client: crate::bus::BusClient) {
    let client = client.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let count = {
            let mut sink = |message| {
                let _ = client.notify(message);
            };
            crate::index::index_jdk(&mut sink)
        };
        let _ = client.notify(DriverMessage::StageDone {
            stage: Stage::Jdk,
            count,
        });
    })
    .await;
}

/// The source downloader: on the artifact list, fetches and indexes dependency
/// sources, then the `Downloads` stage-done (which gates only the summary).
async fn download_driver(
    mut rx: mpsc::UnboundedReceiver<DriverMessage>,
    client: crate::bus::BusClient,
) {
    while let Some(message) = rx.recv().await {
        if let DriverMessage::Artifacts { artifacts } = message {
            crate::sources::index_sources((*artifacts).clone(), client.clone()).await;
            let _ = client.notify(DriverMessage::StageDone {
                stage: Stage::Downloads,
                count: 0,
            });
        }
    }
}

/// Dispatches a command: filesystem commands go to the filesystem driver,
/// mutations are applied inline in arrival order, and read-only queries are
/// spawned so none can delay a later command.
fn dispatch(
    command: Command,
    engine: &Arc<TreeSitterEngine>,
    events: &mpsc::UnboundedSender<EngineEvent>,
    client: &crate::bus::BusClient,
) {
    match command {
        Command::SetWorkspaceRoot(root) => {
            engine.set_workspace_root(&root);
            client.notify(DriverMessage::FolderAdded { uri: root });
        }
        Command::SetClientCapabilities {
            resource_operations,
        } => {
            engine.set_resource_operations(resource_operations);
            client.notify(DriverMessage::ClientCapabilities {
                resource_operations,
            });
        }
        Command::Open { uri, text, version } => {
            engine.open(&uri, &text, version);
            client.notify(DriverMessage::DocumentOpened {
                uri,
                text: Arc::new(text),
                version,
            });
        }
        Command::Change { uri, text, version } => {
            engine.change(&uri, &text, version);
            client.notify(DriverMessage::DocumentChanged {
                uri,
                text: Arc::new(text),
                version,
            });
        }
        Command::Close(uri) => {
            engine.close(&uri);
            // Cheap, no analysis: clear the closed document inline, then let the
            // sweep republish the rest off the dispatcher.
            let _ = events.send(EngineEvent::Diagnostics {
                uri: uri.clone(),
                version: None,
                diagnostics: Vec::new(),
            });
            client.notify(DriverMessage::DocumentClosed { uri });
        }
        Command::WatchedFiles { changes } => {
            // Re-read the changed files into the core, then tell the bus so the
            // diagnostics subsystem republishes the documents that can see the
            // change.
            engine.watched_files(&changes);
            for (uri, change) in changes {
                client.notify(DriverMessage::FileEvent { uri, change });
            }
        }
        Command::Hover {
            uri,
            position,
            reply,
        } => read(engine, move |engine| engine.hover(&uri, position), reply),
        Command::Definition {
            uri,
            position,
            reply,
        } => read(
            engine,
            move |engine| engine.definition(&uri, position),
            reply,
        ),
        Command::Implementation {
            uri,
            position,
            reply,
        } => read(
            engine,
            move |engine| engine.implementation(&uri, position),
            reply,
        ),
        Command::Completions {
            uri,
            position,
            reply,
        } => read(
            engine,
            move |engine| engine.completions(&uri, position),
            reply,
        ),
        Command::DocumentSymbols { uri, reply } => {
            read(engine, move |engine| engine.document_symbols(&uri), reply)
        }
        Command::FoldingRanges { uri, reply } => {
            read(engine, move |engine| engine.folding_ranges(&uri), reply)
        }
        Command::SemanticTokens { uri, reply } => {
            read(engine, move |engine| engine.semantic_tokens(&uri), reply)
        }
        Command::InlayHints { uri, range, reply } => {
            read(engine, move |engine| engine.inlay_hints(&uri, range), reply)
        }
        Command::SignatureHelp {
            uri,
            position,
            reply,
        } => read(
            engine,
            move |engine| engine.signature_help(&uri, position),
            reply,
        ),
        Command::References {
            uri,
            position,
            include_declaration,
            reply,
        } => read(
            engine,
            move |engine| engine.references(&uri, position, include_declaration),
            reply,
        ),
        Command::Rename {
            uri,
            position,
            new_name,
            reply,
        } => read(
            engine,
            move |engine| engine.rename(&uri, position, &new_name),
            reply,
        ),
        Command::WorkspaceSymbols { query, reply } => read(
            engine,
            move |engine| engine.workspace_symbols(&query),
            reply,
        ),
        Command::CodeActions {
            uri,
            diagnostics,
            reply,
        } => {
            // The quick-fix module answers through the bus; blocking for the
            // reply uses the blocking pool, so a runtime worker is never starved.
            let client = client.clone();
            tokio::task::spawn_blocking(move || {
                let _ = reply.send(client.code_actions(&uri, diagnostics));
            });
        }
        Command::IndexedSymbols { reply } => read(engine, |engine| engine.indexed_symbols(), reply),
        Command::IndexReady { reply } => read(engine, |engine| engine.index_ready(), reply),
    }
}

/// Runs one query on a spawned task and sends its reply. The dispatcher does
/// not wait for it, so a slow query holds up nothing.
fn read<T: Send + 'static>(
    engine: &Arc<TreeSitterEngine>,
    query: impl FnOnce(&TreeSitterEngine) -> T + Send + 'static,
    reply: Reply<T>,
) {
    let engine = Arc::clone(engine);
    tokio::spawn(async move {
        let _ = reply.send(query(engine.as_ref()));
    });
}

/// A cheap, cloneable handle to the engine task.
#[derive(Clone)]
pub struct EngineHandle {
    commands: mpsc::Sender<Command>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexHandle;

    #[test]
    fn the_index_subsystem_applies_messages_and_translate_emits_events() {
        let index = IndexHandle::standalone();
        let (events, mut received) = mpsc::unbounded_channel();
        let jar = Url::parse("file:///lib.jar").unwrap();

        index.apply(DriverMessage::BaseArtifact {
            uri: jar,
            entries: Arc::new(Vec::new()),
            types: Arc::new(crate::types::TypeModel::new()),
        });
        crate::messages::translate(
            &DriverMessage::Progress(ProgressUpdate::Update {
                message: "mid".to_string(),
                percentage: Some(3),
            }),
            &events,
        );
        crate::messages::translate(
            &DriverMessage::Notice {
                level: MessageLevel::Info,
                text: "note".to_string(),
            },
            &events,
        );
        index.apply(DriverMessage::Ready);

        assert!(index.ready());
        assert!(index.type_model().is_some());
        match received.try_recv().expect("progress event") {
            EngineEvent::Progress(ProgressUpdate::Update {
                message,
                percentage,
            }) => {
                assert_eq!(message, "mid");
                assert_eq!(percentage, Some(3));
            }
            other => panic!("unexpected event: {other:?}"),
        }
        match received.try_recv().expect("notice event") {
            EngineEvent::Message { level, text } => {
                assert_eq!(level, MessageLevel::Info);
                assert_eq!(text, "note");
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(received.try_recv().is_err(), "no further events");
    }
}
