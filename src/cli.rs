//! The launch configuration: the command line.
//!
//! [`Cli`] is parsed from `argv` with `clap`. The per-module toggles
//! (`--no-dependencies`, `--no-downloads`) gate the maven module; the rest
//! (`--no-workspace-index`, `--no-jars`, …) return together with the modules
//! they switch off.

use clap::Parser;

/// The command-line flags. `--stdio` is accepted and ignored — the server always
/// speaks stdio.
#[derive(Parser, Debug, Default, Clone)]
#[command(name = "java-lsp", version, about = "A JVM-free Java language server")]
pub struct Cli {
    /// Accepted for editor compatibility; ignored (the server always speaks
    /// stdio).
    #[arg(long)]
    pub stdio: bool,
    /// Do not resolve or index Maven dependencies (implies `--no-downloads`).
    #[arg(long)]
    pub no_dependencies: bool,
    /// Do not fetch or index dependency sources; resolution stays enabled.
    #[arg(long)]
    pub no_downloads: bool,
}
