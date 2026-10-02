//! The workspace source scanner: parses the project's `.java` files and
//! publishes each file's index entries and its declared-type model.
//!
//! The scanner is its own module on the bus. It subscribes for the source
//! inventory the project driver emits, scans off the request path, and reports
//! the `Sources` stage-done.

use std::path::PathBuf;
use std::sync::Arc;

use tower_lsp::lsp_types::Url;

use crate::bus::{next_notification, BusClient};
use crate::index::{extract_entries, java_parser};
use crate::messages::{DriverMessage, ProgressUpdate, Stage};

/// Starts the source scanner on the bus: on the source inventory it parses each
/// file and emits its entries and model, then the `Sources` stage-done.
pub fn spawn(bus: &BusClient) {
    let client = bus.labeled("source");
    let mut rx = client.subscribe();
    tokio::spawn(async move {
        while let Some(message) = next_notification(&mut rx).await {
            if let DriverMessage::SourceInventory { files } = message {
                let client = client.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    let count = {
                        let mut sink = |message| {
                            let _ = client.notify(message);
                        };
                        scan_sources(&files, &mut sink)
                    };
                    let _ = client.notify(DriverMessage::StageDone {
                        stage: Stage::Sources,
                        count,
                    });
                })
                .await;
            }
        }
    });
}

/// Scans the workspace `.java` files, publishing each file's index entries and
/// its declared-type model. Returns the number of files indexed.
pub(crate) fn scan_sources(files: &[PathBuf], sink: &mut dyn FnMut(DriverMessage)) -> usize {
    let mut parser = java_parser();
    let mut indexed = 0usize;
    for path in files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let Some(tree) = parser.parse(text.as_bytes(), None) else {
            continue;
        };
        let Ok(uri) = Url::from_file_path(path) else {
            continue;
        };
        let package = crate::types::file_package(&tree, &text);
        let mut model = crate::types::TypeModel::new();
        model.extend(crate::types::collect_type_infos(
            package.as_deref(),
            &tree,
            &text,
        ));
        let entries = extract_entries(&uri, &tree, &text);
        sink(DriverMessage::SourceFile {
            uri,
            entries: Arc::new(entries),
            types: Arc::new(model),
        });
        indexed += 1;
    }
    sink(DriverMessage::Progress(ProgressUpdate::Update {
        message: format!("Indexed {indexed} source files"),
        percentage: None,
    }));
    indexed
}
