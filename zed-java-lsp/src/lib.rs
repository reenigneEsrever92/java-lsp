//! Zed extension that registers the java-lsp binary as a language server for
//! Java. The extension does not download anything: it resolves the binary the
//! user already built or installed.

use std::path::PathBuf;

use zed_extension_api::process::Command as ProcessCommand;
use zed_extension_api::settings::LspSettings;
use zed_extension_api::{self as zed, Command, LanguageServerId, Result, Worktree};

const SERVER_BINARY_NAME: &str = "java-lsp";
const LOCAL_BUILD_PATHS: [&str; 2] = ["target/release/java-lsp", "target/debug/java-lsp"];

/// `Path::is_file` cannot stat host paths from the WASM sandbox, so test
/// existence through a host shell instead.
fn host_file_exists(path: &str) -> bool {
    let output = ProcessCommand::new("/bin/sh")
        .arg("-c")
        .arg(format!("[ -f '{path}' ]"))
        .output()
        .ok();
    output
        .and_then(|o| o.status)
        .map(|code| code == 0)
        .unwrap_or(false)
}

struct JavaLspExtension;

impl zed::Extension for JavaLspExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Command> {
        let server_id = language_server_id.as_ref();

        // 1. Explicit user override:
        //    "lsp": { "java-lsp": { "binary": { "path": "...", "arguments": [...] } } }
        if let Some(binary) = LspSettings::for_worktree(server_id, worktree)?.binary {
            let path = binary
                .path
                .ok_or_else(|| format!("lsp.\"{server_id}\".binary.path is required"))?;
            let mut command = Command::new(path);
            if let Some(arguments) = binary.arguments {
                command = command.args(arguments);
            }
            if let Some(env) = binary.env {
                command = command.envs(env);
            }
            return Ok(command);
        }

        // 2. Installed on the worktree's PATH (e.g. `cargo install --path .`).
        if let Some(path) = worktree.which(SERVER_BINARY_NAME) {
            return Ok(Command::new(path));
        }

        // 3. A build inside the server's own repository (`cargo build` in the
        //    java-lsp checkout); prefer the release build over the debug one.
        let root = PathBuf::from(worktree.root_path());
        for relative in LOCAL_BUILD_PATHS {
            let candidate = root.join(relative);
            if host_file_exists(&candidate.to_string_lossy()) {
                return Ok(Command::new(candidate.to_string_lossy().into_owned()));
            }
        }

        Err(format!(
            "The {SERVER_BINARY_NAME} binary was not found. Either run `cargo build --release` \
             in the java-lsp repository (and open it as the worktree), install it with \
             `cargo install --path <java-lsp-repo>`, or point Zed at the binary with \
             `lsp.\"{server_id}\".binary.path` in your settings."
        ))
    }
}

zed::register_extension!(JavaLspExtension);
