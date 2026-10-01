//! The engine wiring and the drivers.
//!
//! [`start`] is the one setup path: it starts the hub and builds every
//! participant on it — the LSP shell, the index, the analysis core, the
//! diagnostics and quick-fix modules, and the six drivers. Each participant
//! takes only its own [`BusClient`] and subscribes itself, so nothing here
//! wires channels. Nothing here talks to the editor.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use tower_lsp::lsp_types::Url;

use crate::bus::BusClient;
use crate::messages::{Bus, DriverMessage, MessageLevel, ProgressUpdate, Stage};
use crate::server::JavaLanguageServer;

/// Starts the hub and every participant, and returns the LSP shell (which logs
/// as `server`). The shell subscribes first and the drivers — the only
/// participants that publish unprompted — last, so no one misses a message.
/// Must run inside a tokio runtime: the drivers are tasks and the analysis
/// module runs its queries on the runtime's blocking pool.
pub fn start(client: tower_lsp::Client) -> JavaLanguageServer {
    let bus = crate::bus::spawn_hub();
    let server = JavaLanguageServer::new(client, bus.labeled("server"));

    // The modules: each owns its state on its own thread, consumes the
    // notifications it cares about, and answers the requests it serves.
    crate::index::spawn_module(&bus.labeled("index"));
    crate::analysis::spawn_module(bus.labeled("analysis"), tokio::runtime::Handle::current());
    crate::diagnostics::spawn_module(bus.labeled("diagnostics"));
    crate::quickfix::spawn_module(bus.labeled("quickfix"));

    tokio::spawn(project_driver(bus.labeled("project")));
    tokio::spawn(dependency_driver(bus.labeled("dependency")));
    tokio::spawn(source_driver(bus.labeled("source")));
    tokio::spawn(jar_driver(bus.labeled("jar")));
    tokio::spawn(download_driver(bus.labeled("download")));
    tokio::spawn(jdk_driver(bus.labeled("jdk")));

    server
}

/// The next notification on a driver's subscription, or `None` once the bus is
/// gone. A driver serves no module, so it never receives a request.
async fn next_notification(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Bus>,
) -> Option<DriverMessage> {
    loop {
        match rx.recv().await? {
            Bus::Notify(message) => return Some(message),
            Bus::Request(_) => continue,
        }
    }
}

/// The project driver and the warm-up coordinator: on an added folder it walks
/// for the Maven model and the source inventory, and once the core stages and the
/// downloader have reported it flips `ready` and closes the progress item.
///
/// Like every driver that listens, it subscribes when called, before its task
/// starts, so it cannot miss a notification sent after [`start`].
fn project_driver(client: BusClient) -> impl Future<Output = ()> {
    let mut rx = client.subscribe();
    async move {
        let mut root: Option<Url> = None;
        let mut started: Option<std::time::Instant> = None;
        let mut maven = false;
        let mut counts: HashMap<Stage, usize> = HashMap::new();
        let mut ready_sent = false;
        let mut summary_sent = false;
        while let Some(message) = next_notification(&mut rx).await {
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
}

/// The dependency driver: on the project model, resolves each module's closure
/// against the local repository and emits the jar list (and the offline notice).
fn dependency_driver(client: BusClient) -> impl Future<Output = ()> {
    let mut rx = client.subscribe();
    async move {
        while let Some(message) = next_notification(&mut rx).await {
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
}

/// The source scanner: on the source inventory, parses each file and emits its
/// entries and model, then the `Sources` stage-done.
fn source_driver(client: BusClient) -> impl Future<Output = ()> {
    let mut rx = client.subscribe();
    async move {
        while let Some(message) = next_notification(&mut rx).await {
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
}

/// The jar indexer: on the artifact list, reads each jar and emits its entries
/// and model, then the `Jars` stage-done.
fn jar_driver(client: BusClient) -> impl Future<Output = ()> {
    let mut rx = client.subscribe();
    async move {
        while let Some(message) = next_notification(&mut rx).await {
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
}

/// The JDK indexer: runs once at start, independent of the workspace, and emits
/// its archives' entries and models, then the `Jdk` stage-done. It listens to
/// nothing, so it does not subscribe.
async fn jdk_driver(client: BusClient) {
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
fn download_driver(client: BusClient) -> impl Future<Output = ()> {
    let mut rx = client.subscribe();
    async move {
        while let Some(message) = next_notification(&mut rx).await {
            if let DriverMessage::Artifacts { artifacts } = message {
                crate::sources::index_sources((*artifacts).clone(), client.clone()).await;
                let _ = client.notify(DriverMessage::StageDone {
                    stage: Stage::Downloads,
                    count: 0,
                });
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexHandle;

    #[test]
    fn the_index_subsystem_applies_messages() {
        let index = IndexHandle::standalone();
        let jar = Url::parse("file:///lib.jar").unwrap();

        index.apply(DriverMessage::BaseArtifact {
            uri: jar,
            entries: Arc::new(Vec::new()),
            types: Arc::new(crate::types::TypeModel::new()),
        });
        index.apply(DriverMessage::Ready);

        assert!(index.ready().blocking_recv());
        assert!(index.type_model().blocking_recv().is_some());
    }

    #[tokio::test]
    async fn a_subscriber_receives_the_editor_facing_notifications() {
        let client = crate::bus::spawn_hub();
        let mut received = client.labeled("server").subscribe();
        client.notify(DriverMessage::Notice {
            level: MessageLevel::Info,
            text: "note".to_string(),
        });
        loop {
            match received.recv().await.expect("the subscription stays open") {
                Bus::Notify(DriverMessage::Notice { level, text }) => {
                    assert_eq!(level, MessageLevel::Info);
                    assert_eq!(text, "note");
                    break;
                }
                _ => continue,
            }
        }
    }
}
