//! Tree-sitter backed engine core: one Java syntax tree per open document,
//! feeding parse-error diagnostics, document symbols, folding ranges, and
//! semantic tokens, plus a workspace-wide symbol index ([`WorkspaceIndex`])
//! warmed by a background scan and answering go-to-definition and
//! workspace-symbol queries.
//!
//! A concrete, synchronous, `Send + Sync` type: the shell reaches it through
//! [`crate::engine`], which owns it and exchanges commands and events with the
//! LSP layer.
//!
//! Trees are rebuilt from the full document text on every `open`/`change`.
//! The engine never reparses other documents, so an edit reparses exactly one
//! file and updates only that file's index entries; true `InputEdit`-based
//! incremental reparsing (needs edit ranges forwarded from the shell) is a
//! later optimization.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde_json::json;
use tower_lsp::lsp_types::{
    CodeAction, CodeActionKind, CompletionItem, CompletionItemKind, CompletionResponse, CreateFile,
    CreateFileOptions, Diagnostic, DiagnosticSeverity, DocumentChangeOperation, DocumentChanges,
    DocumentSymbol, FoldingRange, FoldingRangeKind, Hover, HoverContents, InlayHint, InlayHintKind,
    InlayHintLabel, Location, MarkupContent, MarkupKind, NumberOrString, OneOf,
    OptionalVersionedTextDocumentIdentifier, Position, Range, ResourceOp, SemanticToken,
    SemanticTokenType, SemanticTokens, SignatureHelp, SignatureInformation, SymbolInformation,
    SymbolKind, TextDocumentEdit, TextEdit, Url, WorkspaceEdit,
};
use tree_sitter::{Node, Parser, Tree};

use crate::engine::Reporter;
use crate::index::{
    extract_entries, java_parser, scan_workspace, scan_workspace_async, IndexKind, SymbolEntry,
    WorkspaceIndex,
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
    /// Whether semantic (unresolved-symbol) diagnostics are enabled; read from
    /// `JAVA_LSP_SEMANTIC_DIAGNOSTICS` at construction (on by default).
    semantic_diagnostics: AtomicBool,
    /// Whether the client advertised `workspace.workspaceEdit.resourceOperations`
    /// with `CreateFile`, so the create-type quick fix can be offered.
    resource_operations: AtomicBool,
    /// Where the background warm-up reports progress; detached (a no-op) until
    /// the shell installs one.
    reporter: Mutex<Reporter>,
}

impl TreeSitterEngine {
    pub fn new() -> Self {
        Self {
            parser: Mutex::new(java_parser()),
            documents: Mutex::new(HashMap::new()),
            index: WorkspaceIndex::new(),
            workspace_root: Mutex::new(None),
            semantic_diagnostics: AtomicBool::new(semantic_diagnostics_enabled()),
            resource_operations: AtomicBool::new(false),
            reporter: Mutex::new(Reporter::default()),
        }
    }

    /// Records whether the client supports the `CreateFile` resource operation,
    /// so create-type quick fixes are only offered when they can be applied.
    pub fn set_resource_operations(&self, supported: bool) {
        self.resource_operations.store(supported, Ordering::Relaxed);
    }

    /// Enables or disables semantic diagnostics, overriding the environment
    /// default (used by tests).
    pub fn set_semantic_diagnostics(&self, enabled: bool) {
        self.semantic_diagnostics.store(enabled, Ordering::Relaxed);
    }

    /// Installs the reporter the background warm-up reports through.
    pub fn set_reporter(&self, reporter: Reporter) {
        if let Ok(mut slot) = self.reporter.lock() {
            *slot = reporter;
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
        let Some(object) = receiver_before_dot(&document.tree, text, offset) else {
            return Vec::new();
        };
        let local = local_model(document);
        let workspace = self.index.type_model();
        let empty = TypeModel::new();
        let base = workspace.as_deref().unwrap_or(&empty);
        let query = TypeQuery::new(base, &local);
        let scope = types::scope_at(object, text, &document.tree, &query);

        let static_only = matches!(object.kind(), "identifier" | "type_identifier")
            && types::resolve_name(&text[object.byte_range()], &scope, &query)
                .map(|resolved| resolved.is_type)
                .unwrap_or(false);

        let ty = types::receiver_type(&object, text, &scope, &query);
        let mut items = Vec::new();
        let mut seen = HashSet::new();
        // Overloads stay separate: each method is its own item, labelled with its
        // full signature, so `add(int)` and `add(int, int)` can be told apart.
        for member in query.members_with_overloads(&ty, scope.package.as_deref()) {
            if static_only && !member.is_static {
                continue;
            }
            if !member.name.starts_with(prefix) {
                continue;
            }
            let is_method = member.kind == IndexKind::Method;
            if is_method {
                if !seen.insert(member.signature()) {
                    continue;
                }
                items.push(CompletionItem {
                    label: member.signature(),
                    kind: Some(CompletionItemKind::METHOD),
                    filter_text: Some(member.name.clone()),
                    insert_text: Some(format!("{}(", member.name)),
                    sort_text: Some(format!("0{}", member.signature())),
                    ..Default::default()
                });
            } else {
                if !seen.insert(member.name.clone()) {
                    continue;
                }
                items.push(CompletionItem {
                    label: member.name.clone(),
                    kind: Some(CompletionItemKind::FIELD),
                    detail: Some(member.signature()),
                    filter_text: Some(member.name.clone()),
                    insert_text: Some(member.name.clone()),
                    sort_text: Some(format!("0{}", member.name)),
                    ..Default::default()
                });
            }
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
                                uri,
                                document,
                                &name,
                                IndexKind::Field,
                                &receiver,
                                &query,
                                scope.package.as_deref(),
                                None,
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
                        let args = call_argument_types(&parent, text, &scope, &query);
                        return self.member_target(
                            uri,
                            document,
                            &name,
                            IndexKind::Method,
                            &receiver,
                            &query,
                            scope.package.as_deref(),
                            Some(&args),
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
            // An unqualified call carries its arguments on the enclosing node.
            let args = node
                .parent()
                .filter(|parent| parent.kind() == "method_invocation")
                .map(|parent| call_argument_types(&parent, text, &scope, &query));
            return self.member_target(
                uri,
                document,
                &name,
                member.kind,
                &receiver,
                &query,
                scope.package.as_deref(),
                args.as_deref(),
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
            overload: None,
            overload_declaration: None,
        })
    }

    /// A member as a target: the receiver's type must resolve the name to the
    /// type that declares it, and that type must be a workspace source. When a
    /// call's argument types are supplied, they select the overload, and its
    /// declaration is located for `definition`/`include_declaration`.
    fn member_target(
        &self,
        uri: &Url,
        document: &ParsedDocument,
        name: &str,
        kind: IndexKind,
        receiver: &Ty,
        model: &dyn TypeLookup,
        package: Option<&str>,
        args: Option<&[Ty]>,
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
        let mut overload = None;
        let mut overload_declaration = None;
        // Narrowing only matters when the name is genuinely overloaded; a lone
        // declaration keeps the name-based behaviour (and, for a record
        // component accessor, its bare-name uses).
        if is_method && declarations.len() > 1 {
            if let Some(args) = args {
                if let Some(member) =
                    types::member_for_arguments(receiver, name, args, model, package)
                {
                    let params: Vec<Ty> =
                        member.params.iter().map(|param| param.ty.clone()).collect();
                    overload_declaration = self
                        .match_declaration(&declarations, &params, uri, document)
                        .map(|(_, location)| location);
                    overload = Some(params);
                }
            }
        }
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
            overload,
            overload_declaration,
        })
    }

    /// The declaration entry that declares exactly `params`, with its location,
    /// read from the requested document or the entry's file on disk.
    fn match_declaration(
        &self,
        declarations: &[SymbolEntry],
        params: &[Ty],
        requested: &Url,
        requested_doc: &ParsedDocument,
    ) -> Option<(SymbolEntry, Location)> {
        declarations.iter().find_map(|entry| {
            let entry_params = self.entry_method_params(entry, requested, requested_doc)?;
            (entry_params.as_slice() == params).then(|| {
                (
                    entry.clone(),
                    Location {
                        uri: entry.uri.clone(),
                        range: entry.selection_range,
                    },
                )
            })
        })
    }

    /// The parameter types the declaration `entry` actually declares, read from
    /// the requested document when the entry lives in it, else from disk.
    fn entry_method_params(
        &self,
        entry: &SymbolEntry,
        requested: &Url,
        requested_doc: &ParsedDocument,
    ) -> Option<Vec<Ty>> {
        if entry.uri == *requested {
            return method_params_in(
                &requested_doc.tree,
                &requested_doc.text,
                entry.selection_range,
            );
        }
        let path = entry.uri.to_file_path().ok()?;
        let text = std::fs::read_to_string(path).ok()?;
        let tree = self.parser.lock().ok()?.parse(text.as_bytes(), None)?;
        method_params_in(&tree, &text, entry.selection_range)
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

impl TreeSitterEngine {
    pub fn open(&self, uri: &Url, text: &str) {
        self.store_tree(uri, text);
    }

    pub fn change(&self, uri: &Url, text: &str) {
        self.store_tree(uri, text);
    }

    pub fn close(&self, uri: &Url) {
        if let Ok(mut documents) = self.documents.lock() {
            documents.remove(uri);
        }
        if !self.reindex_from_disk(uri) {
            self.index.remove_file(uri);
        }
    }

    pub fn diagnostics(&self, uri: &Url) -> Vec<Diagnostic> {
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
        if self.semantic_diagnostics.load(Ordering::Relaxed) {
            diagnostics.extend(semantic_diagnostics(document, uri, &self.index));
        }
        diagnostics
    }

    /// Quick fixes for the unresolved-symbol diagnostics the client is showing:
    /// add an import, change to a near member, or create a stub type/member.
    /// Built from each diagnostic's `data`, so the fix matches what was
    /// reported.
    pub fn code_actions(&self, uri: &Url, diagnostics: &[Diagnostic]) -> Vec<CodeAction> {
        let Ok(documents) = self.documents.lock() else {
            return Vec::new();
        };
        let Some(document) = documents.get(uri) else {
            return Vec::new();
        };
        let mut actions = Vec::new();
        for diagnostic in diagnostics {
            if diagnostic.source.as_deref() != Some("java-lsp") {
                continue;
            }
            let Some(data) = diagnostic.data.as_ref() else {
                continue;
            };
            match data.get("fix").and_then(|value| value.as_str()) {
                Some(FIX_ADD_IMPORT) => {
                    self.add_import_actions(uri, document, diagnostic, data, &mut actions)
                }
                Some(FIX_RENAME) => self.rename_member_action(uri, diagnostic, data, &mut actions),
                Some(FIX_CREATE_TYPE) => {
                    self.create_type_action(uri, document, diagnostic, data, &mut actions)
                }
                Some(FIX_CREATE_MEMBER) => {
                    self.create_member_action(uri, document, diagnostic, data, &mut actions)
                }
                _ => {}
            }
        }
        actions
    }

    /// One "Add import" action per importable candidate (the client shows a
    /// picker when several are offered). Edits come from `import_edit`.
    fn add_import_actions(
        &self,
        uri: &Url,
        document: &ParsedDocument,
        diagnostic: &Diagnostic,
        data: &serde_json::Value,
        actions: &mut Vec<CodeAction>,
    ) {
        let Some(candidates) = data.get("candidates").and_then(|value| value.as_array()) else {
            return;
        };
        let root = document.tree.root_node();
        let imports = collect_imports(&root, &document.text);
        let package = types::file_package(&document.tree, &document.text);
        let package_line = package_line(&document.tree);
        for candidate in candidates {
            let Some(target) = candidate.as_str() else {
                continue;
            };
            let simple = target.rsplit('.').next().unwrap_or(target);
            let Some(entry) = self
                .index
                .query_name(simple)
                .into_iter()
                .find(|entry| import_target(entry).as_deref() == Some(target))
            else {
                continue;
            };
            let edits = import_edit(
                uri,
                &entry,
                package.as_deref(),
                package_line,
                &imports,
                &self.index,
            );
            if edits.is_empty() {
                continue;
            }
            actions.push(CodeAction {
                title: format!("Add import `{target}`"),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![diagnostic.clone()]),
                edit: Some(workspace_edit_changes(uri, edits)),
                ..CodeAction::default()
            });
        }
    }

    /// A "Change to `x`" action for a member name close to a real one.
    fn rename_member_action(
        &self,
        uri: &Url,
        diagnostic: &Diagnostic,
        data: &serde_json::Value,
        actions: &mut Vec<CodeAction>,
    ) {
        let Some(replacement) = data
            .get("replacement")
            .and_then(|value| value.as_str())
            .filter(|replacement| !replacement.is_empty())
        else {
            return;
        };
        actions.push(CodeAction {
            title: format!("Change to `{replacement}`"),
            kind: Some(CodeActionKind::QUICKFIX),
            diagnostics: Some(vec![diagnostic.clone()]),
            edit: Some(workspace_edit_changes(
                uri,
                vec![TextEdit {
                    range: diagnostic.range,
                    new_text: replacement.to_string(),
                }],
            )),
            is_preferred: Some(true),
            ..CodeAction::default()
        });
    }

    /// A "Create class/interface `X`" action that adds a new file under the
    /// source root of the file's own package. Only offered when the client
    /// supports the `CreateFile` resource operation.
    fn create_type_action(
        &self,
        uri: &Url,
        document: &ParsedDocument,
        diagnostic: &Diagnostic,
        data: &serde_json::Value,
        actions: &mut Vec<CodeAction>,
    ) {
        if !self.resource_operations.load(Ordering::Relaxed) {
            return;
        }
        let Some(name) = data.get("name").and_then(|value| value.as_str()) else {
            return;
        };
        if !is_java_identifier(name) {
            return;
        }
        let kind = data
            .get("kind")
            .and_then(|value| value.as_str())
            .unwrap_or("class");
        let package = types::file_package(&document.tree, &document.text);
        let Some(new_file) = self.new_type_file_uri(uri, package.as_deref(), name) else {
            return;
        };
        let contents = stub_type_source(package.as_deref(), kind, name);
        let operations = vec![
            DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                uri: new_file.clone(),
                options: Some(CreateFileOptions {
                    overwrite: None,
                    ignore_if_exists: Some(true),
                }),
                annotation_id: None,
            })),
            DocumentChangeOperation::Edit(TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: new_file.clone(),
                    version: None,
                },
                edits: vec![OneOf::Left(TextEdit {
                    range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                    new_text: contents,
                })],
            }),
        ];
        actions.push(CodeAction {
            title: format!("Create {kind} `{name}`"),
            kind: Some(CodeActionKind::QUICKFIX),
            diagnostics: Some(vec![diagnostic.clone()]),
            edit: Some(WorkspaceEdit {
                changes: None,
                document_changes: Some(DocumentChanges::Operations(operations)),
                change_annotations: None,
            }),
            is_preferred: Some(true),
            ..CodeAction::default()
        });
    }

    /// A "Create method/field" action that inserts a stub into the enclosing
    /// type declaration in the open file.
    fn create_member_action(
        &self,
        uri: &Url,
        document: &ParsedDocument,
        diagnostic: &Diagnostic,
        data: &serde_json::Value,
        actions: &mut Vec<CodeAction>,
    ) {
        let Some(name) = data.get("name").and_then(|value| value.as_str()) else {
            return;
        };
        if !is_java_identifier(name) {
            return;
        }
        let kind = data
            .get("kind")
            .and_then(|value| value.as_str())
            .unwrap_or("field");
        let offset = byte_offset(&document.text, diagnostic.range.start);
        let Some(body) = enclosing_type_body(&document.tree, offset) else {
            return;
        };
        // Insert whole lines before the type body's closing brace.
        let close = lsp_position(&document.text, body.end_byte().saturating_sub(1));
        let stub = if kind == "method" {
            format!("    public void {name}() {{\n    }}\n")
        } else {
            format!("    private Object {name};\n")
        };
        let edit = TextEdit {
            range: Range::new(Position::new(close.line, 0), Position::new(close.line, 0)),
            new_text: stub,
        };
        actions.push(CodeAction {
            title: format!(
                "Create {} `{name}`",
                if kind == "method" { "method" } else { "field" }
            ),
            kind: Some(CodeActionKind::QUICKFIX),
            diagnostics: Some(vec![diagnostic.clone()]),
            edit: Some(workspace_edit_changes(uri, vec![edit])),
            ..CodeAction::default()
        });
    }

    /// The URI for a new `name.java`: under the deepest source root containing
    /// the file, in the file's package, else beside the file.
    fn new_type_file_uri(&self, uri: &Url, package: Option<&str>, name: &str) -> Option<Url> {
        let path = uri.to_file_path().ok()?;
        let base = self
            .index
            .source_roots()
            .into_iter()
            .filter(|root| path.starts_with(root))
            .max_by_key(|root| root.components().count())
            .or_else(|| path.parent().map(std::path::Path::to_path_buf))?;
        let mut target = base;
        if let Some(package) = package {
            for segment in package.split('.') {
                target.push(segment);
            }
        }
        target.push(format!("{name}.java"));
        Url::from_file_path(target).ok()
    }

    pub fn hover(&self, uri: &Url, position: Position) -> Option<Hover> {
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

    pub fn definition(&self, uri: &Url, position: Position) -> Option<Location> {
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
                    .filter(|entry| !entry.dependency || entry.library_source)
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
            .filter(|entry| !entry.dependency || entry.library_source)
            .filter(|entry| entry.kind != IndexKind::Import)
            .filter(|entry| match name_constraint(&node) {
                NameConstraint::Type => is_type_kind(entry.kind),
                NameConstraint::Member => {
                    matches!(entry.kind, IndexKind::Method | IndexKind::Field)
                }
                NameConstraint::Any => true,
            })
            .collect();
        // A member call: the receiver's declaring type and the call's argument
        // types select the overload, so `x.add(1)` lands on `add(int)`.
        if let Some(location) = self.call_definition(uri, document, &node, word) {
            return Some(location);
        }
        unique_location(candidates)
    }

    /// The declaration a member call's receiver and argument types select, or
    /// `None` when the cursor is not a call or the overload cannot be pinned
    /// down (the caller then falls back to the name-only lookup).
    fn call_definition(
        &self,
        uri: &Url,
        document: &ParsedDocument,
        node: &Node,
        name: &str,
    ) -> Option<Location> {
        let text = &document.text;
        let parent = node.parent()?;
        if parent.kind() != "method_invocation"
            || !parent
                .child_by_field_name("name")
                .is_some_and(|named| named.id() == node.id())
        {
            return None;
        }
        let local = local_model(document);
        let workspace = self.index.type_model();
        let empty = TypeModel::new();
        let base = workspace.as_deref().unwrap_or(&empty);
        let query = TypeQuery::new(base, &local);
        let scope = types::scope_at(*node, text, &document.tree, &query);
        let receiver = match parent.child_by_field_name("object") {
            Some(object) => types::receiver_type(&object, text, &scope, &query),
            None => scope
                .enclosing_type
                .clone()
                .map(Ty::reference)
                .unwrap_or(Ty::Unknown),
        };
        let owner = types::member_owner(&receiver, name, &query, scope.package.as_deref())?;
        let args = call_argument_types(&parent, text, &scope, &query);
        let member =
            types::member_for_arguments(&receiver, name, &args, &query, scope.package.as_deref())?;
        let params: Vec<Ty> = member.params.iter().map(|param| param.ty.clone()).collect();
        let candidates: Vec<SymbolEntry> = self
            .index
            .query_name(name)
            .into_iter()
            .filter(|entry| !entry.dependency || entry.library_source)
            .filter(|entry| entry.kind == IndexKind::Method)
            .filter(|entry| {
                entry.container.last().map(String::as_str) == Some(owner.name.as_str())
                    && entry.package == owner.package
            })
            .collect();
        self.match_declaration(&candidates, &params, uri, document)
            .map(|(_, location)| location)
    }

    pub fn references(
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

    pub fn rename(&self, uri: &Url, position: Position, new_name: &str) -> Option<WorkspaceEdit> {
        if !is_valid_identifier(new_name) {
            return None;
        }
        let (mut target, text) = self.name_target(uri, position)?;
        // Rename stays name-group-wide: it renames every overload of the name,
        // never a single overload (which could leave a call site behind).
        target.overload = None;
        target.overload_declaration = None;
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

    pub fn workspace_symbols(&self, query: &str) -> Vec<SymbolInformation> {
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

    pub fn completions(&self, uri: &Url, position: Position) -> Option<CompletionResponse> {
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
        // A type model layered with the open buffer, so a method's overloads
        // reflect unsaved edits.
        let local = local_model(document);
        let workspace = self.index.type_model();
        let empty = TypeModel::new();
        let base = workspace.as_deref().unwrap_or(&empty);
        let query = TypeQuery::new(base, &local);
        for entry in entries {
            let Some(kind) = completion_kind(entry.kind) else {
                continue;
            };
            let name = entry.name.clone();
            let sort_text = format!("2{name}");
            let container = entry.container.join(".");
            let base_detail = if matches!(entry.kind, IndexKind::Method | IndexKind::Field)
                && !container.is_empty()
            {
                format!("{} of {container}", kind_word(entry.kind))
            } else {
                kind_word(entry.kind).to_string()
            };
            let detail = if ambiguous.contains(&name) {
                format!("{} of {}", kind_word(entry.kind), qualified_owner(&entry))
            } else {
                base_detail
            };
            let additional_edits = import_edit(
                uri,
                &entry,
                file_package.as_deref(),
                package_line,
                &imports,
                &self.index,
            );
            // A method's overloads are offered individually, with their real
            // signatures, when the type model can name the declaring type.
            if entry.kind == IndexKind::Method {
                if let Some(overloads) = model_overloads(&query, &entry) {
                    for member in overloads {
                        // Same signature + same import is a duplicate; the same
                        // signature in another package is a different symbol.
                        let key = match additional_edits.first() {
                            Some(edit) => format!("{}|{}", member.signature(), edit.new_text),
                            None => member.signature(),
                        };
                        offer(
                            &mut items,
                            &mut seen,
                            key,
                            member.signature(),
                            member.name.clone(),
                            format!("{}(", member.name),
                            CompletionItemKind::METHOD,
                            Some(detail.clone()),
                            format!("2{}", member.signature()),
                            additional_edits.clone(),
                        );
                    }
                    continue;
                }
            }
            let label = if matches!(entry.kind, IndexKind::Method | IndexKind::Field)
                && !container.is_empty()
            {
                format!("{container}.{name}")
            } else {
                name.clone()
            };
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
                name.clone(),
                name,
                kind,
                Some(detail),
                sort_text,
                additional_edits,
            );
        }

        Some(CompletionResponse::Array(items))
    }

    pub fn document_symbols(&self, uri: &Url) -> Option<Vec<DocumentSymbol>> {
        let documents = self.documents.lock().ok()?;
        let document = documents.get(uri)?;
        let mut symbols = Vec::new();
        collect_symbols(&document.tree.root_node(), &document.text, &mut symbols);
        Some(symbols)
    }

    pub fn folding_ranges(&self, uri: &Url) -> Option<Vec<FoldingRange>> {
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

    pub fn semantic_tokens(&self, uri: &Url) -> Option<SemanticTokens> {
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

    pub fn inlay_hints(&self, uri: &Url, range: Range) -> Vec<InlayHint> {
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

    /// Signature help for the call the cursor sits in: the callee's overloads
    /// rendered with their declared parameters, and the active parameter taken
    /// from the cursor's position among the arguments. `None` when there is no
    /// enclosing call or the callee cannot be resolved; constructors are not
    /// modelled, so `new T(...)` answers nothing.
    pub fn signature_help(&self, uri: &Url, position: Position) -> Option<SignatureHelp> {
        let documents = self.documents.lock().ok()?;
        let document = documents.get(uri)?;
        let text = &document.text;
        let offset = byte_offset(text, position);
        let call = enclosing_call(&document.tree, offset)?;
        let name_node = call.child_by_field_name("name")?;
        let name = &text[name_node.byte_range()];

        let local = local_model(document);
        let workspace = self.index.type_model();
        let empty = TypeModel::new();
        let base = workspace.as_deref().unwrap_or(&empty);
        let query = TypeQuery::new(base, &local);
        let scope = types::scope_at(name_node, text, &document.tree, &query);

        let (receiver, static_only) = match call.child_by_field_name("object") {
            Some(object) => {
                let static_only = matches!(object.kind(), "identifier" | "type_identifier")
                    && types::resolve_name(&text[object.byte_range()], &scope, &query)
                        .map(|resolved| resolved.is_type)
                        .unwrap_or(false);
                (
                    types::receiver_type(&object, text, &scope, &query),
                    static_only,
                )
            }
            None => (
                scope
                    .enclosing_type
                    .clone()
                    .map(Ty::reference)
                    .unwrap_or(Ty::Unknown),
                false,
            ),
        };

        let overloads: Vec<Member> = query
            .members_with_overloads(&receiver, scope.package.as_deref())
            .into_iter()
            .filter(|member| member.kind == IndexKind::Method && member.name == name)
            .filter(|member| !static_only || member.is_static)
            .collect();
        if overloads.is_empty() {
            return None;
        }
        let active = active_parameter(call, offset);
        let signatures = overloads
            .iter()
            .map(|member| SignatureInformation {
                label: member.signature(),
                documentation: None,
                parameters: None,
                active_parameter: Some(active),
            })
            .collect();
        Some(SignatureHelp {
            signatures,
            active_signature: Some(0),
            active_parameter: None,
        })
    }

    pub fn set_workspace_root(&self, root: &Url) {
        if let Ok(mut slot) = self.workspace_root.lock() {
            *slot = Some(root.clone());
        }
        let index = self.index.clone();
        let root = root.clone();
        let reporter = self
            .reporter
            .lock()
            .map(|reporter| reporter.clone())
            .unwrap_or_default();
        // Off the request path: spawned onto the runtime when one is available
        // (always true for the shell), inline otherwise. The runtime path also
        // fetches dependency sources; without a runtime (tests) the sync core
        // runs and the source pass is skipped.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(scan_workspace_async(root, index, reporter));
            }
            Err(_) => scan_workspace(root, index),
        }
    }

    pub fn index_ready(&self) -> bool {
        self.index.ready()
    }

    pub fn indexed_symbols(&self) -> Vec<crate::index::SymbolEntry> {
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
    /// For a method, the selected overload's parameter types, so occurrences are
    /// attributed to the right overload; `None` when the overload could not be
    /// pinned down or the target is not a method.
    overload: Option<Vec<Ty>>,
    /// The selected overload's declaration, when it could be located.
    overload_declaration: Option<Location>,
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
                overload: None,
                overload_declaration: None,
            })
        }
        "method_declaration" => {
            let name = text[node.byte_range()].to_string();
            let owner = enclosing_type_name(parent, text)?;
            workspace_type_entry(&owner, package, index)?;
            let declarations =
                member_declarations(&name, IndexKind::Method, &owner, package, index);
            // The declaration under the cursor is this specific overload.
            let range = lsp_range(text, &node);
            let (overload, overload_declaration) = if declarations.len() > 1 {
                let params: Vec<Ty> = types::parameter_list(parent, text)
                    .into_iter()
                    .map(|param| param.ty)
                    .collect();
                let declaration = declarations
                    .iter()
                    .find(|entry| entry.selection_range == range)
                    .map(|entry| Location {
                        uri: entry.uri.clone(),
                        range: entry.selection_range,
                    });
                (Some(params), declaration)
            } else {
                (None, None)
            };
            Some(Target {
                name,
                kind: TargetKind::Method,
                owner: Some(owner),
                package: package.map(str::to_string),
                local_span: None,
                declarations,
                local_declaration: None,
                overload,
                overload_declaration,
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
                        overload: None,
                        overload_declaration: None,
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
        overload: None,
        overload_declaration: None,
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

/// The inferred types of a method invocation's arguments, comments excluded.
fn call_argument_types(
    call: &Node,
    text: &str,
    scope: &types::Scope,
    model: &dyn TypeLookup,
) -> Vec<Ty> {
    let Some(arguments) = call.child_by_field_name("arguments") else {
        return Vec::new();
    };
    let mut cursor = arguments.walk();
    arguments
        .named_children(&mut cursor)
        .filter(|child| !is_comment_node(child.kind()))
        .map(|child| types::receiver_type(&child, text, scope, model))
        .collect()
}

/// The parameter types of the `method_declaration` whose name sits at
/// `selection` in `tree`, read from the declaration's own parameter list.
fn method_params_in(tree: &Tree, text: &str, selection: Range) -> Option<Vec<Ty>> {
    let offset = byte_offset(text, selection.start);
    let node = tree.root_node().descendant_for_byte_range(offset, offset)?;
    let mut current = Some(node);
    while let Some(candidate) = current {
        if candidate.kind() == "method_declaration" {
            return Some(
                types::parameter_list(&candidate, text)
                    .into_iter()
                    .map(|param| param.ty)
                    .collect(),
            );
        }
        current = candidate.parent();
    }
    None
}

/// Whether an occurrence's call arguments accept the selected overload's
/// parameters. With no overload selected every occurrence qualifies; a method
/// reference or a bare name has no argument list and does not.
fn occurrence_matches_overload(
    node: &Node,
    text: &str,
    tree: &Tree,
    model: &dyn TypeLookup,
    package: Option<&str>,
    overload: &Option<Vec<Ty>>,
) -> bool {
    let Some(params) = overload else {
        return true;
    };
    let Some(parent) = node.parent() else {
        return false;
    };
    if parent.kind() != "method_invocation" {
        return false;
    }
    let scope = types::scope_at(*node, text, tree, model);
    let actual = call_argument_types(&parent, text, &scope, model);
    actual.len() == params.len()
        && actual
            .iter()
            .zip(params)
            .all(|(arg, param)| types::assignable(arg, param, model, package))
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
            // Unqualified uses count only inside the declaring type's own span,
            // and only when they are calls that accept the selected overload.
            occurrence_matches_overload(node, text, tree, model, package, &target.overload)
                && owner_span.is_some_and(|(start, end)| {
                    node.start_byte() >= start && node.end_byte() <= end
                })
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
            owner.package == target.package
                && Some(owner.name.as_str()) == target.owner.as_deref()
                && occurrence_matches_overload(node, text, tree, model, package, &target.overload)
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
    if target.overload.is_some() {
        // Only the selected overload's declaration, never a sibling's. A rename
        // clears the overload, so it falls through to every declaration below.
        if let Some(location) = &target.overload_declaration {
            add(location.clone());
        }
        return;
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
    // A record carries its components in a `parameters` field; no other
    // declaration kind this renders has one.
    let components = parent
        .child_by_field_name("parameters")
        .map(|node| text[node.byte_range()].to_string())
        .unwrap_or_default();
    format!("{keyword} {name}{parameters}{components}")
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
/// The innermost `method_invocation` whose argument list contains `offset`.
fn enclosing_call(tree: &Tree, offset: usize) -> Option<Node<'_>> {
    let node = tree.root_node().descendant_for_byte_range(offset, offset)?;
    let mut current = Some(node);
    while let Some(candidate) = current {
        if candidate.kind() == "method_invocation" {
            if let Some(arguments) = candidate.child_by_field_name("arguments") {
                if arguments.start_byte() <= offset && offset <= arguments.end_byte() {
                    return Some(candidate);
                }
            }
        }
        current = candidate.parent();
    }
    None
}

/// The index of the argument the cursor sits in: the number of argument nodes
/// ending before the cursor, clamped to the last argument.
fn active_parameter(call: Node, offset: usize) -> u32 {
    let Some(arguments) = call.child_by_field_name("arguments") else {
        return 0;
    };
    let mut cursor = arguments.walk();
    let mut index = 0u32;
    let mut count = 0u32;
    for argument in arguments.named_children(&mut cursor) {
        if is_comment_node(argument.kind()) {
            continue;
        }
        count += 1;
        if argument.end_byte() <= offset {
            index += 1;
        }
    }
    index.min(count.saturating_sub(1))
}

fn receiver_before_dot<'a>(tree: &'a Tree, text: &str, offset: usize) -> Option<Node<'a>> {
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
    receiver_before_dot_from_text(tree, text, offset)
}

/// Recovers the receiver of an incomplete `receiver.` from the source when the
/// tree has not formed a member access. A dot at the end of a line can be
/// absorbed into the following token — a `var` line makes the parser read
/// `gson.var` as a scoped type identifier — so the receiver is taken as the
/// outermost expression ending at the last non-whitespace byte before the dot.
/// The node is a real node of the same tree, so callers can still locate its
/// enclosing scope.
fn receiver_before_dot_from_text<'a>(
    tree: &'a Tree,
    text: &str,
    offset: usize,
) -> Option<Node<'a>> {
    let bytes = text.as_bytes();
    let mut end = offset.checked_sub(1)?;
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    if end == 0 {
        return None;
    }
    let mut node = tree
        .root_node()
        .descendant_for_byte_range(end - 1, end - 1)?;
    let mut best = is_receiver_kind(node.kind()).then_some(node);
    // Climb to the outermost expression that still ends at the receiver, so a
    // call or parenthesized receiver is taken whole (`list.get(0).`, `(x).`).
    while let Some(parent) = node.parent() {
        if parent.end_byte() != end {
            break;
        }
        node = parent;
        if is_receiver_kind(node.kind()) {
            best = Some(node);
        }
    }
    best
}

/// True for the node kinds [`receiver_type`](crate::types::receiver_type) can
/// type — the shapes an expression receiver may take.
fn is_receiver_kind(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "type_identifier"
            | "scoped_identifier"
            | "scoped_type_identifier"
            | "this"
            | "super"
            | "object_creation_expression"
            | "array_creation_expression"
            | "field_access"
            | "method_invocation"
            | "parenthesized_expression"
            | "cast_expression"
            | "array_access"
            | "ternary_expression"
            | "instanceof_expression"
            | "switch_expression"
            | "string_literal"
            | "decimal_integer_literal"
            | "hex_integer_literal"
            | "octal_integer_literal"
            | "binary_integer_literal"
            | "decimal_floating_point_literal"
            | "hex_floating_point_literal"
            | "character_literal"
            | "true"
            | "false"
            | "null_literal"
    )
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
        "enhanced_for_statement" => enhanced_for_hint(&node, text, tree, model, out),
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

/// The type of an enhanced-for binding: the declared type, or — when written
/// `var` — the element type of the iterable.
fn enhanced_for_hint(
    node: &Node,
    text: &str,
    tree: &Tree,
    model: &dyn TypeLookup,
    out: &mut Vec<InlayHint>,
) {
    let Some(name) = node.child_by_field_name("name") else {
        return;
    };
    let type_node = node.child_by_field_name("type");
    let is_var = type_node
        .as_ref()
        .is_some_and(|ty| &text[ty.byte_range()] == "var");
    let ty = if is_var {
        let Some(value) = node.child_by_field_name("value") else {
            return;
        };
        let scope = types::scope_at(*node, text, tree, model);
        types::element_type(&types::receiver_type(&value, text, &scope, model))
    } else {
        match type_node {
            Some(type_node) => types::type_from_node(&type_node, text),
            None => return,
        }
    };
    push_type_hint(name.end_byte(), &ty, text, out);
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
    // Select the callee from the argument types, so an overloaded method's
    // names are the ones the call actually targets. A single same-arity
    // candidate is still used, but an overload the layer cannot pin down (a
    // type it could not infer among several same-arity candidates) yields no
    // hint rather than a wrong name.
    let argument_types: Vec<Ty> = call_arguments
        .iter()
        .map(|argument| types::receiver_type(argument, text, &scope, model))
        .collect();
    let Some(member) = types::member_for_arguments_confirmed(
        &receiver,
        &text[name.byte_range()],
        &argument_types,
        model,
        scope.package.as_deref(),
    ) else {
        return;
    };
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

/// Marks a semantic diagnostic and the quick fix it carries; the code-action
/// handler rebuilds the edit from the diagnostic's `data`.
const CODE_UNRESOLVED_TYPE: &str = "unresolved-type";
const CODE_UNRESOLVED_MEMBER: &str = "unresolved-member";
const CODE_UNRESOLVED_SYMBOL: &str = "unresolved-symbol";
const CODE_UNRESOLVED_IMPORT: &str = "unresolved-import";
const FIX_ADD_IMPORT: &str = "add-import";
const FIX_RENAME: &str = "rename";
const FIX_CREATE_TYPE: &str = "create-type";
const FIX_CREATE_MEMBER: &str = "create-member";

/// Whether semantic diagnostics are enabled, from `JAVA_LSP_SEMANTIC_DIAGNOSTICS`:
/// unset (or any value but `0`/`false`) keeps them on.
fn semantic_diagnostics_enabled() -> bool {
    semantic_diagnostics_setting(
        std::env::var("JAVA_LSP_SEMANTIC_DIAGNOSTICS")
            .ok()
            .as_deref(),
    )
}

/// The setting a `JAVA_LSP_SEMANTIC_DIAGNOSTICS` value denotes.
fn semantic_diagnostics_setting(value: Option<&str>) -> bool {
    match value {
        Some(value) => !(value == "0" || value.eq_ignore_ascii_case("false")),
        None => true,
    }
}

/// Unresolved-symbol diagnostics for a document: type references, member
/// accesses on a known receiver, bare identifiers, and import declarations, all
/// at `ERROR` severity. Gated on the model actually vouching for `java.lang` and
/// on the file parsing cleanly, so a missing JDK or a broken file never produces
/// a wall of false positives.
fn semantic_diagnostics(
    document: &ParsedDocument,
    uri: &Url,
    index: &WorkspaceIndex,
) -> Vec<Diagnostic> {
    let Some(workspace) = index.type_model() else {
        return Vec::new();
    };
    if !workspace.contains("Object") || !workspace.contains("String") {
        return Vec::new();
    }
    let local = local_model(document);
    let query = TypeQuery::new(&workspace, &local);
    let mut check = SemanticCheck::new(uri, document, index, &query);
    check.visit(document.tree.root_node());
    check.out
}

/// The state of one diagnostics pass over an open document.
struct SemanticCheck<'a> {
    uri: &'a Url,
    text: &'a str,
    tree: &'a Tree,
    index: &'a WorkspaceIndex,
    model: &'a dyn TypeLookup,
    imports: Vec<ExistingImport>,
    package: Option<String>,
    package_line: Option<u32>,
    /// Every simple name bound anywhere in the file (locals, parameters,
    /// fields, types, methods, lambda parameters, catch bindings, ...). A bare
    /// identifier matching one of these is never reported: the scope collector
    /// does not model every binding kind, and a false positive is worse than a
    /// missed one.
    declared_names: HashSet<String>,
    out: Vec<Diagnostic>,
}

impl<'a> SemanticCheck<'a> {
    fn new(
        uri: &'a Url,
        document: &'a ParsedDocument,
        index: &'a WorkspaceIndex,
        model: &'a dyn TypeLookup,
    ) -> Self {
        let root = document.tree.root_node();
        let mut declared_names = HashSet::new();
        collect_declared_names(root, &document.text, &mut declared_names);
        Self {
            uri,
            text: &document.text,
            tree: &document.tree,
            index,
            model,
            imports: collect_imports(&root, &document.text),
            package: types::file_package(&document.tree, &document.text),
            package_line: package_line(&document.tree),
            declared_names,
            out: Vec::new(),
        }
    }

    /// One pass over the tree, dispatching on the constructs that can carry an
    /// unresolved symbol.
    fn visit(&mut self, node: Node) {
        match node.kind() {
            "import_declaration" => self.check_import(node),
            "object_creation_expression"
            | "cast_expression"
            | "field_declaration"
            | "constant_declaration"
            | "local_variable_declaration"
            | "formal_parameter"
            | "spread_parameter"
            | "method_declaration"
            | "enhanced_for_statement" => {
                if let Some(ty) = node.child_by_field_name("type") {
                    self.check_type(ty);
                }
            }
            "superclass" => {
                if let Some(ty) = node.named_child(0) {
                    self.check_type(ty);
                }
            }
            // An `implements`/`extends` clause wraps a `type_list`; checking the
            // list's members is enough.
            "type_list" => {
                let mut cursor = node.walk();
                let types: Vec<Node> = node.named_children(&mut cursor).collect();
                for ty in types {
                    self.check_type(ty);
                }
            }
            "method_invocation" | "field_access" => self.check_member(node),
            "identifier" => self.check_identifier(node),
            _ => {}
        }
        let mut cursor = node.walk();
        let children: Vec<Node> = node.named_children(&mut cursor).collect();
        for child in children {
            self.visit(child);
        }
    }

    /// Flags a simple, unqualified type name that neither an import, a type
    /// parameter, nor the file's visible types account for. Qualified names
    /// (`scoped_type_identifier`) and `var` are left alone; generic arguments
    /// are not inspected, only the type's base name.
    fn check_type(&mut self, node: Node) {
        let Some(name_node) = base_type_name(node) else {
            return;
        };
        let name = &self.text[name_node.byte_range()];
        if name.is_empty() || name == "var" {
            return;
        }
        let scope = types::scope_at(name_node, self.text, self.tree, self.model);
        if scope.type_params.iter().any(|param| param == name) {
            return;
        }
        // A single-type or static import binds the name; the import check owns
        // whether that binding resolves (D5).
        if scope
            .imports
            .iter()
            .any(|import| !import.is_wildcard && import.simple_name() == Some(name))
        {
            return;
        }
        if self.type_visible(name, &scope) {
            return;
        }
        if self.type_candidates(name).is_empty() {
            let data = json!({ "fix": FIX_CREATE_TYPE, "name": name, "kind": "class" });
            self.push(
                name_node,
                CODE_UNRESOLVED_TYPE,
                format!("cannot resolve type `{name}`"),
                data,
            );
        } else {
            let candidates = self.type_candidates(name);
            let importable = self.importable(&candidates);
            let data = json!({ "fix": FIX_ADD_IMPORT, "name": name, "candidates": importable });
            self.push(
                name_node,
                CODE_UNRESOLVED_TYPE,
                format!("cannot resolve type `{name}`"),
                data,
            );
        }
    }

    /// Whether a simple type name is visible here: declared in the file's
    /// package, reached by a wildcard import, or in `java.lang`. Consulted
    /// against both the index and the declared-type model.
    fn type_visible(&self, name: &str, scope: &types::Scope) -> bool {
        let in_package =
            |package: Option<&str>| {
                self.index.query_name(name).iter().any(|entry| {
                    types::is_type_kind(entry.kind) && entry.package.as_deref() == package
                }) || self.model.find_in_package(name, package).is_some()
            };
        if in_package(scope.package.as_deref()) {
            return true;
        }
        for import in scope
            .imports
            .iter()
            .filter(|import| import.is_wildcard && !import.is_static)
        {
            if in_package(import.package().as_deref()) {
                return true;
            }
        }
        in_package(Some("java.lang"))
    }

    fn type_candidates(&self, name: &str) -> Vec<SymbolEntry> {
        self.index
            .query_name(name)
            .into_iter()
            .filter(|entry| types::is_type_kind(entry.kind))
            .collect()
    }

    /// The fully-qualified import targets among `candidates` that an `import`
    /// edit can actually add — `import_edit` already excludes same-file,
    /// same-package, `java.lang`, already-imported, and conflicting names.
    fn importable(&self, candidates: &[SymbolEntry]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for entry in candidates {
            if import_edit(
                self.uri,
                entry,
                self.package.as_deref(),
                self.package_line,
                &self.imports,
                self.index,
            )
            .is_empty()
            {
                continue;
            }
            if let Some(target) = import_target(entry) {
                if !out.contains(&target) {
                    out.push(target);
                }
            }
        }
        out
    }

    /// Flags a member the receiver's known type does not declare. The receiver
    /// must be a modelled reference type (never `Unknown`, an array, or a type
    /// variable), so a member the model cannot enumerate is never reported.
    fn check_member(&mut self, node: Node) {
        let (object, name_node, is_call) = match node.kind() {
            "method_invocation" => (
                node.child_by_field_name("object"),
                node.child_by_field_name("name"),
                true,
            ),
            "field_access" => (
                node.child_by_field_name("object"),
                node.child_by_field_name("field"),
                false,
            ),
            _ => return,
        };
        let (Some(object), Some(name_node)) = (object, name_node) else {
            return;
        };
        if matches!(
            object.kind(),
            "scoped_identifier" | "scoped_type_identifier"
        ) {
            return;
        }
        let name = &self.text[name_node.byte_range()];
        if name.is_empty() {
            return;
        }
        let scope = types::scope_at(node, self.text, self.tree, self.model);
        let package = scope.package.as_deref();
        let receiver = types::receiver_type(&object, self.text, &scope, self.model);
        if matches!(receiver, Ty::Unknown | Ty::Array(_)) {
            return;
        }
        if self.model.lookup(&receiver, package).is_none() {
            return;
        }
        if types::member_of(&receiver, name, self.model, package).is_some() {
            return;
        }
        if is_call {
            let arg_count = node
                .child_by_field_name("arguments")
                .map(|args| args.named_child_count())
                .unwrap_or(0);
            if types::member_for_call(&receiver, name, arg_count, self.model, package).is_some() {
                return;
            }
        }
        let mut names: Vec<String> = self
            .model
            .members(&receiver, package)
            .into_iter()
            .map(|member| member.name)
            .collect();
        names.sort();
        names.dedup();
        let data = match nearest_name(name, &names) {
            Some(replacement) => {
                json!({ "fix": FIX_RENAME, "name": name, "replacement": replacement })
            }
            None => json!({ "fix": "" }),
        };
        self.push(
            name_node,
            CODE_UNRESOLVED_MEMBER,
            format!("cannot resolve `{name}`"),
            data,
        );
    }

    /// Flags a bare name used as a symbol reference that resolves to no local,
    /// field, member, type, or import.
    fn check_identifier(&mut self, node: Node) {
        if !self.is_symbolic_identifier(node) {
            return;
        }
        let name = &self.text[node.byte_range()];
        if name.is_empty() {
            return;
        }
        // A name bound anywhere in the file is left alone: lambda parameters,
        // catch bindings, and pattern variables are not all modelled by
        // `scope_at`, and a false positive is worse than a missed one.
        if self.declared_names.contains(name) {
            return;
        }
        let scope = types::scope_at(node, self.text, self.tree, self.model);
        if scope.type_params.iter().any(|param| param == name) {
            return;
        }
        if scope
            .imports
            .iter()
            .any(|import| !import.is_wildcard && import.simple_name() == Some(name))
        {
            return;
        }
        if self.type_visible(name, &scope) {
            return;
        }
        if let Some(resolved) = types::resolve_name(name, &scope, self.model) {
            if !resolved.is_type {
                return; // a local, field, parameter, or enclosing member
            }
        }
        // A name the model knows as a type, used where a type is not expected,
        // is a missing import; otherwise it is an unknown symbol with a stub.
        let candidates = self.type_candidates(name);
        let importable = self.importable(&candidates);
        if !importable.is_empty() {
            let data = json!({ "fix": FIX_ADD_IMPORT, "name": name, "candidates": importable });
            self.push(
                node,
                CODE_UNRESOLVED_SYMBOL,
                format!("cannot resolve `{name}`"),
                data,
            );
            return;
        }
        // Any non-type declaration with this name elsewhere may be an
        // outer-class field or a static-imported member this file cannot see; a
        // wrong "create" would be worse than silence.
        if self
            .index
            .query_name(name)
            .iter()
            .any(|entry| !types::is_type_kind(entry.kind))
        {
            return;
        }
        let is_call = node.parent().is_some_and(|parent| {
            parent.kind() == "method_invocation"
                && parent
                    .child_by_field_name("name")
                    .is_some_and(|named| named.id() == node.id())
        });
        let data = json!({
            "fix": FIX_CREATE_MEMBER,
            "name": name,
            "kind": if is_call { "method" } else { "field" },
        });
        self.push(
            node,
            CODE_UNRESOLVED_SYMBOL,
            format!("cannot resolve `{name}`"),
            data,
        );
    }

    /// Whether `node` is a bare identifier used as a symbol reference: not a
    /// declaration's own name, not a qualified-name segment, and not a member
    /// the member check already owns, and in a recognized expression position.
    fn is_symbolic_identifier(&self, node: Node) -> bool {
        if node.kind() != "identifier" {
            return false;
        }
        let Some(parent) = node.parent() else {
            return false;
        };
        if is_declared_name(node, &parent) {
            return false;
        }
        // The member check owns these two positions.
        if parent.kind() == "field_access"
            && parent
                .child_by_field_name("field")
                .is_some_and(|field| field.id() == node.id())
        {
            return false;
        }
        if parent.kind() == "method_invocation"
            && parent.child_by_field_name("object").is_some()
            && parent
                .child_by_field_name("name")
                .is_some_and(|named| named.id() == node.id())
        {
            return false;
        }
        matches!(
            parent.kind(),
            "argument_list"
                | "assignment_expression"
                | "binary_expression"
                | "unary_expression"
                | "parenthesized_expression"
                | "ternary_expression"
                | "array_access"
                | "array_initializer"
                | "return_statement"
                | "throw_statement"
                | "expression_statement"
                | "if_statement"
                | "while_statement"
                | "do_statement"
                | "enhanced_for_statement"
                | "variable_declarator"
                | "method_invocation"
                | "field_access"
        )
    }

    /// Flags an `import` whose target the index cannot supply: a single type by
    /// package and name, a wildcard by package existence, a static import by
    /// its type (and named member).
    fn check_import(&mut self, node: Node) {
        let raw = self.text[node.byte_range()].trim();
        let raw = raw.strip_prefix("import").map_or(raw, str::trim);
        let is_static = raw.starts_with("static");
        let raw = raw.strip_prefix("static").map_or(raw, str::trim);
        let path = raw.trim_end_matches(';').trim();
        if path.is_empty() {
            return;
        }
        let (path, is_wildcard) = match path.strip_suffix(".*") {
            Some(head) => (head, true),
            None => (path, false),
        };
        let resolved = if is_static {
            self.static_import_resolves(path, is_wildcard)
        } else if is_wildcard {
            self.index.has_package(path)
        } else {
            self.type_import_resolves(path)
        };
        if resolved {
            return;
        }
        self.push(
            node,
            CODE_UNRESOLVED_IMPORT,
            format!("cannot resolve import `{path}`"),
            json!({ "fix": "" }),
        );
    }

    /// Whether an `import a.b.C;` (possibly a nested `a.b.Outer.Inner`) names a
    /// type the index or the declared-type model holds.
    fn type_import_resolves(&self, path: &str) -> bool {
        let simple = path.rsplit('.').next().unwrap_or(path);
        if self.index.query_name(simple).iter().any(|entry| {
            types::is_type_kind(entry.kind) && import_target(entry).as_deref() == Some(path)
        }) {
            return true;
        }
        match path.rsplit_once('.') {
            Some((package, _)) => self.model.find_in_package(simple, Some(package)).is_some(),
            None => self.model.find_in_package(simple, None).is_some(),
        }
    }

    fn static_import_resolves(&self, path: &str, is_wildcard: bool) -> bool {
        let owner = if is_wildcard {
            path
        } else {
            match path.rsplit_once('.') {
                Some((owner, _)) => owner,
                None => return false,
            }
        };
        let owner_simple = owner.rsplit('.').next().unwrap_or(owner);
        let owner_in_index = self.index.query_name(owner_simple).iter().any(|entry| {
            types::is_type_kind(entry.kind) && import_target(entry).as_deref() == Some(owner)
        });
        let owner_in_model = match owner.rsplit_once('.') {
            Some((package, _)) => self
                .model
                .find_in_package(owner_simple, Some(package))
                .is_some(),
            None => false,
        };
        if !(owner_in_index || owner_in_model) {
            return false;
        }
        if is_wildcard {
            return true;
        }
        let member = path.rsplit('.').next().unwrap_or(path);
        let owner_ref = Ty::reference(owner);
        types::member_of(&owner_ref, member, self.model, None).is_some()
            || self
                .model
                .members(&owner_ref, None)
                .iter()
                .any(|candidate| candidate.name == member)
    }

    fn push(&mut self, node: Node, code: &str, message: String, data: serde_json::Value) {
        self.out.push(Diagnostic {
            range: lsp_range(self.text, &node),
            severity: Some(DiagnosticSeverity::ERROR),
            code: Some(NumberOrString::String(code.to_string())),
            code_description: None,
            source: Some("java-lsp".to_string()),
            message,
            related_information: None,
            tags: None,
            data: Some(data),
        });
    }
}

/// The simple type name a type node denotes, looking through `generic_type` and
/// `array_type`; a qualified (`scoped_type_identifier`) name is left alone.
fn base_type_name(node: Node) -> Option<Node> {
    match node.kind() {
        "type_identifier" => Some(node),
        "generic_type" => node.child_by_field_name("type").and_then(base_type_name),
        "array_type" => node.child_by_field_name("element").and_then(base_type_name),
        _ => None,
    }
}

/// The line after the file's `package` declaration, where an import goes when
/// there are none yet.
fn package_line(tree: &Tree) -> Option<u32> {
    let root = tree.root_node();
    let mut cursor = root.walk();
    let line = root
        .children(&mut cursor)
        .find(|child| child.kind() == "package_declaration")
        .map(|child| (child.end_position().row + 1) as u32);
    line
}

/// Whether an identifier node is a declaration's own name rather than a use.
fn is_declared_name(node: Node, parent: &Node) -> bool {
    let is_name = parent
        .child_by_field_name("name")
        .is_some_and(|named| named.id() == node.id());
    if !is_name {
        return false;
    }
    matches!(
        parent.kind(),
        "variable_declarator"
            | "method_declaration"
            | "constructor_declaration"
            | "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
            | "formal_parameter"
            | "spread_parameter"
            | "catch_formal_parameter"
            | "lambda_parameter"
            | "enhanced_for_statement"
            | "local_variable_declaration"
            | "field_declaration"
            | "annotation_type_element_declaration"
    )
}

/// Collects every simple name the document binds: any identifier that is a
/// parent's `name` field, plus lambda parameters. Used as a conservative guard
/// so an unmodelled binding is never reported as unknown.
fn collect_declared_names(node: Node, text: &str, out: &mut HashSet<String>) {
    if node.kind() == "identifier" {
        let is_binding = node.parent().is_some_and(|parent| {
            parent
                .child_by_field_name("name")
                .is_some_and(|named| named.id() == node.id())
                || matches!(parent.kind(), "inferred_parameters" | "lambda_parameters")
        });
        if is_binding {
            out.insert(text[node.byte_range()].to_string());
        }
    }
    let mut cursor = node.walk();
    let children: Vec<Node> = node.named_children(&mut cursor).collect();
    for child in children {
        collect_declared_names(child, text, out);
    }
}

/// The innermost type body enclosing `offset`, for inserting a created member.
fn enclosing_type_body(tree: &Tree, offset: usize) -> Option<Node<'_>> {
    let mut node = tree.root_node().descendant_for_byte_range(offset, offset)?;
    loop {
        if matches!(
            node.kind(),
            "class_body" | "interface_body" | "enum_body" | "record_body" | "annotation_type_body"
        ) {
            return Some(node);
        }
        node = node.parent()?;
    }
}

/// A trivial source stub for a created type file.
fn stub_type_source(package: Option<&str>, kind: &str, name: &str) -> String {
    let keyword = if kind == "interface" {
        "interface"
    } else {
        "class"
    };
    let body = format!("public {keyword} {name} {{\n}}\n");
    match package {
        Some(package) => format!("package {package};\n\n{body}"),
        None => body,
    }
}

/// `name` if it is a legal Java identifier, so a create-stub cannot inject
/// arbitrary text into a generated file or member.
fn is_java_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_alphabetic() || c == '_' || c == '$' => {}
        _ => return false,
    }
    chars.all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

/// A workspace edit holding plain text changes for one document.
fn workspace_edit_changes(uri: &Url, edits: Vec<TextEdit>) -> WorkspaceEdit {
    WorkspaceEdit {
        changes: Some(HashMap::from([(uri.clone(), edits)])),
        document_changes: None,
        change_annotations: None,
    }
}

/// The closest candidate to `name` within a small edit distance, for a
/// did-you-mean fix.
fn nearest_name(name: &str, candidates: &[String]) -> Option<String> {
    let mut best: Option<(usize, &String)> = None;
    for candidate in candidates {
        if candidate == name {
            continue;
        }
        let distance = edit_distance(name, candidate);
        if distance > 2 {
            continue;
        }
        if best.is_none_or(|(best_distance, best_name)| {
            distance < best_distance || (distance == best_distance && candidate < best_name)
        }) {
            best = Some((distance, candidate));
        }
    }
    best.map(|(_, candidate)| candidate.clone())
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            current[j + 1] = (previous[j + 1] + 1)
                .min(current[j] + 1)
                .min(previous[j] + cost);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
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

/// The declared overloads of a method entry, resolved through the type model by
/// the entry's owning type. `None` when the model cannot name that type, so the
/// caller falls back to a single name-only item.
fn model_overloads(model: &dyn TypeLookup, entry: &SymbolEntry) -> Option<Vec<Member>> {
    let owner = entry.container.last()?;
    let info = model
        .find_in_package(owner, entry.package.as_deref())
        .or_else(|| model.find_unique(owner, entry.package.as_deref()))?;
    let overloads: Vec<Member> = info
        .methods
        .iter()
        .filter(|member| member.name == entry.name)
        .cloned()
        .collect();
    (!overloads.is_empty()).then_some(overloads)
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
    filter_text: String,
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
        filter_text: Some(filter_text),
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
    fn definition_reaches_extracted_library_sources() {
        let text = "class Use {\n    Gson gson;\n}\n";
        let engine = engine_with(text);
        let cache_uri = Url::parse(
            "file:///home/u/.cache/java-lsp/sources/com.google.code.gson/gson/2.10.1/com/google/gson/Gson.java",
        )
        .unwrap();
        let range = Range::new(Position::new(30, 13), Position::new(30, 17));
        let library_entry = SymbolEntry {
            uri: cache_uri.clone(),
            name: "Gson".to_string(),
            kind: IndexKind::Class,
            package: Some("com.google.gson".to_string()),
            container: Vec::new(),
            full_range: range,
            selection_range: range,
            dependency: true,
            library_source: true,
        };
        engine.index.upsert_file(&cache_uri, vec![library_entry]);

        // An extracted source is a location the editor can open.
        let location =
            location_at(&engine, text, "Gson gson", 2).expect("a library source must resolve");
        assert_eq!(location.uri, cache_uri);
        assert_eq!(location.range, range);

        // A plain class-file entry (no openable source) is still refused.
        let jar_uri = Url::parse("file:///repo/lib-1.0.jar").unwrap();
        let jar_entry = SymbolEntry {
            uri: jar_uri.clone(),
            name: "JarOnly".to_string(),
            kind: IndexKind::Class,
            package: Some("demo".to_string()),
            container: Vec::new(),
            full_range: range,
            selection_range: range,
            dependency: true,
            library_source: false,
        };
        let jar_text = "class Use {\n    JarOnly x;\n}\n";
        let jar_engine = engine_with(jar_text);
        jar_engine.index.upsert_file(&jar_uri, vec![jar_entry]);
        assert!(location_at(&jar_engine, jar_text, "JarOnly x", 2).is_none());
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
    fn definition_selects_the_overload_matching_the_arguments() {
        let text = "\
class Calc {
    int add(int a) { return a; }
    int add(int a, int b) { return a + b; }
    int total = add(1, 2);
}
";
        let engine = engine_with(text);
        let location = location_at(&engine, text, "= add", 3).expect("overloads must resolve");
        // The two-argument call selects the two-argument overload on line 2, not
        // the first-declared one on line 1.
        assert_eq!(
            location.range.start.line, 2,
            "expected the (int, int) overload: {location:?}"
        );
        assert_eq!(
            location.range.end.character - location.range.start.character,
            3,
            "the location names `add`: {location:?}"
        );
    }

    #[test]
    fn references_report_only_the_selected_overloads_calls() {
        let text = "\
class Calc {
    int add(int a) { return a; }
    int add(String s) { return 0; }
    void use() {
        add(1);
        add(\"x\");
    }
}
";
        let engine = engine_with(text);
        let offset = text.find("add(1)").unwrap() + 1; // inside `add`
        let references = engine.references(&uri(), lsp_position(text, offset), true);
        // The `add(int)` declaration (line 1) and its one call (line 4) — never
        // the `add(String)` declaration or its call.
        let lines: Vec<u32> = references
            .iter()
            .map(|location| location.range.start.line)
            .collect();
        assert_eq!(lines, [1, 4], "{references:?}");
    }

    #[test]
    fn definition_falls_back_to_arity_when_argument_types_are_unknown() {
        let text = "\
class Calc {
    int add(int a) { return a; }
    int add(int a, int b) { return a + b; }
    int total = add(unknown, 2);
}
";
        let engine = engine_with(text);
        let location = location_at(&engine, text, "= add", 3).expect("arity still decides");
        // The first argument's type is unknown, so the two-argument overload is
        // still selected by arity.
        assert_eq!(location.range.start.line, 2, "{location:?}");
    }

    #[test]
    fn navigation_honors_overloaded_argument_types_for_float_arguments() {
        let text = "\
record Data(int number) {
    void test() {}
    void test(int count) {}
    void test(double number) {}
}
class Use {
    void m() {
        Data data = new Data(1);
        data.test(5.0f);
    }
}
";
        let engine = engine_with(text);
        let offset = text.find("data.test(5.0f)").unwrap() + 6;

        // Definition lands on the `double` overload (line 3), not `int` (line 2).
        let location = location_at(&engine, text, "data.test(5.0f)", 6).expect("a target");
        assert_eq!(
            location.range.start.line, 3,
            "expected the double overload: {location:?}"
        );

        // References report the `double` declaration and its call only.
        let references = engine.references(&uri(), lsp_position(text, offset), true);
        let lines: Vec<u32> = references
            .iter()
            .map(|location| location.range.start.line)
            .collect();
        assert_eq!(lines, [3, 8], "{references:?}");

        // References on the `int` declaration never include the `double` call.
        let int_decl = lsp_position(text, text.find("test(int count)").unwrap());
        let int_refs = engine.references(&uri(), int_decl, false);
        assert!(int_refs.is_empty(), "no (int) calls here: {int_refs:?}");

        // References on the `double` declaration include its one call.
        let double_decl = lsp_position(text, text.find("test(double number)").unwrap());
        let double_refs = engine.references(&uri(), double_decl, false);
        let decl_lines: Vec<u32> = double_refs
            .iter()
            .map(|location| location.range.start.line)
            .collect();
        assert_eq!(decl_lines, [8], "{double_refs:?}");

        // The parameter hint names the `double` overload's parameter.
        let hints = hints_in(&engine, full_range(text));
        let argument = lsp_position(text, text.find("5.0f").unwrap());
        assert!(
            hints.contains(&(argument, "number:".to_string())),
            "expected `number:` on the argument: {hints:?}"
        );
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

        // The workspace method is offered as its overload(s), labelled with the
        // signature but still filtered and inserted by its bare name.
        let method = items
            .iter()
            .find(|i| i.insert_text.as_deref() == Some("sumIt("))
            .unwrap();
        assert_eq!(method.label, "void sumIt()");
        assert_eq!(method.filter_text.as_deref(), Some("sumIt"));
        assert_eq!(method.kind, Some(CompletionItemKind::METHOD));
        assert_eq!(method.detail.as_deref(), Some("method of summer"));
        assert_eq!(method.sort_text.as_deref(), Some("2void sumIt()"));

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
                assert!(names.iter().any(|n| n.starts_with("void run")), "{names:?}");
                assert!(
                    names.iter().any(|n| n.starts_with("void inherited")),
                    "inherited member missing: {names:?}"
                );
            }
            other => panic!("expected member items, got {other:?}"),
        }
    }

    #[test]
    fn member_completions_list_each_overload_with_its_signature() {
        let text = "\
class Calc {
    int add(int a) { return a; }
    int add(int a, int b) { return a + b; }
}
class Use {
    void m() {
        Calc c = null;
        c.
    }
}
";
        let engine = engine_with(text);
        let offset = text.find("c.\n").unwrap() + "c.".len();
        match engine.completions(&uri(), lsp_position(text, offset)) {
            Some(CompletionResponse::Array(items)) => {
                let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
                assert!(labels.contains(&"int add(int a)"), "{labels:?}");
                assert!(labels.contains(&"int add(int a, int b)"), "{labels:?}");
                let item = items
                    .iter()
                    .find(|item| item.label == "int add(int a, int b)")
                    .unwrap();
                assert_eq!(item.kind, Some(CompletionItemKind::METHOD));
                assert_eq!(item.insert_text.as_deref(), Some("add("));
                assert_eq!(item.filter_text.as_deref(), Some("add"));
            }
            other => panic!("expected member items, got {other:?}"),
        }
    }

    #[test]
    fn signature_help_lists_overloads_and_tracks_the_active_parameter() {
        let text = "\
class Calc {
    int add(int a) { return a; }
    int add(int a, int b) { return a + b; }
    void use() {
        add(1, 2);
    }
}
";
        let engine = engine_with(text);
        // The cursor sits in the second argument.
        let offset = text.find("1, ").unwrap() + "1, ".len();
        let help = engine
            .signature_help(&uri(), lsp_position(text, offset))
            .expect("signature help for a call");
        let labels: Vec<&str> = help
            .signatures
            .iter()
            .map(|signature| signature.label.as_str())
            .collect();
        assert!(labels.contains(&"int add(int a)"), "{labels:?}");
        assert!(labels.contains(&"int add(int a, int b)"), "{labels:?}");
        assert_eq!(
            help.signatures[0].active_parameter,
            Some(1),
            "the cursor is in the second argument"
        );
    }

    #[test]
    fn signature_help_is_none_without_a_resolvable_callee() {
        let text = "\
class Use {
    void m() {
        unknown(1);
    }
}
";
        let engine = engine_with(text);
        let offset = text.find("unknown(1)").unwrap() + "unknown(".len();
        assert!(engine
            .signature_help(&uri(), lsp_position(text, offset))
            .is_none());
    }

    #[test]
    fn hints_and_navigation_honor_float_and_int_overloads() {
        let text = "\
record Data(int number) {
    void test() {}
    void test(int count) {}
    void test(float number) {}
}
class Use {
    void m() {
        Data data = new Data(1);
        data.test(5);
        data.test(5.0f);
    }
}
";
        let engine = engine_with(text);

        // The float call selects test(float) (line 3); the int call test(int)
        // (line 2).
        let float_offset = text.find("data.test(5.0f)").unwrap() + 6;
        let location = engine
            .definition(&uri(), lsp_position(text, float_offset))
            .expect("a target");
        assert_eq!(location.range.start.line, 3, "{location:?}");
        let int_offset = text.find("data.test(5)").unwrap() + 6;
        let location = engine
            .definition(&uri(), lsp_position(text, int_offset))
            .expect("a target");
        assert_eq!(location.range.start.line, 2, "{location:?}");

        // Each hint names its own call's overload parameter.
        let hints = hints_in(&engine, full_range(text));
        let float_argument = lsp_position(text, text.find("5.0f").unwrap());
        assert!(
            hints.contains(&(float_argument, "number:".to_string())),
            "{hints:?}"
        );
        let int_argument = lsp_position(
            text,
            text.find("data.test(5)").unwrap() + "data.test(".len(),
        );
        assert!(
            hints.contains(&(int_argument, "count:".to_string())),
            "{hints:?}"
        );
    }

    #[test]
    fn parameter_hints_are_withheld_when_the_overload_is_unpinned() {
        let text = "\
class Sample {
    void run(int amount) {}
    void run(double total) {}
    void m() {
        run(unknown);
    }
}
";
        let engine = engine_with(text);
        let hints = hints_in(&engine, full_range(text));
        assert!(
            !hints
                .iter()
                .any(|(_, label)| label == "amount:" || label == "total:"),
            "no parameter name for an unpinned overload: {hints:?}"
        );
    }

    #[test]
    fn member_completions_offer_record_component_accessors() {
        let text = "\
record Point(int x, int y) {}
class Use {
    void m() {
        Point p = null;
        p.x();
    }
}
";
        let engine = engine_with(text);
        let offset = text.find("p.x();").unwrap() + "p.".len();
        match engine.completions(&uri(), lsp_position(text, offset)) {
            Some(CompletionResponse::Array(items)) => {
                let names: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
                assert!(names.contains(&"int x()"), "{names:?}");
                assert!(names.contains(&"int y()"), "{names:?}");
                // The component is the accessor `x()`, not a private field.
                let x = items
                    .iter()
                    .find(|item| item.insert_text.as_deref() == Some("x("))
                    .expect("x");
                assert_eq!(x.kind, Some(CompletionItemKind::METHOD));
                assert_eq!(x.label, "int x()");
            }
            other => panic!("expected member items, got {other:?}"),
        }

        // Narrowing by the typed prefix keeps only the matching component.
        let narrowed = text.replace("p.x();", "p.y();");
        let offset = narrowed.find("p.y();").unwrap() + "p.y".len();
        match engine_with(&narrowed).completions(&uri(), lsp_position(&narrowed, offset)) {
            Some(CompletionResponse::Array(items)) => {
                let names: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
                assert_eq!(names, ["int y()"], "{names:?}");
            }
            other => panic!("expected member items, got {other:?}"),
        }
    }

    #[test]
    fn a_record_without_components_offers_nothing_extra() {
        let text = "\
record Empty() {}
class Use {
    void m() {
        Empty e = null;
        e.none;
    }
}
";
        let engine = engine_with(text);
        let offset = text.find("e.none;").unwrap() + "e.".len();
        match engine.completions(&uri(), lsp_position(text, offset)) {
            Some(CompletionResponse::Array(items)) => {
                assert!(items.is_empty(), "a member-less record, got {items:?}");
            }
            other => panic!("expected member items, got {other:?}"),
        }
    }

    #[test]
    fn a_dot_at_line_end_keeps_its_receiver() {
        // The reported bug: `gson.` at the end of a line, with the next line
        // starting a new statement, parses `gson.var` as a scoped type
        // identifier, so the receiver must be recovered from the source.
        for following in ["var test = gson.size;", "int n = 1;", "Widget w = null;"] {
            let text = format!(
                "class Gson {{\n    int size;\n    void run() {{}}\n}}\nclass Widget {{}}\nclass Use {{\n    Gson gson;\n    void m() {{\n        gson.\n        {following}\n    }}\n}}\n"
            );
            let engine = engine_with(&text);
            let offset = text.find("gson.\n").expect("receiver") + "gson.".len();
            match engine.completions(&uri(), lsp_position(&text, offset)) {
                Some(CompletionResponse::Array(items)) => {
                    let names: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
                    assert!(names.contains(&"size"), "{following}: {names:?}");
                    assert!(
                        names.iter().any(|n| n.starts_with("void run")),
                        "{following}: {names:?}"
                    );
                }
                other => panic!("expected member items for {following}, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_dot_continuing_on_the_same_line_keeps_its_receiver() {
        let text = "\
class Gson {
    int size;
}
class Use {
    Gson gson;
    void m() {
        gson.size;
    }
}
";
        let engine = engine_with(text);
        let offset = text.find("gson.size").unwrap() + "gson.".len();
        match engine.completions(&uri(), lsp_position(text, offset)) {
            Some(CompletionResponse::Array(items)) => {
                let names: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
                assert!(names.contains(&"size"), "{names:?}");
            }
            other => panic!("expected member items, got {other:?}"),
        }
    }

    #[test]
    fn member_completion_reads_enhanced_for_bindings() {
        for binding in ["Widget w", "var w"] {
            let text = format!(
                "class Widget {{\n    int size;\n}}\nclass Use {{\n    void m(Widget[] ws) {{\n        for ({binding} : ws) {{\n            w.\n        }}\n    }}\n}}\n"
            );
            let engine = engine_with(&text);
            let offset = text.find("w.\n").expect("receiver") + "w.".len();
            match engine.completions(&uri(), lsp_position(&text, offset)) {
                Some(CompletionResponse::Array(items)) => {
                    let names: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
                    assert!(names.contains(&"size"), "{binding}: {names:?}");
                }
                other => panic!("expected member items for {binding}, got {other:?}"),
            }
        }
    }

    #[test]
    fn member_completion_reads_try_with_resources_bindings() {
        for binding in ["Widget w = open()", "var w = open()"] {
            let text = format!(
                "class Widget {{\n    int size;\n}}\nclass Use {{\n    static Widget open() {{ return null; }}\n    void m() {{\n        try ({binding}) {{\n            w.\n        }} catch (Exception e) {{\n        }}\n    }}\n}}\n"
            );
            let engine = engine_with(&text);
            let offset = text.find("w.\n").expect("receiver") + "w.".len();
            match engine.completions(&uri(), lsp_position(&text, offset)) {
                Some(CompletionResponse::Array(items)) => {
                    let names: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
                    assert!(names.contains(&"size"), "{binding}: {names:?}");
                }
                other => panic!("expected member items for {binding}, got {other:?}"),
            }
        }
    }

    #[test]
    fn member_completion_reads_a_ternary_initializer() {
        let text = "\
class Widget {
    int size;
}
class Use {
    void m(boolean c) {
        var w = c ? new Widget() : null;
        w.
    }
}
";
        let engine = engine_with(text);
        let offset = text.find("w.\n").unwrap() + "w.".len();
        match engine.completions(&uri(), lsp_position(text, offset)) {
            Some(CompletionResponse::Array(items)) => {
                let names: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
                assert!(names.contains(&"size"), "{names:?}");
            }
            other => panic!("expected member items, got {other:?}"),
        }
    }

    fn method_member(name: &str, ty: Ty) -> Member {
        Member {
            name: name.to_string(),
            kind: IndexKind::Method,
            ty,
            params: Vec::new(),
            type_params: Vec::new(),
            is_static: false,
        }
    }

    #[test]
    fn a_var_from_an_object_method_infers_and_offers_its_result_members() {
        let engine = TreeSitterEngine::new();
        let mut model = TypeModel::new();
        let mut object = crate::types::TypeInfo::new(
            "Object".to_string(),
            Some("java.lang".to_string()),
            IndexKind::Class,
        );
        object
            .methods
            .push(method_member("toString", Ty::reference("String")));
        model.insert(object);
        let mut string = crate::types::TypeInfo::new(
            "String".to_string(),
            Some("java.lang".to_string()),
            IndexKind::Class,
        );
        string
            .methods
            .push(method_member("length", Ty::Prim(crate::types::Prim::Int)));
        model.insert(string);
        engine.index.set_types(std::sync::Arc::new(model));

        let text = "\
class Gson {}
class Use {
    Gson gson;
    void m() {
        var x = gson.toString();
        x.
    }
}
";
        engine.open(&uri(), text);

        // The inferred type renders as a hint.
        let hints = hints_in(&engine, full_range(text));
        assert!(
            hints.contains(&(after(text, "var x"), ": String".to_string())),
            "{hints:?}"
        );

        // `x.` offers `String`'s members, and no inherited `Object` member.
        let offset = text.find("x.\n").unwrap() + "x.".len();
        match engine.completions(&uri(), lsp_position(text, offset)) {
            Some(CompletionResponse::Array(items)) => {
                let names: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
                assert!(
                    names.iter().any(|n| n.starts_with("int length")),
                    "{names:?}"
                );
                assert!(!names.iter().any(|n| n.contains("toString")), "{names:?}");
            }
            other => panic!("expected member items, got {other:?}"),
        }
    }

    #[test]
    fn a_record_still_offers_its_declared_methods() {
        let text = "\
record Sized(int size) {
    int doubled() { return size * 2; }
}
class Use {
    void m() {
        Sized s = null;
        s.none;
    }
}
";
        let engine = engine_with(text);
        let offset = text.find("s.none;").unwrap() + "s.".len();
        match engine.completions(&uri(), lsp_position(text, offset)) {
            Some(CompletionResponse::Array(items)) => {
                let names: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
                assert!(names.contains(&"int size()"), "{names:?}");
                assert!(names.contains(&"int doubled()"), "{names:?}");
            }
            other => panic!("expected member items, got {other:?}"),
        }
    }

    #[test]
    fn hover_on_a_record_component_accessor_shows_its_signature() {
        let text = "\
record Point(int x, int y) {}
class Use {
    void m() {
        Point p = null;
        p.x();
    }
}
";
        let engine = engine_with(text);
        let offset = text.find("p.x();").unwrap() + "p.".len();
        let hover = engine
            .hover(&uri(), lsp_position(text, offset))
            .expect("hover");
        match hover.contents {
            HoverContents::Markup(markup) => {
                assert!(markup.value.contains("int x()"), "{}", markup.value);
                assert!(markup.value.contains("of `Point`"), "{}", markup.value);
            }
            other => panic!("expected markup, got {other:?}"),
        }
    }

    #[test]
    fn hover_on_a_record_declaration_shows_its_components() {
        let text = "record Point(int x, int y) {}\n";
        let engine = engine_with(text);
        let offset = text.find("Point").unwrap() + 1;
        let hover = engine
            .hover(&uri(), lsp_position(text, offset))
            .expect("hover");
        match hover.contents {
            HoverContents::Markup(markup) => {
                assert!(
                    markup.value.contains("record Point(int x, int y)"),
                    "{}",
                    markup.value
                );
            }
            other => panic!("expected markup, got {other:?}"),
        }
    }

    #[test]
    fn navigation_targets_a_record_component_in_the_header() {
        let text = "\
record Point(int x, int y) {
    int sum() { return x + y; }
}
class Use {
    void m() {
        Point p = null;
        p.x();
    }
}
";
        let engine = engine_with(text);
        let cursor = lsp_position(text, text.find("p.x();").unwrap() + "p.".len());

        // Go-to-definition lands on the component in the header, not the body.
        let location = engine.definition(&uri(), cursor).expect("definition");
        assert_eq!(location.range.start.line, 0, "{location:?}");
        assert_eq!(location.range.start.character, 17, "{location:?}");

        // References cover the header, the unqualified use inside the record,
        // and the qualified use outside it.
        let references = engine.references(&uri(), cursor, true);
        assert_eq!(references.len(), 3, "{references:?}");

        // Rename edits every occurrence, the header declaration included.
        let edit = engine.rename(&uri(), cursor, "first").expect("rename");
        let changes = edit.changes.expect("changes");
        let edits = changes.values().next().expect("edits");
        assert_eq!(edits.len(), 3, "{edits:?}");
        assert!(edits.iter().all(|edit| edit.new_text == "first"));

        // The component is a workspace symbol.
        let symbols: Vec<(String, SymbolKind)> = engine
            .workspace_symbols("")
            .into_iter()
            .map(|symbol| (symbol.name, symbol.kind))
            .collect();
        assert!(
            symbols.contains(&("x".to_string(), SymbolKind::METHOD)),
            "{symbols:?}"
        );
        assert!(symbols.contains(&("Point".to_string(), SymbolKind::STRUCT)));
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
        let engine = engine_with_types(vec![
            jdk_entry("Object", IndexKind::Class, Some("java.lang")),
            jdk_entry("String", IndexKind::Class, Some("java.lang")),
        ]);
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
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            diagnostics[0].code,
            Some(NumberOrString::String("unresolved-type".to_string()))
        );
    }

    #[test]
    fn imported_and_wildcard_types_are_never_diagnosed() {
        let engine = engine_with_types(vec![
            jdk_entry("Object", IndexKind::Class, Some("java.lang")),
            jdk_entry("String", IndexKind::Class, Some("java.lang")),
            jdk_entry("Thing", IndexKind::Class, Some("com.external")),
            jdk_entry("List", IndexKind::Interface, Some("java.util")),
        ]);
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
        let diagnostics = engine.diagnostics(&uri());
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn an_unresolved_member_is_flagged_and_a_near_name_suggested() {
        let engine = engine_with_model(list_model());
        let text = "\
package com.a;

import java.util.List;

class Sample {
    void m() {
        List<String> xs = null;
        xs.sixe();
    }
}
";
        engine.open(&uri(), text);
        let diagnostics = engine.diagnostics(&uri());
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            diagnostics[0].code,
            Some(NumberOrString::String("unresolved-member".to_string()))
        );
        assert_eq!(
            diagnostics[0]
                .data
                .as_ref()
                .and_then(|data| data.get("replacement"))
                .and_then(|value| value.as_str()),
            Some("size")
        );
    }

    #[test]
    fn a_did_you_mean_action_renames_the_member() {
        let engine = engine_with_model(list_model());
        let text = "\
package com.a;

import java.util.List;

class Sample {
    void m() {
        List<String> xs = null;
        xs.sixe();
    }
}
";
        engine.open(&uri(), text);
        let diagnostics = engine.diagnostics(&uri());
        let actions = engine.code_actions(&uri(), &diagnostics);
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].title, "Change to `size`");
        let edits = actions[0]
            .edit
            .as_ref()
            .unwrap()
            .changes
            .as_ref()
            .unwrap()
            .get(&uri())
            .unwrap();
        assert_eq!(edits[0].new_text, "size");
    }

    #[test]
    fn a_type_known_elsewhere_offers_an_import() {
        let engine = engine_with_types(vec![
            jdk_entry("Object", IndexKind::Class, Some("java.lang")),
            jdk_entry("String", IndexKind::Class, Some("java.lang")),
            jdk_entry("Widget", IndexKind::Class, Some("com.b")),
        ]);
        let text = "\
class Sample {
    Widget field;
}
";
        engine.open(&uri(), text);
        let diagnostics = engine.diagnostics(&uri());
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].code,
            Some(NumberOrString::String("unresolved-type".to_string()))
        );
        let candidates = diagnostics[0]
            .data
            .as_ref()
            .and_then(|data| data.get("candidates"))
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        assert_eq!(candidates, vec![serde_json::json!("com.b.Widget")]);

        let actions = engine.code_actions(&uri(), &diagnostics);
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].title, "Add import `com.b.Widget`");
        let edits = actions[0]
            .edit
            .as_ref()
            .unwrap()
            .changes
            .as_ref()
            .unwrap()
            .get(&uri())
            .unwrap();
        assert_eq!(edits[0].new_text, "import com.b.Widget;\n");
    }

    #[test]
    fn a_create_type_action_needs_the_client_capability() {
        let engine = unknown_symbol_engine();
        let text = "\
class Sample {
    Widget field;
}
";
        engine.open(&uri(), text);
        let diagnostics = engine.diagnostics(&uri());
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");

        // Without the client capability, no create-type action is offered.
        assert!(engine.code_actions(&uri(), &diagnostics).is_empty());

        engine.set_resource_operations(true);
        let actions = engine.code_actions(&uri(), &diagnostics);
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].title, "Create class `Widget`");
        assert!(actions[0]
            .edit
            .as_ref()
            .and_then(|edit| edit.document_changes.as_ref())
            .is_some());
    }

    #[test]
    fn an_unknown_identifier_offers_a_member_stub() {
        let engine = unknown_symbol_engine();
        let text = "\
class Sample {
    void m() {
        missing = 1;
    }
}
";
        engine.open(&uri(), text);
        let diagnostics = engine.diagnostics(&uri());
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].code,
            Some(NumberOrString::String("unresolved-symbol".to_string()))
        );
        let actions = engine.code_actions(&uri(), &diagnostics);
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].title, "Create field `missing`");
        let edits = actions[0]
            .edit
            .as_ref()
            .unwrap()
            .changes
            .as_ref()
            .unwrap()
            .get(&uri())
            .unwrap();
        assert!(
            edits[0].new_text.contains("private Object missing;"),
            "{:?}",
            edits[0].new_text
        );
    }

    #[test]
    fn an_unresolvable_import_is_flagged() {
        let engine = unknown_symbol_engine();
        let text = "\
package com.a;

import com.b.Nope;

class Sample {
}
";
        engine.open(&uri(), text);
        let diagnostics = engine.diagnostics(&uri());
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].code,
            Some(NumberOrString::String("unresolved-import".to_string()))
        );
        assert!(diagnostics[0].message.contains("com.b.Nope"));
    }

    #[test]
    fn a_wildcard_import_of_an_unknown_package_is_flagged() {
        let engine = unknown_symbol_engine();
        let text = "\
import com.unknown.*;

class Sample {
}
";
        engine.open(&uri(), text);
        let diagnostics = engine.diagnostics(&uri());
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].code,
            Some(NumberOrString::String("unresolved-import".to_string()))
        );
    }

    #[test]
    fn a_static_import_checks_its_member() {
        let engine = engine_with_model(source_model(
            "java.util",
            "public class Collections {\n    public static void sort(Object o) {\n    }\n}\n",
        ));
        let text = "\
import static java.util.Collections.sort;
import static java.util.Collections.sortt;

class Sample {
}
";
        engine.open(&uri(), text);
        let diagnostics = engine.diagnostics(&uri());
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].message.contains("sortt"));
    }

    #[test]
    fn a_lambda_parameter_is_not_an_unknown_symbol() {
        // `x` in the lambda body is a binding `scope_at` does not model; the
        // declared-name guard must keep it from being reported.
        let engine = engine_with_model(list_model());
        let text = "\
package com.a;

class Sample {
    void m() {
        var f = (int x) -> x + 1;
    }
}
";
        engine.open(&uri(), text);
        let diagnostics = engine.diagnostics(&uri());
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn semantic_diagnostics_can_be_switched_off() {
        let engine = unknown_symbol_engine();
        let text = "class Sample {\n    Missing field;\n}\n";
        engine.open(&uri(), text);
        assert_eq!(engine.diagnostics(&uri()).len(), 1);
        engine.set_semantic_diagnostics(false);
        assert!(engine.diagnostics(&uri()).is_empty());
    }

    #[test]
    fn the_semantic_diagnostics_setting_maps_the_environment_value() {
        assert!(semantic_diagnostics_setting(None));
        assert!(semantic_diagnostics_setting(Some("1")));
        assert!(semantic_diagnostics_setting(Some("true")));
        assert!(!semantic_diagnostics_setting(Some("0")));
        assert!(!semantic_diagnostics_setting(Some("false")));
        assert!(!semantic_diagnostics_setting(Some("FALSE")));
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
            library_source: false,
        }
    }

    /// A synthetic dependency entry in a given package.
    fn jdk_entry(name: &str, kind: IndexKind, package: Option<&str>) -> SymbolEntry {
        let mut entry = synthetic_entry(name, kind);
        entry.package = package.map(str::to_string);
        entry
    }

    /// An engine whose index and declared-type model both hold `entries`, so
    /// the resolution and the import checks see the same world.
    fn engine_with_types(entries: Vec<SymbolEntry>) -> TreeSitterEngine {
        let engine = TreeSitterEngine::new();
        let jar = Url::parse("file:///jdk.jar").unwrap();
        engine.index.upsert_file(&jar, entries.clone());
        engine
            .index
            .set_types(std::sync::Arc::new(TypeModel::from_entries(&entries)));
        engine
    }

    /// A workspace type model from a Java source snippet in `package`, with
    /// `java.lang.Object`/`String` added so the diagnostics gate is satisfied.
    fn source_model(package: &str, source: &str) -> TypeModel {
        let mut parser = java_parser();
        let tree = parser.parse(source.as_bytes(), None).expect("parse");
        let mut model = TypeModel::new();
        model.extend(types::collect_type_infos(Some(package), &tree, source));
        model.insert(types::TypeInfo::new(
            "Object".to_string(),
            Some("java.lang".to_string()),
            IndexKind::Class,
        ));
        model.insert(types::TypeInfo::new(
            "String".to_string(),
            Some("java.lang".to_string()),
            IndexKind::Class,
        ));
        model
    }

    fn engine_with_model(model: TypeModel) -> TreeSitterEngine {
        let engine = TreeSitterEngine::new();
        engine.index.set_types(std::sync::Arc::new(model));
        engine
    }

    fn list_model() -> TypeModel {
        source_model(
            "java.util",
            "public interface List<E> {\n    int size();\n    boolean isEmpty();\n}\n",
        )
    }

    fn unknown_symbol_engine() -> TreeSitterEngine {
        engine_with_types(vec![
            jdk_entry("Object", IndexKind::Class, Some("java.lang")),
            jdk_entry("String", IndexKind::Class, Some("java.lang")),
        ])
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
    fn a_var_enhanced_for_binding_gets_its_element_type_hint() {
        let text = "class Widget {\n    void run() {}\n}\nclass Sample {\n    void m(Widget[] ws) {\n        for (var w : ws) {\n            w.run();\n        }\n    }\n}\n";
        let engine = engine_with(text);
        let hints = hints_in(&engine, full_range(text));
        assert!(
            hints.contains(&(after(text, "var w"), ": Widget".to_string())),
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
