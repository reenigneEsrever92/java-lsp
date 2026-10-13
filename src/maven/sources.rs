//! Fetching and extracting Maven dependency sources.
//!
//! Rework of `src/deprecated/sources.rs` that *only* fetches and extracts — the
//! tree-sitter parsing/indexing and the `DriverMessage` progress vocabulary are
//! gone; the extracted root directories are returned so the `maven` spawn can
//! publish them via `FoldersDiscoverred`.
//!
//! For every resolved artifact, a `<a>-<v>-sources.jar` already in the local
//! repository is reused; otherwise it is downloaded from the settings-aware
//! download base (Maven Central by default) and written back into the local
//! repository at Maven's standard path, `.sha1`-verified when published. The
//! sources are then extracted into a java-lsp cache, and the root directory of
//! every artifact that yielded at least one `.java` file is returned.
//!
//! A missing/unreadable `.sha1` is accepted (not every artifact publishes one);
//! a mismatch fails that one artifact, which is logged and skipped. Everything
//! here runs off the request path, and `offline()`/`download_base()` delegate to
//! [`settings`](crate::maven::settings).

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use crate::maven::resolve::Artifact;

/// Concurrent downloads per warm-up: enough to keep a large closure moving
/// without opening a socket per artifact.
const MAX_CONCURRENCY: usize = 8;
const USER_AGENT: &str = concat!("java-lsp/", env!("CARGO_PKG_VERSION"));

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

/// The sources jar for these coordinates, in the local repository.
pub fn sources_jar_path(root: &Path, group: &str, id: &str, version: &str) -> PathBuf {
    root.join(group.replace('.', "/"))
        .join(id)
        .join(version)
        .join(format!("{id}-{version}-sources.jar"))
}

/// Fetches and extracts the dependency sources of `artifacts` into `cache`,
/// returning the extracted root directory (`<cache>/<group>/<id>/<version>`) of
/// every artifact that yielded at least one `.java` file, sorted and
/// deduplicated. A `-sources.jar` already in `repo` is reused; a missing one is
/// downloaded from the settings-aware base URL, `.sha1`-verified when
/// published, and written into `repo`. A no-op when downloads are disabled or
/// nothing resolved. Failures are logged and skipped.
pub async fn fetch_and_extract(artifacts: &[Artifact], repo: &Path, cache: &Path) -> Vec<PathBuf> {
    if artifacts.is_empty() || crate::maven::settings::offline() {
        return Vec::new();
    }
    let settings = crate::maven::settings::Settings::load();
    let base = crate::maven::settings::download_base(&settings);
    fetch_and_extract_from(artifacts, repo, cache, &base).await
}

/// The fetch-then-extract pass over an explicit download `base`, so the bulk of
/// the behaviour can be exercised without touching the process environment.
async fn fetch_and_extract_from(
    artifacts: &[Artifact],
    repo: &Path,
    cache: &Path,
    base: &str,
) -> Vec<PathBuf> {
    let available = fetch_sources(artifacts, repo, base).await;
    if available.is_empty() {
        return Vec::new();
    }
    let mut roots = extract(&available, repo, cache);
    roots.sort();
    roots.dedup();
    roots
}

// -- the source downloader --------------------------------------------------

/// Returns the subset of `artifacts` whose sources jar is on disk after this
/// call — reused from the local repository or downloaded into it. A failure is
/// logged and skipped.
async fn fetch_sources(artifacts: &[Artifact], repo: &Path, base: &str) -> Vec<Artifact> {
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

    let client = match reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!("no HTTP client for dependency sources; skipping downloads ({error})");
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
                        "dependency sources unavailable for {}:{}:{}; skipping ({error})",
                        artifact.0,
                        artifact.1,
                        artifact.2
                    );
                    None
                }
            }
        });
    }
    while let Some(joined) = tasks.join_next().await {
        if let Ok(Some(artifact)) = joined {
            available.push(artifact);
        }
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

// -- extraction -------------------------------------------------------------

/// Extracts each artifact's sources jar into the cache, returning the root
/// directory of every artifact that yielded at least one `.java` file.
///
/// The artifacts are extracted across `available_parallelism` worker threads,
/// each of which reads its jar, walks its entries, and writes the `.java` files
/// it does not already have; the finished roots are handed back over an
/// `mpsc` channel to this thread, which collects them.
fn extract(artifacts: &[Artifact], repo: &Path, cache: &Path) -> Vec<PathBuf> {
    let total = artifacts.len();
    if total == 0 {
        return Vec::new();
    }
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(total);
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<Option<PathBuf>>();
    let mut roots = Vec::new();

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let tx = tx.clone();
            let next = &next;
            scope.spawn(move || loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= total {
                    break;
                }
                if tx
                    .send(extract_one(&artifacts[index], repo, cache))
                    .is_err()
                {
                    break;
                }
            });
        }
        // This thread also holds a sender; drop it so `rx` ends with the workers.
        drop(tx);

        for root in rx.into_iter().flatten() {
            roots.push(root);
        }
    });

    roots
}

/// Extracts one artifact's `.java` files into the cache — creating each package
/// directory once and rewriting a file only when it is missing or the wrong
/// length, so a restart over an already-extracted tree does no write work — and
/// returns the cache root (`<cache>/<group>/<id>/<version>`) when at least one
/// `.java` file ended up on disk. Nothing to parse; nothing to index.
fn extract_one(artifact: &Artifact, repo: &Path, cache: &Path) -> Option<PathBuf> {
    let (group, id, version) = artifact;
    let data = std::fs::read(sources_jar_path(repo, group, id, version)).ok()?;
    let root = cache.join(group).join(id).join(version);
    // Package directories already created for this artifact, so `create_dir_all`
    // runs once per directory instead of once per file.
    let mut dirs: HashSet<PathBuf> = HashSet::new();
    let mut extracted = 0usize;

    for_each_zip_entry(&data, |name, bytes| {
        if !is_java_source(&name) {
            return;
        }
        let Some(target) = safe_target(&root, &name) else {
            return;
        };
        let Some(parent) = target.parent() else {
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
        if ok && std::fs::metadata(&target).map(|meta| meta.len()).ok() != Some(bytes.len() as u64)
        {
            ok = std::fs::write(&target, &bytes).is_ok();
        }
        if ok {
            extracted += 1;
        }
    });

    if extracted == 0 {
        None
    } else {
        Some(root)
    }
}

/// True for a source file we extract: a `.java` entry that is not a
/// `package-info.java`.
fn is_java_source(name: &str) -> bool {
    name.ends_with(".java") && !name.ends_with("package-info.java")
}

/// The on-disk path for a zip `name` under `root`, or `None` when the name is
/// not a safe relative path (absolute, empty, or containing a `..` or an empty
/// component). A zip entry must never escape `root`.
fn safe_target(root: &Path, name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.starts_with('/') || name.starts_with('\\') || name.contains('\0') {
        return None;
    }
    let mut target = root.to_path_buf();
    for part in name.split('/') {
        if part.is_empty() || part == ".." {
            return None;
        }
        target.push(part);
    }
    Some(target)
}

// -- a minimal in-memory ZIP reader -----------------------------------------
//
// The sources jars are read directly from a byte slice: the central directory
// is walked, each entry is located by its local header, and STORED/deflated
// content is materialized. This mirrors the deprecated `classfile` reader (not
// in the build) closely enough for extraction and nothing more.

/// Hands every central-directory entry of an in-memory ZIP to `callback` one at
/// a time, so a large archive never materializes fully in memory.
fn for_each_zip_entry(data: &[u8], mut callback: impl FnMut(String, Vec<u8>)) {
    let Some(eocd) = find_eocd(data) else {
        return;
    };
    let Some(entry_count) = u16_le(data, eocd + 10).map(usize::from) else {
        return;
    };
    let Some(mut offset) = u32_le(data, eocd + 16).map(|value| value as usize) else {
        return;
    };

    for _ in 0..entry_count {
        if !has(data, offset, [0x50, 0x4b, 0x01, 0x02]) {
            break; // central directory over; tolerate trailing junk
        }
        let Some(method) = u16_le(data, offset + 10) else {
            break;
        };
        let Some(compressed_size) = u32_le(data, offset + 20).map(|value| value as usize) else {
            break;
        };
        let Some(name_len) = u16_le(data, offset + 28).map(usize::from) else {
            break;
        };
        let Some(extra_len) = u16_le(data, offset + 30).map(usize::from) else {
            break;
        };
        let Some(comment_len) = u16_le(data, offset + 32).map(usize::from) else {
            break;
        };
        let Some(local_offset) = u32_le(data, offset + 42).map(|value| value as usize) else {
            break;
        };
        let Some(name_bytes) = data.get(offset + 46..offset + 46 + name_len) else {
            break;
        };
        let name = String::from_utf8_lossy(name_bytes).into_owned();

        if let Some(content) = read_entry_data(data, local_offset, method, compressed_size, &name) {
            callback(name, content);
        }

        offset += 46 + name_len + extra_len + comment_len;
    }
}

/// Materializes one entry's content from its local header, inflating it when the
/// entry is deflated. Returns `None` for an unsupported method or a malformed
/// header.
fn read_entry_data(
    data: &[u8],
    local_offset: usize,
    method: u16,
    compressed_size: usize,
    name: &str,
) -> Option<Vec<u8>> {
    if !has(data, local_offset, [0x50, 0x4b, 0x03, 0x04]) {
        return None;
    }
    let name_len = u16_le(data, local_offset + 26)? as usize;
    let extra_len = u16_le(data, local_offset + 28)? as usize;
    let start = local_offset + 30 + name_len + extra_len;
    let compressed = data.get(start..start + compressed_size)?;
    match method {
        0 => Some(compressed.to_vec()),
        8 => {
            let mut out = Vec::new();
            flate2::read::DeflateDecoder::new(compressed)
                .read_to_end(&mut out)
                .ok()?;
            Some(out)
        }
        other => {
            tracing::warn!(
                entry = name,
                method = other,
                "unsupported sources-jar compression"
            );
            None
        }
    }
}

/// Scans backwards for the end-of-central-directory record (a comment may
/// follow it, so the signature is located from the archive's tail).
fn find_eocd(data: &[u8]) -> Option<usize> {
    let scan = data.len().saturating_sub(66_000);
    let mut i = data.len().checked_sub(22)?;
    while i >= scan {
        if has(data, i, [0x50, 0x4b, 0x05, 0x06]) {
            return Some(i);
        }
        i = i.checked_sub(1)?;
    }
    None
}

/// True when the four bytes at `offset` are exactly `signature`.
fn has(data: &[u8], offset: usize, signature: [u8; 4]) -> bool {
    data.get(offset..offset + 4) == Some(signature.as_slice())
}

fn u16_le(data: &[u8], offset: usize) -> Option<u16> {
    let bytes = data.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn u32_le(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::await_holding_lock)]

    use super::*;
    use crate::maven::settings;
    use crate::maven::tests::env_lock;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};

    /// Sets or removes an environment variable, restoring the previous value on
    /// drop.
    struct EnvVar {
        name: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVar {
        fn set(name: &'static str, value: impl AsRef<std::ffi::OsStr>) -> EnvVar {
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

    /// Points the settings-aware download base at `base_url`, clears
    /// `$JAVA_LSP_OFFLINE`, and makes `Settings::load` read a missing file so no
    /// real `~/.m2/settings.xml` leaks in. The returned guards must be kept
    /// alive for the test.
    fn hermetic_env(base_url: &str) -> (EnvVar, EnvVar, EnvVar) {
        (
            EnvVar::set(
                "JAVA_LSP_MAVEN_SETTINGS",
                std::env::temp_dir().join("java-lsp-sources-absent-settings.xml"),
            ),
            EnvVar::remove("JAVA_LSP_OFFLINE"),
            EnvVar::set("JAVA_LSP_MAVEN_CENTRAL_URL", base_url),
        )
    }

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
    /// Counts the connections it accepts, so a test can prove no request was
    /// made.
    struct TestServer {
        address: SocketAddr,
        hits: Arc<AtomicUsize>,
    }

    impl TestServer {
        fn start(routes: HashMap<String, Vec<u8>>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let hits = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&hits);
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let routes = routes.clone();
                    std::thread::spawn(move || serve(stream, &routes));
                }
            });
            Self { address, hits }
        }

        fn base_url(&self) -> String {
            format!("http://{}", self.address)
        }

        fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
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

    /// A STORED (uncompressed) ZIP containing `files`, readable by the reader
    /// under test without any deflate.
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

    /// Writes a stored sources jar straight into the repository path, returning
    /// its path, so a test needs no server.
    fn write_local_sources_jar(repo: &Path, artifact: &Artifact, sources: &[u8]) -> PathBuf {
        let jar = sources_jar_path(repo, &artifact.0, &artifact.1, &artifact.2);
        std::fs::create_dir_all(jar.parent().unwrap()).unwrap();
        std::fs::write(&jar, sources).unwrap();
        jar
    }

    const SOURCE: &[u8] = b"package demo;\n\npublic class Thing {\n    public void go() {}\n}\n";
    const OTHER: &[u8] = b"package demo;\n\npublic class Other {\n}\n";

    #[test]
    fn cache_dir_and_settings_delegates_follow_the_environment() {
        let _lock = env_lock();
        let fixture = TempDir::new("env");
        let home = fixture.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let _settings = EnvVar::set(
            "JAVA_LSP_MAVEN_SETTINGS",
            fixture.path().join("missing.xml"),
        );
        let _offline = EnvVar::remove("JAVA_LSP_OFFLINE");
        let _central = EnvVar::remove("JAVA_LSP_MAVEN_CENTRAL_URL");
        let _sources_cache = EnvVar::remove("JAVA_LSP_SOURCES_CACHE");
        let _xdg = EnvVar::remove("XDG_CACHE_HOME");
        let _home = EnvVar::set("HOME", &home);

        assert!(!settings::offline());
        assert_eq!(
            settings::download_base(&settings::Settings::load()),
            settings::DEFAULT_CENTRAL
        );
        // With no overrides, the XDG-less home fallback is used.
        assert_eq!(
            cache_dir(),
            home.join(".cache").join("java-lsp").join("sources")
        );

        // `$XDG_CACHE_HOME` is the next fallback.
        let _xdg = EnvVar::set("XDG_CACHE_HOME", home.join("xdg"));
        assert_eq!(
            cache_dir(),
            home.join("xdg").join("java-lsp").join("sources")
        );

        // `$JAVA_LSP_SOURCES_CACHE` wins over XDG.
        let _sources_cache = EnvVar::set("JAVA_LSP_SOURCES_CACHE", "/tmp/java-lsp-sources-cache");
        assert_eq!(cache_dir(), PathBuf::from("/tmp/java-lsp-sources-cache"));

        // The env base URL is normalized, and `$JAVA_LSP_OFFLINE` forces offline.
        let _central = EnvVar::set(
            "JAVA_LSP_MAVEN_CENTRAL_URL",
            "https://mirror.example/maven2/",
        );
        assert_eq!(
            settings::download_base(&settings::Settings::load()),
            "https://mirror.example/maven2"
        );
        let _offline = EnvVar::set("JAVA_LSP_OFFLINE", "1");
        assert!(settings::offline());
    }

    #[tokio::test]
    async fn downloads_verifies_and_extracts_sources() {
        let _lock = env_lock();
        let fixture = TempDir::new("download");
        let repo = fixture.path().join("repo");
        let cache = fixture.path().join("cache");
        let sources = stored_zip(&[("demo/Thing.java", SOURCE)]);
        let server = TestServer::start(sources_jar_routes(
            sources.clone(),
            Some(&sha1_hex(&sources)),
        ));
        let _env = hermetic_env(&server.base_url());

        let roots = fetch_and_extract(&[artifact()], &repo, &cache).await;
        assert_eq!(roots, vec![cache.join("demo").join("lib").join("1.0")]);
        assert!(sources_jar_path(&repo, "demo", "lib", "1.0").is_file());
        assert!(
            server.hits() > 0,
            "the missing jar should have been fetched"
        );

        let extracted = cache
            .join("demo")
            .join("lib")
            .join("1.0")
            .join("demo")
            .join("Thing.java");
        assert_eq!(
            std::fs::read(&extracted).unwrap(),
            SOURCE,
            "the source should be extracted verbatim into the cache"
        );
    }

    #[tokio::test]
    async fn nothing_resolved_is_a_no_op() {
        let fixture = TempDir::new("empty");
        let repo = fixture.path().join("repo");
        let cache = fixture.path().join("cache");

        // An empty artifact list short-circuits before any settings/network use.
        let roots = fetch_and_extract(&[], &repo, &cache).await;
        assert!(roots.is_empty());
        assert!(!repo.exists());
        assert!(!cache.exists());
    }

    #[tokio::test]
    async fn a_local_sources_jar_is_reused_without_a_request() {
        let fixture = TempDir::new("reuse");
        let repo = fixture.path().join("repo");
        let cache = fixture.path().join("cache");
        write_local_sources_jar(
            &repo,
            &artifact(),
            &stored_zip(&[("demo/Thing.java", SOURCE)]),
        );
        let server = TestServer::start(HashMap::new());

        let roots = fetch_and_extract_from(&[artifact()], &repo, &cache, &server.base_url()).await;
        assert_eq!(roots, vec![cache.join("demo").join("lib").join("1.0")]);
        assert_eq!(server.hits(), 0, "a reused jar must not touch the network");
    }

    #[tokio::test]
    async fn a_checksum_mismatch_skips_only_that_artifact() {
        let fixture = TempDir::new("bad-checksum");
        let repo = fixture.path().join("repo");
        let cache = fixture.path().join("cache");
        let good = ("demo".to_string(), "good".to_string(), "2.0".to_string());
        let bad_jar = stored_zip(&[("demo/Thing.java", SOURCE)]);
        let good_jar = stored_zip(&[("demo/Other.java", OTHER)]);
        let mut routes = HashMap::new();
        routes.insert(
            "/demo/lib/1.0/lib-1.0-sources.jar".to_string(),
            bad_jar.clone(),
        );
        routes.insert(
            "/demo/lib/1.0/lib-1.0-sources.jar.sha1".to_string(),
            b"0000000000000000000000000000000000000000".to_vec(),
        );
        routes.insert(
            "/demo/good/2.0/good-2.0-sources.jar".to_string(),
            good_jar.clone(),
        );
        routes.insert(
            "/demo/good/2.0/good-2.0-sources.jar.sha1".to_string(),
            sha1_hex(&good_jar).into_bytes(),
        );
        let server = TestServer::start(routes);

        let roots = fetch_and_extract_from(
            &[artifact(), good.clone()],
            &repo,
            &cache,
            &server.base_url(),
        )
        .await;
        assert_eq!(roots, vec![cache.join("demo").join("good").join("2.0")]);
        assert!(
            !sources_jar_path(&repo, "demo", "lib", "1.0").is_file(),
            "the mismatched jar must not be written into the repository"
        );
        assert!(sources_jar_path(&repo, "demo", "good", "2.0").is_file());
    }

    #[tokio::test]
    async fn a_missing_published_checksum_is_tolerated() {
        let fixture = TempDir::new("no-checksum");
        let repo = fixture.path().join("repo");
        let cache = fixture.path().join("cache");
        let sources = stored_zip(&[("demo/Thing.java", SOURCE)]);
        let server = TestServer::start(sources_jar_routes(sources, None));

        let roots = fetch_and_extract_from(&[artifact()], &repo, &cache, &server.base_url()).await;
        assert_eq!(roots, vec![cache.join("demo").join("lib").join("1.0")]);
        assert!(sources_jar_path(&repo, "demo", "lib", "1.0").is_file());
    }

    #[tokio::test]
    async fn an_unreachable_repository_is_skipped() {
        let fixture = TempDir::new("unreachable");
        let repo = fixture.path().join("repo");
        let cache = fixture.path().join("cache");
        // Port 1 is not served here; the fetch must fail without panicking.

        let roots =
            fetch_and_extract_from(&[artifact()], &repo, &cache, "http://127.0.0.1:1").await;
        assert!(roots.is_empty());
        assert!(!sources_jar_path(&repo, "demo", "lib", "1.0").is_file());
    }

    #[tokio::test]
    async fn package_info_and_non_java_entries_are_skipped() {
        let fixture = TempDir::new("skip-entries");
        let repo = fixture.path().join("repo");
        let cache = fixture.path().join("cache");
        let sources = stored_zip(&[
            ("demo/Thing.java", SOURCE),
            ("demo/package-info.java", b"package demo;\n"),
            ("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\n"),
            ("demo/Thing.class", b"\xca\xfe\xba\xbe"),
        ]);
        write_local_sources_jar(&repo, &artifact(), &sources);
        // An artifact whose jar holds only a `package-info.java` yields no root.
        let only_pkg = ("demo".to_string(), "meta".to_string(), "3.0".to_string());
        write_local_sources_jar(
            &repo,
            &only_pkg,
            &stored_zip(&[("demo/package-info.java", b"package demo;\n")]),
        );
        let server = TestServer::start(HashMap::new());

        let roots =
            fetch_and_extract_from(&[artifact(), only_pkg], &repo, &cache, &server.base_url())
                .await;
        let root = cache.join("demo").join("lib").join("1.0");
        assert_eq!(roots, vec![root.clone()]);
        assert!(root.join("demo").join("Thing.java").is_file());
        assert!(!root.join("demo").join("package-info.java").exists());
        assert!(!root.join("META-INF").exists());
        assert!(!root.join("demo").join("Thing.class").exists());
        assert_eq!(server.hits(), 0);
    }

    #[tokio::test]
    async fn unsafe_entry_names_are_not_extracted() {
        let fixture = TempDir::new("unsafe-names");
        let repo = fixture.path().join("repo");
        let cache = fixture.path().join("cache");
        let sources = stored_zip(&[
            ("demo/Thing.java", SOURCE),
            ("../escape.java", OTHER),
            ("/absolute.java", OTHER),
            ("demo/../../escape2.java", OTHER),
        ]);
        write_local_sources_jar(&repo, &artifact(), &sources);
        let server = TestServer::start(HashMap::new());

        let roots = fetch_and_extract_from(&[artifact()], &repo, &cache, &server.base_url()).await;
        let root = cache.join("demo").join("lib").join("1.0");
        assert_eq!(roots, vec![root.clone()]);
        assert!(root.join("demo").join("Thing.java").is_file());
        // None of the unsafe entries may have landed anywhere.
        assert!(!cache.join("escape.java").exists());
        assert!(!fixture.path().join("escape.java").exists());
        assert!(!fixture.path().join("escape2.java").exists());
    }

    #[tokio::test]
    async fn a_restart_over_an_extracted_tree_rewrites_nothing() {
        let fixture = TempDir::new("restart");
        let repo = fixture.path().join("repo");
        let cache = fixture.path().join("cache");
        write_local_sources_jar(
            &repo,
            &artifact(),
            &stored_zip(&[("demo/Thing.java", SOURCE), ("demo/Other.java", OTHER)]),
        );
        let server = TestServer::start(HashMap::new());

        let first = fetch_and_extract_from(&[artifact()], &repo, &cache, &server.base_url()).await;
        let file = cache.join("demo/lib/1.0/demo/Thing.java");
        let modified = std::fs::metadata(&file).unwrap().modified().unwrap();

        let second = fetch_and_extract_from(&[artifact()], &repo, &cache, &server.base_url()).await;
        assert_eq!(first, second, "the roots are stable across a restart");
        assert_eq!(
            std::fs::metadata(&file).unwrap().modified().unwrap(),
            modified,
            "an up-to-date file must not be rewritten"
        );
        assert_eq!(std::fs::read(&file).unwrap(), SOURCE);
    }

    #[tokio::test]
    async fn offline_short_circuits_the_whole_pass() {
        let _lock = env_lock();
        let fixture = TempDir::new("offline");
        let repo = fixture.path().join("repo");
        let cache = fixture.path().join("cache");
        let sources = stored_zip(&[("demo/Thing.java", SOURCE)]);
        let server = TestServer::start(sources_jar_routes(sources, None));
        let _settings = EnvVar::set(
            "JAVA_LSP_MAVEN_SETTINGS",
            fixture.path().join("missing.xml"),
        );
        let _central = EnvVar::set("JAVA_LSP_MAVEN_CENTRAL_URL", server.base_url());
        let _offline = EnvVar::set("JAVA_LSP_OFFLINE", "1");

        let roots = fetch_and_extract(&[artifact()], &repo, &cache).await;
        assert!(roots.is_empty());
        assert_eq!(server.hits(), 0, "offline must not reach the network");
        assert!(!sources_jar_path(&repo, "demo", "lib", "1.0").is_file());
    }
}
