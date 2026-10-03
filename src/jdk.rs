//! Standard-library indexing: the installed JDK's `java.*`/`javax.*`
//! declarations, read statically from `jmods` (JDK 9+) or `rt.jar` (JDK 8).
//!
//! No JVM is run and nothing is invoked — the JDK installation is just a
//! local archive source, like `~/.m2` for dependencies. No JDK found or an
//! unusable one is a graceful no-op: warm-up completes without JDK entries.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tower_lsp::lsp_types::Url;

use crate::classfile::{for_each_zip_entry, parse_class};
use crate::hub::HubClient;
use crate::index::SymbolEntry;
use crate::messages::{DriverMessage, LogLevel, ProgressUpdate, Stage};

/// Locates a usable JDK home: explicit override, `JAVA_HOME`, then the
/// common installation directories (including SDKMAN). An explicit override
/// pointing at an unusable home disables JDK indexing entirely (deterministic
/// opt-out); with no override, nothing found is a `None`.
pub fn locate_jdk() -> Option<PathBuf> {
    if let Ok(override_path) = std::env::var("JAVA_LSP_JDK") {
        let home = PathBuf::from(override_path);
        if is_jdk_home(&home) {
            return Some(home);
        }
        tracing::warn!(
            home = %home.display(),
            "JAVA_LSP_JDK points at a JDK without jmods, src.zip, or rt.jar; \
             JDK indexing disabled"
        );
        return None;
    }
    if let Ok(home) = std::env::var("JAVA_HOME") {
        let home = PathBuf::from(home);
        if is_jdk_home(&home) {
            return Some(home);
        }
    }
    common_homes().into_iter().find(|home| is_jdk_home(home))
}

/// Serializes tests that mutate/read JDK- and repo-related env vars.
pub fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn is_jdk_home(home: &Path) -> bool {
    home.join("jmods").is_dir() || src_zip(home).is_some() || rt_jar(home).is_some()
}

/// The sources archive: `lib/src.zip` (JDK 9+ installs without jmods).
fn src_zip(home: &Path) -> Option<PathBuf> {
    let path = home.join("lib/src.zip");
    path.is_file().then_some(path)
}

/// rt.jar lives at `jre/lib/rt.jar` (JDK 8 layouts) or `lib/rt.jar`.
fn rt_jar(home: &Path) -> Option<PathBuf> {
    [home.join("jre/lib/rt.jar"), home.join("lib/rt.jar")]
        .into_iter()
        .find(|path| path.is_file())
}

fn common_homes() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut roots: Vec<String> = vec![
        "/usr/lib/jvm".into(),
        "/Library/Java/JavaVirtualMachines".into(),
    ];
    if let Ok(sdkman) = std::env::var("SDKMAN_DIR") {
        roots.push(format!("{sdkman}/candidates/java"));
    } else if let Ok(home) = std::env::var("HOME") {
        roots.push(format!("{home}/.sdkman/candidates/java"));
    }
    for pattern in &roots {
        let Ok(entries) = std::fs::read_dir(pattern) else {
            continue;
        };
        let mut homes: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        homes.sort();
        for home in homes {
            // macOS installs nest the home one level down.
            let mac_home = home.join("Contents").join("Home");
            out.push(if mac_home.is_dir() { mac_home } else { home });
        }
    }
    // The SDKMAN `current` symlink as a direct candidate.
    if let Ok(home) = std::env::var("HOME") {
        out.push(PathBuf::from(home).join(".sdkman/candidates/java/current"));
    }
    out
}

/// Parses every standard-library class in the JDK at `home`, grouped per
/// archive (jmods, src.zip, or rt.jar) so the index can attribute entries. Each
/// archive also yields the declared types its classes make up.
pub fn jdk_entries(home: &Path) -> Vec<(Url, Vec<SymbolEntry>, Vec<crate::types::TypeInfo>)> {
    if home.join("jmods").is_dir() {
        jmod_entries(home)
    } else if let Some(src) = src_zip(home) {
        src_zip_entries(&src)
    } else if let Some(rt) = rt_jar(home) {
        vec![class_archive_entries(&rt, false)]
            .into_iter()
            .flatten()
            .collect()
    } else {
        Vec::new()
    }
}

/// The archives `jdk_entries` reads for `home`, for the base cache's identity
/// key. Empty when no readable standard-library archive exists.
pub(crate) fn jdk_archive_paths(home: &Path) -> Vec<PathBuf> {
    if home.join("jmods").is_dir() {
        let mut archives: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(home.join("jmods")) {
            archives.extend(
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "jmod")),
            );
        }
        archives.sort();
        archives
    } else if let Some(src) = src_zip(home) {
        vec![src]
    } else if let Some(rt) = rt_jar(home) {
        vec![rt]
    } else {
        Vec::new()
    }
}

/// Class files from `jmods/*.jmod` (JDK 9+).
fn jmod_entries(home: &Path) -> Vec<(Url, Vec<SymbolEntry>, Vec<crate::types::TypeInfo>)> {
    let mut archives: Vec<PathBuf> = Vec::new();
    let jmods = home.join("jmods");
    if let Ok(entries) = std::fs::read_dir(&jmods) {
        archives.extend(
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "jmod")),
        );
    }
    archives.sort();
    archives
        .into_iter()
        .filter_map(|archive| class_archive_entries(&archive, true))
        .collect()
}

/// Class files from one archive; jmods keep classes under `classes/`.
fn class_archive_entries(
    archive: &Path,
    in_jmod: bool,
) -> Option<(Url, Vec<SymbolEntry>, Vec<crate::types::TypeInfo>)> {
    let raw = std::fs::read(archive).ok()?;
    // A jmod is a ZIP prefixed by a 4-byte `JM` magic (the version follows), so
    // every central-directory offset is relative to byte 4. Strip the magic so
    // the offsets index this buffer directly; a plain jar/zip starts with the
    // local-file-header signature and is used as-is.
    let data: &[u8] = if raw.starts_with(b"JM") && raw.len() >= 4 {
        &raw[4..]
    } else {
        &raw
    };
    let uri = Url::from_file_path(archive).ok()?;
    let shared = Arc::new(uri.clone());
    let mut entries = Vec::new();
    let mut types = Vec::new();
    let mut classes = 0usize;
    for_each_zip_entry(data, |name, class_data| {
        if !name.ends_with(".class")
            || name.ends_with("module-info.class")
            || name.ends_with("package-info.class")
        {
            return;
        }
        let internal = if in_jmod {
            match name.strip_prefix("classes/") {
                Some(rest) => rest,
                None => return,
            }
        } else {
            &name
        }
        .trim_end_matches(".class");
        let dotted = internal.replace('/', ".");
        if !(dotted.starts_with("java.") || dotted.starts_with("javax.")) {
            return; // jdk.*, com.sun.*, sun.* internals stay out
        }
        let Ok(info) = parse_class(&class_data) else {
            return;
        };
        entries.extend(crate::classfile::class_entries(&shared, &info));
        if let Some(type_info) = crate::classfile::class_type_info(&info) {
            types.push(type_info);
        }
        classes += 1;
    })?;
    if classes == 0 {
        return None;
    }
    tracing::debug!(archive = %archive.display(), classes, "indexed JDK archive");
    Some((uri, entries, types))
}

/// Java sources from `lib/src.zip` (JDK installs without jmods, e.g. some
/// SDKMAN distributions). Entries are `<module>/<pkg>/<Type>.java`; the
/// leading module directory is dropped.
fn src_zip_entries(src: &Path) -> Vec<(Url, Vec<SymbolEntry>, Vec<crate::types::TypeInfo>)> {
    let Ok(data) = std::fs::read(src) else {
        return Vec::new();
    };
    let Ok(uri) = Url::from_file_path(src) else {
        return Vec::new();
    };
    let mut parser = crate::index::java_parser();
    let mut entries = Vec::new();
    let mut types = Vec::new();
    let mut files = 0usize;
    for_each_zip_entry(&data, |name, source_data| {
        if !name.ends_with(".java") || name.ends_with("package-info.java") {
            return;
        }
        // Strip the module directory: `java.base/java/util/List.java`.
        let path = match name.split_once('/') {
            Some((_, rest)) => rest,
            None => return,
        };
        let dotted = path
            .trim_end_matches(".java")
            .rsplit_once('/')
            .map_or("", |(directory, _)| directory)
            .replace('/', ".");
        if !(dotted.starts_with("java.") || dotted.starts_with("javax.")) {
            return;
        }
        let Ok(text) = String::from_utf8(source_data) else {
            return;
        };
        let Some(tree) = parser.parse(text.as_bytes(), None) else {
            return;
        };
        let mut file_entries = crate::index::extract_entries(&uri, &tree, &text);
        // Import entries are never read by a feature; a JDK tree has many.
        crate::index::drop_import_entries(&mut file_entries);
        for entry in &mut file_entries {
            entry.dependency = true;
        }
        entries.extend(file_entries);
        // The same tree already parsed for the index also feeds the type model,
        // so a source-only JDK gets real signatures rather than bare names.
        types.extend(crate::types::collect_type_infos(
            Some(&dotted),
            &tree,
            &text,
        ));
        files += 1;
    });
    if files > 0 {
        tracing::debug!(archive = %src.display(), files, "indexed JDK sources");
    }
    vec![(uri, entries, types)]
}

// -- the JDK indexer driver -------------------------------------------------

/// Starts the JDK indexer on the hub: it runs once at start, independent of the
/// workspace, and emits its archives' entries and models, then the `Jdk`
/// stage-done. It listens to nothing, so it does not subscribe.
pub fn spawn(hub: &HubClient) {
    let client = hub.labeled("jdk");
    tokio::spawn(async move {
        let _ = tokio::task::spawn_blocking(move || {
            let count = {
                let mut sink = |message| {
                    let _ = client.notify(message);
                };
                index_jdk(&mut sink)
            };
            let _ = client.notify(DriverMessage::StageDone {
                stage: Stage::Jdk,
                count,
            });
        })
        .await;
    });
}

/// Indexes the standard library through the same path as dependency jars
/// (offered in completions, filtered from navigation). A missing JDK is a
/// no-op. Returns the number of classes indexed.
///
/// The whole JDK is served from [`crate::base_cache`] when its home and every
/// archive's identity are unchanged; a JDK upgrade (any archive changed) or a
/// missing archive re-parses it in full.
pub(crate) fn index_jdk(sink: &mut dyn FnMut(DriverMessage)) -> usize {
    let Some(home) = locate_jdk() else {
        sink(DriverMessage::Log {
            level: LogLevel::Info,
            message: "no usable JDK found; standard library not indexed".to_string(),
        });
        return 0;
    };
    let store = crate::base_cache::ArchiveStore::new("jdk");
    let key = jdk_cache_key(&home);
    let archives = match key
        .as_ref()
        .and_then(|key| store.get::<Vec<crate::base_cache::JdkArchive>>(key))
    {
        Some(archives) => archives,
        None => {
            let archives: Vec<crate::base_cache::JdkArchive> = jdk_entries(&home)
                .into_iter()
                .map(|(uri, entries, types)| crate::base_cache::JdkArchive {
                    uri,
                    entries,
                    types,
                })
                .collect();
            if let Some(key) = &key {
                store.insert(key, &archives);
            }
            archives
        }
    };
    let mut jdk_classes = 0usize;
    for archive in archives {
        jdk_classes += archive.entries.len();
        let mut types = crate::types::TypeModel::new();
        types.extend(archive.types);
        sink(DriverMessage::BaseArtifact {
            uri: archive.uri,
            entries: Arc::new(archive.entries),
            types: Arc::new(types),
        });
    }
    sink(DriverMessage::Progress(ProgressUpdate::Update {
        message: format!("Indexed {jdk_classes} JDK classes"),
        percentage: None,
    }));
    jdk_classes
}

/// The JDK's cache identity: its home plus every archive's identity, so a JDK
/// upgrade (any archive changed) invalidates the cached parse. `None` when an
/// archive cannot be identified, which forces a full parse.
fn jdk_cache_key(home: &Path) -> Option<String> {
    let archives = jdk_archive_paths(home);
    if archives.is_empty() {
        return None;
    }
    let mut parts = Vec::with_capacity(archives.len() + 1);
    parts.push(format!("jdk:{}", home.display()));
    for archive in &archives {
        parts.push(crate::base_cache::identity(archive)?);
    }
    Some(parts.join("|"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A second warm-up over an unchanged JDK re-parses no class files: the whole
    /// JDK is served from the base cache. Proven by emptying the cached archives
    /// under the live key — a cache hit then emits no classes at all.
    #[test]
    fn a_second_jdk_warmup_is_served_from_the_cache() {
        let _env = crate::jdk::env_lock();
        let Some(home) = locate_jdk() else {
            return; // no JDK here; nothing to cache
        };
        let cache_root = std::env::temp_dir().join(format!(
            "java-lsp-jdk-cache-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::env::set_var("JAVA_LSP_SOURCES_CACHE", &cache_root);

        // First warm-up: parses the JDK and writes the cache.
        let mut sink = |_message| {};
        let classes = index_jdk(&mut sink);
        if classes == 0 {
            // No readable standard-library archive here; nothing to prove.
            std::env::remove_var("JAVA_LSP_SOURCES_CACHE");
            let _ = std::fs::remove_dir_all(&cache_root);
            return;
        }

        // Empty the cached archives under the live key; a hit then emits none,
        // so a zero-class second run proves the cache was used, not re-parsed.
        let store = crate::base_cache::ArchiveStore::new("jdk");
        let key = jdk_cache_key(&home).expect("a JDK key");
        assert!(
            store
                .get::<Vec<crate::base_cache::JdkArchive>>(&key)
                .is_some(),
            "the JDK cache should be written"
        );
        store.insert(&key, &Vec::<crate::base_cache::JdkArchive>::new());

        let mut second = |_message| {};
        assert_eq!(
            index_jdk(&mut second),
            0,
            "the second warm-up must be served from the cache"
        );

        std::env::remove_var("JAVA_LSP_SOURCES_CACHE");
        let _ = std::fs::remove_dir_all(&cache_root);
    }

    /// Minimal STORED-entry zip writer (std has none); duplicated here —
    /// same pattern as in `classfile` and `harness` tests.
    fn test_stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        fn crc32(data: &[u8]) -> u32 {
            let mut table = [0u32; 256];
            for (i, slot) in table.iter_mut().enumerate() {
                let mut c = i as u32;
                for _ in 0..8 {
                    c = if c & 1 != 0 {
                        0xEDB88320 ^ (c >> 1)
                    } else {
                        c >> 1
                    };
                }
                *slot = c;
            }
            let mut crc = 0xFFFF_FFFFu32;
            for &byte in data {
                crc = table[((crc ^ byte as u32) & 0xFF) as usize] ^ (crc >> 8);
            }
            crc ^ 0xFFFF_FFFF
        }

        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let local_offset = out.len() as u32;
            let crc = crc32(data);
            out.extend_from_slice(&[0x50, 0x4b, 0x03, 0x04]);
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // stored
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);

            central.extend_from_slice(&[0x50, 0x4b, 0x01, 0x02]);
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&local_offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let central_offset = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06]);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&central_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// Minimal class-file builder: one Utf8+Class pair for the class and its
    /// superclass, one Utf8 per member; all members public.
    fn class_bytes(internal: &str, flags: u16, super_name: Option<&str>) -> Vec<u8> {
        let mut pool: Vec<(u8, Vec<u8>)> = Vec::new();
        let utf8 = |text: &str, pool: &mut Vec<(u8, Vec<u8>)>| -> u16 {
            pool.push((
                1,
                [
                    (text.len() as u16).to_be_bytes().to_vec(),
                    text.as_bytes().to_vec(),
                ]
                .concat(),
            ));
            pool.len() as u16
        };
        let name_slot = utf8(internal, &mut pool);
        pool.push((7, name_slot.to_be_bytes().to_vec()));
        let this_class = pool.len() as u16;
        let super_class = super_name.map_or(0, |name| {
            let slot = utf8(name, &mut pool);
            pool.push((7, slot.to_be_bytes().to_vec()));
            pool.len() as u16
        });

        let mut out = Vec::new();
        out.extend_from_slice(&0xCAFEBABEu32.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&(52u16).to_be_bytes());
        out.extend_from_slice(&((pool.len() + 1) as u16).to_be_bytes());
        for (tag, operands) in &pool {
            out.push(*tag);
            out.extend_from_slice(operands);
        }
        out.extend_from_slice(&flags.to_be_bytes());
        out.extend_from_slice(&this_class.to_be_bytes());
        out.extend_from_slice(&super_class.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes()); // fields
        out.extend_from_slice(&0u16.to_be_bytes()); // methods
        out.extend_from_slice(&0u16.to_be_bytes()); // attributes
        out
    }

    struct FakeJdk {
        home: PathBuf,
    }

    impl FakeJdk {
        fn new(name: &str) -> Self {
            let home = std::env::temp_dir().join(format!(
                "java-lsp-jdk-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&home).unwrap();
            Self { home }
        }

        fn write(&self, relative: &str, content: &[u8]) -> PathBuf {
            let path = self.home.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
            path
        }
    }

    impl Drop for FakeJdk {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }

    #[test]
    fn jdk_entries_read_jmods_and_filter_internals() {
        let jdk = FakeJdk::new("jmods");
        let string_class = class_bytes("java/lang/String", 0x0021, Some("java/lang/Object"));
        let list_class = class_bytes("java/util/List", 0x0601, Some("java/lang/Object"));
        let internal = class_bytes("jdk/internal/Hidden", 0x0021, Some("java/lang/Object"));
        let sun_class = class_bytes("com/sun/also/Hidden", 0x0021, Some("java/lang/Object"));
        // Real jmods are a ZIP prefixed by the 4-byte `JM` magic; include it so
        // the offset handling is exercised.
        let mut jmod = b"JM\x01\x00".to_vec();
        jmod.extend_from_slice(&test_stored_zip(&[
            ("classes/java/lang/String.class", &string_class),
            ("classes/java/util/List.class", &list_class),
            ("classes/jdk/internal/Hidden.class", &internal),
            ("classes/com/sun/also/Hidden.class", &sun_class),
            ("classes/module-info.class", b"not a class"),
            ("native/libjava.so", b"binary blob"),
            ("conf/security", b"config"),
        ]));
        jdk.write("jmods/java.base.jmod", &jmod);

        let entries = jdk_entries(&jdk.home);
        assert_eq!(entries.len(), 1, "one archive with classes");
        let all = &entries[0].1;
        let names: Vec<&str> = all.iter().map(|entry| entry.name.as_str()).collect();
        assert!(names.contains(&"String"), "{names:?}");
        assert!(names.contains(&"List"), "{names:?}");
        assert!(!names.iter().any(|name| name == &"Hidden"), "{names:?}");

        let string = all.iter().find(|entry| entry.name == "String").unwrap();
        assert_eq!(string.package.as_deref(), Some("java.lang"));
        assert!(string.dependency);

        // The archive's declared types are produced alongside the entries.
        let types = &entries[0].2;
        let string_type = types
            .iter()
            .find(|info| info.name == "String")
            .expect("String type");
        assert_eq!(string_type.package.as_deref(), Some("java.lang"));
    }

    #[test]
    fn src_zip_is_supported_without_jmods() {
        let jdk = FakeJdk::new("srczip");
        let list_source = b"package java.util;\n\npublic interface List {\n    int size();\n}\n";
        let internal_source = b"package jdk.internal;\n\npublic class Hidden {\n}\n";
        // Module directories as in real JDK 9+ src.zip archives.
        jdk.write(
            "lib/src.zip",
            &test_stored_zip(&[
                ("java.base/java/util/List.java", list_source),
                ("jdk.internal/jdk/internal/Hidden.java", internal_source),
            ]),
        );

        // No jmods: src.zip is the source of truth.
        assert!(is_jdk_home(&jdk.home));
        let entries = jdk_entries(&jdk.home);
        assert_eq!(entries.len(), 1);
        let all = &entries[0].1;
        let list = all
            .iter()
            .find(|entry| entry.name == "List")
            .expect("List from sources");
        assert_eq!(list.package.as_deref(), Some("java.util"));
        assert!(list.dependency);
        let size = all
            .iter()
            .find(|entry| entry.name == "size")
            .expect("member");
        assert_eq!(size.package.as_deref(), Some("java.util"));
        assert!(!all.iter().any(|entry| entry.name == "Hidden"));

        // The source archive feeds typed `TypeInfo`s through the same model.
        let types = &entries[0].2;
        let list_type = types
            .iter()
            .find(|info| info.name == "List")
            .expect("List type");
        let size_method = list_type
            .methods
            .iter()
            .find(|member| member.name == "size")
            .expect("typed size method");
        assert_eq!(size_method.ty.display(), "int");
    }

    #[test]
    fn rt_jar_is_the_fallback_for_old_layouts() {
        let jdk = FakeJdk::new("rtjar");
        let string_class = class_bytes("java/lang/String", 0x0021, Some("java/lang/Object"));
        jdk.write(
            "jre/lib/rt.jar",
            &test_stored_zip(&[("java/lang/String.class", &string_class)]),
        );

        let entries = jdk_entries(&jdk.home);
        assert_eq!(entries.len(), 1);
        assert!(entries[0]
            .1
            .iter()
            .any(|entry| entry.name == "String" && entry.package.as_deref() == Some("java.lang")));
    }

    #[test]
    fn discovery_prefers_the_override_then_java_home() {
        let _env = env_lock();
        let jdk = FakeJdk::new("discovery");
        jdk.write("jmods/placeholder", b"");

        std::env::set_var("JAVA_LSP_JDK", jdk.home.display().to_string());
        assert_eq!(locate_jdk().as_deref(), Some(jdk.home.as_path()));

        // No override: JAVA_HOME is honored.
        std::env::remove_var("JAVA_LSP_JDK");
        std::env::set_var("JAVA_HOME", jdk.home.display().to_string());
        assert_eq!(locate_jdk().as_deref(), Some(jdk.home.as_path()));

        // An unusable override disables JDK indexing deterministically —
        // even with a valid JAVA_HOME present.
        std::env::set_var("JAVA_LSP_JDK", "/definitely/not/a/jdk");
        assert_eq!(locate_jdk(), None);
        std::env::remove_var("JAVA_LSP_JDK");
        std::env::remove_var("JAVA_HOME");
    }

    #[test]
    fn a_home_without_archives_is_not_a_jdk() {
        let _env = env_lock();
        let not_jdk = FakeJdk::new("empty");
        std::env::set_var("JAVA_LSP_JDK", not_jdk.home.display().to_string());
        let found = locate_jdk();
        std::env::remove_var("JAVA_LSP_JDK");
        assert_eq!(found, None);
    }
}
