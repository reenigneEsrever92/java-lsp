//! The launch configuration: the command line and the module toggles.
//!
//! [`Cli`] is parsed from `argv` with `clap`; [`Modules`] is the resolved set of
//! on/off decisions the engine consumes. The defaults come from the existing
//! environment variables ([`Modules::from_env`]), so behaviour is unchanged
//! without a command line, and any flag overrides them.

use clap::Parser;

/// The command-line flags: one `--no-<module>` per switchable module, all on by
/// default. `--stdio` is accepted and ignored — the server always speaks stdio.
#[derive(Parser, Debug, Default, Clone)]
#[command(name = "java-lsp", version, about = "A JVM-free Java language server")]
pub struct Cli {
    /// Do not index the workspace's own Java sources.
    #[arg(long)]
    pub no_workspace_index: bool,
    /// Do not resolve or index Maven dependencies (implies `--no-jars` and
    /// `--no-downloads`).
    #[arg(long)]
    pub no_dependencies: bool,
    /// Do not index dependency jars.
    #[arg(long)]
    pub no_jars: bool,
    /// Do not fetch or index dependency sources (as if `JAVA_LSP_OFFLINE` were
    /// set).
    #[arg(long)]
    pub no_downloads: bool,
    /// Do not index the JDK's standard library.
    #[arg(long)]
    pub no_jdk: bool,
    /// Do not run the diagnostics subsystem (implies `--no-quickfix`).
    #[arg(long)]
    pub no_diagnostics: bool,
    /// Keep syntax diagnostics but skip the unresolved-symbol pass (implies
    /// `--no-quickfix`).
    #[arg(long)]
    pub no_semantic_diagnostics: bool,
    /// Do not serve quick fixes.
    #[arg(long)]
    pub no_quickfix: bool,
    /// Accepted for editor compatibility; ignored (the server always speaks
    /// stdio).
    #[arg(long)]
    pub stdio: bool,
}

impl Cli {
    /// The module toggles this command line asks for, seeded from the
    /// environment ([`Modules::from_env`]), with the flags applied on top and
    /// the implications closed.
    pub fn into_modules(self) -> Modules {
        self.apply(Modules::from_env())
    }

    /// Applies the flags to `modules` and closes the implications. Pure: the
    /// environment is read only by [`Modules::from_env`].
    fn apply(self, mut modules: Modules) -> Modules {
        modules.workspace_index &= !self.no_workspace_index;
        modules.dependencies &= !self.no_dependencies;
        modules.jars &= !self.no_jars;
        modules.downloads &= !self.no_downloads;
        modules.jdk &= !self.no_jdk;
        modules.diagnostics &= !self.no_diagnostics;
        modules.semantic_diagnostics &= !self.no_semantic_diagnostics;
        modules.quickfix &= !self.no_quickfix;
        modules.closed()
    }
}

/// The resolved on/off decisions for the engine's modules, all on by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Modules {
    /// The workspace source scanner (`scan`).
    pub workspace_index: bool,
    /// The dependency resolver (`resolve`).
    pub dependencies: bool,
    /// The dependency-jar indexer (`jars`).
    pub jars: bool,
    /// The dependency-source downloader (`sources`).
    pub downloads: bool,
    /// The JDK indexer (`jdk`).
    pub jdk: bool,
    /// The diagnostics subsystem (`diagnostics`).
    pub diagnostics: bool,
    /// The unresolved-symbol pass inside diagnostics.
    pub semantic_diagnostics: bool,
    /// The quick-fix subsystem (`quickfix`).
    pub quickfix: bool,
}

impl Modules {
    /// Every module on.
    pub fn all() -> Self {
        Self {
            workspace_index: true,
            dependencies: true,
            jars: true,
            downloads: true,
            jdk: true,
            diagnostics: true,
            semantic_diagnostics: true,
            quickfix: true,
        }
    }

    /// The defaults from the environment: `JAVA_LSP_OFFLINE` turns downloads
    /// off and `JAVA_LSP_SEMANTIC_DIAGNOSTICS` (`0`/`false`) turns the semantic
    /// pass off; everything else stays on. This is what keeps behaviour
    /// unchanged for callers that never see a command line.
    pub fn from_env() -> Self {
        let mut modules = Self::all();
        if crate::sources::offline() {
            modules.downloads = false;
        }
        if !crate::diagnostics::semantic_diagnostics_setting(
            std::env::var("JAVA_LSP_SEMANTIC_DIAGNOSTICS")
                .ok()
                .as_deref(),
        ) {
            modules.semantic_diagnostics = false;
        }
        modules.closed()
    }

    /// Closes the implications: without dependency resolution there is nothing
    /// to index or download, and without diagnostics (or the semantic pass that
    /// feeds them) there is nothing for a quick fix to act on.
    fn closed(mut self) -> Self {
        if !self.dependencies {
            self.jars = false;
            self.downloads = false;
        }
        if !self.diagnostics || !self.semantic_diagnostics {
            self.quickfix = false;
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modules_for(args: &[&str]) -> Modules {
        Cli::try_parse_from(std::iter::once("java-lsp").chain(args.iter().copied()))
            .expect("the command line must parse")
            .apply(Modules::all())
    }

    #[test]
    fn no_flags_enable_everything() {
        assert_eq!(Cli::default().apply(Modules::all()), Modules::all());
    }

    #[test]
    fn each_flag_disables_its_module() {
        assert!(!modules_for(&["--no-workspace-index"]).workspace_index);
        assert!(!modules_for(&["--no-dependencies"]).dependencies);
        assert!(!modules_for(&["--no-jars"]).jars);
        assert!(!modules_for(&["--no-downloads"]).downloads);
        assert!(!modules_for(&["--no-jdk"]).jdk);
        assert!(!modules_for(&["--no-diagnostics"]).diagnostics);
        assert!(!modules_for(&["--no-semantic-diagnostics"]).semantic_diagnostics);
        assert!(!modules_for(&["--no-quickfix"]).quickfix);
    }

    #[test]
    fn no_dependencies_implies_jars_and_downloads() {
        let modules = modules_for(&["--no-dependencies"]);
        assert!(!modules.dependencies && !modules.jars && !modules.downloads);
        assert!(modules.workspace_index && modules.jdk);
    }

    #[test]
    fn no_diagnostics_implies_no_quickfix() {
        let modules = modules_for(&["--no-diagnostics"]);
        assert!(!modules.diagnostics && !modules.quickfix);
        assert!(modules.semantic_diagnostics);
    }

    #[test]
    fn no_semantic_diagnostics_implies_no_quickfix() {
        let modules = modules_for(&["--no-semantic-diagnostics"]);
        assert!(modules.diagnostics && !modules.semantic_diagnostics && !modules.quickfix);
    }

    #[test]
    fn offline_env_disables_downloads_only() {
        let _lock = crate::jdk::env_lock();
        let previous = std::env::var_os("JAVA_LSP_OFFLINE");
        std::env::set_var("JAVA_LSP_OFFLINE", "1");
        let modules = Modules::from_env();
        assert!(!modules.downloads);
        assert!(modules.dependencies && modules.jars);
        restore("JAVA_LSP_OFFLINE", previous);
    }

    #[test]
    fn semantic_env_disables_the_pass_and_quickfix() {
        let _lock = crate::jdk::env_lock();
        let previous = std::env::var_os("JAVA_LSP_SEMANTIC_DIAGNOSTICS");
        std::env::set_var("JAVA_LSP_SEMANTIC_DIAGNOSTICS", "0");
        let modules = Modules::from_env();
        assert!(modules.diagnostics && !modules.semantic_diagnostics && !modules.quickfix);
        restore("JAVA_LSP_SEMANTIC_DIAGNOSTICS", previous);
    }

    #[test]
    fn into_modules_seeds_from_the_environment_then_applies_flags() {
        let _lock = crate::jdk::env_lock();
        let offline = std::env::var_os("JAVA_LSP_OFFLINE");
        let semantic = std::env::var_os("JAVA_LSP_SEMANTIC_DIAGNOSTICS");
        std::env::set_var("JAVA_LSP_OFFLINE", "1");
        std::env::remove_var("JAVA_LSP_SEMANTIC_DIAGNOSTICS");

        let modules = Cli::try_parse_from(["java-lsp", "--no-jdk"])
            .unwrap()
            .into_modules();
        assert!(!modules.downloads, "JAVA_LSP_OFFLINE turns downloads off");
        assert!(!modules.jdk, "--no-jdk turns the JDK indexer off");
        assert!(modules.dependencies && modules.jars);

        restore("JAVA_LSP_OFFLINE", offline);
        restore("JAVA_LSP_SEMANTIC_DIAGNOSTICS", semantic);
    }

    #[test]
    fn parses_stdio_and_rejects_an_unknown_flag() {
        let cli = Cli::try_parse_from(["java-lsp", "--stdio", "--no-jdk"]).unwrap();
        assert!(cli.stdio && cli.no_jdk);
        assert!(Cli::try_parse_from(["java-lsp", "--nope"]).is_err());
    }

    fn restore(name: &str, previous: Option<std::ffi::OsString>) {
        match previous {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }
}
