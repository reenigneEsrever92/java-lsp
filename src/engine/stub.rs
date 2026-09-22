//! Placeholder backend kept as the trait's minimal conformance reference;
//! the real shell uses [`super::TreeSitterEngine`].

use tower_lsp::lsp_types::{
    CompletionResponse, Diagnostic, DocumentSymbol, FoldingRange, Hover, InlayHint, Location,
    Position, Range, SemanticTokens, Url,
};

use super::SemanticEngine;

/// Every query responds empty so the LSP layer stays exercisable end to end.
#[derive(Debug, Default)]
pub struct SyntaxOnlyEngine;

impl SemanticEngine for SyntaxOnlyEngine {
    fn open(&self, _uri: &Url, _text: &str) {}
    fn change(&self, _uri: &Url, _text: &str) {}
    fn close(&self, _uri: &Url) {}

    fn diagnostics(&self, _uri: &Url) -> Vec<Diagnostic> {
        Vec::new()
    }

    fn hover(&self, _uri: &Url, _position: Position) -> Option<Hover> {
        None
    }

    fn definition(&self, _uri: &Url, _position: Position) -> Option<Location> {
        None
    }

    fn completions(&self, _uri: &Url, _position: Position) -> Option<CompletionResponse> {
        None
    }

    fn document_symbols(&self, _uri: &Url) -> Option<Vec<DocumentSymbol>> {
        None
    }

    fn folding_ranges(&self, _uri: &Url) -> Option<Vec<FoldingRange>> {
        None
    }

    fn semantic_tokens(&self, _uri: &Url) -> Option<SemanticTokens> {
        None
    }

    fn inlay_hints(&self, _uri: &Url, _range: Range) -> Vec<InlayHint> {
        Vec::new()
    }
}
