//! The engine wiring.
//!
//! [`start`] is the one setup path: it starts the hub and builds every
//! participant on it — the LSP shell, the index, the analysis core, the
//! diagnostics and quick-fix modules, and the warm-up drivers. Each participant
//! takes the hub's [`crate::bus::BusClient`], labels itself, subscribes, and
//! starts its own thread or task, so nothing here wires channels or knows a
//! participant's bus identity. Nothing here talks to the editor.

use crate::server::JavaLanguageServer;

/// Starts the hub and every participant, and returns the LSP shell (which logs
/// as `server`). The shell subscribes first and the drivers — the only
/// participants that publish unprompted — last, so no one misses a message.
/// Must run inside a tokio runtime: the drivers are tasks and the analysis
/// module runs its queries on the runtime's blocking pool.
pub fn start(client: tower_lsp::Client) -> JavaLanguageServer {
    let bus = crate::bus::spawn_hub();
    let server = JavaLanguageServer::new(client, &bus);

    // The request-serving modules: each owns its state on its own thread,
    // consumes the notifications it cares about, and answers the requests it
    // serves.
    crate::index::spawn(&bus);
    crate::analysis::spawn(&bus, tokio::runtime::Handle::current());
    crate::diagnostics::spawn(&bus);
    crate::quickfix::spawn(&bus);

    // The warm-up drivers: each owns the work it drives and its own
    // subscription.
    crate::project::spawn(&bus);
    crate::resolve::spawn(&bus);
    crate::scan::spawn(&bus);
    crate::jars::spawn(&bus);
    crate::sources::spawn(&bus);
    crate::jdk::spawn(&bus);

    server
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tower_lsp::lsp_types::Url;

    use crate::index::IndexHandle;
    use crate::messages::{Bus, DriverMessage, MessageLevel};

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
