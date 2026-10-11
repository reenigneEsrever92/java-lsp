//! The dependency-jar indexer: parses the class files of the resolved
//! dependency jars into the index.
//!
//! The indexer is its own module on the hub. It subscribes for the artifact
//! list the dependency driver emits, indexes off the request path, and reports
//! the `Jars` stage-done. Each archive is served from [`crate::base_cache`] when
//! its identity is unchanged, so a restart re-parses only what changed.

use std::sync::Arc;

use tower_lsp::lsp_types::Url;

use crate::hub::{next_notification, HubClient};
use crate::messages::{DriverMessage, ProgressUpdate, Stage};
use crate::resolve::{local_repository, Artifact, Resolver};

/// Starts the jar indexer on the hub: on the artifact list it reads each jar and
/// emits its entries and model, then the `Jars` stage-done.
pub fn spawn(hub: &HubClient) {
    let client = hub.labeled("jar");
    let mut rx = client.subscribe();
    tokio::spawn(async move {
        while let Some(message) = next_notification(&mut rx).await {
            if let DriverMessage::Artifacts { artifacts } = message {
                let client = client.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    let count = {
                        let mut sink = |message| {
                            let _ = client.notify(message);
                        };
                        index_jars(&artifacts, &mut sink)
                    };
                    let _ = client.notify(DriverMessage::StageDone {
                        stage: Stage::Jars,
                        count,
                    });
                })
                .await;
            }
        }
    });
}

/// Indexes every resolved dependency jar that is on disk, publishing each as a
/// base artifact (its entries and its declared types as one layer). Returns the
/// number of jars indexed.
///
/// Each archive is served from [`crate::base_cache`] when its identity is
/// unchanged, so a restart re-parses only what changed.
pub(crate) fn index_jars(artifacts: &[Artifact], sink: &mut dyn FnMut(DriverMessage)) -> usize {
    let resolver = Resolver::new(local_repository());
    let store = crate::base_cache::ArchiveStore::new("jars");
    let mut jars = 0usize;
    for (group, artifact_id, version) in artifacts {
        let jar_path = resolver.jar_path(group, artifact_id, version);
        let Some(identity) = crate::base_cache::identity(&jar_path) else {
            continue;
        };
        let output = match store.get::<crate::base_cache::ArchiveOutput>(&identity) {
            Some(output) => output,
            None => {
                let Some((entries, types)) = crate::classfile::jar_outputs(&jar_path) else {
                    continue;
                };
                let output = crate::base_cache::ArchiveOutput { entries, types };
                store.insert(&identity, &output);
                output
            }
        };
        let Ok(jar_uri) = Url::from_file_path(&jar_path) else {
            continue;
        };
        let mut types = crate::types::TypeModel::new();
        types.extend(output.types);
        sink(DriverMessage::BaseArtifact {
            uri: jar_uri,
            entries: Arc::new(output.entries),
            types: Arc::new(types),
        });
        jars += 1;
        sink(DriverMessage::Progress(ProgressUpdate::Update {
            message: format!("Indexed {jars} dependency jars"),
            percentage: None,
        }));
    }
    jars
}
