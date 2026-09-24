use java_lsp::server::JavaLanguageServer;
use tower_lsp::{LspService, Server};

/// Thread stack size for the runtime's worker and blocking-pool threads.
///
/// The analysis paths recurse deeply — the warm-up source scan, the open-document
/// parse/extract (`TreeSitterEngine::store_tree`), and the diagnostics sweep all
/// walk the AST and the type model recursively — and on a large workspace that
/// overflows the standard library's default 2 MiB stack, aborting the process
/// with `has overflowed its stack`. Sizing the runtime explicitly keeps the
/// server safe by default, without depending on the `RUST_MIN_STACK` environment
/// variable, which the editor extension does not set. The value matches the size
/// verified to fix the abort on a large Maven workspace; it can be lowered once
/// the deepest walk is identified and made iterative.
const RUNTIME_STACK_SIZE: usize = 256 * 1024 * 1024;

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

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(RUNTIME_STACK_SIZE)
        .build()
        .expect("failed to start tokio runtime");
    runtime.block_on(async {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        let (service, socket) = LspService::build(JavaLanguageServer::new).finish();
        Server::new(stdin, stdout, socket).serve(service).await;
    });
}
