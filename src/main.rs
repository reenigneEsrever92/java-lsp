use java_lsp::server::JavaLanguageServer;
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

    let runtime = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
    runtime.block_on(async {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        let (service, socket) = LspService::build(JavaLanguageServer::new).finish();
        Server::new(stdin, stdout, socket).serve(service).await;
    });
}
