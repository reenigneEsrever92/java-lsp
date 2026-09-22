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
    DocumentSymbol, FoldingRange, FoldingRangeKind, Hover, HoverContents, InlayHint, InlayHintKind,
    InlayHintLabel, Location, MarkupContent, MarkupKind, Position, Range, SemanticToken,
    SemanticTokenType, SemanticTokens, SymbolInformation, SymbolKind, TextEdit, Url, WorkspaceEdit,
};
use tree_sitter::{Node, Parser, Tree};

use super::SemanticEngine;
use crate::index::{
    extract_entries, java_parser, scan_workspace, IndexKind, SymbolEntry, WorkspaceIndex,
};
use crate::types::{self, Member, Ty, TypeLookup, TypeModel, TypeQuery};

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

    /// Completion items for a member access after `.`: the receiver's inferred
    /// type's members, inherited ones included for workspace types. A receiver
    /// that names a type offers only its static members.
    fn member_items(
        &self,
        document: &ParsedDocument,
        offset: usize,
        prefix: &str,
    ) -> Vec<CompletionItem> {
        let text = &document.text;
        let Some(object) = receiver_before_dot(&document.tree, offset) else {
            return Vec::new();
        };
        let local = local_model(document);
        let workspace = self.index.type_model();
        let empty = TypeModel::new();
        let base = workspace.as_deref().unwrap_or(&empty);
        let query = TypeQuery::new(base, &local);
        let scope = types::scope_at(object, text, &document.tree, &query);

        let static_only = object.kind() == "identifier"
            && types::resolve_name(&text[object.byte_range()], &scope, &query)
                .map(|resolved| resolved.is_type)
                .unwrap_or(false);

        let ty = types::receiver_type(&object, text, &scope, &query);
        let mut items = Vec::new();
        let mut seen = HashSet::new();
        for member in query.members(&ty, scope.package.as_deref()) {
            if static_only && !member.is_static {
                continue;
            }
            if !member.name.starts_with(prefix) {
                continue;
            }
            let is_method = member.kind == IndexKind::Method;
            if !seen.insert((member.name.clone(), is_method)) {
                continue;
            }
            items.push(CompletionItem {
                label: member.name.clone(),
                kind: Some(if is_method {
                    CompletionItemKind::METHOD
                } else {
                    CompletionItemKind::FIELD
                }),
                detail: Some(member.signature()),
                sort_text: Some(format!("0{}", member.name)),
                ..Default::default()
            });
        }
        items
    }

    /// Resolves the symbol under the cursor to what a references or rename
    /// request targets, or `None` when it cannot be pinned to one declaration.
    fn resolve_target(
        &self,
        uri: &Url,
        document: &ParsedDocument,
        offset: usize,
    ) -> Option<Target> {
        let text = &document.text;
        let tree = &document.tree;
        let node = tree.root_node().descendant_for_byte_range(offset, offset)?;
        let node = cursor_node(tree, node, text, offset);
        let local = local_model(document);
        let workspace = self.index.type_model();
        let empty = TypeModel::new();
        let base = workspace.as_deref().unwrap_or(&empty);
        let query = TypeQuery::new(base, &local);
        let scope = types::scope_at(node, text, tree, &query);

        // Inside an import declaration, the dotted path's last segment names the
        // imported type.
        let mut ancestor = Some(node);
        while let Some(current) = ancestor {
            if current.kind() == "import_declaration" {
                let raw = text[current.byte_range()].trim();
                let raw = raw.strip_prefix("import").map_or(raw, str::trim);
                let raw = raw.strip_prefix("static").map_or(raw, str::trim);
                let path = raw.trim_end_matches(';').trim();
                let (package, simple) = path.rsplit_once('.')?;
                // Only the terminal segment names the imported type; a cursor on
                // a package segment or a `.*` names nothing.
                if simple.is_empty() || simple == "*" || text[node.byte_range()] != *simple {
                    return None;
                }
                return self.type_target_for(simple, Some(package), true);
            }
            ancestor = current.parent();
        }

        let name = word_at(text, offset);
        if name.is_empty() {
            return None;
        }

        // A declaration name.
        if let Some(parent) = node.parent() {
            if let Some(target) = declaration_target(
                uri,
                node,
                &parent,
                text,
                scope.package.as_deref(),
                &self.index,
            ) {
                return Some(target);
            }
        }

        // A type position (`new`, a declared type, `extends`, a cast).
        if is_type_node(node.kind()) {
            let package = preferred_type_package(&scope, &name);
            return self.type_target_for(&name, package.as_deref(), false);
        }

        // A member access through a receiver.
        if let Some(parent) = node.parent() {
            match parent.kind() {
                "field_access" => {
                    let is_field = parent
                        .child_by_field_name("field")
                        .is_some_and(|field| field.id() == node.id());
                    if is_field {
                        if let Some(object) = parent.child_by_field_name("object") {
                            let receiver = types::receiver_type(&object, text, &scope, &query);
                            return self.member_target(
                                &name,
                                IndexKind::Field,
                                &receiver,
                                &query,
                                scope.package.as_deref(),
                            );
                        }
                    }
                }
                "method_invocation" => {
                    let is_name = parent
                        .child_by_field_name("name")
                        .is_some_and(|named| named.id() == node.id());
                    if is_name {
                        let receiver = match parent.child_by_field_name("object") {
                            Some(object) => types::receiver_type(&object, text, &scope, &query),
                            None => scope
                                .enclosing_type
                                .clone()
                                .map(Ty::reference)
                                .unwrap_or(Ty::Unknown),
                        };
                        return self.member_target(
                            &name,
                            IndexKind::Method,
                            &receiver,
                            &query,
                            scope.package.as_deref(),
                        );
                    }
                }
                _ => {}
            }
        }

        // A bare name: locals first, then members, then types.
        let resolved = types::resolve_name(&name, &scope, &query)?;
        if resolved.is_type {
            let package = preferred_type_package(&scope, &name);
            return self.type_target_for(&name, package.as_deref(), false);
        }
        if let Some(member) = resolved.member {
            let receiver = scope
                .enclosing_type
                .clone()
                .map(Ty::reference)
                .unwrap_or(Ty::Unknown);
            return self.member_target(
                &name,
                member.kind,
                &receiver,
                &query,
                scope.package.as_deref(),
            );
        }
        local_target(uri, node, &name, text)
    }

    /// Resolves the cursor to a target, returning it with the requested file's
    /// text so the caller can release the document lock before searching.
    fn name_target(&self, uri: &Url, position: Position) -> Option<(Target, String)> {
        let documents = self.documents.lock().ok()?;
        let document = documents.get(uri)?;
        let offset = byte_offset(&document.text, position);
        let target = self.resolve_target(uri, document, offset)?;
        Some((target, document.text.clone()))
    }

    /// A workspace-declared type as a target, resolving the simple name in
    /// `package` (an exact import, or the file's own package). `strict` demands
    /// that package; otherwise a unique candidate anywhere is accepted, so a
    /// wildcard import still resolves. A dependency declaration is always
    /// refused — a jar cannot be written back.
    fn type_target_for(&self, name: &str, package: Option<&str>, strict: bool) -> Option<Target> {
        let mut entries: Vec<SymbolEntry> = self
            .index
            .query_name(name)
            .into_iter()
            .filter(|entry| !entry.dependency && is_type_kind(entry.kind))
            .collect();
        if entries.is_empty() {
            return None;
        }
        match package {
            Some(package) => {
                let in_package: Vec<SymbolEntry> = entries
                    .iter()
                    .filter(|entry| entry.package.as_deref() == Some(package))
                    .cloned()
                    .collect();
                if strict || !in_package.is_empty() {
                    entries = in_package;
                }
            }
            None => {}
        }
        let entry = unique_entry(entries)?;
        Some(Target {
            name: name.to_string(),
            kind: TargetKind::Type,
            owner: Some(name.to_string()),
            package: entry.package.clone(),
            local_span: None,
            declarations: vec![entry],
            local_declaration: None,
        })
    }

    /// A member as a target: the receiver's type must resolve the name to the
    /// type that declares it, and that type must be a workspace source.
    fn member_target(
        &self,
        name: &str,
        kind: IndexKind,
        receiver: &Ty,
        model: &dyn TypeLookup,
        package: Option<&str>,
    ) -> Option<Target> {
        let owner = types::member_owner(receiver, name, model, package)?;
        // A member of a library type cannot be renamed: refuse.
        if !declared_in_workspace(&owner.name, owner.package.as_deref(), &self.index) {
            return None;
        }
        let is_method = kind == IndexKind::Method;
        let declarations = member_declarations(
            name,
            if is_method {
                IndexKind::Method
            } else {
                IndexKind::Field
            },
            &owner.name,
            owner.package.as_deref(),
            &self.index,
        );
        Some(Target {
            name: name.to_string(),
            kind: if is_method {
                TargetKind::Method
            } else {
                TargetKind::Field
            },
            owner: Some(owner.name),
            package: owner.package,
            local_span: None,
            declarations,
            local_declaration: None,
        })
    }

    /// Every occurrence of `target` across the workspace sources, with whether
    /// the search completed. Refusal is an empty vector: an uncertain occurrence
    /// is never included. A candidate that cannot be read or parsed marks the
    /// search incomplete, so a destructive caller can refuse.
    fn collect_occurrences(
        &self,
        target: &Target,
        requested: &Url,
        requested_text: &str,
    ) -> (Vec<Location>, bool) {
        let mut out: Vec<Location> = Vec::new();
        let mut seen: HashSet<(String, u32, u32)> = HashSet::new();
        let Ok(mut parser) = self.parser.lock() else {
            return (Vec::new(), false);
        };
        let mut complete = true;

        let mut candidates = self.index.source_files();
        if !candidates.iter().any(|uri| uri == requested) {
            candidates.push(requested.clone());
        }

        for uri in candidates {
            let text = if &uri == requested {
                requested_text.to_string()
            } else {
                let Some(path) = uri.to_file_path().ok() else {
                    complete = false;
                    continue;
                };
                match std::fs::read_to_string(path) {
                    Ok(text) => text,
                    Err(_) => {
                        complete = false;
                        continue;
                    }
                }
            };
            // Cheap prefilter: only files that mention the name can hold a hit.
            if !text.contains(&target.name) {
                continue;
            }
            let Some(tree) = parser.parse(text.as_bytes(), None) else {
                complete = false;
                continue;
            };
            match target.kind {
                TargetKind::Type => {
                    if !type_visible_in(&uri, target, &text, &tree) {
                        continue;
                    }
                    collect_type_occurrences(
                        &tree.root_node(),
                        &text,
                        &target.name,
                        &uri,
                        &mut out,
                        &mut seen,
                    );
                }
                TargetKind::Method | TargetKind::Field => {
                    self.collect_member_occurrences(
                        &tree, &text, target, &uri, &mut out, &mut seen,
                    );
                }
                TargetKind::Local => {
                    if &uri != requested {
                        continue;
                    }
                    let package = types::file_package(&tree, &text);
                    let mut local = TypeModel::new();
                    local.extend(types::collect_type_infos(package.as_deref(), &tree, &text));
                    let workspace = self.index.type_model();
                    let empty = TypeModel::new();
                    let base = workspace.as_deref().unwrap_or(&empty);
                    let query = TypeQuery::new(base, &local);
                    collect_local_occurrences(
                        &tree.root_node(),
                        &text,
                        &tree,
                        &query,
                        target,
                        &uri,
                        &mut out,
                        &mut seen,
                    );
                }
            }
        }

        out.sort_by(|a, b| {
            a.uri
                .as_str()
                .cmp(b.uri.as_str())
                .then_with(|| a.range.start.line.cmp(&b.range.start.line))
                .then_with(|| a.range.start.character.cmp(&b.range.start.character))
        });
        (out, complete)
    }

    fn collect_member_occurrences(
        &self,
        tree: &Tree,
        text: &str,
        target: &Target,
        uri: &Url,
        out: &mut Vec<Location>,
        seen: &mut HashSet<(String, u32, u32)>,
    ) {
        let package = types::file_package(tree, text);
        let mut local = TypeModel::new();
        local.extend(types::collect_type_infos(package.as_deref(), tree, text));
        let workspace = self.index.type_model();
        let empty = TypeModel::new();
        let base = workspace.as_deref().unwrap_or(&empty);
        let query = TypeQuery::new(base, &local);

        // The declaring type's byte span in this file, when this is its file.
        let owner_span = target.owner.as_ref().and_then(|owner| {
            let entry = workspace_type_entry(owner, target.package.as_deref(), &self.index)?;
            if entry.uri != *uri {
                return None;
            }
            Some((
                byte_offset(text, entry.full_range.start),
                byte_offset(text, entry.full_range.end),
            ))
        });

        collect_member_nodes(
            &tree.root_node(),
            text,
            tree,
            target,
            &query,
            package.as_deref(),
            owner_span,
            uri,
            out,
            seen,
        );
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
        let mut diagnostics = Vec::new();
        if document.tree.root_node().has_error() {
            collect_errors(&document.tree.root_node(), &document.text, &mut diagnostics);
            // A file that does not parse yields no meaningful semantic findings;
            // adding them would only pile noise onto broken code.
            return diagnostics;
        }
        diagnostics.extend(semantic_diagnostics(document, &self.index));
        diagnostics
    }

    fn hover(&self, uri: &Url, position: Position) -> Option<Hover> {
        let documents = self.documents.lock().ok()?;
        let document = documents.get(uri)?;
        let text = &document.text;
        let offset = byte_offset(text, position);
        let node = document
            .tree
            .root_node()
            .descendant_for_byte_range(offset, offset)?;
        let node = cursor_node(&document.tree, node, text, offset);
        let local = local_model(document);
        let workspace = self.index.type_model();
        let empty = TypeModel::new();
        let base = workspace.as_deref().unwrap_or(&empty);
        let query = TypeQuery::new(base, &local);
        let value = hover_value(node, text, &document.tree, &query)?;
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: None,
        })
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

    fn references(
        &self,
        uri: &Url,
        position: Position,
        include_declaration: bool,
    ) -> Vec<Location> {
        let Some((target, text)) = self.name_target(uri, position) else {
            return Vec::new();
        };
        let (mut locations, _) = self.collect_occurrences(&target, uri, &text);
        if include_declaration {
            add_declarations(&target, &mut locations);
        }
        locations.sort_by(|a, b| {
            a.uri
                .as_str()
                .cmp(b.uri.as_str())
                .then_with(|| a.range.start.line.cmp(&b.range.start.line))
                .then_with(|| a.range.start.character.cmp(&b.range.start.character))
        });
        locations
    }

    fn rename(&self, uri: &Url, position: Position, new_name: &str) -> Option<WorkspaceEdit> {
        if !is_valid_identifier(new_name) {
            return None;
        }
        let (target, text) = self.name_target(uri, position)?;
        let (mut locations, complete) = self.collect_occurrences(&target, uri, &text);
        // A destructive edit must not be built from a search that could not read
        // every candidate: refuse rather than silently drop a reference.
        if !complete {
            return None;
        }
        add_declarations(&target, &mut locations);
        if locations.is_empty() {
            return None;
        }
        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        for location in locations {
            changes.entry(location.uri).or_default().push(TextEdit {
                range: location.range,
                new_text: new_name.to_string(),
            });
        }
        Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        })
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
        let before_prefix = offset - prefix.len();
        if document.text[..before_prefix].ends_with('.') {
            // Member access: the receiver's inferred type decides the
            // membership; an uninferrable receiver yields an empty list, never
            // a guess at what might be there.
            return Some(CompletionResponse::Array(
                self.member_items(document, offset, prefix),
            ));
        }
        if prefix.is_empty() {
            // Nothing typed: no suggestions from nothing.
            return None;
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
        // attached (never-worsen: see `import_edit`). A simple name shared by
        // several symbols stays several items — each with its own import, and
        // each labeled with its owner — so the user can tell them apart.
        let (file_package, package_line) = file_header(&document.tree.root_node(), &document.text);
        let imports = collect_imports(&document.tree.root_node(), &document.text);
        let entries = self.index.query_prefix(prefix);
        let ambiguous = ambiguous_names(&entries);
        for entry in entries {
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
            let detail = if ambiguous.contains(&name) {
                format!("{} of {}", kind_word(entry.kind), qualified_owner(&entry))
            } else {
                detail
            };
            let additional_edits = import_edit(
                uri,
                &entry,
                file_package.as_deref(),
                package_line,
                &imports,
                &self.index,
            );
            // Same name + same import is a duplicate; same name with a
            // different import is a different symbol and stays separate.
            let key = match additional_edits.first() {
                Some(edit) => format!("{name}|{}", edit.new_text),
                None => name.clone(),
            };
            offer(
                &mut items,
                &mut seen,
                key,
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

    fn inlay_hints(&self, uri: &Url, range: Range) -> Vec<InlayHint> {
        let Ok(documents) = self.documents.lock() else {
            return Vec::new();
        };
        let Some(document) = documents.get(uri) else {
            return Vec::new();
        };
        let text = &document.text;
        let start = byte_offset(text, range.start);
        let end = byte_offset(text, range.end);
        if end <= start {
            return Vec::new();
        }
        let local = local_model(document);
        let workspace = self.index.type_model();
        let empty = TypeModel::new();
        let base = workspace.as_deref().unwrap_or(&empty);
        let query = TypeQuery::new(base, &local);
        let mut hints = Vec::new();
        collect_inlay_hints(
            document.tree.root_node(),
            text,
            &document.tree,
            &query,
            start,
            end,
            &mut hints,
        );
        hints.sort_by_key(|hint| (hint.position.line, hint.position.character));
        hints.dedup_by(|a, b| a.position == b.position && hint_label(a) == hint_label(b));
        // A node straddling the range boundary can emit a hint outside it; keep
        // only hints the requested range actually covers.
        hints.retain(|hint| position_in_range(hint.position, range));
        hints
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

// ---------------------------------------------------------------------------
// Type-aware feature helpers
// ---------------------------------------------------------------------------

/// The node a cursor at `offset` points at. At a token boundary tree-sitter
/// hands back the enclosing declaration, so a one-byte span recovers the token
/// the cursor is actually on (editors place cursors at token starts).
fn cursor_node<'a>(tree: &'a Tree, node: Node<'a>, text: &str, offset: usize) -> Node<'a> {
    if is_cursor_target(node.kind()) {
        return node;
    }
    let end = (offset + 1).min(text.len());
    match tree.root_node().descendant_for_byte_range(offset, end) {
        Some(inner) if inner.start_byte() == offset && is_cursor_target(inner.kind()) => inner,
        _ => node,
    }
}

fn is_cursor_target(kind: &str) -> bool {
    matches!(kind, "identifier" | "type_identifier") || is_type_node(kind)
}

// ---------------------------------------------------------------------------
// References and rename
// ---------------------------------------------------------------------------

/// What a references or rename request targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetKind {
    Type,
    Method,
    Field,
    Local,
}

/// A resolved target: the symbol a request is about, and everything needed to
/// decide which occurrences really refer to it.
#[derive(Debug, Clone)]
struct Target {
    name: String,
    kind: TargetKind,
    /// The simple name of the type that declares a member (or the type itself).
    owner: Option<String>,
    /// A type's package, for the visibility rule.
    package: Option<String>,
    /// The enclosing method's byte span, for a local or parameter.
    local_span: Option<(usize, usize)>,
    /// The index's declaration entries, for `include_declaration`.
    declarations: Vec<SymbolEntry>,
    /// A local's or parameter's own declaration, which the index does not hold.
    local_declaration: Option<Location>,
}

/// The workspace type declared with `name` in `package`, or `None` when it is a
/// dependency (jar/JDK) or spread over several files — the ambiguity policy
/// `definition` already applies.
fn workspace_type_entry(
    name: &str,
    package: Option<&str>,
    index: &WorkspaceIndex,
) -> Option<SymbolEntry> {
    let candidates: Vec<SymbolEntry> = index
        .query_name(name)
        .into_iter()
        .filter(|entry| !entry.dependency && is_type_kind(entry.kind))
        .filter(|entry| entry.package.as_deref() == package)
        .collect();
    unique_entry(candidates)
}

/// True when a workspace source declares a type with this name in `package`.
fn declared_in_workspace(name: &str, package: Option<&str>, index: &WorkspaceIndex) -> bool {
    index.query_name(name).into_iter().any(|entry| {
        !entry.dependency && is_type_kind(entry.kind) && entry.package.as_deref() == package
    })
}

/// The package a simple type name resolves in: an exact single-type import
/// wins, otherwise the file's own package.
fn preferred_type_package(scope: &types::Scope, name: &str) -> Option<String> {
    scope
        .imports
        .iter()
        .filter(|import| !import.is_static && !import.is_wildcard)
        .find(|import| import.simple_name() == Some(name))
        .and_then(|import| {
            import
                .path
                .rsplit_once('.')
                .map(|(package, _)| package.to_string())
        })
        .or_else(|| scope.package.clone())
}

fn unique_entry(mut candidates: Vec<SymbolEntry>) -> Option<SymbolEntry> {
    candidates.sort_by(|a, b| {
        a.uri
            .as_str()
            .cmp(b.uri.as_str())
            .then_with(|| {
                a.selection_range
                    .start
                    .line
                    .cmp(&b.selection_range.start.line)
            })
            .then_with(|| {
                a.selection_range
                    .start
                    .character
                    .cmp(&b.selection_range.start.character)
            })
    });
    let (first, rest) = candidates.split_first()?;
    if rest.iter().any(|entry| entry.uri != first.uri) {
        return None;
    }
    Some(first.clone())
}

/// The index entries declaring `name` as a member of `owner` in `package`.
fn member_declarations(
    name: &str,
    kind: IndexKind,
    owner: &str,
    package: Option<&str>,
    index: &WorkspaceIndex,
) -> Vec<SymbolEntry> {
    index
        .query_name(name)
        .into_iter()
        .filter(|entry| !entry.dependency && entry.kind == kind)
        .filter(|entry| entry.container.last().map(String::as_str) == Some(owner))
        .filter(|entry| entry.package.as_deref() == package)
        .collect()
}

/// A declaration name under the cursor, resolved to its target.
fn declaration_target(
    uri: &Url,
    node: Node,
    parent: &Node,
    text: &str,
    package: Option<&str>,
    index: &WorkspaceIndex,
) -> Option<Target> {
    // Only the declaration's own name is a target; a sibling such as a
    // variable initializer or an enhanced-for iterable is not.
    if !parent
        .child_by_field_name("name")
        .is_some_and(|name| name.id() == node.id())
    {
        return None;
    }
    match parent.kind() {
        "class_declaration"
        | "interface_declaration"
        | "enum_declaration"
        | "record_declaration"
        | "annotation_type_declaration" => {
            let name = text[node.byte_range()].to_string();
            let entry = workspace_type_entry(&name, package, index)?;
            Some(Target {
                name,
                kind: TargetKind::Type,
                owner: None,
                package: entry.package.clone(),
                local_span: None,
                declarations: vec![entry],
                local_declaration: None,
            })
        }
        "method_declaration" => {
            let name = text[node.byte_range()].to_string();
            let owner = enclosing_type_name(parent, text)?;
            workspace_type_entry(&owner, package, index)?;
            let declarations =
                member_declarations(&name, IndexKind::Method, &owner, package, index);
            Some(Target {
                name,
                kind: TargetKind::Method,
                owner: Some(owner),
                package: package.map(str::to_string),
                local_span: None,
                declarations,
                local_declaration: None,
            })
        }
        "variable_declarator" => {
            let declaration = parent.parent()?;
            let name = text[node.byte_range()].to_string();
            match declaration.kind() {
                "field_declaration" | "constant_declaration" => {
                    let owner = enclosing_type_name(&declaration, text)?;
                    workspace_type_entry(&owner, package, index)?;
                    let declarations =
                        member_declarations(&name, IndexKind::Field, &owner, package, index);
                    Some(Target {
                        name,
                        kind: TargetKind::Field,
                        owner: Some(owner),
                        package: package.map(str::to_string),
                        local_span: None,
                        declarations,
                        local_declaration: None,
                    })
                }
                "local_variable_declaration" => local_target(uri, node, &name, text),
                _ => None,
            }
        }
        "formal_parameter" | "spread_parameter" | "enhanced_for_statement" => {
            let name = text[node.byte_range()].to_string();
            local_target(uri, node, &name, text)
        }
        _ => None,
    }
}

/// A local or parameter target, refused unless the enclosing method declares
/// exactly one symbol of that name — otherwise an occurrence cannot be
/// attributed with confidence.
fn local_target(uri: &Url, node: Node, name: &str, text: &str) -> Option<Target> {
    let method = enclosing_declarator(node)?;
    let mut declarations = 0usize;
    count_local_declarations(&method, text, name, &mut declarations);
    if declarations != 1 {
        return None;
    }
    // The declaration is located by name, not by the cursor node, so a cursor on
    // a use (an initializer, the enhanced-for iterable) still names the right
    // declaration.
    let local_declaration = find_local_declaration(&method, text, name).map(|declared| Location {
        uri: uri.clone(),
        range: lsp_range(text, &declared),
    });
    Some(Target {
        name: name.to_string(),
        kind: TargetKind::Local,
        owner: None,
        package: None,
        local_span: Some((method.start_byte(), method.end_byte())),
        declarations: Vec::new(),
        local_declaration,
    })
}

/// The name node of the single local, parameter, or enhanced-for binding named
/// `name` inside `node`.
fn find_local_declaration<'a>(node: &Node<'a>, text: &str, name: &str) -> Option<Node<'a>> {
    match node.kind() {
        "variable_declarator" => {
            let in_local = node
                .parent()
                .is_some_and(|parent| parent.kind() == "local_variable_declaration");
            if in_local {
                if let Some(declared) = node.child_by_field_name("name") {
                    if &text[declared.byte_range()] == name {
                        return Some(declared);
                    }
                }
            }
        }
        "formal_parameter" | "spread_parameter" | "enhanced_for_statement" => {
            if let Some(declared) = node.child_by_field_name("name") {
                if &text[declared.byte_range()] == name {
                    return Some(declared);
                }
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = find_local_declaration(&child, text, name) {
            return Some(found);
        }
    }
    None
}

fn enclosing_declarator(node: Node) -> Option<Node> {
    let mut current = Some(node);
    while let Some(node) = current {
        if matches!(
            node.kind(),
            "method_declaration" | "constructor_declaration"
        ) {
            return Some(node);
        }
        current = node.parent();
    }
    None
}

fn enclosing_type_name(node: &Node, text: &str) -> Option<String> {
    let mut current = Some(*node);
    while let Some(node) = current {
        if is_type_decl_kind(node.kind()) {
            return node
                .child_by_field_name("name")
                .map(|name| text[name.byte_range()].to_string());
        }
        current = node.parent();
    }
    None
}

fn is_type_decl_kind(kind: &str) -> bool {
    matches!(
        kind,
        "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
    )
}

/// Counts the local declarations (locals, enhanced-for variables, parameters)
/// named `name` inside a method.
fn count_local_declarations(node: &Node, text: &str, name: &str, count: &mut usize) {
    match node.kind() {
        "variable_declarator" => {
            let in_local = node
                .parent()
                .is_some_and(|parent| parent.kind() == "local_variable_declaration");
            if in_local {
                if let Some(declared) = node.child_by_field_name("name") {
                    if &text[declared.byte_range()] == name {
                        *count += 1;
                    }
                }
            }
        }
        "formal_parameter" | "spread_parameter" | "enhanced_for_statement" => {
            if let Some(declared) = node.child_by_field_name("name") {
                if &text[declared.byte_range()] == name {
                    *count += 1;
                }
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        count_local_declarations(&child, text, name, count);
    }
}

/// A type is only considered where the file can see it: its own file, its
/// package, or a file that imports it (exactly or via a package wildcard).
fn type_visible_in(uri: &Url, target: &Target, text: &str, tree: &Tree) -> bool {
    let Some(entry) = target.declarations.first() else {
        return false;
    };
    if entry.uri == *uri {
        return true;
    }
    if types::file_package(tree, text) == target.package {
        return true;
    }
    let Some(package) = &target.package else {
        return false;
    };
    let wildcard = format!("{package}.*");
    let exact = format!("{package}.{}", target.name);
    if types::imports_of(tree, text).iter().any(|import| {
        if import.is_static {
            return false;
        }
        if import.is_wildcard {
            import.path == wildcard
        } else {
            import.path == exact
        }
    }) {
        return true;
    }
    // A fully-qualified use names the type without an import.
    text.contains(&format!("{package}.{}", target.name))
}

fn collect_type_occurrences(
    node: &Node,
    text: &str,
    name: &str,
    uri: &Url,
    out: &mut Vec<Location>,
    seen: &mut HashSet<(String, u32, u32)>,
) {
    if &text[node.byte_range()] == name && !is_declaration_name(node) {
        match node.kind() {
            "type_identifier" => push_location(node, text, uri, out, seen),
            // The last segment of an import path is an `identifier`.
            "identifier" if has_import_ancestor(node) => push_location(node, text, uri, out, seen),
            _ => {}
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_type_occurrences(&child, text, name, uri, out, seen);
    }
}

/// True when `node` is the `name` field of a declaration (a type, method,
/// field, local, parameter, or enhanced-for binding) rather than a use of it,
/// so `include_declaration` alone decides whether it is reported.
fn is_declaration_name(node: &Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if !matches!(
        parent.kind(),
        "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
            | "method_declaration"
            | "constructor_declaration"
            | "variable_declarator"
            | "formal_parameter"
            | "spread_parameter"
            | "enhanced_for_statement"
    ) {
        return false;
    }
    parent
        .child_by_field_name("name")
        .is_some_and(|name| name.id() == node.id())
}

fn has_import_ancestor(node: &Node) -> bool {
    let mut current = node.parent();
    while let Some(node) = current {
        if node.kind() == "import_declaration" {
            return true;
        }
        current = node.parent();
    }
    false
}

/// True when the identifier is the member half of `recv.name` / `recv.name()`.
fn is_member_access_name(node: &Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "field_access" => parent
            .child_by_field_name("field")
            .is_some_and(|field| field.id() == node.id()),
        "method_invocation" => {
            if parent.child_by_field_name("object").is_none() {
                return false;
            }
            parent
                .child_by_field_name("name")
                .is_some_and(|named| named.id() == node.id())
        }
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_member_nodes(
    node: &Node,
    text: &str,
    tree: &Tree,
    target: &Target,
    model: &dyn TypeLookup,
    package: Option<&str>,
    owner_span: Option<(usize, usize)>,
    uri: &Url,
    out: &mut Vec<Location>,
    seen: &mut HashSet<(String, u32, u32)>,
) {
    if node.kind() == "identifier"
        && &text[node.byte_range()] == target.name.as_str()
        && !is_declaration_name(node)
    {
        let include = if is_member_access_name(node) {
            member_access_matches(node, text, tree, target, model, package)
        } else {
            // Unqualified uses count only inside the declaring type's own span.
            owner_span
                .is_some_and(|(start, end)| node.start_byte() >= start && node.end_byte() <= end)
        };
        if include {
            push_location(node, text, uri, out, seen);
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_member_nodes(
            &child, text, tree, target, model, package, owner_span, uri, out, seen,
        );
    }
}

/// Whether `recv.name` resolves to the target's declaring type.
fn member_access_matches(
    node: &Node,
    text: &str,
    tree: &Tree,
    target: &Target,
    model: &dyn TypeLookup,
    package: Option<&str>,
) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    let Some(object) = parent.child_by_field_name("object") else {
        return false;
    };
    let scope = types::scope_at(*node, text, tree, model);
    let receiver = types::receiver_type(&object, text, &scope, model);
    match types::member_owner(&receiver, &target.name, model, package) {
        Some(owner) => {
            owner.package == target.package && Some(owner.name.as_str()) == target.owner.as_deref()
        }
        None => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_local_occurrences(
    node: &Node,
    text: &str,
    tree: &Tree,
    model: &dyn TypeLookup,
    target: &Target,
    uri: &Url,
    out: &mut Vec<Location>,
    seen: &mut HashSet<(String, u32, u32)>,
) {
    let Some((start, end)) = target.local_span else {
        return;
    };
    if node.kind() == "identifier"
        && &text[node.byte_range()] == target.name.as_str()
        && node.start_byte() >= start
        && node.end_byte() <= end
        && !is_member_access_name(node)
        && !is_declaration_name(node)
    {
        // The name must still mean the local here; a field or type of the same
        // name would be resolved first otherwise.
        let scope = types::scope_at(*node, text, tree, model);
        let resolves_to_local = types::resolve_name(&target.name, &scope, model)
            .is_some_and(|resolved| !resolved.is_type && resolved.member.is_none());
        if resolves_to_local {
            push_location(node, text, uri, out, seen);
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_local_occurrences(&child, text, tree, model, target, uri, out, seen);
    }
}

fn push_location(
    node: &Node,
    text: &str,
    uri: &Url,
    out: &mut Vec<Location>,
    seen: &mut HashSet<(String, u32, u32)>,
) {
    let range = lsp_range(text, node);
    let key = (
        uri.as_str().to_string(),
        range.start.line,
        range.start.character,
    );
    if seen.insert(key) {
        out.push(Location {
            uri: uri.clone(),
            range,
        });
    }
}

/// Adds the target's declaration locations, skipping any the search already
/// found.
fn add_declarations(target: &Target, locations: &mut Vec<Location>) {
    let mut add = |location: Location| {
        let already_present = locations
            .iter()
            .any(|existing| existing.uri == location.uri && existing.range == location.range);
        if !already_present {
            locations.push(location);
        }
    };
    if let Some(location) = &target.local_declaration {
        add(location.clone());
    }
    for entry in &target.declarations {
        add(Location {
            uri: entry.uri.clone(),
            range: entry.selection_range,
        });
    }
}

/// A plausible Java identifier: never a keyword or a restricted identifier,
/// with correct first/rest character classes.
fn is_valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    if !chars.all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '$') {
        return false;
    }
    if JAVA_KEYWORDS.contains(&name) {
        return false;
    }
    // Contextual / restricted identifiers a rename must not introduce.
    !matches!(
        name,
        "_" | "var" | "yield" | "record" | "sealed" | "permits"
    )
}

/// A fenced Java code block, the body of every hover we produce.
fn block(code: &str) -> String {
    format!("```java\n{code}\n```")
}

fn is_type_node(kind: &str) -> bool {
    matches!(
        kind,
        "type_identifier"
            | "scoped_type_identifier"
            | "generic_type"
            | "integral_type"
            | "floating_point_type"
            | "boolean_type"
            | "void_type"
            | "array_type"
            | "annotated_type"
    )
}

/// The declared-type model of one open document, to be layered over the
/// workspace model so unsaved edits are what features answer from.
fn local_model(document: &ParsedDocument) -> TypeModel {
    let package = types::file_package(&document.tree, &document.text);
    let mut model = TypeModel::new();
    model.extend(types::collect_type_infos(
        package.as_deref(),
        &document.tree,
        &document.text,
    ));
    model
}

/// The Markdown hover for the node under the cursor, or `None` when the symbol
/// is not resolvable to a single declaration.
fn hover_value(node: Node, text: &str, tree: &Tree, model: &dyn TypeLookup) -> Option<String> {
    let scope = types::scope_at(node, text, tree, model);

    if is_type_node(node.kind()) {
        return type_hover(
            &types::type_from_node(&node, text),
            model,
            scope.package.as_deref(),
        );
    }

    if !matches!(node.kind(), "identifier" | "type_identifier") {
        return None;
    }
    let name = &text[node.byte_range()];
    let parent = node.parent()?;

    match parent.kind() {
        "field_access" => {
            // Only the field itself, not the receiver expression.
            let field = parent.child_by_field_name("field")?;
            if field.id() != node.id() {
                return None;
            }
            let object = parent.child_by_field_name("object")?;
            let receiver = types::receiver_type(&object, text, &scope, model);
            let member = types::member_of(&receiver, name, model, scope.package.as_deref())?;
            return Some(member_hover(&member, &receiver));
        }
        "method_invocation" => {
            let named = parent.child_by_field_name("name")?;
            if named.id() != node.id() {
                return None;
            }
            let receiver = match parent.child_by_field_name("object") {
                Some(object) => types::receiver_type(&object, text, &scope, model),
                None => scope
                    .enclosing_type
                    .clone()
                    .map(Ty::reference)
                    .unwrap_or(Ty::Unknown),
            };
            let member = types::member_of(&receiver, name, model, scope.package.as_deref())?;
            return Some(member_hover(&member, &receiver));
        }
        _ => {}
    }

    if let Some(value) = declaration_hover(node, &parent, text) {
        return Some(value);
    }

    let resolved = types::resolve_name(name, &scope, model)?;
    if resolved.is_type {
        return type_hover(&resolved.ty, model, scope.package.as_deref());
    }
    if let Some(member) = resolved.member {
        let container = scope
            .enclosing_type
            .clone()
            .map(Ty::reference)
            .unwrap_or(Ty::Unknown);
        return Some(member_hover(&member, &container));
    }
    Some(format!(
        "{}\n\nlocal variable `{name}`",
        block(&format!("{} {name}", resolved.ty.display()))
    ))
}

fn type_hover(ty: &Ty, model: &dyn TypeLookup, package: Option<&str>) -> Option<String> {
    if matches!(ty, Ty::Unknown) {
        return None;
    }
    let mut value = block(&ty.display());
    if let Some(info) = model.lookup(ty, package) {
        value = block(&info.display());
        if let Some(pkg) = &info.package {
            value.push_str(&format!("\n\nPackage `{pkg}`"));
        }
    }
    Some(value)
}

fn member_hover(member: &Member, container: &Ty) -> String {
    let kind = if member.kind == IndexKind::Method {
        "method"
    } else {
        "field"
    };
    let mut value = block(&member.signature());
    if matches!(container, Ty::Unknown) {
        value.push_str(&format!("\n\n{kind}"));
    } else {
        value.push_str(&format!("\n\n{kind} of `{}`", container.display()));
    }
    value
}

/// Renders a declaration name as written in the open file. `None` when the
/// cursor is not on a declaration.
fn declaration_hover(node: Node, parent: &Node, text: &str) -> Option<String> {
    let name = &text[node.byte_range()];
    let value = match parent.kind() {
        "class_declaration" => block(&declaration_text("class", name, parent, text)),
        "interface_declaration" | "annotation_type_declaration" => {
            block(&declaration_text("interface", name, parent, text))
        }
        "enum_declaration" => block(&declaration_text("enum", name, parent, text)),
        "record_declaration" => block(&declaration_text("record", name, parent, text)),
        "method_declaration" => {
            let ret = parent
                .child_by_field_name("type")
                .map(|node| text[node.byte_range()].to_string())
                .unwrap_or_default();
            let params = parent
                .child_by_field_name("parameters")
                .map(|node| text[node.byte_range()].to_string())
                .unwrap_or_else(|| "()".to_string());
            let static_prefix = if node_has_modifier(parent, text, "static") {
                "static "
            } else {
                ""
            };
            block(&format!("{static_prefix}{ret} {name}{params}"))
        }
        "constructor_declaration" => {
            let params = parent
                .child_by_field_name("parameters")
                .map(|node| text[node.byte_range()].to_string())
                .unwrap_or_else(|| "()".to_string());
            block(&format!("{name}{params}"))
        }
        "variable_declarator" => {
            let ty = parent.parent()?.child_by_field_name("type")?;
            block(&format!("{} {name}", &text[ty.byte_range()]))
        }
        "formal_parameter" | "spread_parameter" | "enhanced_for_statement" => {
            let ty = parent.child_by_field_name("type")?;
            block(&format!("{} {name}", &text[ty.byte_range()]))
        }
        "enum_constant" => block(name),
        _ => return None,
    };
    Some(value)
}

fn declaration_text(keyword: &str, name: &str, parent: &Node, text: &str) -> String {
    let parameters = parent
        .child_by_field_name("type_parameters")
        .map(|node| text[node.byte_range()].to_string())
        .unwrap_or_default();
    format!("{keyword} {name}{parameters}")
}

fn node_has_modifier(node: &Node, text: &str, modifier: &str) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifiers" {
            return text[child.byte_range()]
                .split_whitespace()
                .any(|word| word == modifier);
        }
    }
    false
}

/// The expression a `.` at `offset` is applied to, if any.
fn receiver_before_dot<'a>(tree: &'a Tree, offset: usize) -> Option<Node<'a>> {
    if offset == 0 {
        return None;
    }
    let dot = tree
        .root_node()
        .descendant_for_byte_range(offset - 1, offset - 1)?;
    let mut current = Some(dot);
    while let Some(node) = current {
        if matches!(node.kind(), "field_access" | "method_invocation") {
            return node.child_by_field_name("object");
        }
        current = node.parent();
    }
    None
}

// ---------------------------------------------------------------------------
// Inlay hints
// ---------------------------------------------------------------------------

/// Walks the tree, pruning every subtree that does not intersect
/// `[start, end)`, and collects an inlay hint for each node in the requested
/// range that type information can describe: variable types (including `var`
/// inference), parameter names at call sites, and the return types of
/// intermediate links in method chains.
fn collect_inlay_hints(
    node: Node,
    text: &str,
    tree: &Tree,
    model: &dyn TypeLookup,
    start: usize,
    end: usize,
    out: &mut Vec<InlayHint>,
) {
    if node.end_byte() < start || node.start_byte() > end {
        return;
    }
    match node.kind() {
        "local_variable_declaration" => variable_hints(&node, text, tree, model, true, out),
        "field_declaration" | "constant_declaration" => {
            variable_hints(&node, text, tree, model, false, out)
        }
        "enhanced_for_statement" => enhanced_for_hint(&node, text, out),
        "method_invocation" => {
            parameter_hints(&node, text, tree, model, out);
            chain_hint(&node, text, tree, model, out);
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_inlay_hints(child, text, tree, model, start, end, out);
    }
}

/// Variable type hints for a declaration node's declarators: the declared type,
/// or — when `may_infer` and the declaration is written with `var` — the type
/// inferred from the initializer. An uninferrable initializer yields no hint.
fn variable_hints(
    node: &Node,
    text: &str,
    tree: &Tree,
    model: &dyn TypeLookup,
    may_infer: bool,
    out: &mut Vec<InlayHint>,
) {
    let Some(type_node) = node.child_by_field_name("type") else {
        return;
    };
    let is_var = &text[type_node.byte_range()] == "var";
    if is_var && !may_infer {
        return;
    }
    let declared = types::type_from_node(&type_node, text);
    let mut cursor = node.walk();
    for declarator in node.named_children(&mut cursor) {
        if declarator.kind() != "variable_declarator" {
            continue;
        }
        let Some(name) = declarator.child_by_field_name("name") else {
            continue;
        };
        let ty = if is_var {
            let Some(value) = declarator.child_by_field_name("value") else {
                continue;
            };
            let scope = types::scope_at(declarator, text, tree, model);
            types::receiver_type(&value, text, &scope, model)
        } else {
            declared.clone()
        };
        push_type_hint(name.end_byte(), &ty, text, out);
    }
}

/// The type of an enhanced-for binding. A `var` binding would need the
/// iterable's element type, which is not inferred here, so it yields no hint.
fn enhanced_for_hint(node: &Node, text: &str, out: &mut Vec<InlayHint>) {
    let Some(type_node) = node.child_by_field_name("type") else {
        return;
    };
    if &text[type_node.byte_range()] == "var" {
        return;
    }
    let Some(name) = node.child_by_field_name("name") else {
        return;
    };
    push_type_hint(
        name.end_byte(),
        &types::type_from_node(&type_node, text),
        text,
        out,
    );
}

/// Parameter-name hints at a call site: the callee is resolved through the
/// receiver type (or the enclosing type for an unqualified call) and each
/// argument is annotated with its parameter's name. Parameters without a known
/// name — every jar/JDK member read from a class file — contribute nothing.
fn parameter_hints(
    node: &Node,
    text: &str,
    tree: &Tree,
    model: &dyn TypeLookup,
    out: &mut Vec<InlayHint>,
) {
    let Some(name) = node.child_by_field_name("name") else {
        return;
    };
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return;
    };
    let mut cursor = arguments.walk();
    let call_arguments: Vec<Node> = arguments
        .named_children(&mut cursor)
        .filter(|child| !is_comment_node(child.kind()))
        .collect();
    let scope = types::scope_at(*node, text, tree, model);
    let receiver = match node.child_by_field_name("object") {
        Some(object) => types::receiver_type(&object, text, &scope, model),
        None => scope
            .enclosing_type
            .clone()
            .map(Ty::reference)
            .unwrap_or(Ty::Unknown),
    };
    // Match the overload by arity, so the names shown belong to the call's
    // actual shape (`List.of(3)` is `of(E e1)`, not the parameterless overload).
    let Some(member) = types::member_for_call(
        &receiver,
        &text[name.byte_range()],
        call_arguments.len(),
        model,
        scope.package.as_deref(),
    ) else {
        return;
    };
    // Only the arity-matched overload's names are trustworthy; a call whose
    // count matches no overload yields no hint rather than a wrong name.
    if member.params.len() != call_arguments.len() {
        return;
    }
    for (index, argument) in call_arguments.iter().enumerate() {
        let Some(param) = member.params.get(index) else {
            break;
        };
        let Some(param_name) = &param.name else {
            continue;
        };
        out.push(InlayHint {
            position: lsp_position(text, argument.start_byte()),
            label: InlayHintLabel::String(format!("{param_name}:")),
            kind: Some(InlayHintKind::PARAMETER),
            tooltip: None,
            text_edits: None,
            padding_left: None,
            padding_right: Some(true),
            data: None,
        });
    }
}

/// The return type of each intermediate link in a chain: an invocation whose
/// result is immediately dereferenced. The outermost call and a standalone call
/// carry no such hint.
fn chain_hint(
    node: &Node,
    text: &str,
    tree: &Tree,
    model: &dyn TypeLookup,
    out: &mut Vec<InlayHint>,
) {
    if !is_intermediate_link(node) {
        return;
    }
    let scope = types::scope_at(*node, text, tree, model);
    let ty = types::receiver_type(node, text, &scope, model);
    push_type_hint(node.end_byte(), &ty, text, out);
}

/// True when the invocation is the receiver of a further `.field` or
/// `.method()` — an intermediate link of a chain rather than its outermost call.
fn is_intermediate_link(node: &Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if !matches!(parent.kind(), "field_access" | "method_invocation") {
        return false;
    }
    parent
        .child_by_field_name("object")
        .is_some_and(|object| object.id() == node.id())
}

fn is_comment_node(kind: &str) -> bool {
    matches!(kind, "line_comment" | "block_comment")
}

/// A `: Type` hint after a name or expression, unless the type is unresolved.
fn push_type_hint(end_byte: usize, ty: &Ty, text: &str, out: &mut Vec<InlayHint>) {
    let rendered = ty.display();
    if matches!(ty, Ty::Unknown) || rendered.is_empty() {
        return;
    }
    out.push(InlayHint {
        position: lsp_position(text, end_byte),
        label: InlayHintLabel::String(format!(": {rendered}")),
        kind: Some(InlayHintKind::TYPE),
        tooltip: None,
        text_edits: None,
        padding_left: None,
        padding_right: None,
        data: None,
    });
}

/// True when `position` lies within `range`, compared as (line, character).
fn position_in_range(position: Position, range: Range) -> bool {
    (range.start.line, range.start.character) <= (position.line, position.character)
        && (position.line, position.character) <= (range.end.line, range.end.character)
}

/// The label text of a hint, for de-duplication.
fn hint_label(hint: &InlayHint) -> &str {
    match &hint.label {
        InlayHintLabel::String(value) => value,
        InlayHintLabel::LabelParts(_) => "",
    }
}

/// Conservative semantic diagnostics for a document: type names that resolve
/// nowhere. Gated on the model actually vouching for `java.lang`, and on the
/// file parsing cleanly, so a missing JDK or a broken file never produces a
/// wall of false positives.
fn semantic_diagnostics(document: &ParsedDocument, index: &WorkspaceIndex) -> Vec<Diagnostic> {
    let Some(workspace) = index.type_model() else {
        return Vec::new();
    };
    if !workspace.contains("Object") || !workspace.contains("String") {
        return Vec::new();
    }
    let local = local_model(document);
    let query = TypeQuery::new(&workspace, &local);
    let mut out = Vec::new();
    visit_type_positions(
        document.tree.root_node(),
        &document.text,
        &document.tree,
        &query,
        &mut out,
    );
    out
}

fn visit_type_positions(
    node: Node,
    text: &str,
    tree: &Tree,
    model: &dyn TypeLookup,
    out: &mut Vec<Diagnostic>,
) {
    for type_node in checked_type_nodes(node) {
        check_type(type_node, text, tree, model, out);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_type_positions(child, text, tree, model, out);
    }
}

/// The type nodes of a construct that declares or constructs a type.
fn checked_type_nodes(node: Node) -> Vec<Node> {
    match node.kind() {
        "object_creation_expression"
        | "cast_expression"
        | "field_declaration"
        | "constant_declaration"
        | "local_variable_declaration"
        | "formal_parameter"
        | "spread_parameter"
        | "method_declaration"
        | "enhanced_for_statement" => node.child_by_field_name("type").into_iter().collect(),
        "superclass" => node.named_child(0).into_iter().collect(),
        // An `implements`/`extends` clause wraps a `type_list`; checking the
        // list's members here is enough, so the enclosing clause is left to the
        // generic recursion rather than expanded a second time.
        "type_list" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor).collect()
        }
        _ => Vec::new(),
    }
}

/// Flags a simple, unqualified type name that neither an import, a type
/// parameter, nor the model accounts for. Qualified names, generic arguments,
/// and `var` are deliberately left alone rather than guessed at.
fn check_type(
    node: Node,
    text: &str,
    tree: &Tree,
    model: &dyn TypeLookup,
    out: &mut Vec<Diagnostic>,
) {
    if node.kind() != "type_identifier" {
        return;
    }
    let name = &text[node.byte_range()];
    if name.is_empty() || name == "var" {
        return;
    }
    let scope = types::scope_at(node, text, tree, model);
    if scope.type_params.iter().any(|param| param == name) {
        return;
    }
    if scope
        .imports
        .iter()
        .any(|import| import.simple_name() == Some(name))
    {
        return;
    }
    if model.contains(name) {
        return;
    }
    out.push(Diagnostic {
        range: lsp_range(text, &node),
        severity: Some(DiagnosticSeverity::WARNING),
        source: Some("java-lsp".to_string()),
        message: format!("type `{name}` cannot be resolved"),
        ..Default::default()
    });
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

/// The fully-qualified owner of an entry: its package plus its container chain.
fn qualified_owner(entry: &SymbolEntry) -> String {
    let package = entry
        .package
        .clone()
        .unwrap_or_else(|| "the default package".to_string());
    if entry.container.is_empty() {
        package
    } else {
        format!("{package}.{}", entry.container.join("."))
    }
}

/// An entry's fully-qualified identity: what distinguishes two symbols that
/// share a simple name.
fn symbol_identity(entry: &SymbolEntry) -> String {
    format!("{}.{}", qualified_owner(entry), entry.name)
}

/// The simple names claimed by more than one distinct symbol, so completions
/// can label each such item with its owner.
fn ambiguous_names(entries: &[SymbolEntry]) -> HashSet<String> {
    let mut owners: HashMap<String, HashSet<String>> = HashMap::new();
    for entry in entries {
        if entry.kind == IndexKind::Import {
            continue;
        }
        owners
            .entry(entry.name.clone())
            .or_default()
            .insert(symbol_identity(entry));
    }
    owners
        .into_iter()
        .filter(|(_, identities)| identities.len() > 1)
        .map(|(name, _)| name)
        .collect()
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
    key: String,
    label: String,
    insert_text: String,
    kind: CompletionItemKind,
    detail: Option<String>,
    sort_text: String,
    additional_edits: Vec<TextEdit>,
) {
    if !seen.insert(key) {
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
    fn completions_keep_same_named_types_from_different_packages_separate() {
        let open_text = "package com.a;\n\nclass Sample {\n    void m() {\n        Li;\n    }\n}\n";
        let engine = TreeSitterEngine::new();
        engine.open(&uri(), open_text);
        engine.open(
            &Url::parse("file:///src/java/util/List.java").unwrap(),
            "package java.util;\n\npublic interface List {}\n",
        );
        engine.open(
            &Url::parse("file:///src/java/awt/List.java").unwrap(),
            "package java.awt;\n\npublic class List {}\n",
        );

        let offset = open_text.find("Li;").unwrap() + "Li".len();
        let Some(CompletionResponse::Array(items)) =
            engine.completions(&uri(), lsp_position(open_text, offset))
        else {
            panic!("completions must return an array");
        };

        // Both `List` types are offered, each with its own import, and each
        // labeled with the package it comes from.
        let lists: Vec<&CompletionItem> = items
            .iter()
            .filter(|item| item.insert_text.as_deref() == Some("List"))
            .collect();
        assert_eq!(lists.len(), 2, "one item per package");
        let details: Vec<&str> = lists
            .iter()
            .filter_map(|item| item.detail.as_deref())
            .collect();
        assert!(details.contains(&"interface of java.util"), "{details:?}");
        assert!(details.contains(&"class of java.awt"), "{details:?}");
        let imports: Vec<String> = lists
            .iter()
            .filter_map(|item| {
                item.additional_text_edits
                    .as_ref()
                    .map(|edits| edits[0].new_text.clone())
            })
            .collect();
        assert!(
            imports.contains(&"import java.util.List;\n".to_string()),
            "{imports:?}"
        );
        assert!(
            imports.contains(&"import java.awt.List;\n".to_string()),
            "{imports:?}"
        );
    }

    #[test]
    fn completions_after_a_dot_offer_members_or_an_empty_list() {
        let text = "\
class Sample {
    String greeting = null;
    void m() {
        int n = greeting.length;
    }
}
";
        let engine = engine_with(text);

        // An uninferrable receiver (`String` is not in this workspace model)
        // yields an empty list, never a guess at its membership.
        let after_dot_word = text.find("length;").unwrap() + "length".len();
        match engine.completions(&uri(), lsp_position(text, after_dot_word)) {
            Some(CompletionResponse::Array(items)) => {
                assert!(
                    items.is_empty(),
                    "no membership claims for an unknown receiver, got {items:?}"
                );
            }
            other => panic!("expected an array after `.`, got {other:?}"),
        }

        // Directly after the dot nothing is typed: still an array (empty here).
        let just_after_dot = text.find(".length;").unwrap() + 1;
        match engine.completions(&uri(), lsp_position(text, just_after_dot)) {
            Some(CompletionResponse::Array(items)) => assert!(items.is_empty()),
            other => panic!("expected an array after `.`, got {other:?}"),
        }

        // And from an empty prefix in general.
        let at_word_start = text.find("int n").unwrap();
        assert!(engine
            .completions(&uri(), lsp_position(text, at_word_start))
            .is_none());
    }

    #[test]
    fn member_completions_follow_the_receivers_inferred_type() {
        let text = "\
class Base {
    void inherited() {}
}
class Widget extends Base {
    int size;
    void run() {}
}
class Use {
    void m() {
        Widget w = null;
        w.run();
    }
}
";
        let engine = engine_with(text);
        let offset = text.find("w.run();").unwrap() + "w.".len();
        match engine.completions(&uri(), lsp_position(text, offset)) {
            Some(CompletionResponse::Array(items)) => {
                let names: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
                assert!(names.contains(&"size"), "{names:?}");
                assert!(names.contains(&"run"), "{names:?}");
                assert!(
                    names.contains(&"inherited"),
                    "inherited member missing: {names:?}"
                );
            }
            other => panic!("expected member items, got {other:?}"),
        }
    }

    #[test]
    fn hover_resolves_a_member_through_the_receiver_type() {
        let text = "\
class Widget {
    int size;
    int getSize(int extra) { return size; }
}
class Use {
    void m() {
        Widget w = null;
        w.getSize(1);
    }
}
";
        let engine = engine_with(text);
        let offset = text.find("getSize(1)").unwrap() + 2;
        let hover = engine
            .hover(&uri(), lsp_position(text, offset))
            .expect("hover");
        match hover.contents {
            HoverContents::Markup(markup) => {
                assert!(
                    markup.value.contains("int getSize(int extra)"),
                    "{}",
                    markup.value
                );
                assert!(markup.value.contains("of `Widget`"), "{}", markup.value);
            }
            other => panic!("expected markup, got {other:?}"),
        }
    }

    #[test]
    fn hover_on_a_local_shows_its_declared_type() {
        let text = "\
class Widget {
    void m() {
        Widget local = null;
    }
}
";
        let engine = engine_with(text);
        let offset = text.find("local = null").unwrap() + 2;
        let hover = engine
            .hover(&uri(), lsp_position(text, offset))
            .expect("hover");
        match hover.contents {
            HoverContents::Markup(markup) => {
                assert!(markup.value.contains("Widget local"), "{}", markup.value)
            }
            other => panic!("expected markup, got {other:?}"),
        }
    }

    #[test]
    fn unresolved_type_names_are_diagnosed_but_known_ones_are_not() {
        let engine = TreeSitterEngine::new();
        engine
            .index
            .set_types(std::sync::Arc::new(TypeModel::from_entries(&[
                synthetic_entry("Object", IndexKind::Class),
                synthetic_entry("String", IndexKind::Class),
            ])));
        let text = "\
class Sample {
    Missing field;
    String name;
}
";
        engine.open(&uri(), text);
        let diagnostics = engine.diagnostics(&uri());
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].message.contains("Missing"));
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::WARNING));
    }

    #[test]
    fn imported_and_wildcard_types_are_never_diagnosed() {
        let engine = TreeSitterEngine::new();
        engine
            .index
            .set_types(std::sync::Arc::new(TypeModel::from_entries(&[
                synthetic_entry("Object", IndexKind::Class),
                synthetic_entry("String", IndexKind::Class),
            ])));
        let text = "\
import com.external.Thing;
import java.util.*;

class Sample {
    Thing thing;
    List<String> names;
    String name;
}
";
        engine.open(&uri(), text);
        assert!(engine.diagnostics(&uri()).is_empty());
    }

    #[test]
    fn hover_on_a_type_declaration_name_is_answered() {
        let text =
            "package bench;\n\npublic class BenchClass00000 {\n    private int field00 = 0;\n}\n";
        let engine = engine_with(text);
        let start = text.find("BenchClass00000").unwrap();
        for offset in [start, start + 3] {
            let position = lsp_position(text, offset);
            let hover = engine.hover(&uri(), position);
            assert!(hover.is_some(), "no hover at byte {offset} ({position:?})");
        }
    }

    fn temp_workspace(name: &str, files: &[(&str, &str)]) -> (std::path::PathBuf, Url) {
        let root = std::env::temp_dir().join(format!(
            "java-lsp-refs-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for (relative, text) in files {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
        }
        let uri = Url::from_file_path(&root).unwrap();
        (root, uri)
    }

    /// An engine with a scanned workspace; `#[test]` has no tokio runtime, so
    /// `set_workspace_root` runs the scan inline.
    fn scanned_engine(root: &Url) -> TreeSitterEngine {
        let engine = TreeSitterEngine::new();
        engine.set_workspace_root(root);
        engine
    }

    fn file_uris(locations: &[Location]) -> Vec<String> {
        let mut uris: Vec<String> = locations
            .iter()
            .map(|location| location.uri.path().to_string())
            .collect();
        uris.sort();
        uris.dedup();
        uris
    }

    #[test]
    fn type_references_stay_within_files_that_can_see_the_type() {
        let _env = crate::jdk::env_lock();
        std::env::set_var("JAVA_LSP_JDK", "/definitely/not/a/jdk");
        let (root, root_uri) = temp_workspace(
            "type-refs",
            &[
                ("a/Widget.java", "package a;\n\npublic class Widget {\n    public void run() {}\n}\n"),
                (
                    "a/Use.java",
                    "package a;\n\npublic class Use {\n    void m() {\n        Widget w = null;\n        w.run();\n    }\n}\n",
                ),
                ("b/Widget.java", "package b;\n\npublic class Widget {\n}\n"),
                (
                    "c/Other.java",
                    "package c;\n\nimport a.Widget;\n\npublic class Other {\n    Widget field;\n}\n",
                ),
            ],
        );
        let engine = scanned_engine(&root_uri);
        let use_text = std::fs::read_to_string(root.join("a/Use.java")).unwrap();
        let use_uri = Url::from_file_path(root.join("a/Use.java")).unwrap();
        engine.open(&use_uri, &use_text);
        let cursor = lsp_position(&use_text, use_text.find("Widget w").unwrap() + 1);

        let references = engine.references(&use_uri, cursor, true);
        let uris = file_uris(&references);
        assert!(
            uris.iter().any(|path| path.ends_with("a/Widget.java")),
            "{uris:?}"
        );
        assert!(
            uris.iter().any(|path| path.ends_with("a/Use.java")),
            "{uris:?}"
        );
        assert!(
            uris.iter().any(|path| path.ends_with("c/Other.java")),
            "{uris:?}"
        );
        assert!(
            !uris.iter().any(|path| path.ends_with("b/Widget.java")),
            "a same-named type in another package must not be touched: {uris:?}"
        );

        let edit = engine.rename(&use_uri, cursor, "Gadget").expect("rename");
        let changes = edit.changes.expect("changes");
        assert_eq!(
            changes.len(),
            3,
            "files: {:?}",
            changes.keys().collect::<Vec<_>>()
        );
        assert!(changes
            .keys()
            .all(|uri| !uri.path().ends_with("b/Widget.java")));
        assert!(changes
            .values()
            .all(|edits| edits.iter().all(|edit| edit.new_text == "Gadget")));

        // A keyword is not a rename target.
        assert!(engine.rename(&use_uri, cursor, "class").is_none());
    }

    #[test]
    fn member_references_follow_the_receivers_type() {
        let _env = crate::jdk::env_lock();
        std::env::set_var("JAVA_LSP_JDK", "/definitely/not/a/jdk");
        let (root, root_uri) = temp_workspace(
            "member-refs",
            &[
                ("a/Widget.java", "package a;\n\npublic class Widget {\n    public void run() {}\n}\n"),
                (
                    "a/Use.java",
                    "package a;\n\npublic class Use {\n    void m() {\n        Widget w = null;\n        w.run();\n    }\n}\n",
                ),
                (
                    "b/Thing.java",
                    "package b;\n\npublic class Thing {\n    public void run() {}\n    void n() {\n        run();\n    }\n}\n",
                ),
            ],
        );
        let engine = scanned_engine(&root_uri);
        let use_text = std::fs::read_to_string(root.join("a/Use.java")).unwrap();
        let use_uri = Url::from_file_path(root.join("a/Use.java")).unwrap();
        engine.open(&use_uri, &use_text);
        let cursor = lsp_position(&use_text, use_text.find("w.run()").unwrap() + 2);

        let references = engine.references(&use_uri, cursor, true);
        let uris = file_uris(&references);
        assert!(
            uris.iter().any(|path| path.ends_with("a/Widget.java")),
            "{uris:?}"
        );
        assert!(
            uris.iter().any(|path| path.ends_with("a/Use.java")),
            "{uris:?}"
        );
        assert!(
            !uris.iter().any(|path| path.ends_with("b/Thing.java")),
            "an unrelated type's same-named member must not be touched: {uris:?}"
        );

        let edit = engine.rename(&use_uri, cursor, "execute").expect("rename");
        let changes = edit.changes.expect("changes");
        assert_eq!(
            changes.len(),
            2,
            "files: {:?}",
            changes.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn local_references_stay_within_the_method() {
        let _env = crate::jdk::env_lock();
        std::env::set_var("JAVA_LSP_JDK", "/definitely/not/a/jdk");
        let source = "package a;\n\npublic class Use {\n    void m() {\n        int total = 1;\n        int x = total + total;\n    }\n}\n";
        let (root, root_uri) = temp_workspace("local-refs", &[("a/Use.java", source)]);
        let engine = scanned_engine(&root_uri);
        let uri = Url::from_file_path(root.join("a/Use.java")).unwrap();
        engine.open(&uri, source);
        let cursor = lsp_position(source, source.find("total = 1").unwrap() + 2);

        let references = engine.references(&uri, cursor, true);
        assert_eq!(references.len(), 3, "{references:?}");

        let edit = engine.rename(&uri, cursor, "sum").expect("rename");
        let changes = edit.changes.expect("changes");
        assert_eq!(changes.len(), 1);
        let edits = changes.values().next().unwrap();
        assert_eq!(edits.len(), 3, "{edits:?}");
        assert!(edits.iter().all(|edit| edit.new_text == "sum"));
    }

    #[test]
    fn two_locals_of_one_name_refuse_rename() {
        let _env = crate::jdk::env_lock();
        std::env::set_var("JAVA_LSP_JDK", "/definitely/not/a/jdk");
        let source = "package a;\n\npublic class Use {\n    void m() {\n        { int total = 1; }\n        { int total = 2; }\n    }\n}\n";
        let (root, root_uri) = temp_workspace("ambiguous-local", &[("a/Use.java", source)]);
        let engine = scanned_engine(&root_uri);
        let uri = Url::from_file_path(root.join("a/Use.java")).unwrap();
        engine.open(&uri, source);
        let cursor = lsp_position(source, source.find("total = 1").unwrap() + 2);

        assert!(engine.references(&uri, cursor, true).is_empty());
        assert!(engine.rename(&uri, cursor, "sum").is_none());
    }

    fn synthetic_entry(name: &str, kind: IndexKind) -> SymbolEntry {
        let zero = Range::new(Position::new(0, 0), Position::new(0, 0));
        SymbolEntry {
            uri: Url::parse("file:///jdk.jar").unwrap(),
            name: name.to_string(),
            kind,
            package: None,
            container: Vec::new(),
            full_range: zero,
            selection_range: zero,
            dependency: true,
        }
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

    const HINT_SAMPLE: &str = "\
package demo;

class Widget {
    private int size;

    int getSize() {
        return size;
    }

    Widget self() {
        return this;
    }

    void run(int amount, String label) {
        var w = new Widget();
        int n = 1;
        int total = w.self().getSize();
        w.run(2, \"x\");
    }
}
";

    fn full_range(text: &str) -> Range {
        Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: lsp_position(text, text.len()),
        }
    }

    /// The position just past the first occurrence of `needle`.
    fn after(text: &str, needle: &str) -> Position {
        let offset = text.find(needle).expect("needle must be present");
        lsp_position(text, offset + needle.len())
    }

    fn hints_in(engine: &TreeSitterEngine, range: Range) -> Vec<(Position, String)> {
        engine
            .inlay_hints(&uri(), range)
            .into_iter()
            .map(|hint| (hint.position, hint_label(&hint).to_string()))
            .collect()
    }

    #[test]
    fn variable_hints_resolve_declared_and_inferred_types() {
        let engine = engine_with(HINT_SAMPLE);
        let hints = hints_in(&engine, full_range(HINT_SAMPLE));
        let at = |needle: &str| {
            let position = after(HINT_SAMPLE, needle);
            hints
                .iter()
                .find(|(position_at, _)| *position_at == position)
                .map(|(_, label)| label.clone())
        };

        // `var` infers the initializer's type; explicit types are restated.
        assert_eq!(at("var w").as_deref(), Some(": Widget"), "{hints:?}");
        assert_eq!(at("int n").as_deref(), Some(": int"), "{hints:?}");
        assert_eq!(at("int total").as_deref(), Some(": int"), "{hints:?}");
        // Fields are hinted too.
        assert_eq!(
            at("private int size").as_deref(),
            Some(": int"),
            "{hints:?}"
        );
    }

    #[test]
    fn an_unresolved_initializer_yields_no_variable_hint() {
        let text = "class Sample {\n    void m() {\n        var x = mystery();\n    }\n}\n";
        let engine = engine_with(text);
        let hints = hints_in(&engine, full_range(text));
        assert!(hints.is_empty(), "{hints:?}");
    }

    #[test]
    fn enhanced_for_binding_gets_a_type_hint() {
        let text = "class Sample {\n    void m(java.util.List<String> xs) {\n        for (String s : xs) {\n            s.length();\n        }\n    }\n}\n";
        let engine = engine_with(text);
        let hints = hints_in(&engine, full_range(text));
        assert!(
            hints.contains(&(after(text, "String s"), ": String".to_string())),
            "{hints:?}"
        );
    }

    #[test]
    fn parameter_hints_name_call_arguments() {
        let engine = engine_with(HINT_SAMPLE);
        let hints = hints_in(&engine, full_range(HINT_SAMPLE));
        let into = |needle: &str, offset: usize| {
            let start = HINT_SAMPLE.find(needle).expect("needle must be present");
            lsp_position(HINT_SAMPLE, start + offset)
        };

        assert!(
            hints.contains(&(into("w.run(2, \"x\")", 6), "amount:".to_string())),
            "{hints:?}"
        );
        assert!(
            hints.contains(&(into("w.run(2, \"x\")", 9), "label:".to_string())),
            "{hints:?}"
        );
    }

    #[test]
    fn parameter_hints_need_a_resolved_callee() {
        // `String` is not in the model (no indexed JDK), so the callee cannot be
        // resolved and no hint is claimed.
        let text = "class Sample {\n    void m(String s) {\n        s.substring(1);\n    }\n}\n";
        let engine = engine_with(text);
        let hints = hints_in(&engine, full_range(text));
        assert!(hints.is_empty(), "{hints:?}");
    }

    #[test]
    fn chained_call_hints_only_intermediate_links() {
        let engine = engine_with(HINT_SAMPLE);
        let hints = hints_in(&engine, full_range(HINT_SAMPLE));

        // `w.self()` is dereferenced by `.getSize()`: an intermediate link.
        assert!(
            hints.contains(&(after(HINT_SAMPLE, "w.self()"), ": Widget".to_string())),
            "{hints:?}"
        );
        // The outermost call is not annotated.
        let outer = after(HINT_SAMPLE, "w.self().getSize()");
        assert!(
            !hints.iter().any(|(position, _)| *position == outer),
            "{hints:?}"
        );
    }

    #[test]
    fn hints_are_scoped_to_the_requested_range() {
        let engine = engine_with(HINT_SAMPLE);
        let call = "        w.run(2, \"x\");\n";
        let start = HINT_SAMPLE.find(call).expect("call line");
        let range = Range {
            start: lsp_position(HINT_SAMPLE, start),
            end: lsp_position(HINT_SAMPLE, start + call.len()),
        };
        let hints = hints_in(&engine, range);
        let labels: Vec<&str> = hints.iter().map(|(_, label)| label.as_str()).collect();
        assert_eq!(labels, vec!["amount:", "label:"], "{hints:?}");
    }

    #[test]
    fn parameter_hints_need_a_matching_arity() {
        let text = r#"class Sample {
    void run(int amount, String label) {}
    void m() {
        run(1, "x", true);
    }
}
"#;
        let engine = engine_with(text);
        let hints = hints_in(&engine, full_range(text));
        assert!(hints.is_empty(), "{hints:?}");
    }

    #[test]
    fn a_hint_is_not_emitted_outside_the_requested_range() {
        let text = r#"class Sample {
    void run(int amount, String label) {}
    void m() {
        run(
            1,
            "x");
    }
}
"#;
        let engine = engine_with(text);
        let start = text.find("run(\n").expect("call");
        let end = text.find("1,").expect("first argument") + 1;
        let range = Range {
            start: lsp_position(text, start),
            end: lsp_position(text, end),
        };
        let hints = hints_in(&engine, range);
        let labels: Vec<&str> = hints.iter().map(|(_, label)| label.as_str()).collect();
        assert_eq!(labels, vec!["amount:"], "{hints:?}");
    }

    #[test]
    fn identifier_validation_rejects_restricted_names() {
        assert!(is_valid_identifier("total"));
        assert!(is_valid_identifier("_x$1"));
        assert!(!is_valid_identifier("class"));
        assert!(!is_valid_identifier("var"));
        assert!(!is_valid_identifier("record"));
        assert!(!is_valid_identifier("yield"));
        assert!(!is_valid_identifier("1abc"));
        assert!(!is_valid_identifier(""));
    }

    #[test]
    fn an_unresolved_interface_is_reported_once() {
        let engine = TreeSitterEngine::new();
        engine
            .index
            .set_types(std::sync::Arc::new(TypeModel::from_entries(&[
                synthetic_entry("Object", IndexKind::Class),
                synthetic_entry("String", IndexKind::Class),
            ])));
        let text = "class Sample implements Missing, Also {\n}\n";
        engine.open(&uri(), text);
        let diagnostics = engine.diagnostics(&uri());
        assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
        assert_eq!(
            diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.message.contains("Missing"))
                .count(),
            1,
            "{diagnostics:?}"
        );
    }

    #[test]
    fn member_references_do_not_cross_packages() {
        let _env = crate::jdk::env_lock();
        std::env::set_var("JAVA_LSP_JDK", "/definitely/not/a/jdk");
        let a_widget = r#"package a;

public class Widget {
    public void run() {}
    void n() {
        Widget w = null;
        w.run();
    }
}
"#;
        let b_widget = r#"package b;

public class Widget {
    public void run() {}
    void n() {
        Widget w = null;
        w.run();
    }
}
"#;
        let use_text = r#"package a;

public class Use {
    void m() {
        Widget w = null;
        w.run();
    }
}
"#;
        let (root, root_uri) = temp_workspace(
            "member-packages",
            &[
                ("a/Widget.java", a_widget),
                ("b/Widget.java", b_widget),
                ("a/Use.java", use_text),
            ],
        );
        let engine = scanned_engine(&root_uri);
        let use_uri = Url::from_file_path(root.join("a/Use.java")).unwrap();
        engine.open(&use_uri, use_text);
        let cursor = lsp_position(use_text, use_text.find("w.run()").unwrap() + 2);

        let references = engine.references(&use_uri, cursor, true);
        let uris = file_uris(&references);
        assert!(
            uris.iter().any(|path| path.ends_with("a/Widget.java")),
            "{uris:?}"
        );
        assert!(
            !uris.iter().any(|path| path.ends_with("b/Widget.java")),
            "a same-named type in another package must not be touched: {uris:?}"
        );

        let edit = engine.rename(&use_uri, cursor, "execute").expect("rename");
        let changes = edit.changes.expect("changes");
        assert!(
            changes
                .keys()
                .all(|uri| !uri.path().ends_with("b/Widget.java")),
            "files: {:?}",
            changes.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn references_can_exclude_the_declaration() {
        let _env = crate::jdk::env_lock();
        std::env::set_var("JAVA_LSP_JDK", "/definitely/not/a/jdk");
        let widget = "package a;\n\npublic class Widget {\n}\n";
        let use_text =
            "package a;\n\npublic class Use {\n    void m() {\n        Widget w = null;\n    }\n}\n";
        let (root, root_uri) = temp_workspace(
            "include-decl",
            &[("a/Widget.java", widget), ("a/Use.java", use_text)],
        );
        let engine = scanned_engine(&root_uri);
        let use_uri = Url::from_file_path(root.join("a/Use.java")).unwrap();
        engine.open(&use_uri, use_text);
        let cursor = lsp_position(use_text, use_text.find("Widget w").unwrap() + 1);

        let with = engine.references(&use_uri, cursor, true);
        let without = engine.references(&use_uri, cursor, false);
        assert_eq!(
            with.len(),
            without.len() + 1,
            "with {with:?} without {without:?}"
        );
        assert!(
            !file_uris(&without)
                .iter()
                .any(|path| path.ends_with("a/Widget.java")),
            "the declaration must be excluded: {without:?}"
        );
        assert!(
            file_uris(&with)
                .iter()
                .any(|path| path.ends_with("a/Widget.java")),
            "the declaration must be included when asked: {with:?}"
        );
    }

    #[test]
    fn a_cursor_on_an_import_package_segment_targets_nothing() {
        let _env = crate::jdk::env_lock();
        std::env::set_var("JAVA_LSP_JDK", "/definitely/not/a/jdk");
        let use_text = "package a;\n\nimport b.Widget;\n\npublic class Use {\n    void m() {\n        Widget w = null;\n    }\n}\n";
        let (root, root_uri) = temp_workspace(
            "import-segment",
            &[
                ("b/Widget.java", "package b;\n\npublic class Widget {\n}\n"),
                ("a/Use.java", use_text),
            ],
        );
        let engine = scanned_engine(&root_uri);
        let uri = Url::from_file_path(root.join("a/Use.java")).unwrap();
        engine.open(&uri, use_text);
        let cursor = lsp_position(
            use_text,
            use_text.find("import b").unwrap() + "import ".len(),
        );
        assert!(engine.references(&uri, cursor, true).is_empty());
        assert!(engine.rename(&uri, cursor, "Gadget").is_none());
    }

    #[test]
    fn a_library_member_cannot_be_renamed() {
        let _env = crate::jdk::env_lock();
        std::env::set_var("JAVA_LSP_JDK", "/definitely/not/a/jdk");
        let source = "package a;\n\npublic class Use {\n    void m() {\n        Dep d = null;\n        d.value();\n    }\n}\n";
        let (root, root_uri) = temp_workspace("library-member", &[("a/Use.java", source)]);
        let engine = scanned_engine(&root_uri);
        // A dependency type with a member, known only to the type model.
        let mut model = TypeModel::new();
        let mut dep =
            crate::types::TypeInfo::new("Dep".to_string(), Some("a".to_string()), IndexKind::Class);
        dep.methods.push(Member {
            name: "value".to_string(),
            kind: IndexKind::Method,
            ty: Ty::Prim(crate::types::Prim::Int),
            params: Vec::new(),
            type_params: Vec::new(),
            is_static: false,
        });
        model.insert(dep);
        engine.index.set_types(std::sync::Arc::new(model));
        let uri = Url::from_file_path(root.join("a/Use.java")).unwrap();
        engine.open(&uri, source);
        let cursor = lsp_position(source, source.find("d.value()").unwrap() + 2);
        assert!(engine.references(&uri, cursor, true).is_empty());
        assert!(engine.rename(&uri, cursor, "amount").is_none());
    }

    #[test]
    fn a_variable_initializer_is_a_use_not_a_declaration() {
        let _env = crate::jdk::env_lock();
        std::env::set_var("JAVA_LSP_JDK", "/definitely/not/a/jdk");
        let source = "package a;\n\npublic class Use {\n    void m() {\n        int a = 1;\n        int b = a;\n    }\n}\n";
        let (root, root_uri) = temp_workspace("initializer-use", &[("a/Use.java", source)]);
        let engine = scanned_engine(&root_uri);
        let uri = Url::from_file_path(root.join("a/Use.java")).unwrap();
        engine.open(&uri, source);
        // Cursor on the initializer `a` of `int b = a;`.
        let cursor = lsp_position(source, source.find("b = a").unwrap() + "b = ".len());

        let edit = engine.rename(&uri, cursor, "first").expect("rename");
        let changes = edit.changes.expect("changes");
        let edits = changes.values().next().unwrap();
        assert_eq!(edits.len(), 2, "{edits:?}");
        assert!(edits.iter().all(|edit| edit.new_text == "first"));
    }

    #[test]
    fn a_qualified_use_counts_as_visibility() {
        let _env = crate::jdk::env_lock();
        std::env::set_var("JAVA_LSP_JDK", "/definitely/not/a/jdk");
        let (root, root_uri) = temp_workspace(
            "qualified-visibility",
            &[
                ("a/Widget.java", "package a;\n\npublic class Widget {\n}\n"),
                (
                    "c/Other.java",
                    "package c;\n\npublic class Other {\n    a.Widget field;\n}\n",
                ),
                (
                    "a/Use.java",
                    "package a;\n\npublic class Use {\n    Widget w;\n}\n",
                ),
            ],
        );
        let engine = scanned_engine(&root_uri);
        let use_uri = Url::from_file_path(root.join("a/Use.java")).unwrap();
        let use_text = std::fs::read_to_string(root.join("a/Use.java")).unwrap();
        engine.open(&use_uri, &use_text);
        let cursor = lsp_position(&use_text, use_text.find("Widget w").unwrap() + 1);

        let references = engine.references(&use_uri, cursor, true);
        let uris = file_uris(&references);
        assert!(
            uris.iter().any(|path| path.ends_with("c/Other.java")),
            "a fully-qualified use names the type without an import: {uris:?}"
        );
    }

    #[test]
    fn rename_refuses_when_a_candidate_cannot_be_read() {
        let _env = crate::jdk::env_lock();
        std::env::set_var("JAVA_LSP_JDK", "/definitely/not/a/jdk");
        let (root, root_uri) = temp_workspace(
            "unreadable-candidate",
            &[
                ("a/Widget.java", "package a;\n\npublic class Widget {\n}\n"),
                (
                    "a/Use.java",
                    "package a;\n\npublic class Use {\n    Widget w;\n}\n",
                ),
            ],
        );
        let engine = scanned_engine(&root_uri);
        let use_uri = Url::from_file_path(root.join("a/Use.java")).unwrap();
        let use_text = std::fs::read_to_string(root.join("a/Use.java")).unwrap();
        engine.open(&use_uri, &use_text);
        // A candidate file disappears after the scan, so it cannot be read.
        std::fs::remove_file(root.join("a/Widget.java")).unwrap();
        let cursor = lsp_position(&use_text, use_text.find("Widget w").unwrap() + 1);

        assert!(engine.rename(&use_uri, cursor, "Gadget").is_none());
    }
}
