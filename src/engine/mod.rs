//! The seam between the LSP shell and whatever analysis backend is plugged in.

pub mod stub;
pub mod syntax;

pub use stub::SyntaxOnlyEngine;
pub use syntax::TreeSitterEngine;

use tower_lsp::lsp_types::{
    CompletionResponse, Diagnostic, DocumentSymbol, FoldingRange, Hover, InlayHint, Location,
    Position, Range, SemanticTokens, SymbolInformation, Url, WorkspaceEdit,
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

    /// Inlay hints for the nodes intersecting `range` in `uri`: inferred and
    /// declared variable types, parameter names at call sites, and the return
    /// types of intermediate links in method chains. Empty when the document is
    /// not open or nothing resolves; computed on demand, never blocking (R6).
    fn inlay_hints(&self, _uri: &Url, _range: Range) -> Vec<InlayHint> {
        Vec::new()
    }

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

    /// Workspace-wide references to the symbol at `position`, the declaration
    /// included when `include_declaration` is set. An empty list means either
    /// none were found or the symbol could not be pinned to a single
    /// declaration — this query is advisory, so it never guesses.
    fn references(
        &self,
        _uri: &Url,
        _position: Position,
        _include_declaration: bool,
    ) -> Vec<Location> {
        Vec::new()
    }

    /// A workspace edit renaming the symbol at `position` to `new_name`, or
    /// `None` when the rename cannot be shown to be safe (an unresolved or
    /// ambiguous target, a library declaration, or an invalid identifier).
    fn rename(&self, _uri: &Url, _position: Position, _new_name: &str) -> Option<WorkspaceEdit> {
        None
    }
}
