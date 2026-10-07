use clap::Parser;
use java_lsp::cli::{Cli, Modules};
use java_lsp::engine;
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

    let modules = parse_modules();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(RUNTIME_STACK_SIZE)
        .build()
        .expect("failed to start tokio runtime");

    runtime.block_on(async {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        let (service, socket) =
            LspService::build(move |client| engine::start_with(client, modules)).finish();
        Server::new(stdin, stdout, socket).serve(service).await;
    });
}

/// Parses the command line into the module toggles. `--help`/`--version` print
/// and exit; any other parse error is logged and the defaults are used, so an
/// editor passing an unknown flag cannot stop the server from starting.
fn parse_modules() -> Modules {
    match Cli::try_parse() {
        Ok(cli) => cli.into_modules(),
        Err(error) => {
            use clap::error::ErrorKind;
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                let _ = error.print();
                std::process::exit(0);
            }
            eprintln!("java-lsp: ignoring command line: {error}");
            Cli::default().into_modules()
        }
    }
}
