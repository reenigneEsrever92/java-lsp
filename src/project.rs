//! The project module: workspace source-folder discovery.
//!
//! The project module is a hub client like any other ([`crate::hub`]). It turns
//! the shell's one [`Notification::ServerStarted`] — the workspace root — into
//! the folders that hold Java sources, and publishes them to the hub as a single
//! batched [`Notification::FoldersDiscoverred`]. The index (and any later module
//! that cares) then learns the workspace's source layout without walking the
//! tree itself.
//!
//! A folder "contains sources" when it directly holds at least one `.java` file.
//! The walk reports the topmost such folders: once a folder is reported it is
//! not descended into, so a package's nested subpackages are covered by the
//! consumer's own recursive scan of the reported folder instead of being
//! reported again. Hidden directories and build outputs (`target`, `build`) are
//! skipped, matching the index's own walk. The whole set rides one notification;
//! an empty batch is still published, so a consumer learns discovery finished
//! even when the workspace holds no sources at all.

use std::path::{Path, PathBuf};

use tower_lsp::lsp_types::Url;

use crate::hub::Hub;
use crate::messages::Notification;

/// Starts the project module on the hub: on [`Notification::ServerStarted`] it
/// discovers the folders containing sources under the workspace root and
/// broadcasts them, batched, as [`Notification::FoldersDiscoverred`].
pub fn spawn(hub: &Hub) {
    let hub_client = hub.register("project");

    tokio::spawn(async move {
        let mut notifications = hub_client.subscribe().await;
        while let Some(message) = notifications.recv().await {
            let Notification::ServerStarted { workspace_root } = &*message else {
                continue;
            };
            let Ok(root) = workspace_root.to_file_path() else {
                continue;
            };
            let folders = tokio::task::spawn_blocking(move || source_folders(&root))
                .await
                .unwrap_or_default();
            let uri = folders
                .into_iter()
                .filter_map(|folder| Url::from_file_path(folder).ok())
                .collect();
            hub_client.notify(Notification::FoldersDiscoverred { uri });
        }
    });
}

/// The folders under `root` that contain Java sources, sorted and deduplicated.
fn source_folders(root: &Path) -> Vec<PathBuf> {
    let mut folders = Vec::new();
    collect_source_folders(root, &mut folders);
    folders.sort();
    folders.dedup();
    folders
}

/// Walks `dir`, reporting the topmost folders that directly hold a `.java` file;
/// a reported folder is not descended into. Hidden directories and build
/// outputs are skipped.
fn collect_source_folders(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    let mut children = Vec::new();
    let mut has_source = false;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if !is_skipped_dir(&path) {
                children.push(path);
            }
        } else if path
            .extension()
            .is_some_and(|extension| extension == "java")
        {
            has_source = true;
        }
    }

    if has_source {
        out.push(dir.to_path_buf());
        return;
    }

    for child in children {
        collect_source_folders(&child, out);
    }
}

/// Whether `path` is a directory the source walk skips: a hidden directory or a
/// build output.
fn is_skipped_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.') || name == "target" || name == "build")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "java-lsp-project-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(root: &Path, relative: &str, text: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
    }

    #[test]
    fn a_folder_directly_holding_java_is_a_source_folder() {
        let root = temp_dir("direct");
        write(&root, "a/Widget.java", "package a; class Widget {}\n");
        write(&root, "a/b/Helper.java", "package a.b; class Helper {}\n");
        write(&root, "notes.txt", "no sources here\n");

        // `a` holds a source directly, so it is reported; its nested package
        // `a/b` is covered by scanning `a` and is not reported again.
        assert_eq!(source_folders(&root), vec![root.join("a")]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_walk_skips_hidden_and_build_outputs() {
        let root = temp_dir("skip");
        write(&root, "src/Main.java", "class Main {}\n");
        write(&root, "target/Built.java", "class Built {}\n");
        write(&root, ".hidden/Hidden.java", "class Hidden {}\n");

        assert_eq!(source_folders(&root), vec![root.join("src")]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_workspace_without_sources_has_no_source_folders() {
        let root = temp_dir("empty");
        write(&root, "README.md", "nothing here\n");

        assert!(source_folders(&root).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn server_started_publishes_the_source_folders_batched() {
        let hub = Hub::new();
        let mut listener = hub.register("listener").subscribe().await;

        let root = temp_dir("spawn");
        write(
            &root,
            "app/src/main/java/com/example/Main.java",
            "package com.example; class Main {}\n",
        );
        write(
            &root,
            "lib/src/main/java/com/example/Lib.java",
            "package com.example; class Lib {}\n",
        );

        spawn(&hub);
        hub.register("server").notify(Notification::ServerStarted {
            workspace_root: Url::from_directory_path(&root).unwrap(),
        });

        // The listener sees the `ServerStarted` broadcast first; the batch the
        // project module publishes follows it.
        let mut discovered = None;
        while let Some(message) = listener.recv().await {
            if let Notification::FoldersDiscoverred { uri } = &*message {
                discovered = Some(uri.clone());
                break;
            }
        }

        let mut expected = vec![
            Url::from_file_path(root.join("app/src/main/java/com/example")).unwrap(),
            Url::from_file_path(root.join("lib/src/main/java/com/example")).unwrap(),
        ];
        expected.sort();
        let mut discovered = discovered.expect("the source folders are published");
        discovered.sort();
        assert_eq!(discovered, expected);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_workspace_without_sources_publishes_an_empty_batch() {
        let hub = Hub::new();
        let mut listener = hub.register("listener").subscribe().await;

        let root = temp_dir("none");
        write(&root, "README.md", "nothing here\n");

        spawn(&hub);
        hub.register("server").notify(Notification::ServerStarted {
            workspace_root: Url::from_directory_path(&root).unwrap(),
        });

        // Discovery always publishes, even when it finds nothing, so a consumer
        // learns the workspace has no sources rather than waiting forever.
        let mut discovered = None;
        while let Some(message) = listener.recv().await {
            if let Notification::FoldersDiscoverred { uri } = &*message {
                discovered = Some(uri.clone());
                break;
            }
        }

        assert_eq!(
            discovered.expect("an empty batch is published"),
            Vec::<Url>::new()
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
