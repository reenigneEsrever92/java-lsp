//! The diagnostics subsystem: unresolved-symbol and syntax diagnostics.
//!
//! Diagnostics are their own subsystem. It owns a parser and the open documents'
//! text, parses each open buffer itself (never the analysis core's parse), and
//! reports the diagnostics it finds to the engine hub as messages; the hub is the
//! sole translator to the editor. The semantic checks consult the **index
//! subsystem** — the shared symbol index and the workspace declared-type layer —
//! through `IndexHandle`, so the subsystem holds no index state of its own. A
//! sweep reads the index through a [`SweepIndex`]: the workspace layer once per
//! sweep, and every name lookup from a cache one batched request fills, so a
//! sweep costs a handful of bus round trips rather than one per reference.
//!
//! The pass is coalesced: a burst of edits bumps a generation and notifies a
//! sweep task, which republishes the latest state at least once and collapses
//! intermediate states away.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::thread;

use serde_json::json;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Url};
use tree_sitter::{Node, Parser, Tree};

use crate::analysis::{
    collect_imports, import_edit, import_target, lsp_range, package_line, ExistingImport,
};
use crate::index::{java_parser, IndexHandle, NameLookup, SymbolEntry};
use crate::messages::{Bus, DriverMessage, Request};
use crate::types::{self, SourceLayerIndex, Ty, TypeLookup, TypeQuery};

/// Marks a semantic diagnostic and the quick fix it carries; the code-action
/// handler rebuilds the edit from the diagnostic's `data`.
pub(crate) const CODE_UNRESOLVED_TYPE: &str = "unresolved-type";
pub(crate) const CODE_UNRESOLVED_MEMBER: &str = "unresolved-member";
pub(crate) const CODE_UNRESOLVED_SYMBOL: &str = "unresolved-symbol";
pub(crate) const CODE_UNRESOLVED_IMPORT: &str = "unresolved-import";
pub(crate) const FIX_ADD_IMPORT: &str = "add-import";
pub(crate) const FIX_RENAME: &str = "rename";
pub(crate) const FIX_CREATE_TYPE: &str = "create-type";
pub(crate) const FIX_CREATE_SYMBOL: &str = "create-symbol";
pub(crate) const FIX_CREATE_RECEIVER_MEMBER: &str = "create-receiver-member";

/// Whether semantic diagnostics are enabled, from `JAVA_LSP_SEMANTIC_DIAGNOSTICS`:
/// unset (or any value but `0`/`false`) keeps them on.
pub(crate) fn semantic_diagnostics_enabled() -> bool {
    semantic_diagnostics_setting(
        std::env::var("JAVA_LSP_SEMANTIC_DIAGNOSTICS")
            .ok()
            .as_deref(),
    )
}

/// The setting a `JAVA_LSP_SEMANTIC_DIAGNOSTICS` value denotes.
pub(crate) fn semantic_diagnostics_setting(value: Option<&str>) -> bool {
    match value {
        Some(value) => !(value == "0" || value.eq_ignore_ascii_case("false")),
        None => true,
    }
}

/// Diagnostics for one parsed document: syntax errors, or the semantic pass when
/// the document parses cleanly and semantic diagnostics are enabled. A
/// single-document sweep: the index is read through its own [`SweepIndex`].
pub fn diagnostics_for(
    tree: &Tree,
    text: &str,
    uri: &Url,
    index: &IndexHandle,
    overlay: &dyn TypeLookup,
    semantic: bool,
) -> Vec<Diagnostic> {
    let sweep = (semantic && !tree.root_node().has_error()).then(|| {
        let sweep = SweepIndex::new(index);
        sweep.prefetch_documents([(tree, text)]);
        sweep
    });
    document_diagnostics(tree, text, uri, sweep.as_ref(), overlay)
}

/// One document's pass within a sweep: syntax errors, or — when the sweep runs
/// the semantic checks (`sweep` is `Some`) — the unresolved-symbol findings.
fn document_diagnostics(
    tree: &Tree,
    text: &str,
    uri: &Url,
    sweep: Option<&SweepIndex>,
    overlay: &dyn TypeLookup,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    if tree.root_node().has_error() {
        collect_errors(&tree.root_node(), text, &mut diagnostics);
        // A file that does not parse yields no meaningful semantic findings;
        // adding them would only pile noise onto broken code.
        return diagnostics;
    }
    if let Some(sweep) = sweep {
        diagnostics.extend(semantic_diagnostics(tree, text, uri, sweep, overlay));
    }
    diagnostics
}

/// Unresolved-symbol diagnostics for a document: type references, member
/// accesses on a known receiver, bare identifiers, and import declarations, all
/// at `ERROR` severity. Gated on the model actually vouching for `java.lang` and
/// on the file parsing cleanly, so a missing JDK or a broken file never produces
/// a wall of false positives.
fn semantic_diagnostics(
    tree: &Tree,
    text: &str,
    uri: &Url,
    sweep: &SweepIndex,
    overlay: &dyn TypeLookup,
) -> Vec<Diagnostic> {
    let Some(workspace) = sweep.workspace.as_deref() else {
        return Vec::new();
    };
    let query = TypeQuery::new(workspace, overlay);
    let mut check = SemanticCheck::new(uri, tree, text, sweep, &query);
    check.visit(tree.root_node());
    check.out
}

/// The index as one sweep reads it. The workspace layer and the readiness gate
/// are fetched once per sweep; name and package lookups are served from caches
/// that one batched prefetch fills, with a single (cached) request as the
/// fallback for a key the prefetch did not anticipate. Every sweep starts from
/// empty caches, so an index change is seen by the next sweep.
pub(crate) struct SweepIndex<'a> {
    index: &'a IndexHandle,
    /// The workspace declared-type layer, or `None` when the semantic pass must
    /// not run this sweep.
    workspace: Option<Arc<SourceLayerIndex>>,
    names: RefCell<HashMap<String, Vec<Arc<SymbolEntry>>>>,
    packages: RefCell<HashMap<String, bool>>,
}

impl<'a> SweepIndex<'a> {
    fn new(index: &'a IndexHandle) -> Self {
        // The non-source base (jars, the JDK) lands as each producer finishes,
        // before the (long) source scan completes; semantic diagnostics still
        // wait for `ready` — an indexed `java.lang` plus a finished source scan —
        // so a workspace type that has not been scanned yet is never reported
        // as unresolved.
        let workspace = index
            .type_model()
            .blocking_recv()
            .filter(|_| index.ready().blocking_recv())
            .filter(|workspace| workspace.contains("Object") && workspace.contains("String"));
        Self {
            index,
            workspace,
            names: RefCell::new(HashMap::new()),
            packages: RefCell::new(HashMap::new()),
        }
    }

    /// Fills the caches for every lookup the given documents' passes are
    /// expected to make, in one `IndexQueryNames` and one `IndexHasPackages`
    /// request. Does nothing when the semantic pass will not run.
    fn prefetch_documents<'t>(&self, documents: impl IntoIterator<Item = (&'t Tree, &'t str)>) {
        if self.workspace.is_none() {
            return;
        }
        let mut names = HashSet::new();
        let mut packages = HashSet::new();
        for (tree, text) in documents {
            if !tree.root_node().has_error() {
                collect_lookups(tree, text, &mut names, &mut packages);
            }
        }
        self.prefetch(names, packages);
    }

    fn prefetch(&self, names: HashSet<String>, packages: HashSet<String>) {
        let names: Vec<String> = {
            let cached = self.names.borrow();
            names
                .into_iter()
                .filter(|name| !cached.contains_key(name))
                .collect()
        };
        if !names.is_empty() {
            let mut found = self.index.query_names(names.clone()).blocking_recv();
            let mut cache = self.names.borrow_mut();
            for name in names {
                let entries = found.remove(&name).unwrap_or_default();
                cache.insert(name, entries);
            }
        }
        let packages: Vec<String> = {
            let cached = self.packages.borrow();
            packages
                .into_iter()
                .filter(|package| !cached.contains_key(package))
                .collect()
        };
        if !packages.is_empty() {
            let known = self.index.has_packages(packages.clone()).blocking_recv();
            let mut cache = self.packages.borrow_mut();
            for package in packages {
                let exists = known.contains(&package);
                cache.insert(package, exists);
            }
        }
    }

    fn query_name(&self, name: &str) -> Vec<Arc<SymbolEntry>> {
        if let Some(entries) = self.names.borrow().get(name) {
            return entries.clone();
        }
        let entries = self.index.query_name(name).blocking_recv();
        self.names
            .borrow_mut()
            .insert(name.to_string(), entries.clone());
        entries
    }

    fn has_package(&self, package: &str) -> bool {
        if let Some(exists) = self.packages.borrow().get(package) {
            return *exists;
        }
        let exists = self.index.has_package(package).blocking_recv();
        self.packages
            .borrow_mut()
            .insert(package.to_string(), exists);
        exists
    }
}

impl NameLookup for SweepIndex<'_> {
    fn query_name(&self, name: &str) -> Vec<Arc<SymbolEntry>> {
        SweepIndex::query_name(self, name)
    }
}

/// Collects the index lookups one document's semantic pass will make: every
/// type name, every symbolic identifier the file does not bind itself, and per
/// import its simple name, its static owner's simple name, or its wildcard
/// package. Over-collecting only costs a cache entry; a name missed here is
/// still looked up, by a single request.
fn collect_lookups(
    tree: &Tree,
    text: &str,
    names: &mut HashSet<String>,
    packages: &mut HashSet<String>,
) {
    let root = tree.root_node();
    let mut declared = HashSet::new();
    collect_declared_names(root, text, &mut declared);
    collect_lookups_in(root, text, &declared, names, packages);
}

fn collect_lookups_in(
    node: Node,
    text: &str,
    declared: &HashSet<String>,
    names: &mut HashSet<String>,
    packages: &mut HashSet<String>,
) {
    match node.kind() {
        "import_declaration" => {
            if let Some((path, is_static, is_wildcard)) = parse_import(&text[node.byte_range()]) {
                if is_static {
                    if let Some(owner) = static_import_owner(path, is_wildcard) {
                        names.insert(simple_name(owner).to_string());
                    }
                } else if is_wildcard {
                    packages.insert(path.to_string());
                } else {
                    names.insert(simple_name(path).to_string());
                }
            }
        }
        "type_identifier" => {
            let name = &text[node.byte_range()];
            if !name.is_empty() && name != "var" {
                names.insert(name.to_string());
            }
        }
        "identifier" if is_symbolic_identifier(node) => {
            let name = &text[node.byte_range()];
            if !name.is_empty() && !declared.contains(name) {
                names.insert(name.to_string());
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    let children: Vec<Node> = node.named_children(&mut cursor).collect();
    for child in children {
        collect_lookups_in(child, text, declared, names, packages);
    }
}

/// An import declaration's target path with its `static` and wildcard flags
/// (`import static a.B.*;` → `("a.B", true, true)`), or `None` when empty.
fn parse_import(declaration: &str) -> Option<(&str, bool, bool)> {
    let raw = declaration.trim();
    let raw = raw.strip_prefix("import").map_or(raw, str::trim);
    let is_static = raw.starts_with("static");
    let raw = raw.strip_prefix("static").map_or(raw, str::trim);
    let path = raw.trim_end_matches(';').trim();
    if path.is_empty() {
        return None;
    }
    Some(match path.strip_suffix(".*") {
        Some(head) => (head, is_static, true),
        None => (path, is_static, false),
    })
}

/// The owning type of a static import: the whole path for a wildcard, the path
/// minus its member otherwise.
fn static_import_owner(path: &str, is_wildcard: bool) -> Option<&str> {
    if is_wildcard {
        Some(path)
    } else {
        path.rsplit_once('.').map(|(owner, _)| owner)
    }
}

/// The last segment of a dotted name.
fn simple_name(path: &str) -> &str {
    path.rsplit('.').next().unwrap_or(path)
}

/// The state of one diagnostics pass over an open document.
struct SemanticCheck<'a> {
    uri: &'a Url,
    text: &'a str,
    tree: &'a Tree,
    index: &'a SweepIndex<'a>,
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
        tree: &'a Tree,
        text: &'a str,
        index: &'a SweepIndex<'a>,
        model: &'a dyn TypeLookup,
    ) -> Self {
        let root = tree.root_node();
        let mut declared_names = HashSet::new();
        collect_declared_names(root, text, &mut declared_names);
        Self {
            uri,
            text,
            tree,
            index,
            model,
            imports: collect_imports(&root, text),
            package: types::file_package(tree, text),
            package_line: package_line(tree),
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
        let candidates = self.type_candidates(name);
        let fixes = if candidates.is_empty() {
            json!([{ "fix": FIX_CREATE_TYPE }])
        } else {
            let importable = self.importable(&candidates);
            if importable.is_empty() {
                json!([])
            } else {
                json!([{ "fix": FIX_ADD_IMPORT, "candidates": importable }])
            }
        };
        let data = json!({ "name": name, "fixes": fixes });
        self.push(
            name_node,
            CODE_UNRESOLVED_TYPE,
            format!("cannot resolve type `{name}`"),
            data,
        );
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

    fn type_candidates(&self, name: &str) -> Vec<Arc<SymbolEntry>> {
        self.index
            .query_name(name)
            .into_iter()
            .filter(|entry| types::is_type_kind(entry.kind))
            .collect()
    }

    /// The fully-qualified import targets among `candidates` that an `import`
    /// edit can actually add — `import_edit` already excludes same-file,
    /// same-package, `java.lang`, already-imported, and conflicting names.
    fn importable(&self, candidates: &[Arc<SymbolEntry>]) -> Vec<String> {
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
        let mut fixes: Vec<serde_json::Value> = Vec::new();
        if let Some(replacement) = nearest_name(name, &names) {
            fixes.push(json!({ "fix": FIX_RENAME, "replacement": replacement }));
        }
        if let Some(owner) = self.workspace_owner(&receiver, package) {
            fixes.push(json!({
                "fix": FIX_CREATE_RECEIVER_MEMBER,
                "owner": owner,
                "kind": if is_call { "method" } else { "field" },
            }));
        }
        let data = json!({ "name": name, "fixes": fixes });
        self.push(
            name_node,
            CODE_UNRESOLVED_MEMBER,
            format!("cannot resolve `{name}`"),
            data,
        );
    }

    /// The fully-qualified name of `receiver`'s type when it is a workspace
    /// source (never a jar/JDK declaration), so a member can be created in it.
    fn workspace_owner(&self, receiver: &Ty, package: Option<&str>) -> Option<String> {
        let info = self.model.lookup(receiver, package)?;
        let entry = self
            .index
            .query_name(&info.name)
            .into_iter()
            .find(|entry| {
                !entry.dependency
                    && types::is_type_kind(entry.kind)
                    && entry.package.as_deref() == info.package.as_deref()
            })?;
        import_target(&entry)
    }

    /// Flags a bare name used as a symbol reference that resolves to no local,
    /// field, member, type, or import.
    fn check_identifier(&mut self, node: Node) {
        if !is_symbolic_identifier(node) {
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
            let data = json!({
                "name": name,
                "fixes": [{ "fix": FIX_ADD_IMPORT, "candidates": importable }],
            });
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
        let data = json!({ "name": name, "fixes": [{ "fix": FIX_CREATE_SYMBOL }] });
        self.push(
            node,
            CODE_UNRESOLVED_SYMBOL,
            format!("cannot resolve `{name}`"),
            data,
        );
    }

    /// Flags an `import` whose target the index cannot supply: a single type by
    /// package and name, a wildcard by package existence, a static import by
    /// its type (and named member).
    fn check_import(&mut self, node: Node) {
        let Some((path, is_static, is_wildcard)) = parse_import(&self.text[node.byte_range()])
        else {
            return;
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
        let Some(owner) = static_import_owner(path, is_wildcard) else {
            return false;
        };
        let owner_simple = simple_name(owner);
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

/// Whether `node` is a bare identifier used as a symbol reference: not a
/// declaration's own name, not a qualified-name segment, and not a member the
/// member check already owns, and in a recognized expression position.
fn is_symbolic_identifier(node: Node) -> bool {
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
            let is_name = parent
                .child_by_field_name("name")
                .is_some_and(|named| named.id() == node.id());
            // A call's callee and a member access are uses, not bindings.
            (is_name && !matches!(parent.kind(), "method_invocation" | "field_access"))
                || matches!(parent.kind(), "inferred_parameters" | "lambda_parameters")
                || (parent.kind() == "lambda_expression"
                    && parent
                        .child_by_field_name("parameters")
                        .is_some_and(|params| params.id() == node.id()))
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

// -- the subsystem ---------------------------------------------------------

/// Starts the diagnostics module: a thread that owns a parser and the open
/// buffers' text, consumes the document notifications the hub broadcasts, and
/// answers the diagnostics requests. It reads the index through its bus client
/// and reports each pass through the hub.
///
/// A document notification only records (or drops) the text; the sweep runs on
/// [`DriverMessage::AnalysisUpdated`], which the analysis module sends after it
/// has applied the same event and sent the index its updates — so the sweep
/// reads an index that already holds the edit.
pub fn spawn_module(client: IndexHandle) {
    let mut rx = client.serve(crate::bus::Module::Diagnostics);
    thread::Builder::new()
        .name("java-lsp-diagnostics".to_string())
        .spawn(move || {
            let mut module = DiagnosticsModule {
                client,
                parser: java_parser(),
                docs: HashMap::new(),
                results: HashMap::new(),
                semantic: semantic_diagnostics_enabled(),
            };
            while let Some(message) = rx.blocking_recv() {
                match message {
                    Bus::Notify(DriverMessage::DocumentOpened { uri, text, version })
                    | Bus::Notify(DriverMessage::DocumentChanged { uri, text, version }) => {
                        module.document(uri, text, version);
                    }
                    Bus::Notify(DriverMessage::DocumentClosed { uri }) => module.closed(&uri),
                    Bus::Notify(DriverMessage::AnalysisUpdated) => module.sweep(),
                    Bus::Request(Request::DiagnosticsForDocument { uri, reply }) => {
                        reply.send(module.diagnostics(&uri));
                    }
                    _ => {}
                }
            }
        })
        .expect("spawn the diagnostics module thread");
}

/// One document's most recently computed diagnostics, kept so the quick-fix
/// subsystem can query them without recomputing.
#[derive(Clone)]
struct CachedDiagnostics {
    version: i32,
    diagnostics: Arc<Vec<Diagnostic>>,
}

/// The diagnostics module's state: a parser, the open buffers' text, and the
/// cache of the latest pass per document.
struct DiagnosticsModule {
    client: IndexHandle,
    parser: Parser,
    docs: HashMap<Url, (i32, Arc<String>)>,
    results: HashMap<Url, CachedDiagnostics>,
    semantic: bool,
}

impl DiagnosticsModule {
    /// Records a document's current text; the sweep follows the analysis
    /// module's `AnalysisUpdated`.
    fn document(&mut self, uri: Url, text: Arc<String>, version: i32) {
        self.docs.insert(uri, (version, text));
    }

    /// Drops a closed document and clears its published diagnostics; the
    /// republish of the rest follows the analysis module's `AnalysisUpdated`.
    fn closed(&mut self, uri: &Url) {
        self.docs.remove(uri);
        self.results.remove(uri);
        self.client.notify(DriverMessage::Diagnostics {
            uri: uri.clone(),
            version: None,
            diagnostics: Vec::new(),
        });
    }

    /// The cached pass for `uri`, for the quick-fix subsystem.
    fn diagnostics(&self, uri: &Url) -> Option<(i32, Arc<Vec<Diagnostic>>)> {
        self.results
            .get(uri)
            .map(|cached| (cached.version, Arc::clone(&cached.diagnostics)))
    }

    /// Parses each open buffer itself and reports its diagnostics through the
    /// hub, caching each pass. The index is read through one [`SweepIndex`] for
    /// the whole sweep: the workspace layer, the readiness gate, and the overlay
    /// are fetched once, and the names every document will look up are
    /// prefetched in one batched request.
    fn sweep(&mut self) {
        if self.docs.is_empty() {
            return;
        }
        let overlay = self.client.type_layers().blocking_recv();
        let mut parsed: Vec<(Url, i32, Arc<String>, Tree)> = Vec::new();
        for (uri, (version, text)) in &self.docs {
            if let Some(tree) = self.parser.parse(text.as_bytes(), None) {
                parsed.push((uri.clone(), *version, Arc::clone(text), tree));
            }
        }
        let any_clean = parsed
            .iter()
            .any(|(_, _, _, tree)| !tree.root_node().has_error());
        let sweep = (self.semantic && any_clean).then(|| SweepIndex::new(&self.client));
        if let Some(sweep) = &sweep {
            sweep.prefetch_documents(
                parsed
                    .iter()
                    .map(|(_, _, text, tree)| (tree, text.as_str())),
            );
        }
        for (uri, version, text, tree) in parsed {
            let diagnostics = document_diagnostics(&tree, &text, &uri, sweep.as_ref(), &overlay);
            self.results.insert(
                uri.clone(),
                CachedDiagnostics {
                    version,
                    diagnostics: Arc::new(diagnostics.clone()),
                },
            );
            self.client.notify(DriverMessage::Diagnostics {
                uri,
                version: Some(version),
                diagnostics,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Mutex, OnceLock};

    use tower_lsp::lsp_types::{Position, Range};

    use super::*;
    use crate::index::IndexKind;
    use crate::types::TypeModel;

    /// Everything the global test subscriber writes.
    static CAPTURED: Mutex<Vec<u8>> = Mutex::new(Vec::new());

    struct Capture;

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            CAPTURED.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Installs (once per test process) a global `debug` subscriber that writes
    /// into [`CAPTURED`]. Global, because the hub logs on its own thread.
    fn capture_bus_log() {
        static INSTALLED: OnceLock<()> = OnceLock::new();
        INSTALLED.get_or_init(|| {
            let subscriber = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::DEBUG)
                .with_ansi(false)
                .with_writer(|| Capture)
                .finish();
            let _ = tracing::subscriber::set_global_default(subscriber);
        });
    }

    /// The requests the hub logged from `label`, as `describe_request` renders
    /// them. A unique label keeps other tests' traffic out.
    fn requests_from(label: &str) -> Vec<String> {
        let needle = format!("sender={label} request ");
        let captured = CAPTURED.lock().unwrap();
        String::from_utf8_lossy(&captured)
            .lines()
            .filter_map(|line| line.split_once(&needle).map(|(_, rest)| rest.to_string()))
            .collect()
    }

    fn library_entry(name: &str, package: &str) -> SymbolEntry {
        let zero = Range::new(Position::new(0, 0), Position::new(0, 0));
        SymbolEntry {
            uri: Arc::new(Url::parse("file:///jdk.jar").unwrap()),
            name: name.to_string(),
            kind: IndexKind::Class,
            package: Some(Arc::from(package)),
            container: Arc::from(Vec::<String>::new()),
            full_range: zero,
            selection_range: zero,
            dependency: true,
            library_source: false,
            synthetic: false,
        }
    }

    /// A diagnostics module over a standalone bus whose index holds `Object`,
    /// `String`, and `java.util.List`, and is ready.
    fn module(label: &str) -> DiagnosticsModule {
        let client = IndexHandle::standalone().labeled(label);
        let entries = vec![
            library_entry("Object", "java.lang"),
            library_entry("String", "java.lang"),
            library_entry("List", "java.util"),
        ];
        client.upsert_file(&Url::parse("file:///jdk.jar").unwrap(), entries.clone());
        client.set_types(Arc::new(TypeModel::from_entries(&entries)));
        client.set_ready();
        DiagnosticsModule {
            client,
            parser: java_parser(),
            docs: HashMap::new(),
            results: HashMap::new(),
            semantic: true,
        }
    }

    fn count(requests: &[String], request: &str) -> usize {
        requests
            .iter()
            .filter(|line| line.as_str() == request)
            .count()
    }

    #[test]
    fn a_sweep_reads_the_index_in_one_batch_for_every_open_document() {
        capture_bus_log();
        let label = "diagnostics-batch-test";
        let mut module = module(label);
        let a = Url::parse("file:///work/app/A.java").unwrap();
        let b = Url::parse("file:///work/app/B.java").unwrap();
        let a_text = "package app;\n\nimport java.util.*;\n\nclass A {\n    String name;\n    String echo(String s) { return s; }\n    Missing missing;\n}\n";
        let b_text = "package app;\n\nimport java.util.List;\n\nclass B {\n    String name;\n    List<String> items;\n}\n";
        module
            .docs
            .insert(a.clone(), (1, Arc::new(a_text.to_string())));
        module
            .docs
            .insert(b.clone(), (1, Arc::new(b_text.to_string())));

        module.sweep();

        let requests = requests_from(label);
        assert_eq!(count(&requests, "IndexTypeLayers"), 1, "{requests:#?}");
        assert_eq!(count(&requests, "IndexTypeModel"), 1, "{requests:#?}");
        assert_eq!(count(&requests, "IndexReady"), 1, "{requests:#?}");
        let batches = requests
            .iter()
            .filter(|line| line.starts_with("IndexQueryNames count="))
            .count();
        assert_eq!(batches, 1, "{requests:#?}");
        let package_batches = requests
            .iter()
            .filter(|line| line.starts_with("IndexHasPackages count="))
            .count();
        assert!(package_batches <= 1, "{requests:#?}");
        assert_eq!(
            count(&requests, "IndexHasPackage java.util"),
            0,
            "{requests:#?}"
        );
        let singles: Vec<&String> = requests
            .iter()
            .filter(|line| line.starts_with("IndexQueryName "))
            .collect();
        let distinct: HashSet<&String> = singles.iter().copied().collect();
        assert_eq!(singles.len(), distinct.len(), "{requests:#?}");
        assert!(!singles
            .iter()
            .any(|line| line.as_str() == "IndexQueryName String"));

        // The findings are the pass's usual ones.
        let (_, a_diagnostics) = module.diagnostics(&a).expect("A was swept");
        let messages: Vec<&str> = a_diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();
        assert_eq!(messages, vec!["cannot resolve type `Missing`"]);
        let (_, b_diagnostics) = module.diagnostics(&b).expect("B was swept");
        assert!(b_diagnostics.is_empty(), "{b_diagnostics:?}");
    }

    #[test]
    fn a_sweep_without_semantic_diagnostics_does_not_query_names() {
        capture_bus_log();
        let label = "diagnostics-batch-test-off";
        let mut module = module(label);
        module.semantic = false;
        let a = Url::parse("file:///work/app/A.java").unwrap();
        module.docs.insert(
            a.clone(),
            (1, Arc::new("class A {\n    Missing m;\n}\n".to_string())),
        );

        module.sweep();

        let requests = requests_from(label);
        assert!(
            !requests
                .iter()
                .any(|line| line.starts_with("IndexQueryName")
                    || line.starts_with("IndexReady")
                    || line.starts_with("IndexTypeModel")),
            "{requests:#?}"
        );
        let (_, diagnostics) = module.diagnostics(&a).expect("A was swept");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }
}
