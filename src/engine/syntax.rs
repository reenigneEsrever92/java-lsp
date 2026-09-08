//! Tree-sitter backed engine: one Java syntax tree per open document, feeding
//! parse-error diagnostics, document symbols, folding ranges, and semantic
//! tokens, plus a workspace-wide symbol index ([`WorkspaceIndex`]) warmed by
//! a background scan and answering go-to-definition and workspace-symbol
//! queries.
//!
//! Trees are rebuilt from the full document text on every `open`/`change`.
//! The engine never reparses other documents, so an edit reparses exactly one
//! file and updates only that file's index entries; true `InputEdit`-based
//! incremental reparsing (needs edit ranges forwarded from the shell) is a
//! later optimization.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, CompletionResponse, Diagnostic, DiagnosticSeverity,
    DocumentSymbol, FoldingRange, FoldingRangeKind, Hover, Location, Position, Range,
    SemanticToken, SemanticTokenType, SemanticTokens, SymbolInformation, SymbolKind, TextEdit, Url,
};
use tree_sitter::{Node, Parser, Tree};

use super::SemanticEngine;
use crate::index::{
    extract_entries, java_parser, scan_workspace, IndexKind, SymbolEntry, WorkspaceIndex,
};

/// The legend for [`TreeSitterEngine::semantic_tokens`]; indexes into this
/// list are what goes on the wire.
pub const SEMANTIC_TOKEN_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::NAMESPACE,
    SemanticTokenType::TYPE,
    SemanticTokenType::CLASS,
    SemanticTokenType::INTERFACE,
    SemanticTokenType::ENUM,
    SemanticTokenType::ENUM_MEMBER,
    SemanticTokenType::PROPERTY,
    SemanticTokenType::VARIABLE,
    SemanticTokenType::PARAMETER,
    SemanticTokenType::METHOD,
    SemanticTokenType::STRING,
    SemanticTokenType::NUMBER,
    SemanticTokenType::COMMENT,
];

const TYPE: u32 = 1;
const CLASS: u32 = 2;
const INTERFACE: u32 = 3;
const ENUM: u32 = 4;
const ENUM_MEMBER: u32 = 5;
const PROPERTY: u32 = 6;
const VARIABLE: u32 = 7;
const PARAMETER: u32 = 8;
const METHOD: u32 = 9;
const STRING: u32 = 10;
const NUMBER: u32 = 11;
const COMMENT: u32 = 12;

struct ParsedDocument {
    tree: Tree,
    text: String,
}

pub struct TreeSitterEngine {
    parser: Mutex<Parser>,
    documents: Mutex<HashMap<Url, ParsedDocument>>,
    index: WorkspaceIndex,
    workspace_root: Mutex<Option<Url>>,
}

impl TreeSitterEngine {
    pub fn new() -> Self {
        Self {
            parser: Mutex::new(java_parser()),
            documents: Mutex::new(HashMap::new()),
            index: WorkspaceIndex::new(),
            workspace_root: Mutex::new(None),
        }
    }

    fn store_tree(&self, uri: &Url, text: &str) {
        if let Ok(mut parser) = self.parser.lock() {
            if let Some(tree) = parser.parse(text.as_bytes(), None) {
                let entries = extract_entries(uri, &tree, text);
                if let Ok(mut documents) = self.documents.lock() {
                    documents.insert(
                        uri.clone(),
                        ParsedDocument {
                            tree,
                            text: text.to_string(),
                        },
                    );
                }
                self.index.upsert_file(uri, entries);
            }
        }
    }

    /// After a document closes: if it lives in a workspace source root, disk
    /// truth wins — re-read and re-index it; report whether that happened.
    fn reindex_from_disk(&self, uri: &Url) -> bool {
        let Some(path) = uri.to_file_path().ok() else {
            return false;
        };
        let roots = self.index.source_roots();
        let inside = if roots.is_empty() {
            // No scan has run (or no root was set): fall back to the raw
            // workspace root check, matching pre-model behaviour.
            self.workspace_root
                .lock()
                .ok()
                .and_then(|guard| guard.clone())
                .and_then(|root| root.to_file_path().ok())
                .is_some_and(|root_path| path.starts_with(root_path))
        } else {
            roots.iter().any(|root| path.starts_with(root))
        };
        if !inside {
            return false;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            self.index.remove_file(uri);
            return true;
        };
        if let Ok(mut parser) = self.parser.lock() {
            if let Some(tree) = parser.parse(text.as_bytes(), None) {
                let entries = extract_entries(uri, &tree, &text);
                self.index.upsert_file(uri, entries);
                return true;
            }
        }
        self.index.remove_file(uri);
        true
    }
}

impl Default for TreeSitterEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl SemanticEngine for TreeSitterEngine {
    fn open(&self, uri: &Url, text: &str) {
        self.store_tree(uri, text);
    }

    fn change(&self, uri: &Url, text: &str) {
        self.store_tree(uri, text);
    }

    fn close(&self, uri: &Url) {
        if let Ok(mut documents) = self.documents.lock() {
            documents.remove(uri);
        }
        if !self.reindex_from_disk(uri) {
            self.index.remove_file(uri);
        }
    }

    fn diagnostics(&self, uri: &Url) -> Vec<Diagnostic> {
        let Some(documents) = self.documents.lock().ok() else {
            return Vec::new();
        };
        let Some(document) = documents.get(uri) else {
            return Vec::new();
        };
        if !document.tree.root_node().has_error() {
            return Vec::new();
        }
        let mut diagnostics = Vec::new();
        collect_errors(&document.tree.root_node(), &document.text, &mut diagnostics);
        diagnostics
    }

    fn hover(&self, _uri: &Url, _position: Position) -> Option<Hover> {
        None
    }

    fn definition(&self, uri: &Url, position: Position) -> Option<Location> {
        let documents = self.documents.lock().ok()?;
        let document = documents.get(uri)?;
        let text = &document.text;
        let offset = byte_offset(text, position);
        let word = word_at(text, offset);
        if word.is_empty() {
            return None;
        }
        let node = document
            .tree
            .root_node()
            .descendant_for_byte_range(offset, offset)?;

        // Import-qualified cursor: the dotted path's last segment names the
        // target. A `.*` wildcard (or empty segment) names nothing, and
        // JDK/library imports are simply not indexed, so both honestly
        // resolve to `None`.
        let mut ancestor = Some(node);
        while let Some(current) = ancestor {
            if current.kind() == "import_declaration" {
                let mut raw = text[current.byte_range()].trim();
                raw = raw.strip_prefix("import").map_or(raw, str::trim);
                let is_static = raw.strip_prefix("static").is_some();
                raw = raw.strip_prefix("static").map_or(raw, str::trim);
                let path = raw.trim_end_matches(';').trim();
                let simple = path.rsplit('.').next().unwrap_or("");
                if simple.is_empty() || simple == "*" {
                    return None;
                }
                let candidates: Vec<SymbolEntry> = self
                    .index
                    .query_name(simple)
                    .into_iter()
                    .filter(|entry| !entry.dependency)
                    .filter(|entry| {
                        if is_static {
                            entry.kind != IndexKind::Import
                        } else {
                            is_type_kind(entry.kind)
                        }
                    })
                    .collect();
                return unique_location(candidates);
            }
            ancestor = current.parent();
        }

        // Plain identifier: exact-name lookup, narrowed by what the node kind
        // at the cursor says about the name, never an `Import` entry.
        let candidates: Vec<SymbolEntry> = self
            .index
            .query_name(word)
            .into_iter()
            .filter(|entry| !entry.dependency)
            .filter(|entry| entry.kind != IndexKind::Import)
            .filter(|entry| match name_constraint(&node) {
                NameConstraint::Type => is_type_kind(entry.kind),
                NameConstraint::Member => {
                    matches!(entry.kind, IndexKind::Method | IndexKind::Field)
                }
                NameConstraint::Any => true,
            })
            .collect();
        unique_location(candidates)
    }

    fn workspace_symbols(&self, query: &str) -> Vec<SymbolInformation> {
        self.index
            .query_prefix(query)
            .into_iter()
            .filter(|entry| !entry.dependency)
            .filter_map(|entry| {
                let kind = symbol_kind(entry.kind)?;
                Some(SymbolInformation {
                    name: entry.name,
                    kind,
                    tags: None,
                    #[allow(deprecated)]
                    deprecated: None,
                    location: Location {
                        uri: entry.uri,
                        range: entry.selection_range,
                    },
                    container_name: entry.container.last().cloned(),
                })
            })
            .collect()
    }

    fn completions(&self, uri: &Url, position: Position) -> Option<CompletionResponse> {
        let documents = self.documents.lock().ok()?;
        let document = documents.get(uri)?;
        let offset = byte_offset(&document.text, position);
        let prefix = word_prefix(&document.text, offset);
        if prefix.is_empty() {
            // Nothing typed: no suggestions from nothing.
            return None;
        }
        if document.text[..offset - prefix.len()].ends_with('.') {
            // Member access needs a type-aware engine (deferred); an empty
            // list claims no membership the type-free engine cannot verify.
            return Some(CompletionResponse::Array(Vec::new()));
        }

        let mut items = Vec::new();
        let mut seen = HashSet::new();

        // 1. Language keywords.
        for keyword in JAVA_KEYWORDS {
            if keyword.starts_with(prefix) {
                offer(
                    &mut items,
                    &mut seen,
                    (*keyword).to_string(),
                    (*keyword).to_string(),
                    CompletionItemKind::KEYWORD,
                    None,
                    format!("0{keyword}"),
                    Vec::new(),
                );
            }
        }

        // 2. Locals, parameters, and enclosing fields from the open tree.
        if let Some(cursor) = document
            .tree
            .root_node()
            .descendant_for_byte_range(offset, offset)
        {
            let mut scoped = Vec::new();
            collect_scoped_names(&cursor, &document.text, &mut scoped);
            for name in scoped {
                if !name.name.starts_with(prefix) {
                    continue;
                }
                let sort_text = format!("1{}", name.name);
                offer(
                    &mut items,
                    &mut seen,
                    name.name.clone(),
                    name.name,
                    name.kind,
                    Some(name.detail),
                    sort_text,
                    Vec::new(),
                );
            }
        }

        // 3. Workspace and dependency-jar symbols; `query_prefix` takes only
        // a brief read lock, so while warm-up is still running this simply
        // contributes whatever is indexed so far — partial, never blocking.
        // Items whose symbol lives outside this file get an auto-import edit
        // attached (never-worsen: see `import_edit`).
        let (file_package, package_line) = file_header(&document.tree.root_node(), &document.text);
        let imports = collect_imports(&document.tree.root_node(), &document.text);
        for entry in self.index.query_prefix(prefix) {
            let Some(kind) = completion_kind(entry.kind) else {
                continue;
            };
            let name = entry.name.clone();
            let sort_text = format!("2{name}");
            let container = entry.container.join(".");
            let (label, detail) = if matches!(entry.kind, IndexKind::Method | IndexKind::Field) {
                if container.is_empty() {
                    (name.clone(), kind_word(entry.kind).to_string())
                } else {
                    (
                        format!("{container}.{name}"),
                        format!("{} of {container}", kind_word(entry.kind)),
                    )
                }
            } else {
                (name.clone(), kind_word(entry.kind).to_string())
            };
            let additional_edits = import_edit(
                uri,
                &entry,
                file_package.as_deref(),
                package_line,
                &imports,
                &self.index,
            );
            offer(
                &mut items,
                &mut seen,
                label,
                name,
                kind,
                Some(detail),
                sort_text,
                additional_edits,
            );
        }

        Some(CompletionResponse::Array(items))
    }

    fn document_symbols(&self, uri: &Url) -> Option<Vec<DocumentSymbol>> {
        let documents = self.documents.lock().ok()?;
        let document = documents.get(uri)?;
        let mut symbols = Vec::new();
        collect_symbols(&document.tree.root_node(), &document.text, &mut symbols);
        Some(symbols)
    }

    fn folding_ranges(&self, uri: &Url) -> Option<Vec<FoldingRange>> {
        let documents = self.documents.lock().ok()?;
        let document = documents.get(uri)?;
        let mut ranges = Vec::new();
        let mut seen = HashSet::new();
        collect_folds(
            &document.tree.root_node(),
            &document.text,
            &mut ranges,
            &mut seen,
        );
        Some(ranges)
    }

    fn semantic_tokens(&self, uri: &Url) -> Option<SemanticTokens> {
        let documents = self.documents.lock().ok()?;
        let document = documents.get(uri)?;
        let mut raw = Vec::new();
        collect_tokens(&document.tree.root_node(), &document.text, &mut raw);
        raw.sort_by_key(|token| token.start_byte);

        let mut data = Vec::with_capacity(raw.len());
        let mut prev_line = 0u32;
        let mut prev_character = 0u32;
        for token in raw {
            let position = lsp_position(&document.text, token.start_byte);
            let (delta_line, delta_start) = if position.line == prev_line {
                (0, position.character - prev_character)
            } else {
                (position.line - prev_line, position.character)
            };
            data.push(SemanticToken {
                delta_line,
                delta_start,
                length: token.length_utf16,
                token_type: token.token_type,
                token_modifiers_bitset: 0,
            });
            prev_line = position.line;
            prev_character = position.character;
        }
        Some(SemanticTokens {
            result_id: None,
            data,
        })
    }

    fn set_workspace_root(&self, root: &Url) {
        if let Ok(mut slot) = self.workspace_root.lock() {
            *slot = Some(root.clone());
        }
        let index = self.index.clone();
        let root = root.clone();
        // Off the request path: spawned onto the blocking pool when a runtime
        // is available (always true for the shell), inline otherwise.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn_blocking(move || scan_workspace(root, index));
            }
            Err(_) => scan_workspace(root, index),
        }
    }

    fn index_ready(&self) -> bool {
        self.index.ready()
    }

    fn indexed_symbols(&self) -> Vec<crate::index::SymbolEntry> {
        self.index.all_symbols()
    }
}

/// Converts a byte offset to an LSP position (line + UTF-16 code units).
fn lsp_position(text: &str, byte_offset: usize) -> Position {
    let mut offset = byte_offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let line = text.as_bytes()[..offset]
        .iter()
        .filter(|&&b| b == b'\n')
        .count() as u32;
    let line_start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
    let character = text[line_start..offset]
        .chars()
        .map(|c| c.len_utf16() as u32)
        .sum();
    Position { line, character }
}

pub(crate) fn lsp_range(text: &str, node: &Node) -> Range {
    Range {
        start: lsp_position(text, node.start_byte()),
        end: lsp_position(text, node.end_byte()),
    }
}

/// Collects `ERROR` and missing nodes from subtrees that contain errors.
fn collect_errors(node: &Node, text: &str, out: &mut Vec<Diagnostic>) {
    if !node.has_error() {
        return;
    }
    if node.is_error() {
        let snippet: String = text[node.byte_range()].chars().take(32).collect();
        out.push(Diagnostic {
            range: lsp_range(text, node),
            severity: Some(DiagnosticSeverity::ERROR),
            code: None,
            code_description: None,
            source: Some("java-lsp".to_string()),
            message: format!("Syntax error near `{snippet}`"),
            related_information: None,
            tags: None,
            data: None,
        });
    } else if node.is_missing() {
        out.push(Diagnostic {
            range: lsp_range(text, node),
            severity: Some(DiagnosticSeverity::ERROR),
            code: None,
            code_description: None,
            source: Some("java-lsp".to_string()),
            message: format!("Syntax error: missing `{}`", node.kind()),
            related_information: None,
            tags: None,
            data: None,
        });
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_errors(&child, text, out);
    }
}

/// Hierarchical document symbols from Java declaration nodes; any other node
/// is descended into looking for nested declarations.
fn collect_symbols(node: &Node, text: &str, out: &mut Vec<DocumentSymbol>) {
    let symbol = match node.kind() {
        "class_declaration" => Some(decl_symbol(node, text, SymbolKind::CLASS, "class")),
        "interface_declaration" => {
            Some(decl_symbol(node, text, SymbolKind::INTERFACE, "interface"))
        }
        "enum_declaration" => Some(decl_symbol(node, text, SymbolKind::ENUM, "enum")),
        "record_declaration" => Some(decl_symbol(node, text, SymbolKind::STRUCT, "record")),
        "method_declaration" => Some(decl_symbol(node, text, SymbolKind::METHOD, "method")),
        "constructor_declaration" => Some(decl_symbol(
            node,
            text,
            SymbolKind::CONSTRUCTOR,
            "constructor",
        )),
        "field_declaration" => {
            out.extend(field_symbols(node, text));
            None
        }
        _ => None,
    };

    if let Some(symbol) = symbol {
        out.push(symbol);
    } else {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.is_named() {
                collect_symbols(&child, text, out);
            }
        }
    }
}

fn decl_symbol(node: &Node, text: &str, kind: SymbolKind, detail: &str) -> DocumentSymbol {
    #[allow(deprecated)] // the field is still required in the struct initializer
    fn build(
        node: &Node,
        text: &str,
        kind: SymbolKind,
        detail: &str,
        children: Vec<DocumentSymbol>,
        name_node: Option<Node>,
    ) -> DocumentSymbol {
        DocumentSymbol {
            name: name_node
                .map(|n| text[n.byte_range()].to_string())
                .unwrap_or_else(|| "<anonymous>".to_string()),
            detail: Some(detail.to_string()),
            kind,
            range: lsp_range(text, node),
            selection_range: name_node
                .map(|n| lsp_range(text, &n))
                .unwrap_or_else(|| lsp_range(text, node)),
            children: (!children.is_empty()).then_some(children),
            tags: None,
            deprecated: None,
        }
    }

    let name_node = node.child_by_field_name("name");
    let mut children = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.is_named() {
            collect_symbols(&child, text, &mut children);
        }
    }
    build(node, text, kind, detail, children, name_node)
}

/// One symbol per declared variable in a field declaration.
fn field_symbols(node: &Node, text: &str) -> Vec<DocumentSymbol> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "variable_declarator" {
            continue;
        }
        let Some(name) = child.child_by_field_name("name") else {
            continue;
        };
        out.push(DocumentSymbol {
            name: text[name.byte_range()].to_string(),
            detail: Some("field".to_string()),
            kind: SymbolKind::FIELD,
            range: lsp_range(text, &child),
            selection_range: lsp_range(text, &name),
            children: None,
            tags: None,
            #[allow(deprecated)]
            deprecated: None,
        });
    }
    out
}

/// Fold regions for declarations and brace blocks that span multiple lines;
/// `seen` deduplicates declaration-vs-block overlaps.
fn collect_folds(
    node: &Node,
    text: &str,
    out: &mut Vec<FoldingRange>,
    seen: &mut HashSet<(u32, u32)>,
) {
    if matches!(
        node.kind(),
        "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "method_declaration"
            | "constructor_declaration"
            | "block"
            | "switch_block"
            | "array_initializer"
    ) {
        let start_row = node.start_position().row;
        let end_row = node.end_position().row;
        if end_row > start_row && seen.insert((start_row as u32, (end_row - 1) as u32)) {
            out.push(FoldingRange {
                start_line: start_row as u32,
                start_character: None,
                end_line: (end_row - 1) as u32,
                end_character: None,
                kind: Some(FoldingRangeKind::Region),
                collapsed_text: None,
            });
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.is_named() {
            collect_folds(&child, text, out, seen);
        }
    }
}

struct RawToken {
    start_byte: usize,
    length_utf16: u32,
    token_type: u32,
}

fn push_token(node: &Node, text: &str, token_type: u32, out: &mut Vec<RawToken>) {
    // LSP semantic tokens are single-line; skip tokens spanning lines.
    if node.start_position().row != node.end_position().row {
        return;
    }
    let length_utf16 = text[node.byte_range()]
        .chars()
        .map(|c| c.len_utf16() as u32)
        .sum();
    out.push(RawToken {
        start_byte: node.start_byte(),
        length_utf16,
        token_type,
    });
}

/// Curated node-kind → semantic-token-type mapping, walked in source order.
fn collect_tokens(node: &Node, text: &str, out: &mut Vec<RawToken>) {
    match node.kind() {
        "method_declaration" | "constructor_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                push_token(&name, text, METHOD, out);
            }
        }
        "class_declaration" | "record_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                push_token(&name, text, CLASS, out);
            }
        }
        "interface_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                push_token(&name, text, INTERFACE, out);
            }
        }
        "enum_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                push_token(&name, text, ENUM, out);
            }
        }
        "enum_constant" => {
            if let Some(name) = node.child_by_field_name("name") {
                push_token(&name, text, ENUM_MEMBER, out);
            }
        }
        "formal_parameter" => {
            if let Some(name) = node.child_by_field_name("name") {
                push_token(&name, text, PARAMETER, out);
            }
        }
        "variable_declarator" => {
            if let Some(name) = node.child_by_field_name("name") {
                let is_field = node
                    .parent()
                    .is_some_and(|parent| parent.kind() == "field_declaration");
                let token_type = if is_field { PROPERTY } else { VARIABLE };
                push_token(&name, text, token_type, out);
            }
        }
        "type_identifier" => push_token(node, text, TYPE, out),
        "string_literal" | "text_block" => push_token(node, text, STRING, out),
        "line_comment" | "block_comment" => push_token(node, text, COMMENT, out),
        "decimal_integer_literal"
        | "decimal_floating_point_literal"
        | "hex_integer_literal"
        | "octal_integer_literal"
        | "binary_integer_literal" => push_token(node, text, NUMBER, out),
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.is_named() {
            collect_tokens(&child, text, out);
        }
    }
}

// --- Completions ---

/// Java keywords plus the `true`/`false`/`null` literals — the highest-ranked
/// completion source.
const JAVA_KEYWORDS: &[&str] = &[
    "abstract",
    "assert",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extends",
    "false",
    "final",
    "finally",
    "float",
    "for",
    "goto",
    "if",
    "implements",
    "import",
    "instanceof",
    "int",
    "interface",
    "long",
    "native",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "short",
    "static",
    "strictfp",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "true",
    "try",
    "void",
    "volatile",
    "while",
];

/// Converts an LSP position (line + UTF-16 code units) to a byte offset — the
/// inverse of [`lsp_position`]. Out-of-range lines and columns clamp to the
/// nearest valid offset; a column inside a surrogate pair clamps to the
/// character containing it.
fn byte_offset(text: &str, position: Position) -> usize {
    let mut offset = 0usize;
    for _ in 0..position.line {
        match text[offset..].find('\n') {
            Some(nl) => offset += nl + 1,
            None => return text.len(),
        }
    }
    let line_end = text[offset..]
        .find('\n')
        .map_or(text.len(), |nl| offset + nl);
    let mut units = 0u32;
    for (i, ch) in text[offset..line_end].char_indices() {
        if units + ch.len_utf16() as u32 > position.character {
            return offset + i;
        }
        units += ch.len_utf16() as u32;
    }
    line_end
}

/// The run of identifier characters `[A-Za-z0-9_$]` ending at `offset`.
fn word_prefix(text: &str, offset: usize) -> &str {
    let offset = offset.min(text.len());
    let mut start = offset;
    for (i, ch) in text[..offset].char_indices().rev() {
        if !is_ident_char(ch) {
            break;
        }
        start = i;
    }
    &text[start..offset]
}

/// The full run of identifier characters `[A-Za-z0-9_$]` containing `offset`
/// — the whole word under the cursor, complementing the backward-only
/// [`word_prefix`].
fn word_at(text: &str, offset: usize) -> &str {
    let offset = offset.min(text.len());
    let start = offset - word_prefix(text, offset).len();
    let mut end = offset;
    for (i, ch) in text[offset..].char_indices() {
        if !is_ident_char(ch) {
            break;
        }
        end = offset + i + ch.len_utf8();
    }
    &text[start..end]
}

fn is_ident_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '$'
}

/// What the node kind at the cursor says about the name it references: a
/// `type_identifier` names a type, a method-invocation or field-access name
/// names a member, anything else is unconstrained.
#[derive(Debug, Clone, Copy)]
enum NameConstraint {
    Type,
    Member,
    Any,
}

fn name_constraint(node: &Node) -> NameConstraint {
    if node.kind() == "type_identifier" {
        return NameConstraint::Type;
    }
    if node.kind() == "identifier" {
        if let Some(parent) = node.parent() {
            let is_named_field = |field: &str| {
                parent
                    .child_by_field_name(field)
                    .is_some_and(|name| name.byte_range() == node.byte_range())
            };
            match parent.kind() {
                "method_invocation" if is_named_field("name") => return NameConstraint::Member,
                "field_access" if is_named_field("field") => return NameConstraint::Member,
                _ => {}
            }
        }
    }
    NameConstraint::Any
}

/// The declaration kinds a type name can resolve to.
fn is_type_kind(kind: IndexKind) -> bool {
    matches!(
        kind,
        IndexKind::Class | IndexKind::Interface | IndexKind::Enum | IndexKind::Record
    )
}

/// The ambiguity policy made concrete: a single candidate — or several that
/// share one URI (method overloads, same-file repeats), resolved to the first
/// by position — is an answer; anything spread across multiple files is not.
fn unique_location(mut candidates: Vec<SymbolEntry>) -> Option<Location> {
    candidates.sort_by(|a, b| {
        a.uri.as_str().cmp(b.uri.as_str()).then_with(|| {
            a.full_range
                .start
                .line
                .cmp(&b.full_range.start.line)
                .then_with(|| {
                    a.full_range
                        .start
                        .character
                        .cmp(&b.full_range.start.character)
                })
        })
    });
    let (first, rest) = candidates.split_first()?;
    if rest.iter().any(|entry| entry.uri != first.uri) {
        return None;
    }
    Some(Location {
        uri: first.uri.clone(),
        range: first.selection_range,
    })
}

/// A name offerable unqualified at the cursor, with its presentation.
#[derive(Debug)]
struct ScopedName {
    name: String,
    kind: CompletionItemKind,
    detail: String,
}

/// Locals and members nameable unqualified at the cursor: the innermost
/// enclosing method/constructor contributes its parameters and local
/// variables, the enclosing type declaration its fields (nameable
/// unqualified inside the type, so this claims no membership the code does
/// not already grant). Locals declared later in the method are included — a
/// known, harmless v1 simplification.
fn collect_scoped_names(node: &Node, text: &str, out: &mut Vec<ScopedName>) {
    const TYPE_DECLS: [&str; 4] = [
        "class_declaration",
        "interface_declaration",
        "enum_declaration",
        "record_declaration",
    ];

    let mut method_done = false;
    let mut type_done = false;
    let mut current = Some(*node);
    while let Some(ancestor) = current {
        match ancestor.kind() {
            "method_declaration" | "constructor_declaration" if !method_done => {
                method_done = true;
                collect_method_scope(&ancestor, text, out);
            }
            kind if TYPE_DECLS.contains(&kind) && !type_done => {
                type_done = true;
                let type_name = ancestor
                    .child_by_field_name("name")
                    .map(|name| text[name.byte_range()].to_string())
                    .unwrap_or_default();
                collect_type_fields(&ancestor, &type_name, text, out);
            }
            _ => {}
        }
        if method_done && type_done {
            break;
        }
        current = ancestor.parent();
    }
}

/// Parameters (including `varargs` spread parameters) and local variables of
/// a method or constructor body.
fn collect_method_scope(method: &Node, text: &str, out: &mut Vec<ScopedName>) {
    if let Some(parameters) = method.child_by_field_name("parameters") {
        let mut cursor = parameters.walk();
        for child in parameters.children(&mut cursor) {
            let name: Option<String> = match child.kind() {
                "formal_parameter" => child
                    .child_by_field_name("name")
                    .map(|name| text[name.byte_range()].to_string()),
                // A varargs parameter wraps its name in a variable_declarator.
                "spread_parameter" => {
                    let mut inner = child.walk();
                    let declarator = child
                        .children(&mut inner)
                        .find(|part| part.kind() == "variable_declarator")
                        .and_then(|declarator| declarator.child_by_field_name("name"))
                        .map(|name| text[name.byte_range()].to_string());
                    declarator
                }
                _ => None,
            };
            if let Some(name) = name {
                out.push(ScopedName {
                    name,
                    kind: CompletionItemKind::VARIABLE,
                    detail: "parameter".to_string(),
                });
            }
        }
    }
    collect_local_declarators(method, text, out);
}

/// `variable_declarator` names under a method body, skipping the parameter
/// list (handled above) and nested type declarations (their members are not
/// in scope here).
fn collect_local_declarators(node: &Node, text: &str, out: &mut Vec<ScopedName>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            continue;
        }
        match child.kind() {
            "formal_parameters"
            | "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration" => {}
            "variable_declarator" => {
                if let Some(name) = child.child_by_field_name("name") {
                    out.push(ScopedName {
                        name: text[name.byte_range()].to_string(),
                        kind: CompletionItemKind::VARIABLE,
                        detail: "local".to_string(),
                    });
                }
            }
            _ => collect_local_declarators(&child, text, out),
        }
    }
}

/// Field names declared inside a type's body (directly, or nested in an
/// enum's declarations section), skipping nested type declarations.
fn collect_type_fields(node: &Node, type_name: &str, text: &str, out: &mut Vec<ScopedName>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            continue;
        }
        match child.kind() {
            "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration" => {}
            "field_declaration" => {
                let mut inner = child.walk();
                for declarator in child.children(&mut inner) {
                    if declarator.kind() != "variable_declarator" {
                        continue;
                    }
                    let Some(name) = declarator.child_by_field_name("name") else {
                        continue;
                    };
                    out.push(ScopedName {
                        name: text[name.byte_range()].to_string(),
                        kind: CompletionItemKind::FIELD,
                        detail: format!("field of {type_name}"),
                    });
                }
            }
            _ => collect_type_fields(&child, type_name, text, out),
        }
    }
}

/// `IndexKind` → `CompletionItemKind`; `Import` entries are not nameable and
/// contribute nothing.
/// The dotted name of a file's `package_declaration` (with the line after it,
/// for inserting imports) or `None` for the default package.
fn file_header(root: &Node, text: &str) -> (Option<String>, Option<u32>) {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "package_declaration" {
            let mut inner = child.walk();
            let package = child
                .children(&mut inner)
                .find(|part| part.kind() == "scoped_identifier" || part.kind() == "identifier")
                .map(|part| text[part.byte_range()].to_string());
            return (package, Some((child.end_position().row + 1) as u32));
        }
    }
    (None, None)
}

/// One import statement in the open document.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExistingImport {
    path: String,
    is_static: bool,
    is_wildcard: bool,
    /// Where a new import line goes: the line right after this one.
    line: u32,
}

fn collect_imports(root: &Node, text: &str) -> Vec<ExistingImport> {
    let mut out = Vec::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() != "import_declaration" {
            continue;
        }
        let mut raw = text[child.byte_range()].trim();
        raw = raw.strip_prefix("import").map_or(raw, str::trim);
        let is_static = raw.starts_with("static");
        raw = raw.strip_prefix("static").map_or(raw, str::trim);
        let path = raw.trim_end_matches(';').trim().to_string();
        let (path, is_wildcard) = match path.strip_suffix(".*") {
            Some(head) => (head.to_string(), true),
            None => (path, false),
        };
        out.push(ExistingImport {
            path,
            is_static,
            is_wildcard,
            line: (child.end_position().row + 1) as u32,
        });
    }
    out
}

/// The fully qualified name of the TYPE that must be imported for `entry`:
/// for a type entry its own FQCN, for a member entry its enclosing type's.
/// `None` when there is nothing importable (default package, orphan member).
fn import_target(entry: &SymbolEntry) -> Option<String> {
    let package = entry.package.as_deref()?;
    let mut segments = entry.container.clone();
    if !matches!(entry.kind, IndexKind::Method | IndexKind::Field) {
        segments.push(entry.name.clone());
    }
    if segments.is_empty() {
        return None;
    }
    Some(format!("{package}.{}", segments.join(".")))
}

/// The `import ...;` edit for an index-sourced completion item, or none when
/// the never-worsen rules say the file already resolves the name (or must
/// not be touched: conflicts). Returns an empty vec when no edit is needed.
fn import_edit(
    document_uri: &Url,
    entry: &SymbolEntry,
    document_package: Option<&str>,
    package_line: Option<u32>,
    imports: &[ExistingImport],
    index: &WorkspaceIndex,
) -> Vec<TextEdit> {
    let Some(target) = import_target(entry) else {
        return Vec::new();
    };
    if &entry.uri == document_uri {
        return Vec::new(); // same file: the name already resolves
    }
    if entry.package.as_deref() == document_package {
        return Vec::new(); // same package (covers the default package too)
    }
    if entry.package.as_deref() == Some("java.lang") {
        return Vec::new(); // implicitly imported in Java
    }
    let simple = target.rsplit('.').next().unwrap_or("");
    for import in imports {
        if import.is_wildcard && import.path == entry.package.as_deref().unwrap_or("") {
            return Vec::new(); // already covered by `import pkg.*;`
        }
        if import.is_static || import.is_wildcard {
            continue;
        }
        if import.path == target {
            return Vec::new(); // already imported
        }
        if import.path.rsplit('.').next() == Some(simple) {
            return Vec::new(); // conflict: simple name claimed elsewhere
        }
    }
    // Same-package declarations with this simple name also conflict (the
    // name would already resolve in the file — to something else).
    let same_package_clash = index.query_name(simple).iter().any(|other| {
        !other.dependency && other.package.as_deref() == document_package && other.uri != entry.uri
    });
    if same_package_clash {
        return Vec::new();
    }

    // New imports go after the last import; else after the package
    // statement; else at the very top.
    let line = imports
        .last()
        .map(|import| import.line)
        .or(package_line)
        .unwrap_or(0);
    let text = format!("import {target};\n");
    vec![TextEdit {
        range: Range::new(Position::new(line, 0), Position::new(line, 0)),
        new_text: text,
    }]
}

fn completion_kind(kind: IndexKind) -> Option<CompletionItemKind> {
    match kind {
        IndexKind::Class => Some(CompletionItemKind::CLASS),
        IndexKind::Interface => Some(CompletionItemKind::INTERFACE),
        IndexKind::Enum => Some(CompletionItemKind::ENUM),
        IndexKind::Record => Some(CompletionItemKind::STRUCT),
        IndexKind::Method => Some(CompletionItemKind::METHOD),
        IndexKind::Field => Some(CompletionItemKind::FIELD),
        IndexKind::Import => None,
    }
}

/// `IndexKind` → `SymbolKind` for workspace symbols; `Import` entries are not
/// workspace symbols and contribute nothing (sibling of `completion_kind`).
fn symbol_kind(kind: IndexKind) -> Option<SymbolKind> {
    match kind {
        IndexKind::Class => Some(SymbolKind::CLASS),
        IndexKind::Interface => Some(SymbolKind::INTERFACE),
        IndexKind::Enum => Some(SymbolKind::ENUM),
        IndexKind::Record => Some(SymbolKind::STRUCT),
        IndexKind::Method => Some(SymbolKind::METHOD),
        IndexKind::Field => Some(SymbolKind::FIELD),
        IndexKind::Import => None,
    }
}

fn kind_word(kind: IndexKind) -> &'static str {
    match kind {
        IndexKind::Class => "class",
        IndexKind::Interface => "interface",
        IndexKind::Enum => "enum",
        IndexKind::Record => "record",
        IndexKind::Method => "method",
        IndexKind::Field => "field",
        IndexKind::Import => "import",
    }
}

/// Adds one completion unless its insert text was already offered by a
/// higher-ranked source, so the list never shows the same insert text twice.
#[allow(clippy::too_many_arguments)]
fn offer(
    items: &mut Vec<CompletionItem>,
    seen: &mut HashSet<String>,
    label: String,
    insert_text: String,
    kind: CompletionItemKind,
    detail: Option<String>,
    sort_text: String,
    additional_edits: Vec<TextEdit>,
) {
    if !seen.insert(insert_text.clone()) {
        return;
    }
    items.push(CompletionItem {
        label,
        kind: Some(kind),
        detail,
        filter_text: Some(insert_text.clone()),
        insert_text: Some(insert_text),
        sort_text: Some(sort_text),
        additional_text_edits: (!additional_edits.is_empty()).then_some(additional_edits),
        ..CompletionItem::default()
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
package com.example;

import java.util.List;

public class Sample {
    private final int count = 1;
    private String greeting = \"hi\";

    public Sample(String name) {
        this.greeting = name;
    }

    public int getCount() {
        int doubled = count * 2;
        return doubled;
    }
}
";

    fn uri() -> Url {
        Url::parse("file:///Sample.java").unwrap()
    }

    fn engine_with(text: &str) -> TreeSitterEngine {
        let engine = TreeSitterEngine::new();
        engine.open(&uri(), text);
        engine
    }

    /// Converts an LSP position to a byte offset, for decoding semantic
    /// tokens in tests.
    fn offset_at(text: &str, line: u32, character: u32) -> usize {
        // 1. Start of the requested line.
        let mut offset = 0usize;
        for _ in 0..line {
            match text[offset..].find('\n') {
                Some(nl) => offset += nl + 1,
                None => return text.len(),
            }
        }
        if character == 0 {
            return offset;
        }
        // 2. Walk the line counting UTF-16 units.
        let line_end = text[offset..]
            .find('\n')
            .map_or(text.len(), |nl| offset + nl);
        let mut units = 0u32;
        for (i, ch) in text[offset..line_end].char_indices() {
            if units >= character {
                return offset + i;
            }
            units += ch.len_utf16() as u32;
        }
        line_end
    }

    fn decoded_tokens(engine: &TreeSitterEngine, uri: &Url) -> Vec<(&'static str, String)> {
        let tokens = engine.semantic_tokens(uri).unwrap();
        let text = SAMPLE;
        let mut line = 0u32;
        let mut character = 0u32;
        tokens
            .data
            .iter()
            .map(|token| {
                if token.delta_line > 0 {
                    line += token.delta_line;
                    character = token.delta_start;
                } else {
                    character += token.delta_start;
                }
                let start = offset_at(text, line, character);
                let end = offset_at(text, line, character + token.length);
                let token_text = text[start..end].to_string();
                (
                    SEMANTIC_TOKEN_TYPES[token.token_type as usize].as_str(),
                    token_text,
                )
            })
            .collect()
    }

    #[test]
    fn valid_file_has_no_diagnostics() {
        let engine = engine_with(SAMPLE);
        assert!(engine.diagnostics(&uri()).is_empty());
    }

    #[test]
    fn missing_semicolon_is_reported_and_clears_when_fixed() {
        let engine = engine_with("class Sample {\n    int x\n}\n");
        let diagnostics = engine.diagnostics(&uri());
        assert!(
            diagnostics.iter().any(|d| d.message.contains("missing")),
            "expected a missing-token diagnostic, got {diagnostics:?}"
        );

        engine.change(&uri(), "class Sample {\n    int x;\n}\n");
        assert!(engine.diagnostics(&uri()).is_empty());
    }

    #[test]
    fn broken_construct_is_reported_as_error_node() {
        let engine = engine_with("class Sample {\n    int = ;\n}\n");
        let diagnostics = engine.diagnostics(&uri());
        assert!(
            diagnostics.iter().any(|d| d.message.contains("near")),
            "expected an ERROR-node diagnostic, got {diagnostics:?}"
        );
    }

    #[test]
    fn document_symbols_mirror_the_source_hierarchy() {
        let engine = engine_with(SAMPLE);
        let symbols = engine.document_symbols(&uri()).unwrap();

        assert_eq!(symbols.len(), 1, "one top-level class");
        let class = &symbols[0];
        assert_eq!(class.name, "Sample");
        assert_eq!(class.kind, SymbolKind::CLASS);

        let children = class.children.as_ref().unwrap();
        let names_kinds: Vec<(String, SymbolKind)> =
            children.iter().map(|s| (s.name.clone(), s.kind)).collect();
        assert!(names_kinds.contains(&("count".to_string(), SymbolKind::FIELD)));
        assert!(names_kinds.contains(&("greeting".to_string(), SymbolKind::FIELD)));
        assert!(names_kinds.contains(&("Sample".to_string(), SymbolKind::CONSTRUCTOR)));
        assert!(names_kinds.contains(&("getCount".to_string(), SymbolKind::METHOD)));

        // The method's local variable must not leak into class-level symbols.
        let method = children
            .iter()
            .find(|s| s.name == "getCount")
            .and_then(|s| s.children.as_ref());
        assert!(method.is_none() || !method.unwrap().iter().any(|s| s.name == "doubled"));
    }

    #[test]
    fn folding_ranges_cover_class_and_method_bodies() {
        let engine = engine_with(SAMPLE);
        let ranges = engine.folding_ranges(&uri()).unwrap();

        // Class body, constructor body, method body: at least three regions.
        assert!(
            ranges.len() >= 3,
            "expected >= 3 fold ranges, got {ranges:?}"
        );
        let lines: Vec<(u32, u32)> = ranges.iter().map(|r| (r.start_line, r.end_line)).collect();
        // Sample: class starts line 4 (0-based); its closing brace is the
        // final line.
        assert!(lines.contains(&(4, 15)), "class body fold, got {lines:?}");
    }

    #[test]
    fn closing_a_document_drops_its_tree() {
        let engine = engine_with(SAMPLE);
        engine.close(&uri());
        assert!(engine.document_symbols(&uri()).is_none());
        assert!(engine.folding_ranges(&uri()).is_none());
        assert!(engine.semantic_tokens(&uri()).is_none());
    }

    #[test]
    fn semantic_tokens_contain_expected_kinds_and_texts() {
        let engine = engine_with(SAMPLE);
        let tokens = decoded_tokens(&engine, &uri());

        assert!(
            tokens.contains(&("class", "Sample".to_string())),
            "{tokens:?}"
        );
        assert!(
            tokens.contains(&("method", "getCount".to_string())),
            "{tokens:?}"
        );
        assert!(
            tokens.contains(&("property", "greeting".to_string())),
            "{tokens:?}"
        );
        assert!(
            tokens.contains(&("property", "count".to_string())),
            "{tokens:?}"
        );
        assert!(
            tokens.contains(&("parameter", "name".to_string())),
            "{tokens:?}"
        );
        assert!(
            tokens.contains(&("variable", "doubled".to_string())),
            "{tokens:?}"
        );
        assert!(
            tokens.contains(&("type", "String".to_string())),
            "{tokens:?}"
        );
        assert!(
            tokens.contains(&("string", "\"hi\"".to_string())),
            "{tokens:?}"
        );
        assert!(tokens.contains(&("number", "1".to_string())), "{tokens:?}");
    }

    const SCOPE_SAMPLE: &str = "\
public class Outer {
    private int field = 1;
    private int shadowed = 2;

    public void method(int parameter) {
        int local = parameter + field;
        class Inner {
            int deep = 3;
        }
        int later = shadowed;
        local = later;
    }
}
";

    #[test]
    fn byte_offset_maps_utf16_positions_to_bytes_and_clamps() {
        let text = "ab€\n😀x\n";
        // € is one UTF-16 unit (three bytes); 😀 is two units (four bytes).
        assert_eq!(
            byte_offset(
                text,
                Position {
                    line: 0,
                    character: 2
                }
            ),
            2
        );
        assert_eq!(
            byte_offset(
                text,
                Position {
                    line: 0,
                    character: 3
                }
            ),
            5,
            "end of line 0"
        );
        assert_eq!(
            byte_offset(
                text,
                Position {
                    line: 1,
                    character: 0
                }
            ),
            6
        );
        assert_eq!(
            byte_offset(
                text,
                Position {
                    line: 1,
                    character: 1
                }
            ),
            6,
            "inside the surrogate pair clamps to the emoji's start"
        );
        assert_eq!(
            byte_offset(
                text,
                Position {
                    line: 1,
                    character: 2
                }
            ),
            10,
            "after the emoji"
        );
        assert_eq!(
            byte_offset(
                text,
                Position {
                    line: 1,
                    character: 9
                }
            ),
            11,
            "past the line end clamps to it"
        );
        assert_eq!(
            byte_offset(
                text,
                Position {
                    line: 9,
                    character: 0
                }
            ),
            text.len(),
            "past the last line clamps to the end"
        );
    }

    #[test]
    fn word_prefix_takes_the_identifier_run_before_the_cursor() {
        let text = "int total = count + 1";
        let count_start = text.find("count").unwrap();
        assert_eq!(
            word_prefix(text, count_start),
            "",
            "nothing typed before the `c`"
        );
        assert_eq!(word_prefix(text, count_start + 5), "count");
        assert_eq!(
            word_prefix(text, text.len()),
            "1",
            "digits are identifier characters"
        );
        let dollars = "int x$2 = 0";
        assert_eq!(word_prefix(dollars, dollars.find(" = ").unwrap()), "x$2");
        assert_eq!(word_prefix(text, 0), "");
    }

    #[test]
    fn word_at_takes_the_full_identifier_run_under_the_cursor() {
        let text = "int total$2 = count+1";
        let count_start = text.find("count").unwrap();
        assert_eq!(
            word_at(text, count_start + 1),
            "count",
            "expands in both directions"
        );
        assert_eq!(word_at(text, count_start), "count");
        assert_eq!(
            word_at(text, count_start + "count".len()),
            "count",
            "an offset just past the word still catches it"
        );
        assert_eq!(
            word_at(text, text.find("total$2").unwrap() + 5),
            "total$2",
            "digits and $ are identifier characters"
        );
        assert_eq!(
            word_at(text, text.find('=').unwrap()),
            "",
            "no word at the operator"
        );
        assert_eq!(word_at(text, 0), "int");
    }

    const WIDGET: &str = "\
package demo;

public class Widget {
    int size;
    int getSize() { return size; }
}
";

    /// The selection range of the first occurrence of `needle` in `text`.
    fn range_of(text: &str, needle: &str) -> Range {
        let start = text.find(needle).expect("needle must be present");
        Range {
            start: lsp_position(text, start),
            end: lsp_position(text, start + needle.len()),
        }
    }

    /// Definition at `into` code units into the first occurrence of
    /// `needle` — the caller places the cursor mid-word on purpose.
    fn location_at(
        engine: &TreeSitterEngine,
        text: &str,
        needle: &str,
        into: usize,
    ) -> Option<Location> {
        let offset = text.find(needle).expect("needle must be present");
        engine.definition(&uri(), lsp_position(text, offset + into))
    }

    #[test]
    fn definition_resolves_same_file_declarations() {
        let text = "\
class Widget {
    int size;
    int getSize() { return size; }
    void use() {
        Widget w = null;
        w.getSize();
    }
}
";
        let engine = engine_with(text);

        // A type usage (cursor mid-word) resolves to the class declaration.
        let location = location_at(&engine, text, "Widget w", 2).expect("type usage must resolve");
        assert_eq!(location.uri, uri());
        assert_eq!(location.range, range_of(text, "Widget"));

        // A method-invocation name resolves to the method declaration.
        let location =
            location_at(&engine, text, "w.getSize", 3).expect("method usage must resolve");
        assert_eq!(location.range, range_of(text, "getSize"));

        // A plain reference to a field resolves to the field declaration.
        let location =
            location_at(&engine, text, "return size", 7).expect("field usage must resolve");
        assert_eq!(location.range, range_of(text, "size"));
    }

    #[test]
    fn definition_on_a_declaration_resolves_to_itself() {
        let text = "class Widget {
    int size;
}
";
        let engine = engine_with(text);
        let location = location_at(&engine, text, "Widget {", 2).expect("declaration must resolve");
        assert_eq!(location.range, range_of(text, "Widget"));
        let location =
            location_at(&engine, text, "int size;", 4).expect("declaration must resolve");
        assert_eq!(location.range, range_of(text, "size"));
    }

    #[test]
    fn definition_resolves_import_targets_declared_elsewhere() {
        let engine = TreeSitterEngine::new();
        let decl_uri = Url::parse("file:///Widget.java").unwrap();
        engine.open(&decl_uri, WIDGET);
        let text = "import demo.Widget;

class Use {
}
";
        engine.open(&uri(), text);

        let location =
            location_at(&engine, text, "demo.Widget", 6).expect("import target must resolve");
        assert_eq!(location.uri, decl_uri);
        assert_eq!(location.range, range_of(WIDGET, "Widget"));
    }

    #[test]
    fn definition_resolves_a_unique_cross_file_reference() {
        let engine = TreeSitterEngine::new();
        let decl_uri = Url::parse("file:///Widget.java").unwrap();
        engine.open(&decl_uri, WIDGET);
        let text = "class Use {
    Widget w;
}
";
        engine.open(&uri(), text);

        let location = location_at(&engine, text, "Widget w", 2).expect("reference must resolve");
        assert_eq!(location.uri, decl_uri);
        assert_eq!(location.range, range_of(WIDGET, "Widget"));
    }

    #[test]
    fn definition_of_an_ambiguous_name_is_none() {
        let engine = TreeSitterEngine::new();
        engine.open(&Url::parse("file:///A.java").unwrap(), "class Dup {}\n");
        engine.open(&Url::parse("file:///B.java").unwrap(), "class Dup {}\n");
        let text = "class Use {
    Dup d;
}
";
        engine.open(&uri(), text);

        assert!(
            location_at(&engine, text, "Dup d", 2).is_none(),
            "no result beats a wrong result"
        );
    }

    #[test]
    fn definition_collapses_same_uri_overloads_to_the_first() {
        let text = "\
class Calc {
    int add(int a) { return a; }
    int add(int a, int b) { return a + b; }
    int total = add(1, 2);
}
";
        let engine = engine_with(text);
        let location = location_at(&engine, text, "= add", 3).expect("overloads must resolve");
        assert_eq!(location.range, range_of(text, "add"));
    }

    #[test]
    fn definition_of_wildcard_jdk_and_unindexed_names_is_none() {
        let text = "\
import java.util.*;
import java.util.List;

class Use {
    void m() {
        int x = nosuch;
    }
}
";
        let engine = engine_with(text);
        assert!(
            location_at(&engine, text, "java.util.*", 10).is_none(),
            "a wildcard names nothing"
        );
        assert!(
            location_at(&engine, text, "java.util.List", 13).is_none(),
            "JDK imports are not indexed"
        );
        assert!(
            location_at(&engine, text, "nosuch", 3).is_none(),
            "unindexed names resolve to nothing"
        );
    }

    #[test]
    fn definition_of_a_keyword_or_an_empty_word_is_none() {
        let text = "class Widget {
}
";
        let engine = engine_with(text);
        // On the `class` keyword...
        let offset = text.find("class").unwrap() + 1;
        assert!(engine
            .definition(&uri(), lsp_position(text, offset))
            .is_none());
        // ...and on whitespace: no word under the cursor.
        let offset = text.find("Widget").unwrap() - 1;
        assert!(engine
            .definition(&uri(), lsp_position(text, offset))
            .is_none());
    }

    #[test]
    fn workspace_symbols_map_the_index_to_symbol_information() {
        let engine = TreeSitterEngine::new();
        let decl_uri = Url::parse("file:///Widget.java").unwrap();
        engine.open(&decl_uri, WIDGET);
        let other_uri = Url::parse("file:///Other.java").unwrap();
        engine.open(
            &other_uri,
            "import java.util.List;
import static java.lang.Math.max;

class Other {
    int flag;
}
",
        );

        // A prefix hit on a type: its class kind and its selection range.
        let symbols = engine.workspace_symbols("Wi");
        assert_eq!(symbols.len(), 1, "{symbols:?}");
        assert_eq!(symbols[0].name, "Widget");
        assert_eq!(symbols[0].kind, SymbolKind::CLASS);
        assert_eq!(symbols[0].location.uri, decl_uri);
        assert_eq!(symbols[0].location.range, range_of(WIDGET, "Widget"));
        assert_eq!(symbols[0].container_name, None);

        // A member hit carries its container as `container_name`.
        let symbols = engine.workspace_symbols("getSize");
        assert_eq!(symbols.len(), 1, "{symbols:?}");
        assert_eq!(symbols[0].kind, SymbolKind::METHOD);
        assert_eq!(symbols[0].container_name.as_deref(), Some("Widget"));

        // Import entries are not workspace symbols: an empty query returns
        // everything indexed except them.
        let names: Vec<String> = engine
            .workspace_symbols("")
            .into_iter()
            .map(|symbol| symbol.name)
            .collect();
        assert_eq!(
            names,
            ["Other", "Widget", "flag", "getSize", "size"],
            "ordered by name then position, imports excluded"
        );
    }

    #[test]
    fn workspace_symbols_map_every_declaration_kind() {
        let engine = TreeSitterEngine::new();
        engine.open(
            &uri(),
            "interface Driven {}
enum Mode {}
record Point(int x) {}
class Carrier {
    int load;
}
",
        );

        let names_kinds: Vec<(String, SymbolKind)> = engine
            .workspace_symbols("")
            .into_iter()
            .map(|symbol| (symbol.name, symbol.kind))
            .collect();
        assert!(names_kinds.contains(&("Driven".to_string(), SymbolKind::INTERFACE)));
        assert!(names_kinds.contains(&("Mode".to_string(), SymbolKind::ENUM)));
        assert!(names_kinds.contains(&("Point".to_string(), SymbolKind::STRUCT)));
        assert!(names_kinds.contains(&("Carrier".to_string(), SymbolKind::CLASS)));
        assert!(names_kinds.contains(&("load".to_string(), SymbolKind::FIELD)));
    }

    #[test]
    fn scoped_names_cover_parameters_locals_and_enclosing_fields() {
        let mut parser = java_parser();
        let tree = parser.parse(SCOPE_SAMPLE, None).unwrap();
        let offset = SCOPE_SAMPLE.find("later;").unwrap() + "later".len();
        let node = tree
            .root_node()
            .descendant_for_byte_range(offset, offset)
            .expect("a node at the cursor");
        let mut names = Vec::new();
        collect_scoped_names(&node, SCOPE_SAMPLE, &mut names);

        let named = |name: &str| names.iter().find(|n| n.name == name).unwrap();
        let parameter = named("parameter");
        assert_eq!(parameter.kind, CompletionItemKind::VARIABLE);
        assert_eq!(parameter.detail, "parameter");
        let later = named("later");
        assert_eq!(later.kind, CompletionItemKind::VARIABLE);
        assert_eq!(later.detail, "local");
        let field = named("field");
        assert_eq!(field.kind, CompletionItemKind::FIELD);
        assert_eq!(field.detail, "field of Outer");
        assert!(!names.iter().any(|n| n.name == "deep"), "{names:?}");
        assert!(!names.iter().any(|n| n.name == "method"), "{names:?}");
        assert!(!names.iter().any(|n| n.name == "Inner"), "{names:?}");
    }

    #[test]
    fn completions_offer_ranked_java_keywords() {
        let text = "class Sample {\n    void m() {\n        int x = fina;\n    }\n}\n";
        let engine = engine_with(text);
        let offset = text.find("fina;").unwrap() + "fina".len();
        let Some(CompletionResponse::Array(items)) =
            engine.completions(&uri(), lsp_position(text, offset))
        else {
            panic!("completions must return an array");
        };

        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[0].label, "final");
        assert_eq!(items[0].kind, Some(CompletionItemKind::KEYWORD));
        assert_eq!(items[0].sort_text.as_deref(), Some("0final"));
        assert_eq!(items[1].label, "finally");
        assert_eq!(items[1].sort_text.as_deref(), Some("0finally"));
    }

    #[test]
    fn completions_rank_locals_above_deduped_workspace_symbols() {
        let text = "\
class summer {
    int sum;
    void sumIt() {}
}

class Main {
    void run() {
        int sum = 1;
        int s = su;
    }
}
";
        let engine = engine_with(text);
        let offset = text.find("su;").unwrap() + "su".len();
        let Some(CompletionResponse::Array(items)) =
            engine.completions(&uri(), lsp_position(text, offset))
        else {
            panic!("completions must return an array");
        };

        // Keywords rank first, locals second, workspace symbols last.
        let super_ = &items[0];
        assert_eq!(super_.label, "super");
        assert_eq!(super_.kind, Some(CompletionItemKind::KEYWORD));
        assert_eq!(super_.sort_text.as_deref(), Some("0super"));

        // The local wins over the same-named workspace field: one `sum` only.
        assert_eq!(
            items
                .iter()
                .filter(|i| i.insert_text.as_deref() == Some("sum"))
                .count(),
            1
        );
        let local = items
            .iter()
            .find(|i| i.insert_text.as_deref() == Some("sum"))
            .unwrap();
        assert_eq!(local.kind, Some(CompletionItemKind::VARIABLE));
        assert_eq!(local.detail.as_deref(), Some("local"));
        assert_eq!(local.sort_text.as_deref(), Some("1sum"));

        // The workspace member carries its container as the label but stays
        // unqualified to type and filter.
        let method = items
            .iter()
            .find(|i| i.insert_text.as_deref() == Some("sumIt"))
            .unwrap();
        assert_eq!(method.label, "summer.sumIt");
        assert_eq!(method.filter_text.as_deref(), Some("sumIt"));
        assert_eq!(method.kind, Some(CompletionItemKind::METHOD));
        assert_eq!(method.detail.as_deref(), Some("method of summer"));
        assert_eq!(method.sort_text.as_deref(), Some("2sumIt"));

        // Types keep their plain name and class kind.
        let class = items
            .iter()
            .find(|i| i.insert_text.as_deref() == Some("summer"))
            .unwrap();
        assert_eq!(class.kind, Some(CompletionItemKind::CLASS));
        assert_eq!(class.sort_text.as_deref(), Some("2summer"));

        assert!(items.iter().all(|i| i
            .sort_text
            .as_deref()
            .is_some_and(|s| s.starts_with('0') || s.starts_with('1') || s.starts_with('2'))));
    }

    #[test]
    fn completions_after_a_dot_return_empty_and_from_nothing_typed_none() {
        let text = "\
class Sample {
    String greeting = null;
    void m() {
        int n = greeting.length;
    }
}
";
        let engine = engine_with(text);

        let after_dot_word = text.find("length;").unwrap() + "length".len();
        match engine.completions(&uri(), lsp_position(text, after_dot_word)) {
            Some(CompletionResponse::Array(items)) => {
                assert!(
                    items.is_empty(),
                    "no membership claims after `.`, got {items:?}"
                );
            }
            other => panic!("expected an empty array after `.`, got {other:?}"),
        }

        // Directly after the dot nothing is typed: no suggestions at all.
        let just_after_dot = text.find(".length;").unwrap() + 1;
        assert!(engine
            .completions(&uri(), lsp_position(text, just_after_dot))
            .is_none());

        // And from an empty prefix in general.
        let at_word_start = text.find("int n").unwrap();
        assert!(engine
            .completions(&uri(), lsp_position(text, at_word_start))
            .is_none());
    }

    /// Opens a helper declaration file in the engine so it appears in the
    /// index, then completes `prefix` inside `open_text` at the `offset`.
    fn complete_with_helper(
        engine: &TreeSitterEngine,
        open_text: &str,
        offset: usize,
        helper_uri: &Url,
        helper_text: &str,
        prefix_len: usize,
    ) -> Vec<CompletionItem> {
        engine.open(&uri(), open_text);
        engine.open(helper_uri, helper_text);
        let Some(CompletionResponse::Array(items)) =
            engine.completions(&uri(), lsp_position(open_text, offset))
        else {
            panic!("completions must return an array");
        };
        items
            .into_iter()
            .filter(|item| {
                item.filter_text.as_deref().is_some_and(|text| {
                    open_text[offset - prefix_len..offset].starts_with(text)
                        || text.starts_with(&open_text[offset - prefix_len..offset])
                })
            })
            .collect()
    }

    #[test]
    fn completion_of_a_cross_package_symbol_carries_the_import_edit() {
        let open_text = "\
package com.a;\n\nimport java.util.List;\n\nclass Sample {\n    void m() {\n        Widget w;\n    }\n}\n";
        let engine = TreeSitterEngine::new();
        let helper = Url::parse("file:///other/Widget.java").unwrap();
        let items = complete_with_helper(
            &engine,
            open_text,
            open_text.find("Widget w").unwrap() + "Widget".len(),
            &helper,
            "package com.b;\n\nclass Widget {\n}\n",
            "Widget".len(),
        );

        let widget = items
            .iter()
            .find(|item| item.label == "Widget")
            .expect("workspace type offered");
        let edits = widget.additional_text_edits.as_ref().expect("import edit");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "import com.b.Widget;\n");
        // Inserted right after the last import (line 3, char 0).
        assert_eq!(edits[0].range.start.line, 3);
        assert_eq!(edits[0].range.start.character, 0);
    }

    #[test]
    fn import_edit_position_falls_back_to_package_then_top() {
        // No imports: the edit goes after the package statement.
        let open_text = "\
package com.a;\n\nclass Sample {\n    void m() {\n        Widget w;\n    }\n}\n";
        let engine = TreeSitterEngine::new();
        let helper = Url::parse("file:///other/Widget.java").unwrap();
        let items = complete_with_helper(
            &engine,
            open_text,
            open_text.find("Widget w").unwrap() + "Widget".len(),
            &helper,
            "package com.b;\n\nclass Widget {\n}\n",
            "Widget".len(),
        );
        let widget = items.iter().find(|item| item.label == "Widget").unwrap();
        let edits = widget.additional_text_edits.as_ref().unwrap();
        assert_eq!(edits[0].new_text, "import com.b.Widget;\n");
        assert_eq!(edits[0].range.start.line, 1);

        // No package either: top of file.
        let plain = "class Sample {\n    void m() {\n        Widget w;\n    }\n}\n";
        let engine = TreeSitterEngine::new();
        let items = complete_with_helper(
            &engine,
            plain,
            plain.find("Widget w").unwrap() + "Widget".len(),
            &helper,
            "package com.b;\n\nclass Widget {\n}\n",
            "Widget".len(),
        );
        let widget = items.iter().find(|item| item.label == "Widget").unwrap();
        let edits = widget.additional_text_edits.as_ref().unwrap();
        assert_eq!(edits[0].range.start.line, 0);
    }

    #[test]
    fn no_import_edit_for_same_package_same_file_or_default_package() {
        // Same package: no edit.
        let open_text = "\
package com.a;\n\nclass Sample {\n    void m() {\n        Widget w;\n    }\n}\n";
        let engine = TreeSitterEngine::new();
        let helper = Url::parse("file:///com/a/Widget.java").unwrap();
        let items = complete_with_helper(
            &engine,
            open_text,
            open_text.find("Widget w").unwrap() + "Widget".len(),
            &helper,
            "package com.a;\n\nclass Widget {\n}\n",
            "Widget".len(),
        );
        let widget = items.iter().find(|item| item.label == "Widget").unwrap();
        assert!(widget.additional_text_edits.is_none());

        // Same file: the document's own class is indexed under the same uri
        // — no edit (and same package anyway).
        let engine = TreeSitterEngine::new();
        engine.open(&uri(), open_text);
        let offset = open_text.find("Sample").unwrap() + "Sample".len();
        let Some(CompletionResponse::Array(items)) =
            engine.completions(&uri(), lsp_position(open_text, offset))
        else {
            panic!("completions must return an array");
        };
        let sample = items
            .iter()
            .find(|item| item.label == "Sample")
            .expect("same-file type offered");
        assert!(sample.additional_text_edits.is_none());

        // Default-package file offering a default-package type: nothing to
        // import (default-package types cannot be imported).
        let plain = "class Sample {\n    void m() {\n        Widget w;\n    }\n}\n";
        let engine = TreeSitterEngine::new();
        let helper = Url::parse("file:///other/Widget.java").unwrap();
        let items = complete_with_helper(
            &engine,
            plain,
            plain.find("Widget w").unwrap() + "Widget".len(),
            &helper,
            "class Widget {\n}\n",
            "Widget".len(),
        );
        let widget = items.iter().find(|item| item.label == "Widget").unwrap();
        assert!(widget.additional_text_edits.is_none());
    }

    #[test]
    fn wildcard_import_covers_and_conflicting_import_suppresses() {
        let open_text = "\
package com.a;\n\nimport com.b.*;\n\nclass Sample {\n    void m() {\n        Widget w;\n    }\n}\n";
        let engine = TreeSitterEngine::new();
        let helper = Url::parse("file:///other/Widget.java").unwrap();
        let items = complete_with_helper(
            &engine,
            open_text,
            open_text.find("Widget w").unwrap() + "Widget".len(),
            &helper,
            "package com.b;\n\nclass Widget {\n}\n",
            "Widget".len(),
        );
        let widget = items.iter().find(|item| item.label == "Widget").unwrap();
        assert!(
            widget.additional_text_edits.is_none(),
            "wildcard import must suppress the edit"
        );

        // A different `Widget` is already imported explicitly: conflict —
        // the item stays but the edit is suppressed.
        let conflicting = "\
package com.a;\n\nimport com.c.Widget;\n\nclass Sample {\n    void m() {\n        Widget w;\n    }\n}\n";
        let engine = TreeSitterEngine::new();
        let items = complete_with_helper(
            &engine,
            conflicting,
            conflicting.find("Widget w").unwrap() + "Widget".len(),
            &helper,
            "package com.b;\n\nclass Widget {\n}\n",
            "Widget".len(),
        );
        let widget = items.iter().find(|item| item.label == "Widget").unwrap();
        assert!(widget.additional_text_edits.is_none());
    }

    #[test]
    fn member_completions_import_their_enclosing_type() {
        let open_text = "\
package com.a;\n\nclass Sample {\n    void m() {\n        String s = getNa;\n    }\n}\n";
        let engine = TreeSitterEngine::new();
        let helper = Url::parse("file:///other/Widget.java").unwrap();
        let items = complete_with_helper(
            &engine,
            open_text,
            open_text.find("getNa;").unwrap() + "getNa".len(),
            &helper,
            "package com.b;\n\nclass Widget {\n    String getName() { return \"\"; }\n}\n",
            "getNa".len(),
        );

        let get_name = items
            .iter()
            .find(|item| item.label == "Widget.getName")
            .expect("member offered");
        let edits = get_name
            .additional_text_edits
            .as_ref()
            .expect("import edit");
        assert_eq!(edits[0].new_text, "import com.b.Widget;\n");
    }
}
