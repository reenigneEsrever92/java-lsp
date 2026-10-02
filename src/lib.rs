//! Library for the java-lsp language server; the binary in `main.rs` is a
//! thin stdio entry point around [`server::JavaLanguageServer`].

pub mod analysis;
pub mod base_cache;
pub mod bus;
pub mod classfile;
pub mod diagnostics;
pub mod document;
pub mod engine;
pub mod index;
pub mod jars;
pub mod jdk;
pub mod messages;
pub mod project;
pub mod quickfix;
pub mod resolve;
pub mod scan;
pub mod server;
pub mod sources;
pub mod types;

/// Thread stack size for every thread that runs analysis: the runtime's worker
/// and blocking-pool threads (`main.rs`) and the analysis module's thread.
///
/// The analysis paths recurse deeply — the warm-up source scan, the open-document
/// parse/extract (`TreeSitterEngine::store_tree`), and the diagnostics sweep all
/// walk the AST and the type model recursively — and on a large workspace that
/// overflows the standard library's default 2 MiB stack, aborting the process
/// with `has overflowed its stack`. Sizing the threads explicitly keeps the
/// server safe by default, without depending on the `RUST_MIN_STACK` environment
/// variable, which the editor extension does not set. The value matches the size
/// verified to fix the abort on a large Maven workspace; it can be lowered once
/// the deepest walk is identified and made iterative.
pub const RUNTIME_STACK_SIZE: usize = 256 * 1024 * 1024;
