//! The LSP shell: editor-facing handlers that delegate all analysis to the
//! engine over its command/event boundary ([`crate::engine`]).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, RwLock};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{
    notification::Progress, request::WorkDoneProgressCreate, CodeActionKind, CodeActionOptions,
    CodeActionOrCommand, CodeActionParams, CodeActionProviderCapability, CodeActionResponse,
    CompletionOptions, CompletionParams, CompletionResponse, DidChangeTextDocumentParams,
    DidChangeWatchedFilesParams, DidChangeWatchedFilesRegistrationOptions,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DocumentSymbolParams,
    DocumentSymbolResponse, FileChangeType, FileSystemWatcher, FoldingRange, FoldingRangeParams,
    FoldingRangeProviderCapability, GlobPattern, GotoDefinitionParams, GotoDefinitionResponse,
    Hover, HoverParams, HoverProviderCapability, InitializeParams, InitializeResult,
    InitializedParams, InlayHint, InlayHintParams, Location, MessageType, NumberOrString, OneOf,
    ProgressParams, ProgressParamsValue, ReferenceParams, Registration, RenameParams,
    ResourceOperationKind, SemanticTokenModifier, SemanticTokensFullOptions, SemanticTokensLegend,
    SemanticTokensOptions, SemanticTokensParams, SemanticTokensResult,
    SemanticTokensServerCapabilities, ServerCapabilities, SignatureHelp, SignatureHelpOptions,
    SignatureHelpParams, SymbolInformation, TextDocumentSyncCapability, TextDocumentSyncKind, Url,
    WorkDoneProgress, WorkDoneProgressBegin, WorkDoneProgressCreateParams, WorkDoneProgressEnd,
    WorkDoneProgressOptions, WorkDoneProgressReport, WorkspaceEdit, WorkspaceSymbolParams,
};
use tower_lsp::{Client, LanguageServer};

use crate::document::DocumentStore;
use crate::engine::{self, EngineEvent, EngineHandle, MessageLevel, ProgressUpdate, WatchedChange};

/// The single background job's progress token.
const PROGRESS_TOKEN: &str = "java-lsp/warm-up";

/// The dynamic registration id for the watched-file capability.
const WATCHER_REGISTRATION_ID: &str = "java-lsp/watched-files";

/// The legend for the semantic tokens the engine emits; keep in sync with
/// `analysis::SEMANTIC_TOKEN_TYPES`.
fn semantic_token_legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: crate::analysis::SEMANTIC_TOKEN_TYPES.to_vec(),
        token_modifiers: Vec::<SemanticTokenModifier>::new(),
    }
}

/// Asks the client to create the progress item, once. Fire-and-forget: the
/// response is ignored so a client that never replies cannot block the drain
/// task, and the request still precedes the first `$/progress` on the same
/// ordered transport.
fn create_progress(client: &Client, token: NumberOrString) {
    let client = client.clone();
    tokio::spawn(async move {
        let _ = client
            .send_request::<WorkDoneProgressCreate>(WorkDoneProgressCreateParams { token })
            .await;
    });
}

/// Sends one `$/progress` notification for the background job.
async fn send_progress(client: &Client, token: NumberOrString, update: ProgressUpdate) {
    let value = match update {
        ProgressUpdate::Begin { title, message } => {
            WorkDoneProgress::Begin(WorkDoneProgressBegin {
                title,
                cancellable: Some(false),
                message: Some(message),
                percentage: None,
            })
        }
        ProgressUpdate::Update {
            message,
            percentage,
        } => WorkDoneProgress::Report(WorkDoneProgressReport {
            cancellable: Some(false),
            message: Some(message),
            percentage,
        }),
        ProgressUpdate::End { message } => WorkDoneProgress::End(WorkDoneProgressEnd { message }),
    };
    let _ = client
        .send_notification::<Progress>(ProgressParams {
            token,
            value: ProgressParamsValue::WorkDone(value),
        })
        .await;
}

pub struct JavaLanguageServer {
    documents: Arc<RwLock<DocumentStore>>,
    engine: EngineHandle,
    /// The client, so `initialized` can register the watched-file capability.
    client: Client,
    workspace_root: Mutex<Option<Url>>,
    /// Whether the client advertised `window.workDoneProgress`, read in
    /// `initialize`; progress and notices are dropped when it is false.
    progress: Arc<AtomicBool>,
    /// Whether the client supports dynamic registration of
    /// `workspace/didChangeWatchedFiles`, read in `initialize` (D6).
    watched_files: Arc<AtomicBool>,
}

impl std::fmt::Debug for JavaLanguageServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JavaLanguageServer").finish_non_exhaustive()
    }
}

impl JavaLanguageServer {
    pub fn new(client: Client) -> Self {
        // The engine pushes events; this task turns each into a client
        // notification. Unbounded, so a slow client can never stall the engine.
        let progress = Arc::new(AtomicBool::new(false));
        let (events, mut incoming_events) = mpsc::unbounded_channel();
        let publishing = client.clone();
        let supported = progress.clone();
        tokio::spawn(async move {
            // One background job at a time, so one progress token.
            let token = NumberOrString::String(PROGRESS_TOKEN.to_string());
            let mut created = false;
            while let Some(event) = incoming_events.recv().await {
                match event {
                    EngineEvent::Diagnostics {
                        uri,
                        version,
                        diagnostics,
                    } => {
                        publishing
                            .publish_diagnostics(uri, diagnostics, version)
                            .await
                    }
                    EngineEvent::Progress(update) => {
                        if !supported.load(Ordering::Relaxed) {
                            continue;
                        }
                        if !created {
                            created = true;
                            create_progress(&publishing, token.clone());
                        }
                        send_progress(&publishing, token.clone(), update).await;
                    }
                    EngineEvent::Message { level, text } => {
                        let kind = match level {
                            MessageLevel::Info => MessageType::INFO,
                            MessageLevel::Warning => MessageType::WARNING,
                        };
                        publishing.show_message(kind, text).await;
                    }
                }
            }
        });
        Self {
            documents: Arc::new(RwLock::new(DocumentStore::default())),
            engine: engine::spawn(events),
            client,
            workspace_root: Mutex::new(None),
            progress,
            watched_files: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The shell's document store, shared with tests (and later, other tasks).
    pub fn documents(&self) -> Arc<RwLock<DocumentStore>> {
        Arc::clone(&self.documents)
    }

    /// A handle to the engine task, shared with tests.
    pub fn engine(&self) -> EngineHandle {
        self.engine.clone()
    }

    /// Registers `workspace/didChangeWatchedFiles` for `**/*.java` (D1).
    /// Fire-and-forget, like the progress item: the handshake must never block
    /// on a client that does not answer the registration request.
    fn register_watcher(&self) {
        let options = DidChangeWatchedFilesRegistrationOptions {
            watchers: vec![FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/*.java".to_string()),
                kind: None,
            }],
        };
        let client = self.client.clone();
        tokio::spawn(async move {
            let registration = Registration {
                id: WATCHER_REGISTRATION_ID.to_string(),
                method: "workspace/didChangeWatchedFiles".to_string(),
                register_options: serde_json::to_value(options).ok(),
            };
            if let Err(error) = client.register_capability(vec![registration]).await {
                tracing::warn!(%error, "watched-file registration failed");
            }
        });
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for JavaLanguageServer {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        tracing::info!("initializing java-lsp");
        let progress_supported = params
            .capabilities
            .window
            .as_ref()
            .and_then(|window| window.work_done_progress)
            .unwrap_or(false);
        self.progress.store(progress_supported, Ordering::Relaxed);
        // The create-type quick fix needs the client to accept a `CreateFile`
        // resource operation; without it, those actions are withheld.
        let workspace = params.capabilities.workspace.as_ref();
        let resource_operations = workspace
            .and_then(|workspace| workspace.workspace_edit.as_ref())
            .and_then(|edit| edit.resource_operations.as_ref())
            .is_some_and(|operations| operations.contains(&ResourceOperationKind::Create));
        self.engine
            .set_resource_operations(resource_operations)
            .await;
        // The watched-file fix is only registered with a client that supports
        // dynamic registration (D6); without it the rest of the fix stands.
        let watched_files = workspace
            .and_then(|workspace| workspace.did_change_watched_files.as_ref())
            .and_then(|capabilities| capabilities.dynamic_registration)
            .unwrap_or(false);
        self.watched_files.store(watched_files, Ordering::Relaxed);
        let root = params.root_uri.or_else(|| {
            params
                .workspace_folders
                .as_ref()
                .and_then(|folders| folders.first().map(|folder| folder.uri.clone()))
        });
        if let Some(root) = root {
            tracing::info!(root = %root, "workspace root");
            if let Ok(mut slot) = self.workspace_root.lock() {
                *slot = Some(root);
            }
        }
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_string()]),
                    ..CompletionOptions::default()
                }),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
                    retrigger_characters: None,
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                }),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Left(true)),
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![CodeActionKind::QUICKFIX]),
                        ..CodeActionOptions::default()
                    },
                )),
                inlay_hint_provider: Some(OneOf::Left(true)),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                semantic_tokens_provider: Some(SemanticTokensServerCapabilities::from(
                    SemanticTokensOptions {
                        work_done_progress_options: WorkDoneProgressOptions::default(),
                        legend: semantic_token_legend(),
                        range: Some(false),
                        full: Some(SemanticTokensFullOptions::Bool(true)),
                    },
                )),
                ..ServerCapabilities::default()
            },
            ..InitializeResult::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        tracing::info!("java-lsp initialized");
        // One dynamic registration, when the client supports it: without a
        // watcher the server never learns about files the editor did not open.
        if self.watched_files.load(Ordering::Relaxed) {
            self.register_watcher();
        }
        // The background workspace scan starts once the handshake completes;
        // it never blocks text sync or request handling (R6).
        let root = self
            .workspace_root
            .lock()
            .ok()
            .and_then(|slot| slot.clone());
        if let Some(root) = root {
            self.engine.set_workspace_root(root).await;
        }
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        tracing::debug!(changes = params.changes.len(), "didChangeWatchedFiles");
        let changes = params
            .changes
            .into_iter()
            .map(|event| {
                let change = if event.typ == FileChangeType::CREATED {
                    WatchedChange::Created
                } else if event.typ == FileChangeType::DELETED {
                    WatchedChange::Deleted
                } else {
                    WatchedChange::Changed
                };
                (event.uri, change)
            })
            .collect();
        self.engine.watched_files(changes).await;
    }

    async fn shutdown(&self) -> Result<()> {
        tracing::info!("shutdown requested");
        Ok(())
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri;
        let actions = self
            .engine
            .code_actions(uri, params.context.diagnostics)
            .await;
        if actions.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            actions
                .into_iter()
                .map(CodeActionOrCommand::CodeAction)
                .collect(),
        ))
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let version = params.text_document.version;
        let text = params.text_document.text;
        tracing::info!(uri = %uri, version, "didOpen");
        {
            let mut docs = self.documents.write().await;
            docs.open(uri.clone(), version, &text);
        }
        // Diagnostics follow as an engine event; the shell need not ask.
        self.engine.open(uri, text, version).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let version = params.text_document.version;
        tracing::debug!(uri = %uri, version, changes = params.content_changes.len(), "didChange");
        let text = {
            let mut docs = self.documents.write().await;
            if !docs.change(&uri, version, &params.content_changes) {
                tracing::warn!(uri = %uri, "didChange for unopened document");
                return;
            }
            docs.get(&uri)
                .map(|doc| String::from_utf8_lossy(&doc.bytes).into_owned())
        };
        let Some(text) = text else { return };
        self.engine.change(uri, text, version).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        tracing::info!(uri = %uri, "didClose");
        {
            let mut docs = self.documents.write().await;
            docs.close(&uri);
        }
        // Closing clears the published diagnostics through the engine event.
        self.engine.close(uri).await;
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        Ok(self.engine.hover(uri, position).await)
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        Ok(self
            .engine
            .definition(uri, position)
            .await
            .map(GotoDefinitionResponse::Scalar))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        Ok(self.engine.completions(uri, position).await)
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let include_declaration = params.context.include_declaration;
        let references = self
            .engine
            .references(uri, position, include_declaration)
            .await;
        // An empty result is a refusal as much as a "none found": report null
        // rather than claiming the symbol has no occurrences.
        if references.is_empty() {
            Ok(None)
        } else {
            Ok(Some(references))
        }
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        Ok(self.engine.rename(uri, position, params.new_name).await)
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri;
        Ok(self
            .engine
            .document_symbols(uri)
            .await
            .map(DocumentSymbolResponse::Nested))
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        Ok(Some(self.engine.workspace_symbols(params.query).await))
    }

    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        let uri = params.text_document.uri;
        Ok(self.engine.folding_ranges(uri).await)
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri;
        Ok(self
            .engine
            .semantic_tokens(uri)
            .await
            .map(SemanticTokensResult::Tokens))
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let uri = params.text_document.uri;
        let range = params.range;
        Ok(Some(self.engine.inlay_hints(uri, range).await))
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        Ok(self.engine.signature_help(uri, position).await)
    }
}
