//! Fetching and indexing Maven dependency sources.
//!
//! For every resolved artifact, a `<a>-<v>-sources.jar` already in the local
//! repository is reused; otherwise it is downloaded from a Maven repository
//! (Maven Central by default) and written back into the local repository at
//! Maven's standard path. The sources are extracted into a java-lsp cache and
//! indexed through the same tree-sitter path the JDK's `src.zip` uses, so
//! declarations carry real signatures and go-to-definition can open them.
//!
//! On by default; `$JAVA_LSP_OFFLINE` (any non-empty value) disables all network
//! work and restores the class-file-only behavior. Everything here runs off the
//! request path: the fetch on the runtime, the extraction and parsing on the
//! blocking pool.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tower_lsp::lsp_types::Url;

use crate::engine::Reporter;
use crate::index::{SymbolEntry, WorkspaceIndex};
use crate::resolve::Artifact;
use crate::types::TypeInfo;

/// Concurrent downloads per warm-up: enough to keep a large closure moving
/// without opening a socket per artifact.
const MAX_CONCURRENCY: usize = 8;
const DEFAULT_BASE_URL: &str = "https://repo1.maven.org/maven2";
const USER_AGENT: &str = concat!("java-lsp/", env!("CARGO_PKG_VERSION"));

/// True when `$JAVA_LSP_OFFLINE` is set to a non-empty value: no network work at
/// all, and dependency sources stay class-file-only.
pub fn offline() -> bool {
    std::env::var("JAVA_LSP_OFFLINE")
        .map(|value| !value.is_empty())
        .unwrap_or(false)
}

/// The Maven repository base URL: `$JAVA_LSP_MAVEN_CENTRAL_URL` when set,
/// otherwise Maven Central. A trailing slash is normalized away.
pub fn base_url() -> String {
    let base = std::env::var("JAVA_LSP_MAVEN_CENTRAL_URL")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    base.trim_end_matches('/').to_string()
}

/// Where extracted sources are cached: `$JAVA_LSP_SOURCES_CACHE`, else the XDG
/// cache directory, else `~/.cache/java-lsp/sources`.
pub fn cache_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("JAVA_LSP_SOURCES_CACHE") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("java-lsp").join("sources");
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join(".cache")
        .join("java-lsp")
        .join("sources")
}

/// Fetches and indexes the sources of `artifacts`, upgrading the index and the
/// type model in place. A no-op when downloads are disabled or nothing resolved.
pub async fn index_sources(index: WorkspaceIndex, artifacts: Vec<Artifact>, reporter: Reporter) {
    if artifacts.is_empty() || offline() {
        return;
    }
    let repo = crate::index::local_repository();
    let base = base_url();
    let available = fetch_sources(&artifacts, &repo, &base, &reporter).await;
    if available.is_empty() {
        return;
    }
    let cache = cache_dir();
    let upgrade = index.clone();
    let upgrade_reporter = reporter.clone();
    let _ = tokio::task::spawn_blocking(move || {
        index_extracted(&upgrade, &available, &repo, &cache, &upgrade_reporter);
    })
    .await;
}

/// Returns the subset of `artifacts` whose sources jar is on disk after this
/// call — reused from the local repository or downloaded into it. A failure is
/// logged and skipped, leaving that artifact's class-file entries in place.
async fn fetch_sources(
    artifacts: &[Artifact],
    repo: &Path,
    base: &str,
    reporter: &Reporter,
) -> Vec<Artifact> {
    let mut available = Vec::new();
    let mut missing = Vec::new();
    for artifact in artifacts {
        let (group, id, version) = artifact;
        if sources_jar_path(repo, group, id, version).is_file() {
            available.push(artifact.clone());
        } else {
            missing.push(artifact.clone());
        }
    }
    if missing.is_empty() {
        return available;
    }

    let total = missing.len();
    reporter.update(format!("Fetching {total} dependency sources"), Some(0));

    let client = match reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(%error, "no HTTP client for dependency sources; keeping class files");
            return available;
        }
    };

    let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENCY));
    let mut tasks = tokio::task::JoinSet::new();
    for artifact in missing {
        // Acquire before spawning so at most `MAX_CONCURRENCY` run at once.
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => break,
        };
        let client = client.clone();
        let base = base.to_string();
        let repo = repo.to_path_buf();
        tasks.spawn(async move {
            let _permit = permit;
            match download_sources(&client, &base, &repo, &artifact).await {
                Ok(()) => Some(artifact),
                Err(error) => {
                    tracing::warn!(
                        artifact = %format!("{}:{}:{}", artifact.0, artifact.1, artifact.2),
                        error = %error,
                        "dependency sources unavailable; keeping class files"
                    );
                    None
                }
            }
        });
    }
    let mut done = 0usize;
    while let Some(joined) = tasks.join_next().await {
        done += 1;
        if let Ok(Some(artifact)) = joined {
            available.push(artifact);
        }
        reporter.update(
            format!("Fetched {done}/{total} dependency sources"),
            Some((done * 100 / total) as u32),
        );
    }
    available
}

/// Downloads one sources jar into the local repository, verifying the published
/// `.sha1` when there is one. A missing checksum is accepted; a mismatch fails.
async fn download_sources(
    client: &reqwest::Client,
    base: &str,
    repo: &Path,
    artifact: &Artifact,
) -> Result<(), String> {
    let (group, id, version) = artifact;
    let url = sources_url(base, group, id, version);
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    let bytes = response.bytes().await.map_err(|error| error.to_string())?;
    verify_checksum(client, &url, &bytes).await?;

    let destination = sources_jar_path(repo, group, id, version);
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    // Write beside the target and rename, so a partial download is never read.
    let temporary = destination.with_extension("jar.part");
    std::fs::write(&temporary, &bytes).map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, &destination).map_err(|error| error.to_string())?;
    Ok(())
}

/// Verifies `bytes` against the artifact's published `.sha1`. A missing or
/// unreadable checksum is accepted (not every artifact publishes one).
async fn verify_checksum(client: &reqwest::Client, url: &str, bytes: &[u8]) -> Result<(), String> {
    let published = match client.get(format!("{url}.sha1")).send().await {
        Ok(response) if response.status().is_success() => match response.text().await {
            Ok(text) => text,
            Err(_) => return Ok(()),
        },
        _ => return Ok(()),
    };
    let expected = published
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if expected.is_empty() {
        return Ok(());
    }
    let actual = sha1_hex(bytes);
    if expected == actual {
        Ok(())
    } else {
        Err(format!("checksum mismatch ({expected} != {actual})"))
    }
}

/// Extracts each sources jar into the cache, indexes its `.java` files — first
/// dropping the artifact's class-file entries so the two never coexist, which
/// would make `definition` ambiguous — and overlays the source-derived types on
/// the type model. Runs on the blocking pool.
fn index_extracted(
    index: &WorkspaceIndex,
    artifacts: &[Artifact],
    repo: &Path,
    cache: &Path,
    reporter: &Reporter,
) {
    let mut types = index
        .type_model()
        .map(|model| (*model).clone())
        .unwrap_or_default();
    let mut parser = crate::index::java_parser();
    let mut artifacts_indexed = 0usize;
    let mut files_indexed = 0usize;
    reporter.update(
        format!("Parsing {} dependency source archives", artifacts.len()),
        None,
    );

    for artifact in artifacts {
        let (group, id, version) = artifact;
        let Ok(data) = std::fs::read(sources_jar_path(repo, group, id, version)) else {
            continue;
        };
        let directory = cache.join(group).join(id).join(version);
        let mut files: Vec<(Url, Vec<SymbolEntry>)> = Vec::new();
        let mut infos: Vec<TypeInfo> = Vec::new();

        crate::classfile::for_each_zip_entry(&data, |name, bytes| {
            if !name.ends_with(".java") || name.ends_with("package-info.java") {
                return;
            }
            let Ok(text) = String::from_utf8(bytes) else {
                return;
            };
            let Some(tree) = parser.parse(text.as_bytes(), None) else {
                return;
            };
            let target = directory.join(&name);
            let Some(parent) = target.parent() else {
                return;
            };
            if std::fs::create_dir_all(parent).is_err()
                || std::fs::write(&target, text.as_bytes()).is_err()
            {
                return;
            }
            let Ok(uri) = Url::from_file_path(&target) else {
                return;
            };
            let mut entries = crate::index::extract_entries(&uri, &tree, &text);
            for entry in &mut entries {
                entry.dependency = true;
                entry.library_source = true;
            }
            let package = crate::types::file_package(&tree, &text);
            infos.extend(crate::types::collect_type_infos(
                package.as_deref(),
                &tree,
                &text,
            ));
            files.push((uri, entries));
        });

        if files.is_empty() {
            continue;
        }
        if let Ok(class_uri) = Url::from_file_path(class_jar_path(repo, group, id, version)) {
            index.remove_file(&class_uri);
        }
        for (uri, entries) in files {
            files_indexed += 1;
            index.upsert_file(&uri, entries);
        }
        types.extend(infos);
        artifacts_indexed += 1;
    }

    if artifacts_indexed == 0 {
        return;
    }
    index.set_types(Arc::new(types));
    reporter.update(
        format!("Indexed {files_indexed} dependency source files"),
        None,
    );
    tracing::info!(
        artifacts = artifacts_indexed,
        files = files_indexed,
        "library sources indexed"
    );
}

/// `<root>/<group as path>/<artifact>/<version>`.
fn artifact_dir(root: &Path, group: &str, id: &str, version: &str) -> PathBuf {
    root.join(group.replace('.', "/")).join(id).join(version)
}

/// The main jar Maven would have resolved for these coordinates.
fn class_jar_path(root: &Path, group: &str, id: &str, version: &str) -> PathBuf {
    artifact_dir(root, group, id, version).join(format!("{id}-{version}.jar"))
}

/// The sources jar for these coordinates, in the local repository.
pub fn sources_jar_path(root: &Path, group: &str, id: &str, version: &str) -> PathBuf {
    artifact_dir(root, group, id, version).join(format!("{id}-{version}-sources.jar"))
}

/// The repository URL of the sources jar for these coordinates.
fn sources_url(base: &str, group: &str, id: &str, version: &str) -> String {
    format!(
        "{base}/{}/{id}/{version}/{id}-{version}-sources.jar",
        group.replace('.', "/")
    )
}

/// The lowercase hex SHA-1 of `bytes`, to compare against a published `.sha1`.
fn sha1_hex(bytes: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};

    /// A temp directory removed on drop.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "java-lsp-sources-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// A minimal HTTP server over fixed routes, so no test touches the network.
    struct TestServer {
        address: SocketAddr,
    }

    impl TestServer {
        fn start(routes: HashMap<String, Vec<u8>>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let routes = routes.clone();
                    std::thread::spawn(move || serve(stream, &routes));
                }
            });
            Self { address }
        }

        fn base_url(&self) -> String {
            format!("http://{}", self.address)
        }
    }

    fn serve(mut stream: TcpStream, routes: &HashMap<String, Vec<u8>>) {
        let Ok(clone) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(clone);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            return;
        }
        // Drain the headers.
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) if line == "\r\n" || line == "\n" => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let path = request_line.split_whitespace().nth(1).unwrap_or("/");
        let response = match routes.get(path) {
            Some(body) => {
                let mut out = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                out.extend_from_slice(body);
                out
            }
            None => {
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
            }
        };
        let _ = stream.write_all(&response);
        let _ = stream.flush();
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for byte in data {
            crc ^= *byte as u32;
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
        !crc
    }

    /// A STORED (uncompressed) ZIP containing `files`, readable by the jar
    /// reader under test.
    fn stored_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut local = Vec::new();
        let mut central = Vec::new();
        for (name, content) in files {
            let crc = crc32(content);
            let offset = local.len() as u32;
            local.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            local.extend_from_slice(&20u16.to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes());
            local.extend_from_slice(&crc.to_le_bytes());
            local.extend_from_slice(&(content.len() as u32).to_le_bytes());
            local.extend_from_slice(&(content.len() as u32).to_le_bytes());
            local.extend_from_slice(&(name.len() as u16).to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes());
            local.extend_from_slice(name.as_bytes());
            local.extend_from_slice(content);

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(content.len() as u32).to_le_bytes());
            central.extend_from_slice(&(content.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let mut out = local;
        let central_offset = out.len() as u32;
        let central_size = central.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&central_size.to_le_bytes());
        out.extend_from_slice(&central_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    fn artifact() -> Artifact {
        ("demo".to_string(), "lib".to_string(), "1.0".to_string())
    }

    fn sources_jar_routes(body: Vec<u8>, sha: Option<&str>) -> HashMap<String, Vec<u8>> {
        let mut routes = HashMap::new();
        routes.insert("/demo/lib/1.0/lib-1.0-sources.jar".to_string(), body);
        if let Some(sha) = sha {
            routes.insert(
                "/demo/lib/1.0/lib-1.0-sources.jar.sha1".to_string(),
                sha.as_bytes().to_vec(),
            );
        }
        routes
    }

    const SOURCE: &[u8] = b"package demo;\n\npublic class Thing {\n    public void go() {}\n}\n";

    #[test]
    fn env_helpers_have_defaults_and_overrides() {
        let _env = crate::jdk::env_lock();
        std::env::remove_var("JAVA_LSP_OFFLINE");
        std::env::remove_var("JAVA_LSP_MAVEN_CENTRAL_URL");
        std::env::remove_var("JAVA_LSP_SOURCES_CACHE");
        assert!(!offline());
        assert_eq!(base_url(), DEFAULT_BASE_URL);

        std::env::set_var(
            "JAVA_LSP_MAVEN_CENTRAL_URL",
            "https://mirror.example/maven2/",
        );
        assert_eq!(base_url(), "https://mirror.example/maven2");
        std::env::set_var("JAVA_LSP_SOURCES_CACHE", "/tmp/java-lsp-test-cache");
        assert_eq!(cache_dir(), PathBuf::from("/tmp/java-lsp-test-cache"));
        std::env::set_var("JAVA_LSP_OFFLINE", "1");
        assert!(offline());

        std::env::remove_var("JAVA_LSP_MAVEN_CENTRAL_URL");
        std::env::remove_var("JAVA_LSP_SOURCES_CACHE");
        std::env::remove_var("JAVA_LSP_OFFLINE");
    }

    #[tokio::test]
    async fn downloads_extracts_and_indexes_sources() {
        let _env = crate::jdk::env_lock();
        let fixture = TempDir::new("download");
        let repo = fixture.path().join("repo");
        let cache = fixture.path().join("cache");
        let sources = stored_zip(&[("demo/Thing.java", SOURCE)]);
        let server = TestServer::start(sources_jar_routes(
            sources.clone(),
            Some(&sha1_hex(&sources)),
        ));

        let available = fetch_sources(
            &[artifact()],
            &repo,
            &server.base_url(),
            &Reporter::default(),
        )
        .await;
        assert_eq!(available, vec![artifact()]);
        assert!(sources_jar_path(&repo, "demo", "lib", "1.0").is_file());

        // A class-file entry for the same artifact must be replaced, not kept.
        let index = WorkspaceIndex::new();
        let class_uri = Url::from_file_path(class_jar_path(&repo, "demo", "lib", "1.0")).unwrap();
        let zero = tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(0, 0),
            tower_lsp::lsp_types::Position::new(0, 0),
        );
        index.upsert_file(
            &class_uri,
            vec![SymbolEntry {
                uri: class_uri.clone(),
                name: "Thing".to_string(),
                kind: crate::index::IndexKind::Class,
                package: Some("demo".to_string()),
                container: Vec::new(),
                full_range: zero,
                selection_range: zero,
                dependency: true,
                library_source: false,
            }],
        );

        index_extracted(&index, &available, &repo, &cache, &Reporter::default());

        let extracted = cache
            .join("demo")
            .join("lib")
            .join("1.0")
            .join("demo")
            .join("Thing.java");
        assert!(
            extracted.is_file(),
            "the source should be extracted to the cache"
        );
        assert!(
            index
                .all_symbols()
                .iter()
                .all(|entry| entry.uri != class_uri),
            "the class-file entries should be dropped"
        );
        let entries = index.query_name("Thing");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].dependency && entries[0].library_source);
        assert_eq!(entries[0].uri.to_file_path().unwrap(), extracted);
        // The cache is never a references or rename candidate.
        assert!(index.source_files().is_empty());
    }

    #[tokio::test]
    async fn a_missing_checksum_is_tolerated() {
        let _env = crate::jdk::env_lock();
        let fixture = TempDir::new("no-checksum");
        let repo = fixture.path().join("repo");
        let sources = stored_zip(&[("demo/Thing.java", SOURCE)]);
        let server = TestServer::start(sources_jar_routes(sources, None));

        let available = fetch_sources(
            &[artifact()],
            &repo,
            &server.base_url(),
            &Reporter::default(),
        )
        .await;
        assert_eq!(available, vec![artifact()]);
        assert!(sources_jar_path(&repo, "demo", "lib", "1.0").is_file());
    }

    #[tokio::test]
    async fn a_checksum_mismatch_discards_the_download() {
        let _env = crate::jdk::env_lock();
        let fixture = TempDir::new("bad-checksum");
        let repo = fixture.path().join("repo");
        let sources = stored_zip(&[("demo/Thing.java", SOURCE)]);
        let server = TestServer::start(sources_jar_routes(
            sources,
            Some("0000000000000000000000000000000000000000"),
        ));

        let available = fetch_sources(
            &[artifact()],
            &repo,
            &server.base_url(),
            &Reporter::default(),
        )
        .await;
        assert!(available.is_empty());
        assert!(!sources_jar_path(&repo, "demo", "lib", "1.0").is_file());
    }

    #[tokio::test]
    async fn an_unreachable_repository_keeps_class_files() {
        let _env = crate::jdk::env_lock();
        let fixture = TempDir::new("unreachable");
        let repo = fixture.path().join("repo");
        // Port 1 is not served; the fetch must fail without panicking.
        let available = fetch_sources(
            &[artifact()],
            &repo,
            "http://127.0.0.1:1",
            &Reporter::default(),
        )
        .await;
        assert!(available.is_empty());
        assert!(!sources_jar_path(&repo, "demo", "lib", "1.0").is_file());
    }

    #[tokio::test]
    async fn offline_skips_the_whole_pass() {
        let _env = crate::jdk::env_lock();
        let fixture = TempDir::new("offline");
        let repo = fixture.path().join("repo");
        let sources = stored_zip(&[("demo/Thing.java", SOURCE)]);
        let server = TestServer::start(sources_jar_routes(sources, None));

        std::env::set_var("JAVA_LSP_OFFLINE", "1");
        std::env::set_var("MAVEN_REPO", &repo);
        std::env::set_var("JAVA_LSP_MAVEN_CENTRAL_URL", server.base_url());
        index_sources(WorkspaceIndex::new(), vec![artifact()], Reporter::default()).await;
        std::env::remove_var("JAVA_LSP_OFFLINE");
        std::env::remove_var("MAVEN_REPO");
        std::env::remove_var("JAVA_LSP_MAVEN_CENTRAL_URL");

        assert!(!sources_jar_path(&repo, "demo", "lib", "1.0").is_file());
    }
}
