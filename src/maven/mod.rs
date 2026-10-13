//! The maven module: parses Maven projects, resolves their dependencies, fetches
//! the dependency sources into a home-folder cache, and publishes the source
//! roots to the index via [`crate::messages::Notification::FoldersDiscoverred`].
//!
//! The module is a hub client like any other ([`crate::hub`]). On the shell's
//! one [`Notification::ServerStarted`] it discovers the workspace's Maven model
//! (every `pom.xml` under the root becomes a module with its effective pom and
//! declared source roots; a workspace with no poms falls back to the whole
//! root), resolves each module's dependency closure offline from the local
//! repository, fetches the `-sources.jar` of every resolved artifact into the
//! local repository and extracts it under the sources cache, and publishes two
//! batched [`Notification::FoldersDiscoverred`] notifications:
//!
//! - the workspace source roots with `origin: FolderOrigin::Workspace`, and
//! - the extracted dependency-source roots with
//!   `origin: FolderOrigin::LibrarySources`,
//!
//! so the index scans both and tags the files accordingly (`FileOrigin::Workspace`
//! / `FileOrigin::LibrarySource` — the latter completable and openable but never
//! a references/rename candidate).
//!
//! The module is split by role, each piece reintroduced from `src/deprecated/`:
//! [`settings`] (user Maven settings: `localRepository`, mirrors, offline),
//! [`resolve`] (effective poms and the offline dependency closure), [`model`]
//! (pom discovery and source roots), and [`sources`] (the sources-jar fetch and
//! extraction). [`resolve_pass`] drives the discovery and the closure — pure,
//! off the request path; [`spawn`] publishes its results and runs the async
//! fetch.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use tower_lsp::lsp_types::Url;

use crate::cli::Cli;
use crate::hub::Hub;
use crate::messages::{FolderOrigin, Notification};

use self::resolve::Artifact;
use self::resolve::Resolver;

pub mod model;
pub mod resolve;
pub mod settings;
pub mod sources;

/// What the maven pass derives from a workspace root before any sourcing work:
/// the workspace's source roots and the resolved dependency closure.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Pass {
    /// The workspace source roots to scan (pom-derived, or the whole root when
    /// the workspace has no poms).
    pub workspace_roots: Vec<PathBuf>,
    /// The deduplicated dependency closure: every artifact that resolved from
    /// the local repository, in resolution order.
    pub artifacts: Vec<Artifact>,
}

/// Discovers the project model under `root` against the local repository at
/// `repo` and resolves the dependency closure of every module. Pure — no hub,
/// no environment, no network — so tests drive it directly and the spawn runs
/// it on the blocking pool.
pub(crate) fn resolve_pass(root: &Path, repo: &Path) -> Pass {
    let mut resolver = Resolver::new(repo.to_path_buf());
    let model = model::discover(root, &mut resolver);
    let mut artifacts: Vec<Artifact> = Vec::new();
    let mut seen: HashSet<Artifact> = HashSet::new();
    for module in &model.modules {
        let Some(effective) = &module.effective else {
            continue;
        };
        for artifact in resolve::resolve_closure(effective, &mut resolver) {
            if seen.insert(artifact.clone()) {
                artifacts.push(artifact);
            }
        }
    }
    Pass {
        workspace_roots: model.source_roots(),
        artifacts,
    }
}

/// Starts the maven module on the hub: on [`Notification::ServerStarted`] it
/// discovers the workspace's source roots and publishes them, batched, as a
/// `Workspace` [`Notification::FoldersDiscoverred`]; unless the launch
/// configuration or the environment disables it, it then resolves the
/// dependency closure, fetches and extracts the dependency sources, and
/// publishes the extracted roots as a `LibrarySources` batch.
///
/// `--no-dependencies` skips the resolution and the sourcing; `--no-downloads`
/// (or `$JAVA_LSP_OFFLINE` / the settings `offline` flag) resolves but never
/// fetches or extracts.
pub fn spawn(hub: &Hub, cli: &Cli) {
    let no_dependencies = cli.no_dependencies;
    let no_downloads = cli.no_downloads;
    let hub_client = hub.register("maven");

    tokio::spawn(async move {
        let mut notifications = hub_client.subscribe().await;
        while let Some(message) = notifications.recv().await {
            let Notification::ServerStarted { workspace_root } = &*message else {
                continue;
            };
            let Ok(root) = workspace_root.to_file_path() else {
                continue;
            };
            let offline = settings::offline();
            let pass = tokio::task::spawn_blocking(move || {
                let repo = settings::local_repository();
                resolve_pass(&root, &repo)
            })
            .await
            .unwrap_or_default();

            hub_client.notify(Notification::FoldersDiscoverred {
                uri: pass
                    .workspace_roots
                    .iter()
                    .filter_map(|folder| Url::from_file_path(folder).ok())
                    .collect(),
                origin: FolderOrigin::Workspace,
            });

            if no_dependencies || no_downloads || offline {
                continue;
            }

            let (repo, cache) = (settings::local_repository(), sources::cache_dir());
            let roots = sources::fetch_and_extract(&pass.artifacts, &repo, &cache).await;
            if roots.is_empty() {
                continue;
            }
            hub_client.notify(Notification::FoldersDiscoverred {
                uri: roots
                    .iter()
                    .filter_map(|folder| Url::from_file_path(folder).ok())
                    .collect(),
                origin: FolderOrigin::LibrarySources,
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::sync::{Mutex, MutexGuard};

    use tower_lsp::lsp_types::Url;

    use super::*;

    /// Serializes the crate's environment-mutating tests: env state is
    /// process-global, so no two tests anywhere may change it concurrently.
    /// `settings` and `sources` share this lock through
    /// `crate::maven::tests::env_lock`.
    pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Sets or removes an environment variable, restoring the previous value on
    /// drop.
    struct EnvVar {
        name: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVar {
        fn set(name: &'static str, value: impl AsRef<OsStr>) -> EnvVar {
            let previous = std::env::var_os(name);
            std::env::set_var(name, value);
            EnvVar { name, previous }
        }

        fn remove(name: &'static str) -> EnvVar {
            let previous = std::env::var_os(name);
            std::env::remove_var(name);
            EnvVar { name, previous }
        }
    }

    impl Drop for EnvVar {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(self.name, value),
                None => std::env::remove_var(self.name),
            }
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "java-lsp-maven-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(root: &Path, relative: &str, content: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }

    /// The standard CRC-32 of `data`, for the stored-zip fixture below.
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &byte in data {
            crc ^= byte as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    /// Builds a minimal STORED (uncompressed) zip of the given entries, for a
    /// fixture `-sources.jar` — no deflate, no test dependency.
    fn stored_zip(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut body = Vec::new();
        let mut central = Vec::new();
        let mut offset = 0u32;
        for (name, content) in entries {
            let name = name.as_bytes();
            let crc = crc32(content.as_bytes());
            body.extend_from_slice(&[
                0x50, 0x4b, 0x03, 0x04, // local file header
            ]);
            body.extend_from_slice(&20u16.to_le_bytes()); // version needed
            body.extend_from_slice(&0u16.to_le_bytes()); // flags
            body.extend_from_slice(&0u16.to_le_bytes()); // method: stored
            body.extend_from_slice(&0u16.to_le_bytes()); // mod time
            body.extend_from_slice(&0x21u16.to_le_bytes()); // mod date
            body.extend_from_slice(&crc.to_le_bytes());
            body.extend_from_slice(&(content.len() as u32).to_le_bytes());
            body.extend_from_slice(&(content.len() as u32).to_le_bytes());
            body.extend_from_slice(&(name.len() as u16).to_le_bytes());
            body.extend_from_slice(&0u16.to_le_bytes()); // extra length
            body.extend_from_slice(name);
            body.extend_from_slice(content.as_bytes());

            central.extend_from_slice(&[0x50, 0x4b, 0x01, 0x02]); // central header
            central.extend_from_slice(&20u16.to_le_bytes()); // version made by
            central.extend_from_slice(&20u16.to_le_bytes()); // version needed
            central.extend_from_slice(&0u16.to_le_bytes()); // flags
            central.extend_from_slice(&0u16.to_le_bytes()); // method: stored
            central.extend_from_slice(&0u16.to_le_bytes()); // mod time
            central.extend_from_slice(&0x21u16.to_le_bytes()); // mod date
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(content.len() as u32).to_le_bytes());
            central.extend_from_slice(&(content.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes()); // extra
            central.extend_from_slice(&0u16.to_le_bytes()); // comment
            central.extend_from_slice(&0u16.to_le_bytes()); // disk
            central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            central.extend_from_slice(&offset.to_le_bytes()); // local offset
            central.extend_from_slice(name);

            offset += 30 + name.len() as u32 + content.len() as u32;
        }

        let mut out = body;
        let central_start = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06]); // end of central dir
        out.extend_from_slice(&0u16.to_le_bytes()); // disk
        out.extend_from_slice(&0u16.to_le_bytes()); // central dir disk
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&central_start.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // comment length
        out
    }

    /// A fixture local repository: a pom and a sources jar for `gson`, laid out
    /// at Maven's standard path.
    fn gson_repo(repo: &Path) {
        let dir = repo.join("com/google/code/gson/gson/2.10.1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("gson-2.10.1.pom"),
            r#"<project>
  <groupId>com.google.code.gson</groupId>
  <artifactId>gson</artifactId>
  <version>2.10.1</version>
  <packaging>jar</packaging>
</project>"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("gson-2.10.1-sources.jar"),
            stored_zip(&[(
                "com/google/gson/Gson.java",
                "package com.google.gson;\npublic class Gson {}\n",
            )]),
        )
        .unwrap();
    }

    #[test]
    fn resolve_pass_discovers_roots_and_closure() {
        let root = temp_dir("pass");
        write(
            &root,
            "src/main/java/com/example/Main.java",
            "package com.example; class Main {}\n",
        );
        write(
            &root,
            "pom.xml",
            r#"<project>
  <groupId>com.example</groupId>
  <artifactId>app</artifactId>
  <version>1.0</version>
  <dependencies>
    <dependency>
      <groupId>com.google.code.gson</groupId>
      <artifactId>gson</artifactId>
      <version>2.10.1</version>
    </dependency>
  </dependencies>
</project>"#,
        );
        let repo = temp_dir("pass-repo");
        gson_repo(&repo);

        let pass = resolve_pass(&root, &repo);
        assert_eq!(pass.workspace_roots, vec![root.join("src/main/java")]);
        assert_eq!(
            pass.artifacts,
            vec![(
                "com.google.code.gson".to_string(),
                "gson".to_string(),
                "2.10.1".to_string()
            )]
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn resolve_pass_without_poms_is_the_whole_root_with_no_artifacts() {
        let root = temp_dir("pass-plain");
        write(&root, "Main.java", "class Main {}\n");
        let repo = temp_dir("pass-plain-repo");

        let pass = resolve_pass(&root, &repo);
        assert_eq!(pass.workspace_roots, vec![root.clone()]);
        assert!(pass.artifacts.is_empty());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// Sends `ServerStarted` for `root` and collects the `FoldersDiscoverred`
    /// batches the maven spawn publishes. The hub's broadcast channel never
    /// closes, so collection is bounded by a short deadline: the spawn publishes
    /// both batches back to back (the second only when sourcing runs), so a
    /// missing second batch is detected instead of hanging the test.
    async fn batch(hub: &crate::hub::Hub, root: &Path, cli: &Cli) -> Vec<(FolderOrigin, Vec<Url>)> {
        let mut listener = hub.register("test-listener").subscribe().await;
        spawn(hub, cli);
        hub.register("server").notify(Notification::ServerStarted {
            workspace_root: Url::from_directory_path(root).unwrap(),
        });

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
        let mut out = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let Ok(Some(message)) = tokio::time::timeout(remaining, listener.recv()).await else {
                break;
            };
            if let Notification::FoldersDiscoverred { uri, origin } = &*message {
                let mut uri = uri.clone();
                uri.sort();
                out.push((*origin, uri));
            }
        }
        out
    }

    #[tokio::test]
    async fn server_started_publishes_workspace_then_library_batch() {
        let _lock = env_lock();
        let root = temp_dir("spawn");
        write(
            &root,
            "src/main/java/com/example/Main.java",
            "package com.example; class Main {}\n",
        );
        write(
            &root,
            "pom.xml",
            r#"<project>
  <groupId>com.example</groupId>
  <artifactId>app</artifactId>
  <version>1.0</version>
  <dependencies>
    <dependency>
      <groupId>com.google.code.gson</groupId>
      <artifactId>gson</artifactId>
      <version>2.10.1</version>
    </dependency>
  </dependencies>
</project>"#,
        );
        let fixture = temp_dir("spawn-env");
        let repo = fixture.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        gson_repo(&repo);
        let cache = fixture.join("cache");
        let _repo_var = EnvVar::set("MAVEN_REPO", &repo);
        let _cache_var = EnvVar::set("JAVA_LSP_SOURCES_CACHE", &cache);
        let _settings_var = EnvVar::set(
            "JAVA_LSP_MAVEN_SETTINGS",
            fixture.join("absent-settings.xml"),
        );
        let _offline_var = EnvVar::remove("JAVA_LSP_OFFLINE");

        let hub = crate::hub::Hub::new();
        let batches = batch(&hub, &root, &Cli::default()).await;

        assert_eq!(batches.len(), 2, "{batches:?}");
        assert_eq!(batches[0].0, FolderOrigin::Workspace);
        assert_eq!(
            batches[0].1,
            vec![Url::from_file_path(root.join("src/main/java")).unwrap()]
        );
        assert_eq!(batches[1].0, FolderOrigin::LibrarySources);
        assert_eq!(
            batches[1].1,
            vec![Url::from_file_path(cache.join("com.google.code.gson/gson/2.10.1")).unwrap()]
        );
        assert!(
            cache
                .join("com.google.code.gson/gson/2.10.1/com/google/gson/Gson.java")
                .is_file(),
            "the sources jar is extracted into the cache"
        );
        drop(_repo_var);
        drop(_cache_var);
        drop(_settings_var);
        drop(_offline_var);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&fixture);
    }

    #[tokio::test]
    async fn without_poms_only_the_workspace_batch_publishes() {
        let _lock = env_lock();
        let root = temp_dir("spawn-plain");
        write(&root, "Main.java", "class Main {}\n");
        let fixture = temp_dir("spawn-plain-env");
        let repo = fixture.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let cache = fixture.join("cache");
        let _repo_var = EnvVar::set("MAVEN_REPO", &repo);
        let _settings_var = EnvVar::set(
            "JAVA_LSP_MAVEN_SETTINGS",
            fixture.join("absent-settings.xml"),
        );
        let _offline_var = EnvVar::remove("JAVA_LSP_OFFLINE");

        let hub = crate::hub::Hub::new();
        let batches = batch(&hub, &root, &Cli::default()).await;

        assert_eq!(batches.len(), 1, "{batches:?}");
        assert_eq!(batches[0].0, FolderOrigin::Workspace);
        assert_eq!(
            batches[0].1,
            vec![Url::from_file_path(root.clone()).unwrap()]
        );
        assert!(!cache.exists(), "a non-Maven workspace extracts nothing");
        drop(_repo_var);
        drop(_settings_var);
        drop(_offline_var);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&fixture);
    }

    #[tokio::test]
    async fn no_downloads_resolves_but_never_extracts() {
        let _lock = env_lock();
        let root = temp_dir("spawn-nodl");
        write(&root, "src/main/java/App.java", "class App {}\n");
        write(
            &root,
            "pom.xml",
            r#"<project>
  <groupId>com.example</groupId>
  <artifactId>app</artifactId>
  <version>1.0</version>
  <dependencies>
    <dependency>
      <groupId>com.google.code.gson</groupId>
      <artifactId>gson</artifactId>
      <version>2.10.1</version>
    </dependency>
  </dependencies>
</project>"#,
        );
        let fixture = temp_dir("spawn-nodl-env");
        let repo = fixture.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        gson_repo(&repo);
        let cache = fixture.join("cache");
        let _repo_var = EnvVar::set("MAVEN_REPO", &repo);
        let _cache_var = EnvVar::set("JAVA_LSP_SOURCES_CACHE", &cache);
        let _settings_var = EnvVar::set(
            "JAVA_LSP_MAVEN_SETTINGS",
            fixture.join("absent-settings.xml"),
        );
        let _offline_var = EnvVar::remove("JAVA_LSP_OFFLINE");

        let cli = Cli {
            no_downloads: true,
            ..Cli::default()
        };
        let hub = crate::hub::Hub::new();
        let batches = batch(&hub, &root, &cli).await;

        assert_eq!(batches.len(), 1, "{batches:?}");
        assert_eq!(batches[0].0, FolderOrigin::Workspace);
        assert!(!cache.exists(), "nothing is extracted under --no-downloads");
        drop(_repo_var);
        drop(_cache_var);
        drop(_settings_var);
        drop(_offline_var);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&fixture);
    }
}
