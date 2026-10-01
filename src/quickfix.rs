//! The quick-fix subsystem: the create/import/rename quick fixes as their own
//! subsystem.
//!
//! Quick fixes are generated from the diagnostics the client is showing. The
//! subsystem owns a parser and the open buffers' text (so it parses each buffer
//! itself), and it answers from two other subsystems: it queries the **symbol
//! index** through `IndexHandle` and the **diagnostics cache** through
//! `DiagnosticsHandle`, so it holds no index or diagnostics state of its own.
//!
//! The subsystem runs on its own thread behind a blocking [`QuickFixHandle`],
//! mirroring `IndexHandle`: the engine dispatches a `codeActions` request to it
//! on a spawned task, so a fix never delays typing.

use std::collections::HashMap;
use std::sync::Arc;
use std::thread;

use tower_lsp::lsp_types::{
    CodeAction, CodeActionKind, CreateFile, CreateFileOptions, Diagnostic, DocumentChangeOperation,
    DocumentChanges, OneOf, OptionalVersionedTextDocumentIdentifier, Position, Range, ResourceOp,
    TextDocumentEdit, TextEdit, Url, WorkspaceEdit,
};
use tree_sitter::{Node, Parser, Tree};

use crate::analysis::{
    byte_offset, collect_imports, import_edit, import_target, lsp_position, package_line,
};
use crate::diagnostics::{
    FIX_ADD_IMPORT, FIX_CREATE_RECEIVER_MEMBER, FIX_CREATE_SYMBOL, FIX_CREATE_TYPE, FIX_RENAME,
};
use crate::index::{java_parser, IndexHandle};
use crate::messages::{Bus, DriverMessage, Request};
use crate::types::{self, SourceLayerIndex, Ty, TypeLookup, TypeQuery};

/// The context of one quick-fix request: the open document (its tree and text),
/// the two subsystems it reads from, and the client capability the create-type
/// fix needs. Ported from `TreeSitterEngine::code_actions` so the same fixes are
/// produced from a parse this subsystem owns.
pub struct QuickFix<'a> {
    uri: &'a Url,
    tree: &'a Tree,
    text: &'a str,
    index: &'a IndexHandle,
    overlay: &'a dyn TypeLookup,
    resource_operations: bool,
}

impl<'a> QuickFix<'a> {
    /// Builds the fix context over an already-parsed document, so the analysis
    /// core can delegate to it too.
    pub(crate) fn new(
        uri: &'a Url,
        tree: &'a Tree,
        text: &'a str,
        index: &'a IndexHandle,
        overlay: &'a dyn TypeLookup,
        resource_operations: bool,
    ) -> Self {
        Self {
            uri,
            tree,
            text,
            index,
            overlay,
            resource_operations,
        }
    }
}

impl QuickFix<'_> {
    /// Quick fixes for the unresolved-symbol diagnostics: add an import, change
    /// to a near member, or create a stub type/member. Built from each
    /// diagnostic's `data`, so the fix matches what was reported.
    pub fn actions(&self, diagnostics: &[Diagnostic]) -> Vec<CodeAction> {
        // The type model, for inferring created signatures from the usage.
        let workspace = self.index.type_model().blocking_recv();
        let empty = SourceLayerIndex::default();
        let base = workspace.as_deref().unwrap_or(&empty);
        let query = TypeQuery::new(base, self.overlay);
        let mut actions = Vec::new();
        for diagnostic in diagnostics {
            if diagnostic.source.as_deref() != Some("java-lsp") {
                continue;
            }
            let Some(data) = diagnostic.data.as_ref() else {
                continue;
            };
            let name = data
                .get("name")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let Some(fixes) = data.get("fixes").and_then(|value| value.as_array()) else {
                continue;
            };
            for fix in fixes {
                match fix.get("fix").and_then(|value| value.as_str()) {
                    Some(FIX_ADD_IMPORT) => self.add_import_actions(diagnostic, fix, &mut actions),
                    Some(FIX_RENAME) => self.rename_member_action(diagnostic, fix, &mut actions),
                    Some(FIX_CREATE_TYPE) => {
                        self.create_type_actions(diagnostic, name, &mut actions)
                    }
                    Some(FIX_CREATE_SYMBOL) => {
                        self.create_symbol_actions(diagnostic, name, &query, &mut actions)
                    }
                    Some(FIX_CREATE_RECEIVER_MEMBER) => self.create_receiver_member_action(
                        diagnostic,
                        name,
                        fix,
                        &query,
                        &mut actions,
                    ),
                    _ => {}
                }
            }
        }
        actions
    }

    /// One "Add import" action per importable candidate (the client shows a
    /// picker when several are offered). Edits come from `import_edit`.
    fn add_import_actions(
        &self,
        diagnostic: &Diagnostic,
        data: &serde_json::Value,
        actions: &mut Vec<CodeAction>,
    ) {
        let Some(candidates) = data.get("candidates").and_then(|value| value.as_array()) else {
            return;
        };
        let root = self.tree.root_node();
        let imports = collect_imports(&root, self.text);
        let package = types::file_package(self.tree, self.text);
        let package_line = package_line(self.tree);
        for candidate in candidates {
            let Some(target) = candidate.as_str() else {
                continue;
            };
            let simple = target.rsplit('.').next().unwrap_or(target);
            let Some(entry) = self
                .index
                .query_name(simple)
                .blocking_recv()
                .into_iter()
                .find(|entry| import_target(entry).as_deref() == Some(target))
            else {
                continue;
            };
            let edits = import_edit(
                self.uri,
                &entry,
                package.as_deref(),
                package_line,
                &imports,
                self.index,
            );
            if edits.is_empty() {
                continue;
            }
            actions.push(CodeAction {
                title: format!("Add import `{target}`"),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![diagnostic.clone()]),
                edit: Some(workspace_edit_changes(self.uri, edits)),
                ..CodeAction::default()
            });
        }
    }

    /// A "Change to `x`" action for a member name close to a real one.
    fn rename_member_action(
        &self,
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
                self.uri,
                vec![TextEdit {
                    range: diagnostic.range,
                    new_text: replacement.to_string(),
                }],
            )),
            is_preferred: Some(true),
            ..CodeAction::default()
        });
    }

    /// Four "Create class/interface/enum/record `name`" actions, each writing a
    /// stub file under the source root of the file's own package. Only offered
    /// when the client supports the `CreateFile` resource operation.
    fn create_type_actions(
        &self,
        diagnostic: &Diagnostic,
        name: &str,
        actions: &mut Vec<CodeAction>,
    ) {
        if !self.resource_operations || !is_java_identifier(name) {
            return;
        }
        let package = types::file_package(self.tree, self.text);
        let Some(new_file) = self.new_type_file_uri(package.as_deref(), name) else {
            return;
        };
        for kind in ["class", "interface", "enum", "record"] {
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
                is_preferred: Some(kind == "class"),
                ..CodeAction::default()
            });
        }
    }

    /// Create-stub actions for a bare identifier: a method when it is called
    /// unqualified, else a local variable (preferred) and a field, with the
    /// signature inferred from the usage.
    fn create_symbol_actions(
        &self,
        diagnostic: &Diagnostic,
        name: &str,
        model: &dyn TypeLookup,
        actions: &mut Vec<CodeAction>,
    ) {
        if !is_java_identifier(name) {
            return;
        }
        let Some(node) = node_at(self.tree, self.text, diagnostic.range) else {
            return;
        };
        let text = self.text;
        let scope = types::scope_at(node, text, self.tree, model);
        // An unqualified call name becomes a method on the enclosing type.
        let call = node.parent().filter(|parent| {
            parent.kind() == "method_invocation"
                && parent.child_by_field_name("object").is_none()
                && parent
                    .child_by_field_name("name")
                    .is_some_and(|named| named.id() == node.id())
        });
        if let Some(call) = call {
            let Some(body) = enclosing_type_body(self.tree, node.start_byte()) else {
                return;
            };
            let params = parameter_list(call, text, &scope, model);
            let ret = return_type(call, text, &scope, model);
            let stub = format!(
                "    public {ret} {name}({}) {{\n    }}\n",
                params.join(", ")
            );
            push_body_insert(
                self.uri,
                text,
                diagnostic,
                body,
                stub,
                format!("Create method `{name}`"),
                actions,
            );
            return;
        }
        // A value use becomes a local variable (preferred) and a field.
        let ty = value_type(node, text, &scope, model);
        if let Some(block) = enclosing_block(node).filter(|_| enclosing_method(node).is_some()) {
            let position = lsp_position(text, block.start_byte() + 1);
            let edit = TextEdit {
                range: Range::new(position, position),
                new_text: format!("\n    {ty} {name};"),
            };
            actions.push(CodeAction {
                title: format!("Create local variable `{name}`"),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![diagnostic.clone()]),
                edit: Some(workspace_edit_changes(self.uri, vec![edit])),
                is_preferred: Some(true),
                ..CodeAction::default()
            });
        }
        if let Some(body) = enclosing_type_body(self.tree, node.start_byte()) {
            push_body_insert(
                self.uri,
                text,
                diagnostic,
                body,
                format!("    private {ty} {name};\n"),
                format!("Create field `{name}`"),
                actions,
            );
        }
    }

    /// A "Create method/field `name` in `T`" action for an unresolved member on
    /// a workspace receiver: the stub is inserted into `T`'s file (the open
    /// buffer when it is the current document, else read from disk) with the
    /// signature inferred from the call site.
    #[allow(clippy::too_many_arguments)]
    fn create_receiver_member_action(
        &self,
        diagnostic: &Diagnostic,
        name: &str,
        fix: &serde_json::Value,
        model: &dyn TypeLookup,
        actions: &mut Vec<CodeAction>,
    ) {
        let Some(owner) = fix.get("owner").and_then(|value| value.as_str()) else {
            return;
        };
        if !is_java_identifier(name) {
            return;
        }
        let kind = fix
            .get("kind")
            .and_then(|value| value.as_str())
            .unwrap_or("method");
        let simple = owner.rsplit('.').next().unwrap_or(owner);
        let Some(entry) = self
            .index
            .query_name(simple)
            .blocking_recv()
            .into_iter()
            .find(|entry| !entry.dependency && import_target(entry).as_deref() == Some(owner))
        else {
            return;
        };
        let owner_uri = (*entry.uri).clone();
        let owner_text = if &owner_uri == self.uri {
            self.text.to_string()
        } else {
            match owner_uri
                .to_file_path()
                .ok()
                .and_then(|path| std::fs::read_to_string(path).ok())
            {
                Some(text) => text,
                None => return,
            }
        };
        let mut parser = java_parser();
        let Some(tree) = parser.parse(owner_text.as_bytes(), None) else {
            return;
        };
        let Some(body) = type_body_by_name(&tree, &owner_text, simple) else {
            return;
        };
        // The signature comes from the call site in the current buffer.
        let Some(node) = node_at(self.tree, self.text, diagnostic.range) else {
            return;
        };
        let call_scope = types::scope_at(node, self.text, self.tree, model);
        let stub = if kind == "method" {
            let call = node
                .parent()
                .filter(|parent| parent.kind() == "method_invocation");
            let params = call
                .map(|call| parameter_list(call, self.text, &call_scope, model))
                .unwrap_or_default();
            let ret = call
                .map(|call| return_type(call, self.text, &call_scope, model))
                .unwrap_or_else(|| "void".to_string());
            format!(
                "    public {ret} {name}({}) {{\n    }}\n",
                params.join(", ")
            )
        } else {
            format!("    private Object {name};\n")
        };
        let close = lsp_position(&owner_text, body.end_byte().saturating_sub(1));
        let edit = TextEdit {
            range: Range::new(Position::new(close.line, 0), Position::new(close.line, 0)),
            new_text: stub,
        };
        actions.push(CodeAction {
            title: format!("Create {kind} `{name}` in `{simple}`"),
            kind: Some(CodeActionKind::QUICKFIX),
            diagnostics: Some(vec![diagnostic.clone()]),
            edit: Some(workspace_edit_changes(&owner_uri, vec![edit])),
            is_preferred: Some(true),
            ..CodeAction::default()
        });
    }

    /// The URI for a new `name.java`: under the deepest source root containing
    /// the file, in the file's package, else beside the file.
    fn new_type_file_uri(&self, package: Option<&str>, name: &str) -> Option<Url> {
        let path = self.uri.to_file_path().ok()?;
        let base = self
            .index
            .source_roots()
            .blocking_recv()
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

/// The node covering an LSP range (the identifier a diagnostic points at).
fn node_at<'a>(tree: &'a Tree, text: &str, range: Range) -> Option<Node<'a>> {
    let start = byte_offset(text, range.start);
    let end = byte_offset(text, range.end);
    tree.root_node().descendant_for_byte_range(start, end)
}

/// The innermost `block` containing `node`.
fn enclosing_block(node: Node<'_>) -> Option<Node<'_>> {
    let mut current = Some(node);
    while let Some(candidate) = current {
        if candidate.kind() == "block" {
            return Some(candidate);
        }
        current = candidate.parent();
    }
    None
}

/// The innermost method/constructor declaration containing `node`.
fn enclosing_method(node: Node<'_>) -> Option<Node<'_>> {
    let mut current = Some(node);
    while let Some(candidate) = current {
        if matches!(
            candidate.kind(),
            "method_declaration" | "constructor_declaration"
        ) {
            return Some(candidate);
        }
        current = candidate.parent();
    }
    None
}

/// The `body` of the top-level type declaration named `name` in a parsed file.
fn type_body_by_name<'a>(tree: &'a Tree, text: &str, name: &str) -> Option<Node<'a>> {
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if matches!(
            node.kind(),
            "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration"
        ) && node
            .child_by_field_name("name")
            .is_some_and(|named| &text[named.byte_range()] == name)
        {
            if let Some(body) = node.child_by_field_name("body") {
                return Some(body);
            }
        }
        let mut cursor = node.walk();
        let children: Vec<Node> = node.named_children(&mut cursor).collect();
        stack.extend(children);
    }
    None
}

/// A created method's parameter list: types inferred from the call's arguments,
/// names from bare identifiers (else `argN`).
fn parameter_list(
    call: Node<'_>,
    text: &str,
    scope: &types::Scope,
    model: &dyn TypeLookup,
) -> Vec<String> {
    let Some(arguments) = call.child_by_field_name("arguments") else {
        return Vec::new();
    };
    let mut cursor = arguments.walk();
    arguments
        .named_children(&mut cursor)
        .enumerate()
        .map(|(index, argument)| {
            let ty = display_type(&types::receiver_type(&argument, text, scope, model));
            let name = if argument.kind() == "identifier" {
                text[argument.byte_range()].to_string()
            } else {
                format!("arg{}", index + 1)
            };
            format!("{ty} {name}")
        })
        .collect()
}

/// The return type a created method needs, from the context its call sits in.
fn return_type(call: Node<'_>, text: &str, scope: &types::Scope, model: &dyn TypeLookup) -> String {
    let Some(parent) = call.parent() else {
        return "Object".to_string();
    };
    match parent.kind() {
        "expression_statement" => "void".to_string(),
        "variable_declarator" => {
            declared_type_of(parent, text).unwrap_or_else(|| "Object".to_string())
        }
        "assignment_expression" => parent
            .child_by_field_name("left")
            .map(|left| display_type(&types::receiver_type(&left, text, scope, model)))
            .unwrap_or_else(|| "Object".to_string()),
        "return_statement" => enclosing_method(call)
            .and_then(|method| method.child_by_field_name("type"))
            .map(|ty| text[ty.byte_range()].to_string())
            .unwrap_or_else(|| "Object".to_string()),
        _ => "Object".to_string(),
    }
}

/// A declaration's written type, unless it is `var`.
fn declared_type_of(declarator: Node<'_>, text: &str) -> Option<String> {
    let declaration = declarator.parent()?;
    if !matches!(
        declaration.kind(),
        "local_variable_declaration" | "field_declaration"
    ) {
        return None;
    }
    let ty = declaration.child_by_field_name("type")?;
    let written = &text[ty.byte_range()];
    (written != "var").then(|| written.to_string())
}

/// The type a created local/field needs, from the value's context.
fn value_type(node: Node<'_>, text: &str, scope: &types::Scope, model: &dyn TypeLookup) -> String {
    let Some(parent) = node.parent() else {
        return "Object".to_string();
    };
    match parent.kind() {
        "variable_declarator" => {
            declared_type_of(parent, text).unwrap_or_else(|| "Object".to_string())
        }
        "assignment_expression" => {
            let is_target = parent
                .child_by_field_name("left")
                .is_some_and(|left| left.id() == node.id());
            if is_target {
                return "Object".to_string();
            }
            parent
                .child_by_field_name("left")
                .map(|left| display_type(&types::receiver_type(&left, text, scope, model)))
                .unwrap_or_else(|| "Object".to_string())
        }
        _ => "Object".to_string(),
    }
}

/// A type for a generated stub; an unresolved type becomes `Object`.
fn display_type(ty: &Ty) -> String {
    match ty {
        Ty::Unknown => "Object".to_string(),
        other => other.display(),
    }
}

/// Inserts a stub before a type body's closing brace, as a `changes` edit.
fn push_body_insert(
    uri: &Url,
    text: &str,
    diagnostic: &Diagnostic,
    body: Node<'_>,
    stub: String,
    title: String,
    actions: &mut Vec<CodeAction>,
) {
    let close = lsp_position(text, body.end_byte().saturating_sub(1));
    let edit = TextEdit {
        range: Range::new(Position::new(close.line, 0), Position::new(close.line, 0)),
        new_text: stub,
    };
    actions.push(CodeAction {
        title,
        kind: Some(CodeActionKind::QUICKFIX),
        diagnostics: Some(vec![diagnostic.clone()]),
        edit: Some(workspace_edit_changes(uri, vec![edit])),
        ..CodeAction::default()
    });
}

/// A trivial source stub for a created type file.
fn stub_type_source(package: Option<&str>, kind: &str, name: &str) -> String {
    let body = match kind {
        "interface" => format!("public interface {name} {{\n}}\n"),
        "enum" => format!("public enum {name} {{\n}}\n"),
        "record" => format!("public record {name}() {{\n}}\n"),
        _ => format!("public class {name} {{\n}}\n"),
    };
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

// -- the subsystem ---------------------------------------------------------

/// Starts the quick-fix module: a thread that owns a parser and the open
/// buffers' text, consumes the document notifications the hub broadcasts, and
/// answers the quick-fix requests. It reads the symbol index and the diagnostics
/// cache through its bus client.
pub fn spawn_module(client: crate::bus::BusClient) {
    let mut rx = client.serve(crate::bus::Module::QuickFix);
    thread::Builder::new()
        .name("java-lsp-quickfix".to_string())
        .spawn(move || {
            let mut module = QuickFixModule {
                client,
                parser: java_parser(),
                docs: HashMap::new(),
                resource_operations: false,
            };
            while let Some(message) = rx.blocking_recv() {
                match message {
                    Bus::Notify(DriverMessage::DocumentOpened { uri, text, .. })
                    | Bus::Notify(DriverMessage::DocumentChanged { uri, text, .. }) => {
                        module.docs.insert(uri, text);
                    }
                    Bus::Notify(DriverMessage::DocumentClosed { uri }) => {
                        module.docs.remove(&uri);
                    }
                    Bus::Notify(DriverMessage::ClientCapabilities {
                        resource_operations,
                    }) => module.resource_operations = resource_operations,
                    Bus::Request(Request::QuickFixForDocument {
                        uri,
                        diagnostics,
                        reply,
                    }) => {
                        reply.send(module.code_actions(&uri, &diagnostics));
                    }
                    _ => {}
                }
            }
        })
        .expect("spawn the quick-fix module thread");
}

/// The quick-fix module's state: a parser, the open buffers' text, the client it
/// answers through, and the client capability the create-type fix needs.
struct QuickFixModule {
    client: crate::bus::BusClient,
    parser: Parser,
    docs: HashMap<Url, Arc<String>>,
    resource_operations: bool,
}

impl QuickFixModule {
    fn code_actions(&mut self, uri: &Url, diagnostics: &[Diagnostic]) -> Vec<CodeAction> {
        let Some(text) = self.docs.get(uri).cloned() else {
            return Vec::new();
        };
        let Some(tree) = self.parser.parse(text.as_bytes(), None) else {
            return Vec::new();
        };
        // Prefer the request's diagnostics; fall back to the diagnostics cache.
        let diagnostics: Vec<Diagnostic> = if diagnostics.is_empty() {
            self.client
                .diagnostics(uri)
                .blocking_recv()
                .map(|(_, cached)| (*cached).clone())
                .unwrap_or_default()
        } else {
            diagnostics.to_vec()
        };
        let overlay = self.client.type_layers().blocking_recv();
        QuickFix::new(
            uri,
            &tree,
            &text,
            &self.client,
            &overlay,
            self.resource_operations,
        )
        .actions(&diagnostics)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_module_generates_a_fix_from_the_request_diagnostics() {
        let client = crate::bus::spawn_hub();
        crate::index::spawn_module(&client.labeled("index"));
        crate::quickfix::spawn_module(client.labeled("quickfix"));
        let client = client.labeled("test");

        let uri = Url::parse("file:///Main.java").unwrap();
        client.notify(DriverMessage::DocumentOpened {
            uri: uri.clone(),
            text: Arc::new("class Main {\n    Missing other;\n}\n".to_string()),
            version: 1,
        });

        // A "change to a near name" diagnostic: built without the index, so this
        // proves the module thread, its own parse, and the fix build.
        let diagnostic = Diagnostic {
            range: Range::new(Position::new(1, 4), Position::new(1, 11)),
            severity: Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR),
            code: None,
            code_description: None,
            source: Some("java-lsp".to_string()),
            message: "cannot resolve `other`".to_string(),
            related_information: None,
            tags: None,
            data: Some(serde_json::json!({
                "name": "other",
                "fixes": [{ "fix": "rename", "replacement": "value" }],
            })),
        };

        let actions = client.code_actions(&uri, vec![diagnostic]).await;
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert!(actions[0].title.contains("value"), "{:?}", actions[0].title);
        assert_eq!(actions[0].kind, Some(CodeActionKind::QUICKFIX));
    }
}
