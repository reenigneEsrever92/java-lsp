//! The engine wiring.
//!
//! [`start_with`] is the one setup path: it starts the hub and builds the
//! participants the launch configuration enables — the LSP shell, the document
//! module, the index, the analysis core, the diagnostics and quick-fix modules,
//! and the warm-up coordinator with the drivers it expects. Each participant
//! takes the hub's [`crate::hub::HubClient`], labels itself, subscribes, and
//! starts its own thread or task, so nothing here wires channels or knows a
//! participant's hub identity. [`start`] is the zero-config entry point —
//! everything on — which is what the harness and `LspService::new(engine::start)`
//! use. Nothing here talks to the editor.

use crate::cli::Modules;
use crate::server::JavaLanguageServer;

/// Starts every participant the environment enables, and returns the LSP shell
/// (which logs as `server`). The zero-config entry point.
pub fn start(client: tower_lsp::Client) -> JavaLanguageServer {
    start_with(client, Modules::from_env())
}

/// Starts the hub and every enabled participant, and returns the LSP shell
/// (which logs as `server`). The shell subscribes first and the warm-up
/// coordinator — the first participant that publishes unprompted — last of the
/// modules, so no one misses a message. Must run inside a tokio runtime: the
/// drivers are tasks and the analysis module runs its queries on the runtime's
/// blocking pool.
pub fn start_with(client: tower_lsp::Client, modules: Modules) -> JavaLanguageServer {
    let hub = crate::hub::spawn_hub();
    let server = JavaLanguageServer::new(client, &hub, modules.quickfix);

    // The request-serving modules: each owns its state on its own thread,
    // consumes the notifications it cares about, and answers the requests it
    // serves.
    crate::index::spawn(&hub);
    crate::document::spawn(&hub);
    crate::analysis::spawn(&hub, tokio::runtime::Handle::current());
    if modules.diagnostics {
        crate::diagnostics::spawn(&hub, modules.semantic_diagnostics);
    }
    if modules.quickfix {
        crate::quickfix::spawn(&hub);
    }

    // The warm-up coordinator, then the drivers it expects: each driver owns the
    // work it does and its own subscription, and none of them knows whether the
    // configuration enabled it.
    crate::startup::spawn(&hub, &modules);
    if modules.dependencies {
        crate::resolve::spawn(&hub);
    }
    if modules.workspace_index {
        crate::scan::spawn(&hub);
    }
    if modules.jars {
        crate::jars::spawn(&hub);
    }
    if modules.downloads {
        crate::sources::spawn(&hub);
    }
    if modules.jdk {
        crate::jdk::spawn(&hub);
    }

    server
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tower_lsp::lsp_types::Url;

    use crate::index::IndexHandle;
    use crate::messages::{DriverMessage, Hub, MessageLevel};

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
        let client = crate::hub::spawn_hub();
        let mut received = client.labeled("server").subscribe();
        client.notify(DriverMessage::Notice {
            level: MessageLevel::Info,
            text: "note".to_string(),
        });
        loop {
            match received.recv().await.expect("the subscription stays open") {
                Hub::Notify(DriverMessage::Notice { level, text }) => {
                    assert_eq!(level, MessageLevel::Info);
                    assert_eq!(text, "note");
                    break;
                }
                _ => continue,
            }
        }
    }
}
