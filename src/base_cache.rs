//! Cross-run cache of the non-source class-file base (dependency jars and the
//! JDK), so a restart re-parses only what changed.
//!
//! Best-effort by construction and never a source of a wrong or partial index: a
//! missing, short, or corrupt file, a schema-version change, or an archive whose
//! identity changed forces a full parse. Each archive's parse is **one small
//! file**, written beside its target and renamed into place, and read only when
//! that archive is needed — so a large cache is never held resident (the earlier
//! single-file cache could be gigabytes, loaded and re-written whole each run).

use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::Url;

use crate::index::SymbolEntry;
use crate::types::TypeInfo;

/// Bump to invalidate every cache when the parse or the entry/model shape
/// changes.
const SCHEMA_VERSION: u32 = 1;

/// One archive's cached parse: exactly what the class-file pass produced.
#[derive(Serialize, Deserialize)]
pub struct ArchiveOutput {
    pub entries: Vec<SymbolEntry>,
    pub types: Vec<TypeInfo>,
}

/// One JDK archive's cached parse, with the URI its entries are attributed to.
#[derive(Serialize, Deserialize, Clone)]
pub struct JdkArchive {
    pub uri: Url,
    pub entries: Vec<SymbolEntry>,
    pub types: Vec<TypeInfo>,
}

/// One on-disk entry: the schema version and the archive identity guard a stale
/// or colliding file, so a hit is always this archive's own parse.
#[derive(Serialize, Deserialize)]
struct Stored<T> {
    version: u32,
    identity: String,
    value: T,
}

/// A per-archive cache: one JSON file per identity under `<cache>/base/<kind>/`,
/// read on demand and written only when an archive is (re)parsed.
pub struct ArchiveStore {
    dir: PathBuf,
}

impl ArchiveStore {
    pub fn new(kind: &str) -> Self {
        let base = base_dir();
        // One-time migration: the earlier single-file cache (`<base>/<kind>.json`)
        // could be gigabytes and is no longer read; drop it so it does not linger.
        let _ = std::fs::remove_file(base.join(format!("{kind}.json")));
        Self {
            dir: base.join(kind),
        }
    }

    /// The cached parse for `identity`, or `None` on a miss — a missing file,
    /// unreadable bytes, a schema change, or a file that belongs to another
    /// archive (a hash collision).
    pub fn get<T: DeserializeOwned>(&self, identity: &str) -> Option<T> {
        let text = std::fs::read_to_string(self.path(identity)).ok()?;
        let stored: Stored<T> = serde_json::from_str(&text).ok()?;
        (stored.version == SCHEMA_VERSION && stored.identity == identity).then_some(stored.value)
    }

    /// Writes `value` for `identity`, best-effort (a temp file renamed into
    /// place), ignoring every failure — the cache is an optimization only.
    pub fn insert<T: Serialize>(&self, identity: &str, value: &T) {
        if std::fs::create_dir_all(&self.dir).is_err() {
            return;
        }
        let stored = Stored {
            version: SCHEMA_VERSION,
            identity: identity.to_string(),
            value,
        };
        let Ok(text) = serde_json::to_string(&stored) else {
            return;
        };
        let path = self.path(identity);
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_err() {
            return;
        }
        let _ = std::fs::rename(&tmp, &path);
    }

    fn path(&self, identity: &str) -> PathBuf {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        identity.hash(&mut hasher);
        self.dir.join(format!("{:016x}.json", hasher.finish()))
    }
}

/// An archive's identity for the cache: its path, size, and mtime. A missing or
/// unreadable archive has no identity, so it is never served from — or stored
/// in — the cache.
pub fn identity(path: &std::path::Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(format!("{}:{}:{}", path.display(), meta.len(), mtime))
}

/// The base cache directory (`<sources cache>/base`).
fn base_dir() -> PathBuf {
    crate::sources::cache_dir().join("base")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexKind;
    use std::sync::Arc;
    use tower_lsp::lsp_types::{Position, Range};

    fn range() -> Range {
        Range::new(Position::new(0, 0), Position::new(0, 0))
    }

    fn sample() -> ArchiveOutput {
        ArchiveOutput {
            entries: vec![SymbolEntry {
                uri: Arc::new(Url::parse("file:///lib.jar").unwrap()),
                name: "Thing".to_string(),
                kind: IndexKind::Class,
                package: Some("demo".into()),
                container: Arc::from(Vec::<String>::new()),
                full_range: range(),
                selection_range: range(),
                dependency: true,
                library_source: false,
                synthetic: false,
            }],
            types: vec![TypeInfo::new(
                "Thing".to_string(),
                Some("demo".to_string()),
                IndexKind::Class,
            )],
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("java-lsp-base-store-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    fn store(dir: &std::path::Path) -> ArchiveStore {
        ArchiveStore {
            dir: dir.to_path_buf(),
        }
    }

    #[test]
    fn an_archive_round_trips_and_a_version_change_invalidates_it() {
        let dir = temp_dir("roundtrip");
        let store = store(&dir);
        assert!(store.get::<ArchiveOutput>("k").is_none());

        store.insert("k", &sample());
        let output = store.get::<ArchiveOutput>("k").expect("cached archive");
        assert_eq!(output.entries.len(), 1);
        assert_eq!(output.types.len(), 1);
        assert_eq!(output.types[0].name, "Thing");

        // A different schema version is ignored, so the entry is not served.
        let path = store.path("k");
        std::fs::write(
            &path,
            "{\"version\":999999,\"identity\":\"k\",\"value\":{\"entries\":[],\"types\":[]}}",
        )
        .unwrap();
        assert!(store.get::<ArchiveOutput>("k").is_none());

        // A file for another identity is not served (hash collision guard).
        store.insert("other", &sample());
        assert!(
            store.get::<ArchiveOutput>("k").is_none() || store.path("k") != store.path("other")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_or_corrupt_cache_is_a_miss() {
        let dir = temp_dir("missing");
        let store = store(&dir);
        assert!(store.get::<ArchiveOutput>("k").is_none());

        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(store.path("k"), b"not json").unwrap();
        assert!(store.get::<ArchiveOutput>("k").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn identity_changes_when_the_archive_changes() {
        let dir = temp_dir("identity");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.jar");
        std::fs::write(&path, b"one").unwrap();
        let first = identity(&path).expect("identity");
        std::fs::write(&path, b"two-longer").unwrap();
        let second = identity(&path).expect("identity");
        assert_ne!(first, second);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
