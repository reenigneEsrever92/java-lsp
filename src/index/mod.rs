//! The index subsystem — the workspace's symbols and declared types.
//!
//! The index owns its own warm-up: it parses the workspace's `.java` files and
//! indexes the dependency jars and the JDK itself. The folders to scan it learns
//! from the project module, which publishes them as
//! [`Notification::FoldersDiscoverred`] once the shell broadcasts
//! [`Notification::ServerStarted`]; the changes it cannot observe on its own —
//! open-buffer edits and watched file events — plus the coarse reads it answers
//! complete its vocabulary, which lives in `messages.rs`.
//!
//! The subsystem is split by role. [`types`] holds the data — the symbol table,
//! the declared-type store and model, and the shared identity types; [`analyze`]
//! reads Java, turning a parsed tree or a path on disk into the index's
//! vocabulary; [`analyze`] also stores it — the symbol-table upsert, the
//! declared-type store's layer updates, and the context rebuild — while [`query`]
//! reads those structures; [`read`] answers the coarse, navigation-shaped reads a
//! consumer makes over the hub; and [`util`] holds the value behaviour and the
//! small helpers that fit neither. This module is the entry point: it declares the
//! submodules, re-exports the operations the rest of the crate refers to, and
//! owns [`spawn`], which scans the folders the project module discovers and
//! serves the coarse reads.

pub mod analyze;
pub mod query;
pub mod types;
pub mod util;

mod read;

pub use self::types::{
    FileOrigin, IndexKind, SymbolEntry, SymbolKind, TypeContext, WorkspaceIndex,
};

// The operations that used to live directly in this module, re-exported so
// `crate::index::<fn>` keeps resolving for the modules reintroduced later.
pub use self::analyze::{extract_entries, rebuild_context, set_ready, set_source, upsert_file};
pub(crate) use self::analyze::{scan_sources, workspace_java_files};
// Used by the modules reintroduced later, not by the current build.
#[allow(unused_imports)]
pub(crate) use self::analyze::index_source;
pub use self::query::{
    all_types, constructors, contains, find_in_package, find_unique, lookup, members,
    members_with_overloads, nested_types, types_in_package,
};
// Used by the modules reintroduced later, not by the current build.
#[allow(unused_imports)]
pub(crate) use crate::parser::java_parser;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc::UnboundedReceiver;

use crate::hub::{Hub, HubClient};
use crate::messages::{
    Envelope, IndexBatchRequest, IndexBatchResponse, IndexRequest, Message, Notification, Request,
    Response,
};

// ---------------------------------------------------------------------------
// The workspace source scan driver
// ---------------------------------------------------------------------------

/// Starts the index on the hub: on each [`Notification::FoldersDiscoverred`]
/// batch it walks the `.java` files under the reported folders and fills the
/// index with their entries and declared-type models, then marks it ready; and it
/// serves the coarse reads a consumer makes of it, each answered from the index
/// state it owns. Both kinds of work run on the blocking pool, so neither the
/// runtime nor a slow read is blocked on the other.
pub fn spawn(hub: &Hub) {
    let hub_client = hub.register("index");

    tokio::spawn(async move {
        let notifications = hub_client.subscribe().await;
        let requests = hub_client.serve().await;
        serve(
            hub_client,
            WorkspaceIndex::default(),
            notifications,
            requests,
        )
        .await;
    });
}

/// Registers an index on `hub` that serves the coarse reads from `index` without
/// scanning; the test harness uses it to answer reads from a preloaded index.
#[cfg(test)]
pub fn spawn_with(hub: &Hub, index: WorkspaceIndex) {
    let hub_client = hub.register("index");

    tokio::spawn(async move {
        // No scan: a receiver whose sender is already gone disables the
        // notification branch, leaving only the read requests to serve.
        let (_unused, notifications) = tokio::sync::mpsc::unbounded_channel();
        let requests = hub_client.serve().await;
        serve(hub_client, index, notifications, requests).await;
    });
}

/// Serves the index's read requests from `index`, scanning the discovered folders
/// into it as they arrive on `notifications`. The read answers are computed on the
/// blocking pool, so a slow read never stalls the runtime; the index moves across
/// and back so its state stays on this one task.
async fn serve(
    hub_client: HubClient,
    mut index: WorkspaceIndex,
    mut notifications: UnboundedReceiver<Arc<Notification>>,
    mut requests: UnboundedReceiver<Envelope>,
) {
    loop {
        tokio::select! {
            Some(message) = notifications.recv() => {
                let Notification::FoldersDiscoverred { uri } = &*message else {
                    continue;
                };
                let folders: Vec<PathBuf> = uri
                    .iter()
                    .filter_map(|folder| folder.to_file_path().ok())
                    .collect();
                index = tokio::task::spawn_blocking(move || {
                    let mut index = index;
                    let mut files: Vec<PathBuf> = folders
                        .iter()
                        .flat_map(|folder| workspace_java_files(folder))
                        .collect();
                    files.sort();
                    files.dedup();
                    scan_sources(&files, &mut index);
                    set_ready(&mut index);
                    index
                })
                .await
                .unwrap_or_default();
            }
            Some(envelope) = requests.recv() => {
                let Some(request) = read_request(&envelope) else {
                    continue;
                };
                let id = envelope.id;
                let (index_back, response) = tokio::task::spawn_blocking(move || {
                    let response = match &request {
                        ReadRequest::One(request) => Response::Index(read::answer(&index, request)),
                        ReadRequest::Batch(batch) => Response::IndexBatch(IndexBatchResponse(
                            batch
                                .requests
                                .iter()
                                .map(|request| read::answer(&index, request))
                                .collect(),
                        )),
                    };
                    (index, response)
                })
                .await
                .unwrap();
                index = index_back;
                hub_client.respond(id, response);
            }
            else => break,
        }
    }
}

/// One request the index serves, cloned out of the envelope so it can move onto
/// the blocking pool.
enum ReadRequest {
    One(IndexRequest),
    Batch(IndexBatchRequest),
}

/// The read `envelope` carries, or `None` when it is not an index request.
fn read_request(envelope: &Envelope) -> Option<ReadRequest> {
    match &*envelope.message {
        Message::Request(Request::Index(request)) => Some(ReadRequest::One(request.clone())),
        Message::Request(Request::IndexBatch(request)) => Some(ReadRequest::Batch(request.clone())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tower_lsp::lsp_types::Url;

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "java-lsp-index-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scan_sources_records_entries_and_models() {
        let dir = temp_dir("scan");
        let file = dir.join("a").join("Widget.java");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            "package a;\n\npublic class Widget {\n    public void run() {}\n}\n",
        )
        .unwrap();

        let mut index = WorkspaceIndex::default();
        let count = scan_sources(std::slice::from_ref(&file), &mut index);

        assert_eq!(count, 1);
        let uri = Url::from_file_path(&file).unwrap();
        assert!(index.symbols.by_file.contains_key(&uri));
        assert!(index.types.source.positions.contains_key(&uri));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
