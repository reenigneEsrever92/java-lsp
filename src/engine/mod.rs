//! The seam between the LSP shell and whatever analysis backend is plugged in.

pub mod stub;
pub mod syntax;

pub use stub::SyntaxOnlyEngine;
pub use syntax::TreeSitterEngine;

use tower_lsp::lsp_types::{
    CompletionResponse, Diagnostic, DocumentSymbol, FoldingRange, Hover, Location, Position,
    SemanticTokens, SymbolInformation, Url,
};

use crate::index::SymbolEntry;

pub trait SemanticEngine: Send + Sync {
    /// The document was opened with this full text.
    fn open(&self, uri: &Url, text: &str);
    /// The document changed; `text` is the full new text.
    fn change(&self, uri: &Url, text: &str);
    /// The document was closed; drop any per-document state.
    fn close(&self, uri: &Url);
    fn diagnostics(&self, uri: &Url) -> Vec<Diagnostic>;
    fn hover(&self, uri: &Url, position: Position) -> Option<Hover>;
    fn definition(&self, uri: &Url, position: Position) -> Option<Location>;
    /// Workspace-wide symbols whose name starts with `query` (an empty query
    /// returns everything indexed). Import entries are not workspace symbols.
    /// Like every index-backed query this may be partial during warm-up, but
    /// never blocks (R6).
    fn workspace_symbols(&self, _query: &str) -> Vec<SymbolInformation> {
        Vec::new()
    }
    fn completions(&self, uri: &Url, position: Position) -> Option<CompletionResponse>;
    fn document_symbols(&self, uri: &Url) -> Option<Vec<DocumentSymbol>>;
    fn folding_ranges(&self, uri: &Url) -> Option<Vec<FoldingRange>>;
    fn semantic_tokens(&self, uri: &Url) -> Option<SemanticTokens>;

    /// The workspace root is known; the initial background scan may start.
    /// Features served from the index must never block on it (R6).
    fn set_workspace_root(&self, _root: &Url) {}

    /// True once the initial workspace scan finished. Engines without an
    /// index have nothing to wait for, hence the default.
    fn index_ready(&self) -> bool {
        true
    }

    /// Flat snapshot of everything currently indexed. A verification hook for
    /// tests; the consumer-facing queries (completions, definition,
    /// `workspace_symbols`) read the index through their own lookups.
    fn indexed_symbols(&self) -> Vec<SymbolEntry> {
        Vec::new()
    }
}
