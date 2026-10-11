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

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tower_lsp::lsp_types::Url;

use crate::hub::{next_notification, HubClient};
use crate::index::SymbolEntry;
use crate::messages::{DriverMessage, LogLevel, ProgressUpdate, Stage};
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

/// Fetches and indexes the sources of `artifacts`, publishing the index updates
/// and progress onto the hub. A no-op when downloads are disabled or
/// nothing resolved. Runs concurrently with the workspace source scan.
pub async fn index_sources(artifacts: Vec<Artifact>, hub: crate::hub::HubClient) {
    if artifacts.is_empty() || offline() {
        return;
    }
    let repo = crate::resolve::local_repository();
    let base = base_url();
    let started = Instant::now();
    let available = fetch_sources(&artifacts, &repo, &base, &hub).await;
    let fetch_ms = started.elapsed().as_millis();
    if available.is_empty() {
        return;
    }
    let _ = hub.notify(DriverMessage::Log {
        level: LogLevel::Info,
        message: format!(
            "dependency source fetch: {} of {} archives available in {fetch_ms}ms",
            available.len(),
            artifacts.len(),
        ),
    });
    let cache = cache_dir();
    let extract_hub = hub.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let mut sink = |message| {
            let _ = extract_hub.notify(message);
        };
        index_extracted(&mut sink, &available, &repo, &cache);
    })
    .await;
}

// -- the source downloader --------------------------------------------------

/// Starts the source downloader on the hub: on the artifact list it fetches and
/// indexes dependency sources, then the `Downloads` stage-done (which gates only
/// the summary).
pub fn spawn(hub: &HubClient) {
    let client = hub.labeled("download");
    let mut rx = client.subscribe();
    tokio::spawn(async move {
        while let Some(message) = next_notification(&mut rx).await {
            if let DriverMessage::Artifacts { artifacts } = message {
                index_sources((*artifacts).clone(), client.clone()).await;
                let _ = client.notify(DriverMessage::StageDone {
                    stage: Stage::Downloads,
                    count: 0,
                });
            }
        }
    });
}

/// Returns the subset of `artifacts` whose sources jar is on disk after this
/// call — reused from the local repository or downloaded into it. A failure is
/// logged and skipped, leaving that artifact's class-file entries in place.
async fn fetch_sources(
    artifacts: &[Artifact],
    repo: &Path,
    base: &str,
    hub: &crate::hub::HubClient,
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
    let _ = hub.notify(DriverMessage::Progress(ProgressUpdate::Update {
        message: format!("Fetching {total} dependency sources"),
        percentage: Some(0),
    }));

    let client = match reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            let _ = hub.notify(DriverMessage::Log {
                level: LogLevel::Warn,
                message: format!(
                    "no HTTP client for dependency sources; keeping class files ({error})"
                ),
            });
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
        let log = hub.clone();
        tasks.spawn(async move {
            let _permit = permit;
            match download_sources(&client, &base, &repo, &artifact).await {
                Ok(()) => Some(artifact),
                Err(error) => {
                    let _ = log.notify(DriverMessage::Log {
                        level: LogLevel::Warn,
                        message: format!(
                            "dependency sources unavailable for {}:{}:{}; keeping class files ({error})",
                            artifact.0, artifact.1, artifact.2
                        ),
                    });
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
        let _ = hub.notify(DriverMessage::Progress(ProgressUpdate::Update {
            message: format!("Fetched {done}/{total} dependency sources"),
            percentage: Some((done * 100 / total) as u32),
        }));
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

/// Per-phase wall-clock totals for one `index_extracted` pass, accumulated across
/// the workers so one log line shows where the time went.
#[derive(Default)]
struct Timers {
    read: AtomicU64,
    inflate: AtomicU64,
    write: AtomicU64,
    parse: AtomicU64,
    entries: AtomicU64,
    types: AtomicU64,
}

/// Adds `elapsed` to a nanosecond counter.
fn add_nanos(slot: &AtomicU64, elapsed: Duration) {
    slot.fetch_add(elapsed.as_nanos() as u64, Ordering::Relaxed);
}

impl Timers {
    /// The totals in whole milliseconds: `read inflate write parse entries types`.
    fn millis(&self) -> (u64, u64, u64, u64, u64, u64) {
        let ms = |slot: &AtomicU64| slot.load(Ordering::Relaxed) / 1_000_000;
        (
            ms(&self.read),
            ms(&self.inflate),
            ms(&self.write),
            ms(&self.parse),
            ms(&self.entries),
            ms(&self.types),
        )
    }
}

/// How often the accumulated phase timings are logged during a pass, so a long
/// run shows the split and the rate without waiting for the end.
const TIMING_STEP: usize = 200;

/// Logs the accumulated phase timings so far, named with the progress so far.
fn log_timings(
    sink: &mut dyn FnMut(DriverMessage),
    timers: &Timers,
    attempted: usize,
    total: usize,
) {
    let (read, inflate, write, parse, entries, types) = timers.millis();
    sink(DriverMessage::Log {
        level: LogLevel::Info,
        message: format!(
            "dependency source extract ({attempted}/{total} archives): read {read}ms, \
             inflate {inflate}ms, write {write}ms, parse {parse}ms, entries {entries}ms, \
             types {types}ms"
        ),
    });
}

/// Extracts each sources jar into the cache and publishes the artifact's `.java`
/// entries and declared types as one base artifact — first dropping the artifact's
/// class-file entries and layer so the two never coexist, which would make
/// `definition` ambiguous. Runs on the blocking pool.
///
/// The `Parsed x/N … (F source files)` progress line keeps the work-done item
/// moving; the artifacts are extracted and parsed across `available_parallelism`
/// worker threads (each its own parser), and the workers hand their finished
/// layers to this thread, which is the only one that touches the sink (and so the
/// hub).
fn index_extracted(
    sink: &mut dyn FnMut(DriverMessage),
    artifacts: &[Artifact],
    repo: &Path,
    cache: &Path,
) {
    let total = artifacts.len();
    sink(DriverMessage::Progress(ProgressUpdate::Update {
        message: format!("Parsing {total} dependency source archives"),
        percentage: None,
    }));
    if total == 0 {
        return;
    }

    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(total);
    // Report progress ~5 % of the way, not per archive: a large closure would
    // otherwise be thousands of hub messages and log lines.
    let progress_step = (total / 20).max(1);
    let next = AtomicUsize::new(0);
    let timers = Timers::default();
    let (tx, rx) = mpsc::channel::<Extracted>();

    let mut artifacts_indexed = 0usize;
    let mut files_indexed = 0usize;
    let mut attempted = 0usize;

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let tx = tx.clone();
            let next = &next;
            let timers = &timers;
            scope.spawn(move || {
                let mut parser = crate::index::java_parser();
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= total {
                        break;
                    }
                    if tx
                        .send(index_one(
                            &mut parser,
                            &artifacts[index],
                            repo,
                            cache,
                            timers,
                        ))
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }
        // This thread also holds a sender; drop it so `rx` ends with the workers.
        drop(tx);

        for extracted in rx {
            attempted += 1;
            if let Extracted::Indexed {
                sources_uri,
                class_uri,
                entries,
                types,
                files,
            } = extracted
            {
                if let Some(class_uri) = class_uri {
                    sink(DriverMessage::RemoveBase { uri: class_uri });
                }
                sink(DriverMessage::BaseArtifact {
                    uri: sources_uri,
                    entries: Arc::new(entries),
                    types: Arc::new(types),
                });
                artifacts_indexed += 1;
                files_indexed += files;
            }
            if attempted == total || attempted % progress_step == 0 {
                sink(DriverMessage::Progress(ProgressUpdate::Update {
                    message: format!(
                        "Parsed {attempted}/{total} dependency source archives ({files_indexed} source files)"
                    ),
                    percentage: None,
                }));
            }
            if attempted % TIMING_STEP == 0 {
                log_timings(&mut *sink, &timers, attempted, total);
            }
        }
    });

    if artifacts_indexed == 0 {
        return;
    }
    sink(DriverMessage::Progress(ProgressUpdate::Update {
        message: format!("Indexed {files_indexed} dependency source files"),
        percentage: None,
    }));
    sink(DriverMessage::Log {
        level: LogLevel::Info,
        message: format!(
            "library sources indexed: {artifacts_indexed} artifacts, {files_indexed} files"
        ),
    });
    log_timings(&mut *sink, &timers, attempted, total);
}

/// One artifact's extracted and indexed sources, or nothing when the jar is
/// missing, holds no `.java` files, or its paths are unusable.
enum Extracted {
    Skipped,
    Indexed {
        sources_uri: Url,
        class_uri: Option<Url>,
        entries: Vec<SymbolEntry>,
        types: crate::types::TypeModel,
        files: usize,
    },
}

/// Extracts one artifact's `.java` files into the cache — creating each package
/// directory once and rewriting a file only when it is missing or the wrong
/// length, so a restart over an already-extracted tree does no write work — and
/// parses them with the caller's parser (one per worker thread), returning the
/// single merged base layer. Never touches the sink, so it is safe to run off the
/// hub's thread.
fn index_one(
    parser: &mut tree_sitter::Parser,
    artifact: &Artifact,
    repo: &Path,
    cache: &Path,
    timers: &Timers,
) -> Extracted {
    let (group, id, version) = artifact;
    let t = Instant::now();
    let Ok(data) = std::fs::read(sources_jar_path(repo, group, id, version)) else {
        return Extracted::Skipped;
    };
    add_nanos(&timers.read, t.elapsed());
    let directory = cache.join(group).join(id).join(version);
    let mut files: Vec<(Vec<SymbolEntry>, Vec<TypeInfo>)> = Vec::new();
    // Package directories already created for this artifact, so `create_dir_all`
    // runs once per directory instead of once per file.
    let mut dirs: HashSet<PathBuf> = HashSet::new();

    let mut callback = Duration::ZERO;
    let t = Instant::now();
    crate::classfile::for_each_zip_entry(&data, |name, bytes| {
        if !name.ends_with(".java") || name.ends_with("package-info.java") {
            return;
        }
        let c0 = Instant::now();
        let Ok(text) = String::from_utf8(bytes) else {
            callback += c0.elapsed();
            return;
        };
        let Some(tree) = parser.parse(text.as_bytes(), None) else {
            callback += c0.elapsed();
            return;
        };
        let c1 = Instant::now();
        let target = directory.join(&name);
        let Some(parent) = target.parent() else {
            callback += c0.elapsed();
            return;
        };
        let mut ok = true;
        if !dirs.contains(parent) {
            ok = std::fs::create_dir_all(parent).is_ok();
            if ok {
                dirs.insert(parent.to_path_buf());
            }
        }
        // A restart has the whole tree already extracted: rewrite only when the
        // file is missing or truncated, so the writes (a large share of the pass)
        // are skipped.
        if ok && std::fs::metadata(&target).map(|meta| meta.len()).ok() != Some(text.len() as u64) {
            ok = std::fs::write(&target, text.as_bytes()).is_ok();
        }
        let c2 = Instant::now();
        if !ok {
            callback += c0.elapsed();
            return;
        }
        let Ok(uri) = Url::from_file_path(&target) else {
            callback += c0.elapsed();
            return;
        };
        let mut entries = crate::index::extract_entries(&uri, &tree, &text);
        // Import entries are never read by a feature; a library tree has millions.
        crate::index::drop_import_entries(&mut entries);
        let c3 = Instant::now();
        for entry in &mut entries {
            entry.dependency = true;
            entry.library_source = true;
        }
        let package = crate::types::file_package(&tree, &text);
        let infos = crate::types::collect_type_infos(package.as_deref(), &tree, &text);
        let c4 = Instant::now();
        add_nanos(&timers.parse, c1 - c0);
        add_nanos(&timers.write, c2 - c1);
        add_nanos(&timers.entries, c3 - c2);
        add_nanos(&timers.types, c4 - c3);
        files.push((entries, infos));
        callback += c0.elapsed();
    });
    add_nanos(&timers.inflate, t.elapsed().saturating_sub(callback));

    if files.is_empty() {
        return Extracted::Skipped;
    }
    let Ok(sources_uri) = Url::from_file_path(sources_jar_path(repo, group, id, version)) else {
        return Extracted::Skipped;
    };
    let class_uri = Url::from_file_path(class_jar_path(repo, group, id, version)).ok();
    // One base layer for the whole artifact, as the jar and JDK indexers do and as
    // the base model documents ("one layer per artifact URI"). Each file's entries
    // keep their own source URI, so navigation is unchanged, while the hub, the hub
    // log, and the base stay at one per artifact instead of one per file.
    let mut entries_all: Vec<SymbolEntry> = Vec::new();
    let mut types = crate::types::TypeModel::new();
    let files_count = files.len();
    for (entries, infos) in files {
        entries_all.extend(entries);
        types.extend(infos);
    }
    Extracted::Indexed {
        sources_uri,
        class_uri,
        entries: entries_all,
        types,
        files: files_count,
    }
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

        let hub = crate::hub::HubClient::standalone();
        let available = fetch_sources(&[artifact()], &repo, &server.base_url(), &hub).await;
        assert_eq!(available, vec![artifact()]);
        assert!(sources_jar_path(&repo, "demo", "lib", "1.0").is_file());

        // A class-file entry for the same artifact must be replaced, not kept.
        let class_uri = Url::from_file_path(class_jar_path(&repo, "demo", "lib", "1.0")).unwrap();
        let zero = tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(0, 0),
            tower_lsp::lsp_types::Position::new(0, 0),
        );
        hub.upsert_file(
            &class_uri,
            vec![SymbolEntry {
                uri: std::sync::Arc::new(class_uri.clone()),
                name: "Thing".to_string(),
                kind: crate::index::IndexKind::Class,
                package: Some("demo".into()),
                container: std::sync::Arc::from(Vec::<String>::new()),
                full_range: zero,
                selection_range: zero,
                dependency: true,
                library_source: false,
                synthetic: false,
            }],
        );

        let mut sink = |message| {
            let _ = hub.notify(message);
        };
        index_extracted(&mut sink, &available, &repo, &cache);

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
            hub.all_symbols()
                .await
                .iter()
                .all(|entry| *entry.uri != class_uri),
            "the class-file entries should be dropped"
        );
        let entries = hub.query_name("Thing").await;
        assert_eq!(entries.len(), 1);
        assert!(entries[0].dependency && entries[0].library_source);
        assert_eq!(entries[0].uri.to_file_path().unwrap(), extracted);
        // The cache is never a references or rename candidate.
        assert!(hub.source_files().await.is_empty());
    }

    #[tokio::test]
    async fn a_missing_checksum_is_tolerated() {
        let _env = crate::jdk::env_lock();
        let fixture = TempDir::new("no-checksum");
        let repo = fixture.path().join("repo");
        let sources = stored_zip(&[("demo/Thing.java", SOURCE)]);
        let server = TestServer::start(sources_jar_routes(sources, None));

        let hub = crate::hub::HubClient::standalone();
        let available = fetch_sources(&[artifact()], &repo, &server.base_url(), &hub).await;
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

        let hub = crate::hub::HubClient::standalone();
        let available = fetch_sources(&[artifact()], &repo, &server.base_url(), &hub).await;
        assert!(available.is_empty());
        assert!(!sources_jar_path(&repo, "demo", "lib", "1.0").is_file());
    }

    #[tokio::test]
    async fn an_unreachable_repository_keeps_class_files() {
        let _env = crate::jdk::env_lock();
        let fixture = TempDir::new("unreachable");
        let repo = fixture.path().join("repo");
        // Port 1 is not served; the fetch must fail without panicking.
        let hub = crate::hub::HubClient::standalone();
        let available = fetch_sources(&[artifact()], &repo, "http://127.0.0.1:1", &hub).await;
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
        let hub = crate::hub::HubClient::standalone();
        index_sources(vec![artifact()], hub).await;
        std::env::remove_var("JAVA_LSP_OFFLINE");
        std::env::remove_var("MAVEN_REPO");
        std::env::remove_var("JAVA_LSP_MAVEN_CENTRAL_URL");

        assert!(!sources_jar_path(&repo, "demo", "lib", "1.0").is_file());
    }

    const OTHER: &[u8] = b"package demo;\n\npublic class Other {\n}\n";

    #[test]
    fn a_sources_jar_is_published_as_one_base_artifact() {
        let fixture = TempDir::new("one-base-artifact");
        let repo = fixture.path().join("repo");
        let cache = fixture.path().join("cache");
        // Write the sources jar straight into the repository path, so the test
        // needs no server: `index_extracted` reads it from disk.
        let sources = stored_zip(&[("demo/Thing.java", SOURCE), ("demo/Other.java", OTHER)]);
        let jar = sources_jar_path(&repo, "demo", "lib", "1.0");
        std::fs::create_dir_all(jar.parent().unwrap()).unwrap();
        std::fs::write(&jar, &sources).unwrap();

        let hub = crate::hub::HubClient::standalone();
        let mut published: Vec<DriverMessage> = Vec::new();
        {
            let mut sink = |message: DriverMessage| {
                let _ = hub.notify(message.clone());
                published.push(message);
            };
            index_extracted(&mut sink, &[artifact()], &repo, &cache);
        }

        // One base artifact for the whole jar, not one per source file.
        let base_artifacts = published
            .iter()
            .filter(|message| matches!(message, DriverMessage::BaseArtifact { .. }))
            .count();
        assert_eq!(
            base_artifacts, 1,
            "expected one BaseArtifact per artifact, got {base_artifacts}"
        );
        // A per-archive progress update keeps the work-done item moving.
        assert!(
            published.iter().any(|message| matches!(
                message,
                DriverMessage::Progress(ProgressUpdate::Update { message, .. })
                    if message == "Parsed 1/1 dependency source archives (2 source files)"
            )),
            "expected a per-archive progress update"
        );
        // Both files' entries are in that single layer.
        assert_eq!(hub.query_name("Thing").blocking_recv().len(), 1);
        assert_eq!(hub.query_name("Other").blocking_recv().len(), 1);
    }
}
