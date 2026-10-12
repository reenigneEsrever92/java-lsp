//! Library for the java-lsp language server; the binary in `main.rs` is a
//! thin stdio entry point around the LSP server.
//!
//! The engine modules are deprecated while the hub mechanics are reworked; they
//! are reintroduced one at a time, re-evaluating each module's interface and
//! overall design. Their sources are kept out of the build under
//! `src/deprecated/`, so reintroducing one means moving its file back under
//! `src/` before uncommenting its declaration below. Live for now: `hub` (the
//! mechanics), `messages` (its vocabulary), `cli` (the launch configuration),
//! `server` (the LSP shell skeleton, the hub's first client), `project` (the
//! workspace source-folder discovery), `index` (the workspace symbol index),
//! `navigate` (cursor navigation over the index), and `parser` (the shared,
//! stateless Java-reading helpers).

pub mod cli;
pub mod hub;
pub mod index;
pub mod messages;
pub mod navigate;
pub mod parser;
pub mod project;
pub mod server;

// Deprecated: sources under `src/deprecated/`. Move a file back under `src/`
// before uncommenting its declaration.
// pub mod analysis;
// pub mod base_cache;
// pub mod classfile;
// pub mod diagnostics;
// pub mod document;
// pub mod engine;
// pub mod jars;
// pub mod jdk;
// pub mod quickfix;
// pub mod resolve;
// pub mod sources;
// pub mod startup;
// pub mod types;

// pub use engine::start;

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
