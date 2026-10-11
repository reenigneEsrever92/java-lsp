//! The warm-up coordinator: the one participant that drives the startup
//! sequence and shapes its events to the launch configuration.
//!
//! [`spawn`] labels itself `startup`, walks the project model and emits the
//! source inventory, then reports `Ready`, the summary, and the progress end for
//! exactly the stages the configuration enables — so the warm-up drivers
//! ([`crate::resolve`], [`crate::scan`], [`crate::jars`], [`crate::sources`],
//! [`crate::jdk`]) stay configuration-free and simply report what they did.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tower_lsp::lsp_types::Url;

use crate::cli::Modules;
use crate::hub::{next_notification, HubClient};
use crate::messages::{DriverMessage, MessageLevel, ProgressUpdate, Stage};
use crate::project::{walk_project, ProjectModel};

/// The stages the coordinator waits on, derived from the launch configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Expected {
    sources: bool,
    jars: bool,
    jdk: bool,
    downloads: bool,
}

impl Expected {
    fn from_modules(modules: &Modules) -> Self {
        Self {
            sources: modules.workspace_index,
            jars: modules.jars,
            jdk: modules.jdk,
            downloads: modules.downloads,
        }
    }
}

/// The warm-up state machine: what it expects, what has reported, and what it
/// has already emitted.
struct WarmUp {
    expected: Expected,
    root: Option<Url>,
    started: Option<Instant>,
    maven: bool,
    counts: HashMap<Stage, usize>,
    ready_sent: bool,
    summary_sent: bool,
    notice_sent: bool,
}

impl WarmUp {
    fn new(expected: Expected) -> Self {
        Self {
            expected,
            root: None,
            started: None,
            maven: false,
            counts: HashMap::new(),
            ready_sent: false,
            summary_sent: false,
            notice_sent: false,
        }
    }

    /// True when every enabled core stage has reported. With no core stage
    /// enabled it is true at once, so a minimal server still becomes ready.
    fn complete(&self) -> bool {
        let done = |stage: Stage| self.counts.contains_key(&stage);
        (!self.expected.sources || done(Stage::Sources))
            && (!self.expected.jars || done(Stage::Jars))
            && (!self.expected.jdk || done(Stage::Jdk))
    }

    /// Emits `Ready` once the enabled core stages are done, followed
    /// immediately by the summary when the downloader is not expected.
    fn ready(&mut self, client: &HubClient) {
        if self.ready_sent || !self.complete() {
            return;
        }
        self.ready_sent = true;
        let _ = client.notify(DriverMessage::Ready);
        if !self.expected.downloads {
            self.summary(client);
        }
    }

    /// Emits the closing summary and progress end, once.
    fn summary(&mut self, client: &HubClient) {
        if self.summary_sent || !self.ready_sent {
            return;
        }
        self.summary_sent = true;
        let files = self.counts.get(&Stage::Sources).copied().unwrap_or(0);
        let jars = self.counts.get(&Stage::Jars).copied().unwrap_or(0);
        let jdk_classes = self.counts.get(&Stage::Jdk).copied().unwrap_or(0);
        let elapsed_ms = self
            .started
            .map(|at| at.elapsed().as_millis() as u64)
            .unwrap_or(0);
        let root = self.root.as_ref().map(Url::to_string).unwrap_or_default();
        let _ = client.notify(DriverMessage::Summary {
            root,
            files,
            jars,
            jdk_classes,
            maven: self.maven,
            elapsed_ms,
        });
        let _ = client.notify(DriverMessage::Progress(ProgressUpdate::End {
            message: Some(format!(
                "Indexed {files} files, {jars} dependency jars, {jdk_classes} JDK classes"
            )),
        }));
    }
}

/// Starts the warm-up coordinator: it walks the project model on the first added
/// folder and closes the warm-up according to `modules`.
pub fn spawn(hub: &HubClient, modules: &Modules) {
    let client = hub.labeled("startup");
    let expected = Expected::from_modules(modules);
    let mut rx = client.subscribe();
    tokio::spawn(async move {
        let mut warm_up = WarmUp::new(expected);
        while let Some(message) = next_notification(&mut rx).await {
            match message {
                DriverMessage::FolderAdded { uri } if warm_up.root.is_none() => {
                    warm_up.root = Some(uri.clone());
                    warm_up.started = Some(Instant::now());
                    let _ = client.notify(DriverMessage::Progress(ProgressUpdate::Begin {
                        title: "java-lsp".to_string(),
                        message: "Indexing workspace".to_string(),
                    }));
                    let walk_client = client.clone();
                    let _ = tokio::task::spawn_blocking(move || match uri.to_file_path() {
                        Ok(path) => {
                            let (model, files) = walk_project(&path);
                            let _ = walk_client.notify(DriverMessage::ProjectModel {
                                model: Arc::new(model),
                            });
                            let _ = walk_client.notify(DriverMessage::SourceInventory {
                                files: Arc::new(files),
                            });
                        }
                        Err(_) => {
                            let _ = walk_client.notify(DriverMessage::ProjectModel {
                                model: Arc::new(ProjectModel::default()),
                            });
                            let _ = walk_client.notify(DriverMessage::SourceInventory {
                                files: Arc::new(Vec::new()),
                            });
                        }
                    })
                    .await;
                    // With no core stage enabled, the walk is the whole warm-up.
                    warm_up.ready(&client);
                }
                DriverMessage::ProjectModel { model } => warm_up.maven = model.maven,
                DriverMessage::Artifacts { artifacts } => {
                    if !warm_up.notice_sent && !warm_up.expected.downloads && !artifacts.is_empty()
                    {
                        warm_up.notice_sent = true;
                        let _ = client.notify(DriverMessage::Notice {
                            level: MessageLevel::Info,
                            text: sources_disabled_notice(),
                        });
                    }
                }
                DriverMessage::StageDone { stage, count } => {
                    warm_up.counts.insert(stage, count);
                    warm_up.ready(&client);
                    if stage == Stage::Downloads {
                        warm_up.summary(&client);
                    }
                }
                _ => {}
            }
        }
    });
}

/// The notice shown when dependency sources are disabled and the workspace
/// resolved at least one dependency.
fn sources_disabled_notice() -> String {
    "Dependency sources are disabled (--no-downloads or JAVA_LSP_OFFLINE): \
     go-to-definition into library code is unavailable."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected(sources: bool, jars: bool, jdk: bool, downloads: bool) -> Expected {
        Expected {
            sources,
            jars,
            jdk,
            downloads,
        }
    }

    #[test]
    fn no_core_stage_means_complete() {
        let warm_up = WarmUp::new(expected(false, false, false, false));
        assert!(warm_up.complete());
    }

    #[test]
    fn waits_on_exactly_the_enabled_core_stages() {
        let mut warm_up = WarmUp::new(expected(true, false, true, false));
        assert!(!warm_up.complete());
        warm_up.counts.insert(Stage::Sources, 3);
        assert!(!warm_up.complete(), "the JDK stage is still enabled");
        warm_up.counts.insert(Stage::Jars, 9);
        assert!(
            !warm_up.complete(),
            "the jar stage is disabled, so it is ignored"
        );
        warm_up.counts.insert(Stage::Jdk, 5);
        assert!(warm_up.complete());
    }

    #[test]
    fn the_notice_names_the_flag_and_the_env_var() {
        let notice = sources_disabled_notice();
        assert!(notice.contains("--no-downloads"), "{notice}");
        assert!(notice.contains("JAVA_LSP_OFFLINE"), "{notice}");
    }
}
