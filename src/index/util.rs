//! Index-specific helpers shared by the index's paths and its consumers.

use crate::index::types::SymbolEntry;
use crate::model::{Param, SymbolKind};

/// A stable key for a member's parameter types, for overload deduplication.
pub(crate) fn params_key(params: &[Param]) -> String {
    params
        .iter()
        .map(|param| param.ty.display())
        .collect::<Vec<_>>()
        .join(",")
}

impl SymbolEntry {
    /// Whether this entry is a constructor: a method whose name is its own
    /// type's innermost container segment. A constructor is neither a nameable
    /// completion nor an addressable workspace symbol, so consumers drop it.
    pub fn is_constructor(&self) -> bool {
        self.kind == SymbolKind::Method
            && self.container.last().map(String::as_str) == Some(self.name.as_ref())
    }
}
