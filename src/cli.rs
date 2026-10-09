//! The launch configuration: the command line.
//!
//! [`Cli`] is parsed from `argv` with `clap`. The per-module toggles
//! (`--no-workspace-index`, `--no-jars`, …) and the resolved `Modules` they fed
//! are removed while the engine modules are reintroduced one by one; they return
//! together with the modules they switch off.

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
}
