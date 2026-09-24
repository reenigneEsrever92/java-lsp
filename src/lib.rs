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
pub mod jdk;
pub mod messages;
pub mod project;
pub mod quickfix;
pub mod resolve;
pub mod server;
pub mod sources;
pub mod types;
