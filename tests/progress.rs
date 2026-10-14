//! Integration test: the LSP shell renders the warm-up's progress as LSP
//! work-done progress when the client advertises `window.workDoneProgress`, and
//! sends nothing at all when it does not.
//!
//! The engine wiring that used to build the live server is deprecated, so this
//! test stands the hub up itself — the maven and index publishers plus the shell
//! — and drives it through `tower_lsp::LspService`, reading the server-to-client
//! messages off the `ClientSocket`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedReceiver;
use tower::{Service, ServiceExt};
use tower_lsp::jsonrpc::{Id, Request, Response};
use tower_lsp::lsp_types::Url;
use tower_lsp::LspService;

use java_lsp::cli::Cli;
use java_lsp::hub::Hub;
use java_lsp::messages::{Notification, ProgressUpdate};
use java_lsp::server::JavaLanguageServer;

/// A workspace fixture with one Java file and no Maven project.
fn fixture(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "java-lsp-progress-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("Greet.java"),
        "package demo;\n\npublic class Greet {\n}\n",
    )
    .unwrap();
    root
}

/// Registers the maven and index publishers on `hub` and builds the shell over
/// it. `--no-dependencies --no-downloads` keeps the warm-up off the network.
async fn live_server(hub: &Hub) -> (LspService<JavaLanguageServer>, tower_lsp::ClientSocket) {
    let cli = Cli {
        no_dependencies: true,
        no_downloads: true,
        ..Cli::default()
    };
    java_lsp::maven::spawn(hub, &cli);
    java_lsp::index::spawn(hub);
    let shell = hub.register("server");
    let incoming = shell.subscribe().await;
    LspService::build(move |client| JavaLanguageServer::new(shell, incoming, client)).finish()
}

/// Drives the handshake, advertising `window.workDoneProgress` only when
/// `work_done_progress` is set.
async fn initialize(
    service: &mut LspService<JavaLanguageServer>,
    root: &Url,
    work_done_progress: bool,
) {
    let capabilities = if work_done_progress {
        json!({ "window": { "workDoneProgress": true } })
    } else {
        json!({})
    };
    let response = service
        .ready()
        .await
        .unwrap()
        .call(
            Request::build("initialize")
                .id(Id::Number(1))
                .params(json!({ "capabilities": capabilities, "rootUri": root.as_str() }))
                .finish(),
        )
        .await
        .unwrap();
    assert!(response.is_some(), "initialize must respond");
    service
        .ready()
        .await
        .unwrap()
        .call(Request::build("initialized").params(json!({})).finish())
        .await
        .unwrap();
}

#[tokio::test]
async fn progress_is_rendered_when_the_client_supports_it() {
    let root = fixture("capable");
    let hub = Hub::new();
    let (mut service, mut socket) = live_server(&hub).await;
    initialize(&mut service, &Url::from_file_path(&root).unwrap(), true).await;

    // Collect the server's messages until the progress item has begun and ended.
    let mut created = false;
    let mut began = false;
    let mut reports = 0usize;
    let mut ended = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !(created && began && reports >= 1 && ended) {
        let next = tokio::time::timeout(Duration::from_millis(500), socket.next()).await;
        let Ok(Some(request)) = next else { continue };
        match request.method() {
            "window/workDoneProgress/create" => {
                created = true;
                // The create is a request, so the server's drain waits for it:
                // answer it or no `$/progress` for that token ever follows.
                if let Some(id) = request.id().cloned() {
                    socket
                        .send(Response::from_ok(id, json!(null)))
                        .await
                        .unwrap();
                }
            }
            "$/progress" => {
                let params = request.params().cloned().unwrap_or(Value::Null);
                match params["value"]["kind"].as_str() {
                    Some("begin") => {
                        assert_eq!(params["value"]["title"], "java-lsp");
                        assert!(
                            params["value"]["message"]
                                .as_str()
                                .is_some_and(|message| !message.is_empty()),
                            "the begin names the phase: {params}"
                        );
                        began = true;
                    }
                    Some("report") => reports += 1,
                    Some("end") => ended = true,
                    other => panic!("unexpected progress kind: {other:?} ({params})"),
                }
            }
            _ => {}
        }
    }

    assert!(created, "expected a window/workDoneProgress/create request");
    assert!(began, "expected a $/progress begin");
    assert!(reports >= 1, "expected at least one report update");
    assert!(ended, "expected a $/progress end");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn progress_is_not_rendered_without_the_client_capability() {
    let root = fixture("incapable");
    let hub = Hub::new();
    // Watch the hub's own progress vocabulary, so the check waits for the warm-up
    // to actually finish rather than sleeping a fixed time.
    let watch = hub.register("watch");
    let mut watch_rx = watch.subscribe().await;
    let (mut service, mut socket) = live_server(&hub).await;
    initialize(&mut service, &Url::from_file_path(&root).unwrap(), false).await;

    wait_for_index_end(&mut watch_rx).await;

    // The warm-up has run: drain for a bounded window — nothing progress-like may
    // have reached the client.
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        let next = tokio::time::timeout(Duration::from_millis(100), socket.next()).await;
        if let Ok(Some(request)) = next {
            assert!(
                !matches!(
                    request.method(),
                    "$/progress" | "window/workDoneProgress/create"
                ),
                "progress must not be sent without the capability: {request:?}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// Waits until the index reports the end of its scan on the hub, so a
/// no-capability test is not vacuous.
async fn wait_for_index_end(rx: &mut UnboundedReceiver<Arc<Notification>>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(Some(message)) = tokio::time::timeout(remaining, rx.recv()).await else {
            break;
        };
        if let Notification::Progress {
            token,
            update: ProgressUpdate::End { .. },
        } = &*message
        {
            if token == "java-lsp/index" {
                return;
            }
        }
    }
    panic!("the index never reported its end; the warm-up may not have run");
}
