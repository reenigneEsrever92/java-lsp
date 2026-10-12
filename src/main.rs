//! The stdio binary: parse the command line, start the engine hub, and serve LSP
//! over stdio with the shell as the hub's first client.

use clap::Parser;
use java_lsp::cli::Cli;
use java_lsp::hub::Hub;
use java_lsp::server::JavaLanguageServer;
use java_lsp::RUNTIME_STACK_SIZE;
use tower_lsp::{LspService, Server};

fn main() {
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("java_lsp=info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .finish();

    tracing::subscriber::set_global_default(subscriber)
        .expect("failed to install tracing subscriber");

    let cli = parse_command_line();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(RUNTIME_STACK_SIZE)
        .build()
        .expect("failed to start tokio runtime");

    runtime.block_on(async {
        let hub = setup(cli);
        let shell = hub.register("server");
        let incoming = shell.subscribe().await;

        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        let (service, socket) =
            LspService::build(move |_client| JavaLanguageServer::new(shell, incoming)).finish();
        Server::new(stdin, stdout, socket).serve(service).await;
    });
}

/// Parses the command line. `--help`/`--version` print and exit; any other parse
/// error is logged and the defaults are used, so an editor passing an unknown
/// flag cannot stop the server from starting.
fn parse_command_line() -> Cli {
    match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            use clap::error::ErrorKind;
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                let _ = error.print();
                std::process::exit(0);
            }
            Cli::default()
        }
    }
}

fn setup(_cli: Cli) -> Hub {
    let hub = Hub::new();

    java_lsp::project::spawn(&hub);
    java_lsp::index::spawn(&hub);
    java_lsp::navigate::spawn(&hub);

    hub
}
