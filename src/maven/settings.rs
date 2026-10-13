//! Maven user settings and the local-repository / download-base resolution.
//!
//! Rework of `local_repository()` from `src/deprecated/resolve.rs` and
//! `offline()` / `base_url()` from `src/deprecated/sources.rs`, plus honoring
//! Maven's own `~/.m2/settings.xml`: the `localRepository` override, the first
//! mirror matching the download base, and the `offline` flag. Everything is
//! read through the environment and the settings file only — nothing here
//! touches the network, and a missing, empty, or malformed settings file yields
//! defaults rather than an error.
//!
//! The three public functions are the precedence chains the rest of the maven
//! module builds on: [`local_repository`] (`$MAVEN_REPO` > settings >
//! `~/.m2/repository`), [`download_base`] (`$JAVA_LSP_MAVEN_CENTRAL_URL` >
//! mirror > [`DEFAULT_CENTRAL`]), and [`offline`] (`$JAVA_LSP_OFFLINE` or the
//! settings flag). [`resolve`](crate::maven::resolve),
//! [`model`](crate::maven::model), and [`sources`](crate::maven::sources)
//! compile against them.

use std::path::{Path, PathBuf};

/// Maven Central, the default download base.
pub const DEFAULT_CENTRAL: &str = "https://repo1.maven.org/maven2";

/// The user Maven settings that matter to java-lsp: the `localRepository`
/// override, the first mirror matching the download base, and the `offline`
/// flag. `Settings::load` reads `$JAVA_LSP_MAVEN_SETTINGS` (else
/// `~/.m2/settings.xml`); a missing or malformed file yields defaults, never an
/// error.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub local_repository: Option<PathBuf>,
    pub mirror: Option<String>,
    pub offline: bool,
}

impl Settings {
    /// The user settings from `$JAVA_LSP_MAVEN_SETTINGS` (default
    /// `~/.m2/settings.xml`): `localRepository`, the first mirror whose
    /// `mirrorOf` matches `central`/`*`/`external:*`, and `offline`.
    pub fn load() -> Settings {
        let Ok(text) = std::fs::read_to_string(settings_path()) else {
            return Settings::default();
        };
        parse(&text)
    }
}

/// The local Maven repository: `$MAVEN_REPO` if set, else the settings
/// `localRepository`, else `~/.m2/repository` (a relative settings path
/// resolves against the home directory; `~` and `${user.home}` expand).
pub fn local_repository() -> PathBuf {
    if let Ok(override_path) = std::env::var("MAVEN_REPO") {
        if !override_path.is_empty() {
            return PathBuf::from(override_path);
        }
    }
    if let Some(repo) = Settings::load().local_repository {
        return repo;
    }
    home_dir()
        .map(|home| home.join(".m2").join("repository"))
        .unwrap_or_else(|| PathBuf::from(".m2/repository"))
}

/// The Maven repository base URL for downloads: `$JAVA_LSP_MAVEN_CENTRAL_URL`
/// when set, else the first matching settings mirror, else
/// [`DEFAULT_CENTRAL`]. A trailing slash is normalized away.
pub fn download_base(settings: &Settings) -> String {
    if let Ok(base) = std::env::var("JAVA_LSP_MAVEN_CENTRAL_URL") {
        if !base.is_empty() {
            return normalize_base(&base);
        }
    }
    match &settings.mirror {
        Some(mirror) if !mirror.is_empty() => normalize_base(mirror),
        _ => DEFAULT_CENTRAL.to_string(),
    }
}

/// True when network work is disabled: `$JAVA_LSP_OFFLINE` set to a non-empty
/// value, or the settings `offline` flag.
pub fn offline() -> bool {
    if std::env::var("JAVA_LSP_OFFLINE")
        .map(|value| !value.is_empty())
        .unwrap_or(false)
    {
        return true;
    }
    Settings::load().offline
}

/// The settings file: `$JAVA_LSP_MAVEN_SETTINGS` when set to a non-empty value,
/// else `~/.m2/settings.xml` (`.`-relative when the home directory is unknown).
fn settings_path() -> PathBuf {
    if let Ok(path) = std::env::var("JAVA_LSP_MAVEN_SETTINGS") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    home_dir()
        .map(|home| home.join(".m2").join("settings.xml"))
        .unwrap_or_else(|| PathBuf::from(".m2/settings.xml"))
}

/// The home directory, or `None` when `$HOME` is unset or empty.
fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// Parses a settings document; anything that is not a well-formed `<settings>`
/// tree yields the defaults.
fn parse(text: &str) -> Settings {
    let Ok(document) = roxmltree::Document::parse(text) else {
        return Settings::default();
    };
    let root = document.root_element();
    if root.tag_name().name() != "settings" {
        return Settings::default();
    }
    let home = home_dir();
    Settings {
        local_repository: child_text(root, "localRepository")
            .and_then(|value| expand_path(&value, home.as_deref())),
        mirror: first_matching_mirror(root),
        offline: child_text(root, "offline")
            .map(|value| value.eq_ignore_ascii_case("true"))
            .unwrap_or(false),
    }
}

/// The `url` of the first `<mirror>` whose `<mirrorOf>` is `central`, `*`, or
/// `external:*`; `None` when no mirror matches or the matching entry carries no
/// usable url.
fn first_matching_mirror(root: roxmltree::Node) -> Option<String> {
    let mirrors = child(root, "mirrors")?;
    mirrors
        .children()
        .filter(|node| node.is_element() && node.tag_name().name() == "mirror")
        .find_map(|mirror| {
            let mirror_of = child_text(mirror, "mirrorOf")?;
            if !matches_mirror_of(&mirror_of) {
                return None;
            }
            child_text(mirror, "url").filter(|url| !url.is_empty())
        })
}

/// True for the `mirrorOf` values that redirect the download base: `central`,
/// `*`, and `external:*` (trimmed).
fn matches_mirror_of(mirror_of: &str) -> bool {
    matches!(mirror_of.trim(), "central" | "*" | "external:*")
}

/// Expands a `localRepository` value as Maven does: a leading `~` and any
/// `${user.home}` become the home directory, and a relative path resolves
/// against it. Absolute paths pass through unchanged, so a missing home
/// directory only forfeits the paths that need it.
fn expand_path(value: &str, home: Option<&Path>) -> Option<PathBuf> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let expanded = if value.contains("${user.home}") {
        value.replace("${user.home}", &home?.to_string_lossy())
    } else {
        value.to_string()
    };
    let path = Path::new(&expanded);
    if let Ok(rest) = path.strip_prefix("~") {
        return Some(home?.join(rest));
    }
    if path.is_absolute() {
        return Some(path.to_path_buf());
    }
    Some(home?.join(path))
}

/// Drops a trailing `/` while keeping a bare-root value intact.
fn normalize_base(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    if trimmed.is_empty() {
        base.to_string()
    } else {
        trimmed.to_string()
    }
}

/// The first child element named `name`.
fn child<'a, 'input>(
    node: roxmltree::Node<'a, 'input>,
    name: &str,
) -> Option<roxmltree::Node<'a, 'input>> {
    node.children()
        .find(|child| child.is_element() && child.tag_name().name() == name)
}

/// The trimmed text of the first child element named `name`.
fn child_text(node: roxmltree::Node, name: &str) -> Option<String> {
    child(node, name)
        .and_then(|child| child.text())
        .map(|text| text.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    use crate::maven::tests::env_lock;

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

    /// A temp directory removed on drop.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "java-lsp-settings-{label}-{}-{}",
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

        /// Writes `contents` to a `settings.xml` inside the directory and
        /// returns its path.
        fn settings(&self, contents: &str) -> PathBuf {
            let path = self.path.join("settings.xml");
            std::fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn expand_path_handles_tilde_user_home_absolute_and_relative() {
        let home = Path::new("/home/felix");
        assert_eq!(
            expand_path("~/.m2/repository", Some(home)),
            Some(PathBuf::from("/home/felix/.m2/repository"))
        );
        assert_eq!(
            expand_path("~", Some(home)),
            Some(PathBuf::from("/home/felix"))
        );
        assert_eq!(
            expand_path("${user.home}/.m2", Some(home)),
            Some(PathBuf::from("/home/felix/.m2"))
        );
        assert_eq!(
            expand_path("/var/repo", Some(home)),
            Some(PathBuf::from("/var/repo"))
        );
        assert_eq!(
            expand_path("relative-repo", Some(home)),
            Some(PathBuf::from("/home/felix/relative-repo"))
        );
        assert_eq!(expand_path("", Some(home)), None);
        assert_eq!(expand_path("   ", Some(home)), None);
    }

    #[test]
    fn expand_path_without_home_only_yields_absolute_paths() {
        assert_eq!(expand_path("~/.m2", None), None);
        assert_eq!(expand_path("${user.home}/.m2", None), None);
        assert_eq!(expand_path("relative", None), None);
        assert_eq!(
            expand_path("/var/repo", None),
            Some(PathBuf::from("/var/repo"))
        );
    }

    #[test]
    fn mirror_of_matching_accepts_the_three_forms() {
        assert!(matches_mirror_of("central"));
        assert!(matches_mirror_of("*"));
        assert!(matches_mirror_of("external:*"));
        assert!(matches_mirror_of("  central  "));
        assert!(!matches_mirror_of("repo1"));
        assert!(!matches_mirror_of("external:central"));
        assert!(!matches_mirror_of(""));
    }

    #[test]
    fn load_reads_local_repository_mirror_and_offline() {
        let _lock = env_lock();
        let dir = TempDir::new("basic");
        let path = dir.settings(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<settings>
  <localRepository>/custom/repo</localRepository>
  <mirrors>
    <mirror>
      <id>corp</id>
      <url>https://mirror.example.com/maven2/</url>
      <mirrorOf>*</mirrorOf>
    </mirror>
  </mirrors>
  <offline>true</offline>
</settings>"#,
        );
        let _settings = EnvVar::set("JAVA_LSP_MAVEN_SETTINGS", &path);
        let _repo = EnvVar::remove("MAVEN_REPO");
        let _central = EnvVar::remove("JAVA_LSP_MAVEN_CENTRAL_URL");
        let _offline = EnvVar::remove("JAVA_LSP_OFFLINE");

        let settings = Settings::load();
        assert_eq!(
            settings.local_repository,
            Some(PathBuf::from("/custom/repo"))
        );
        assert_eq!(
            settings.mirror.as_deref(),
            Some("https://mirror.example.com/maven2/")
        );
        assert!(settings.offline);
        assert_eq!(
            download_base(&settings),
            "https://mirror.example.com/maven2"
        );
        assert!(offline());
    }

    #[test]
    fn load_takes_the_first_matching_mirror_only() {
        let _lock = env_lock();
        let dir = TempDir::new("mirror-first");
        // The first mirror does not match (`mirrorOf` is a specific repo); the
        // second matches `central` and must win over the third.
        let path = dir.settings(
            r#"<settings>
  <mirrors>
    <mirror>
      <url>https://ignored.example.com/maven2</url>
      <mirrorOf>some-repo</mirrorOf>
    </mirror>
    <mirror>
      <url>https://central-mirror.example.com/maven2</url>
      <mirrorOf>central</mirrorOf>
    </mirror>
    <mirror>
      <url>https://later.example.com/maven2</url>
      <mirrorOf>*</mirrorOf>
    </mirror>
  </mirrors>
</settings>"#,
        );
        let _settings = EnvVar::set("JAVA_LSP_MAVEN_SETTINGS", &path);
        let _central = EnvVar::remove("JAVA_LSP_MAVEN_CENTRAL_URL");

        let settings = Settings::load();
        assert_eq!(
            settings.mirror.as_deref(),
            Some("https://central-mirror.example.com/maven2")
        );
    }

    #[test]
    fn load_ignores_a_mirror_that_does_not_match() {
        let _lock = env_lock();
        let dir = TempDir::new("mirror-none");
        let path = dir.settings(
            r#"<settings>
  <mirrors>
    <mirror>
      <url>https://ignored.example.com/maven2</url>
      <mirrorOf>external:central</mirrorOf>
    </mirror>
  </mirrors>
</settings>"#,
        );
        let _settings = EnvVar::set("JAVA_LSP_MAVEN_SETTINGS", &path);
        let _central = EnvVar::remove("JAVA_LSP_MAVEN_CENTRAL_URL");

        let settings = Settings::load();
        assert_eq!(settings.mirror, None);
        assert_eq!(download_base(&settings), DEFAULT_CENTRAL);
    }

    #[test]
    fn local_repository_follows_env_then_settings_then_home() {
        let _lock = env_lock();
        let home = TempDir::new("repo-home");
        let dir = TempDir::new("repo-settings");
        let path =
            dir.settings("<settings><localRepository>~/from/settings</localRepository></settings>");
        let _home = EnvVar::set("HOME", home.path());
        let _settings = EnvVar::set("JAVA_LSP_MAVEN_SETTINGS", &path);
        let _repo = EnvVar::remove("MAVEN_REPO");

        // The settings `localRepository` (`~` expanded) wins over the fallback.
        assert_eq!(
            local_repository(),
            home.path().join("from").join("settings")
        );

        // `$MAVEN_REPO` wins over the settings.
        let _repo = EnvVar::set("MAVEN_REPO", "/from/env");
        assert_eq!(local_repository(), PathBuf::from("/from/env"));

        // With neither, the home repository is the fallback.
        let _repo = EnvVar::remove("MAVEN_REPO");
        let _settings = EnvVar::set("JAVA_LSP_MAVEN_SETTINGS", dir.path().join("missing.xml"));
        assert_eq!(
            local_repository(),
            home.path().join(".m2").join("repository")
        );
    }

    #[test]
    fn local_repository_expands_tilde_and_user_home_from_the_file() {
        let _lock = env_lock();
        let home = TempDir::new("expand-home");
        let dir = TempDir::new("expand-settings");
        let _home = EnvVar::set("HOME", home.path());
        let _repo = EnvVar::remove("MAVEN_REPO");

        for value in ["~/.m2/other", "${user.home}/.m2/other"] {
            let path = dir.settings(&format!(
                "<settings><localRepository>{value}</localRepository></settings>"
            ));
            let _settings = EnvVar::set("JAVA_LSP_MAVEN_SETTINGS", &path);
            assert_eq!(local_repository(), home.path().join(".m2").join("other"));
        }
    }

    #[test]
    fn local_repository_resolves_a_relative_path_against_home() {
        let _lock = env_lock();
        let home = TempDir::new("relative-home");
        let dir = TempDir::new("relative-settings");
        let path =
            dir.settings("<settings><localRepository>relative-repo</localRepository></settings>");
        let _home = EnvVar::set("HOME", home.path());
        let _settings = EnvVar::set("JAVA_LSP_MAVEN_SETTINGS", &path);
        let _repo = EnvVar::remove("MAVEN_REPO");

        assert_eq!(local_repository(), home.path().join("relative-repo"));
    }

    #[test]
    fn download_base_follows_env_then_mirror_then_default() {
        let _lock = env_lock();
        let dir = TempDir::new("base");
        let path = dir.settings(
            "<settings><mirrors><mirror><url>https://mirror.example.com/m2/</url><mirrorOf>*</mirrorOf></mirror></mirrors></settings>",
        );
        let _settings = EnvVar::set("JAVA_LSP_MAVEN_SETTINGS", &path);
        let _central = EnvVar::remove("JAVA_LSP_MAVEN_CENTRAL_URL");

        let settings = Settings::load();
        // The mirror, trailing slash normalized away.
        assert_eq!(download_base(&settings), "https://mirror.example.com/m2");

        // The env override beats the mirror, also normalized.
        let _central = EnvVar::set("JAVA_LSP_MAVEN_CENTRAL_URL", "https://env.example.com/m2/");
        assert_eq!(download_base(&settings), "https://env.example.com/m2");

        // An empty env value is ignored, so the mirror is used again.
        let _central = EnvVar::set("JAVA_LSP_MAVEN_CENTRAL_URL", "");
        assert_eq!(download_base(&settings), "https://mirror.example.com/m2");

        // No mirror at all: Maven Central.
        let _central = EnvVar::remove("JAVA_LSP_MAVEN_CENTRAL_URL");
        assert_eq!(download_base(&Settings::default()), DEFAULT_CENTRAL);
    }

    #[test]
    fn offline_is_env_or_settings_flag() {
        let _lock = env_lock();
        let dir = TempDir::new("offline");
        let _env = EnvVar::remove("JAVA_LSP_OFFLINE");

        let path = dir.settings("<settings><offline>false</offline></settings>");
        let _settings = EnvVar::set("JAVA_LSP_MAVEN_SETTINGS", &path);
        assert!(!offline());

        // `$JAVA_LSP_OFFLINE` (non-empty) forces offline.
        let _env = EnvVar::set("JAVA_LSP_OFFLINE", "1");
        assert!(offline());

        // An empty value is ignored...
        let _env = EnvVar::set("JAVA_LSP_OFFLINE", "");
        assert!(!offline());

        // ...and the settings flag (`true`) is honored.
        let _env = EnvVar::remove("JAVA_LSP_OFFLINE");
        let path = dir.settings("<settings><offline>true</offline></settings>");
        let _settings = EnvVar::set("JAVA_LSP_MAVEN_SETTINGS", &path);
        assert!(offline());
    }

    #[test]
    fn malformed_empty_or_non_settings_files_yield_defaults() {
        let _lock = env_lock();
        let dir = TempDir::new("malformed");
        let _central = EnvVar::remove("JAVA_LSP_MAVEN_CENTRAL_URL");
        let _rep_env = EnvVar::remove("JAVA_LSP_OFFLINE");

        for contents in [
            "",                                                         // Empty.
            "not xml at all",                                           // Not XML.
            "<settings><localRepository>/x",                            // Unclosed.
            "<project><localRepository>/x</localRepository></project>", // Wrong root.
        ] {
            let path = dir.settings(contents);
            let _settings = EnvVar::set("JAVA_LSP_MAVEN_SETTINGS", &path);
            let settings = Settings::load();
            assert_eq!(settings.local_repository, None, "for {contents:?}");
            assert_eq!(settings.mirror, None, "for {contents:?}");
            assert!(!settings.offline, "for {contents:?}");
            assert_eq!(download_base(&settings), DEFAULT_CENTRAL);
        }

        // A missing file is equally defaulted.
        let _settings = EnvVar::set("JAVA_LSP_MAVEN_SETTINGS", dir.path().join("absent.xml"));
        let settings = Settings::load();
        assert_eq!(settings.local_repository, None);
        assert_eq!(settings.mirror, None);
        assert!(!settings.offline);
    }
}
