//! The shell <-> engine boundary: commands in, events out, over channels.
//!
//! The shell holds an [`EngineHandle`] and never touches the analysis core
//! directly. Each request sends a [`Command`] carrying a oneshot reply; the
//! dispatcher task owns the [`TreeSitterEngine`] core, applies mutations and
//! orchestration inline (in arrival order — the one place ordering matters) and
//! runs read-only queries on spawned tasks, so a slow query never delays
//! typing. [`EngineEvent`] is the reverse channel: the shell drains it and turns
//! each event into a client notification (today, diagnostics; later, warm-up
//! progress).

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

/// How many commands may queue before a sender waits. Generous: commands are
/// small and only mutations hold the dispatcher.
const COMMANDS_IN_FLIGHT: usize = 64;

type Reply<T> = oneshot::Sender<T>;

/// Commands the shell sends to the engine. Queries carry a `oneshot` reply.
pub enum Command {
    /// The workspace root is known; the background warm-up may start.
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
    /// editor reported but never opened; the engine re-indexes or drops them.
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
#[derive(Debug)]
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

/// A cheap, cloneable handle the background job reports through. A detached
/// reporter (the default) drops everything, so inline scans and unit tests stay
/// silent.
#[derive(Debug, Clone, Default)]
pub struct Reporter {
    events: Option<mpsc::UnboundedSender<EngineEvent>>,
}

impl Reporter {
    /// A reporter that forwards to the shell's event channel.
    pub fn attached(events: mpsc::UnboundedSender<EngineEvent>) -> Self {
        Self {
            events: Some(events),
        }
    }

    pub fn begin(&self, title: &str, message: &str) {
        self.send(EngineEvent::Progress(ProgressUpdate::Begin {
            title: title.to_string(),
            message: message.to_string(),
        }));
    }

    pub fn update(&self, message: String, percentage: Option<u32>) {
        self.send(EngineEvent::Progress(ProgressUpdate::Update {
            message,
            percentage,
        }));
    }

    pub fn end(&self, message: Option<String>) {
        self.send(EngineEvent::Progress(ProgressUpdate::End { message }));
    }

    pub fn message(&self, level: MessageLevel, text: String) {
        self.send(EngineEvent::Message { level, text });
    }

    fn send(&self, event: EngineEvent) {
        if let Some(events) = &self.events {
            let _ = events.send(event);
        }
    }
}

/// A cheap, cloneable handle to the engine task.
#[derive(Clone)]
pub struct EngineHandle {
    commands: mpsc::Sender<Command>,
}

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

/// Spawns the engine task over a fresh core and returns its handle. `events` is
/// the reverse channel; the caller drains it into client notifications.
pub fn spawn(events: mpsc::UnboundedSender<EngineEvent>) -> EngineHandle {
    let (commands, mut incoming) = mpsc::channel(COMMANDS_IN_FLIGHT);
    let engine = Arc::new(TreeSitterEngine::new());
    // The warm-up reports through the same channel the dispatcher pushes to.
    engine.set_reporter(Reporter::attached(events.clone()));
    tokio::spawn(async move {
        // The last version seen per open URI, so a diagnostics republish that
        // covers other documents can stamp them correctly.
        let mut versions: HashMap<Url, i32> = HashMap::new();
        while let Some(command) = incoming.recv().await {
            dispatch(command, &engine, &events, &mut versions);
        }
    });
    EngineHandle { commands }
}

/// Applies mutations and orchestration inline, in arrival order, and hands
/// read-only queries to spawned tasks so none can delay a later command.
fn dispatch(
    command: Command,
    engine: &Arc<TreeSitterEngine>,
    events: &mpsc::UnboundedSender<EngineEvent>,
    versions: &mut HashMap<Url, i32>,
) {
    match command {
        Command::SetWorkspaceRoot(root) => engine.set_workspace_root(&root),
        Command::SetClientCapabilities {
            resource_operations,
        } => engine.set_resource_operations(resource_operations),
        Command::Open { uri, text, version } => {
            engine.open(&uri, &text);
            versions.insert(uri.clone(), version);
            publish_all_diagnostics(engine, events, versions, Some(&uri));
        }
        Command::Change { uri, text, version } => {
            engine.change(&uri, &text);
            versions.insert(uri.clone(), version);
            publish_all_diagnostics(engine, events, versions, Some(&uri));
        }
        Command::Close(uri) => {
            engine.close(&uri);
            versions.remove(&uri);
            let _ = events.send(EngineEvent::Diagnostics {
                uri,
                version: None,
                diagnostics: Vec::new(),
            });
            publish_all_diagnostics(engine, events, versions, None);
        }
        Command::WatchedFiles { changes } => {
            // The index or model changed, so files that can see the change must
            // be re-analysed even though they were not edited (D3).
            if engine.watched_files(&changes) {
                publish_all_diagnostics(engine, events, versions, None);
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
        } => read(
            engine,
            move |engine| engine.code_actions(&uri, &diagnostics),
            reply,
        ),
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

/// Computes and emits diagnostics for **every** open document after an index or
/// model change, so a referring file that was not itself edited refreshes too
/// (D3). `preferred`, when given, is published first. Cost is bounded by the
/// number of open documents and runs on the dispatcher's inline path; the bench
/// is what decides if that ever needs to move.
fn publish_all_diagnostics(
    engine: &TreeSitterEngine,
    events: &mpsc::UnboundedSender<EngineEvent>,
    versions: &HashMap<Url, i32>,
    preferred: Option<&Url>,
) {
    let mut uris = engine.open_documents();
    if let Some(preferred) = preferred {
        if let Some(index) = uris.iter().position(|uri| uri == preferred) {
            let uri = uris.remove(index);
            uris.insert(0, uri);
        }
    }
    for uri in uris {
        let version = versions.get(&uri).copied();
        let diagnostics = engine.diagnostics(&uri);
        let _ = events.send(EngineEvent::Diagnostics {
            uri,
            version,
            diagnostics,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reporter_forwards_progress_and_messages() {
        let (events, mut received) = mpsc::unbounded_channel();
        let reporter = Reporter::attached(events);

        reporter.begin("java-lsp", "Indexing workspace");
        reporter.update("Indexed 3 source files".to_string(), None);
        reporter.update("Fetched 1/2 dependency sources".to_string(), Some(50));
        reporter.message(MessageLevel::Info, "note".to_string());
        reporter.end(Some("done".to_string()));

        let mut kinds = Vec::new();
        while let Ok(event) = received.try_recv() {
            match event {
                EngineEvent::Progress(ProgressUpdate::Begin { title, message }) => {
                    assert_eq!(title, "java-lsp");
                    assert_eq!(message, "Indexing workspace");
                    kinds.push("begin");
                }
                EngineEvent::Progress(ProgressUpdate::Update {
                    message,
                    percentage,
                }) => {
                    let expected = message.starts_with("Fetched").then_some(50);
                    assert_eq!(percentage, expected, "{message}");
                    kinds.push("update");
                }
                EngineEvent::Progress(ProgressUpdate::End { message }) => {
                    assert_eq!(message.as_deref(), Some("done"));
                    kinds.push("end");
                }
                EngineEvent::Message { level, text } => {
                    assert_eq!(level, MessageLevel::Info);
                    assert_eq!(text, "note");
                    kinds.push("message");
                }
                EngineEvent::Diagnostics { .. } => panic!("unexpected diagnostics"),
            }
        }
        assert_eq!(kinds, vec!["begin", "update", "update", "message", "end"]);
    }

    #[test]
    fn a_detached_reporter_is_silent() {
        let reporter = Reporter::default();
        reporter.begin("t", "m");
        reporter.update("u".to_string(), None);
        reporter.end(None);
        reporter.message(MessageLevel::Warning, "w".to_string());
    }
}
