//! Workspace-wide, in-memory index of Java declarations and imports.
//!
//! Entries are flat [`SymbolEntry`]s (no trees, no text — memory stays
//! proportional to workspace size). The initial scan runs off the request
//! path ([`scan_workspace`], spawned by the engine); edits to open documents
//! update only that file's entries. `ready()` gates index-backed features
//! until warm-up completes — they may be briefly unavailable, never blocking.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use tower_lsp::lsp_types::{Range, Url};
use tree_sitter::{Node, Parser, Tree};

use crate::engine::syntax::lsp_range;
use crate::resolve::{resolve_closure, Resolver};

/// What kind of declaration an indexed entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    Class,
    Interface,
    Enum,
    Record,
    Method,
    Field,
    Import,
}

/// One indexed declaration or import in one file or dependency jar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolEntry {
    /// The file this entry comes from — how navigation tells same-file
    /// overloads (resolvable) from cross-file ambiguity (not).
    pub uri: Url,
    pub name: String,
    pub kind: IndexKind,
    /// The symbol's package (`None` for the default package). With the
    /// container chain this yields the fully qualified name the completion
    /// pipeline imports from.
    pub package: Option<String>,
    /// Enclosing type names, outermost first.
    pub container: Vec<String>,
    pub full_range: Range,
    pub selection_range: Range,
    /// True for entries from dependency jars: offered in completions, but
    /// filtered out of navigation (jar locations are not openable).
    pub dependency: bool,
}

#[derive(Debug, Default)]
struct IndexState {
    files: HashMap<Url, Vec<SymbolEntry>>,
    by_name: HashMap<String, Vec<SymbolEntry>>,
}

/// Cheaply cloneable handle to the shared index state and its warm flag.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceIndex {
    state: Arc<RwLock<IndexState>>,
    warm: Arc<AtomicBool>,
    /// The project model discovered by the background scan; drives the
    /// close-policy's "is this file a workspace source" check.
    model: Arc<std::sync::Mutex<Option<crate::project::ProjectModel>>>,
    /// The declared-type model (R7), built by the same warm-up scan: source
    /// types with signatures alongside the jars' and JDK's, parsed from their
    /// class files. `None` until warm-up has built it.
    types: Arc<std::sync::Mutex<Option<Arc<crate::types::TypeModel>>>>,
}

impl WorkspaceIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the entries for `uri`, keeping the name lookup consistent.
    pub fn upsert_file(&self, uri: &Url, entries: Vec<SymbolEntry>) {
        let Ok(mut state) = self.state.write() else {
            return;
        };
        if let Some(old) = state.files.insert(uri.clone(), entries.clone()) {
            for old_entry in old {
                remove_from_name_index(&mut state, &old_entry);
            }
        }
        for entry in entries {
            state
                .by_name
                .entry(entry.name.clone())
                .or_default()
                .push(entry);
        }
    }

    /// Drops the entries for `uri`.
    pub fn remove_file(&self, uri: &Url) {
        let Ok(mut state) = self.state.write() else {
            return;
        };
        if let Some(old) = state.files.remove(uri) {
            for old_entry in old {
                remove_from_name_index(&mut state, &old_entry);
            }
        }
    }

    /// All entries declared with exactly `name`.
    pub fn query_name(&self, name: &str) -> Vec<SymbolEntry> {
        self.state
            .read()
            .ok()
            .and_then(|state| state.by_name.get(name).cloned())
            .unwrap_or_default()
    }

    /// All entries whose name starts with `prefix` (case-sensitive), ordered
    /// by name and then position.
    pub fn query_prefix(&self, prefix: &str) -> Vec<SymbolEntry> {
        let Ok(state) = self.state.read() else {
            return Vec::new();
        };
        let mut out: Vec<SymbolEntry> = state
            .by_name
            .iter()
            .filter(|(name, _)| name.starts_with(prefix))
            .flat_map(|(_, entries)| entries.iter().cloned())
            .collect();
        out.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| cmp_position(&a.full_range, &b.full_range))
        });
        out
    }

    /// Flat snapshot of everything currently indexed, ordered by URI and then
    /// position. The verification hook for tests; the consumer-facing queries
    /// arrive with the completions and navigation CRs.
    pub fn all_symbols(&self) -> Vec<SymbolEntry> {
        let Ok(state) = self.state.read() else {
            return Vec::new();
        };
        let mut files: Vec<(&Url, &Vec<SymbolEntry>)> = state.files.iter().collect();
        files.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        let mut out = Vec::new();
        for (_, entries) in files {
            let mut entries = entries.clone();
            entries.sort_by(|a, b| cmp_position(&a.full_range, &b.full_range));
            out.extend(entries);
        }
        out
    }

    /// Number of files currently contributing entries.
    pub fn file_count(&self) -> usize {
        self.state
            .read()
            .map(|state| state.files.len())
            .unwrap_or(0)
    }

    /// True once the initial workspace scan finished (or if there is no scan
    /// to wait for).
    pub fn ready(&self) -> bool {
        self.warm.load(Ordering::Acquire)
    }

    pub fn set_ready(&self) {
        self.warm.store(true, Ordering::Release);
    }

    /// The project model from the background scan (source roots etc.).
    pub fn set_model(&self, model: crate::project::ProjectModel) {
        if let Ok(mut slot) = self.model.lock() {
            *slot = Some(model);
        }
    }

    /// The workspace's declared-type model, if warm-up has built it yet.
    pub fn type_model(&self) -> Option<Arc<crate::types::TypeModel>> {
        self.types
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(Arc::clone))
    }

    pub fn set_types(&self, model: Arc<crate::types::TypeModel>) {
        if let Ok(mut slot) = self.types.lock() {
            *slot = Some(model);
        }
    }

    /// The workspace's `.java` source files (jar and JDK archive URIs are
    /// excluded), sorted — the candidate set for a references or rename search,
    /// requiring no second directory walk.
    pub fn source_files(&self) -> Vec<Url> {
        let Ok(state) = self.state.read() else {
            return Vec::new();
        };
        let mut files: Vec<Url> = state
            .files
            .keys()
            .filter(|uri| uri.path().ends_with(".java"))
            .cloned()
            .collect();
        files.sort();
        files
    }

    /// The workspace's source roots (empty until the scan ran; the fallback
    /// model covers the whole root).
    pub fn source_roots(&self) -> Vec<std::path::PathBuf> {
        self.model
            .lock()
            .ok()
            .and_then(|slot| {
                slot.as_ref()
                    .map(crate::project::ProjectModel::source_roots)
            })
            .unwrap_or_default()
    }
}

fn remove_from_name_index(state: &mut IndexState, entry: &SymbolEntry) {
    if let Some(list) = state.by_name.get_mut(&entry.name) {
        list.retain(|candidate| candidate != entry);
        if list.is_empty() {
            state.by_name.remove(&entry.name);
        }
    }
}

fn cmp_position(a: &Range, b: &Range) -> std::cmp::Ordering {
    a.start
        .line
        .cmp(&b.start.line)
        .then_with(|| a.start.character.cmp(&b.start.character))
}

/// A parser configured for the Java grammar.
pub(crate) fn java_parser() -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .expect("failed to load the Java grammar");
    parser
}

/// Extracts the indexable declarations and imports from a parsed Java file,
/// attributing every entry to `uri` and to the file's package.
pub fn extract_entries(uri: &Url, tree: &Tree, text: &str) -> Vec<SymbolEntry> {
    let package = file_package(&tree.root_node(), text);
    let mut out = Vec::new();
    collect_entries(uri, &tree.root_node(), text, &mut Vec::new(), &mut out);
    for entry in &mut out {
        entry.package = package.clone();
    }
    out
}

/// The dotted name of the file's `package_declaration`, or `None` for the
/// default package.
fn file_package(root: &Node, text: &str) -> Option<String> {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "package_declaration" {
            let mut inner = child.walk();
            return child
                .children(&mut inner)
                .find(|part| part.kind() == "scoped_identifier" || part.kind() == "identifier")
                .map(|part| text[part.byte_range()].to_string());
        }
    }
    None
}

/// Classes, interfaces, enums, records, methods (constructors included),
/// fields, and imports; other nodes are descended into looking for nested
/// declarations (including local and anonymous classes).
fn collect_entries(
    uri: &Url,
    node: &Node,
    text: &str,
    container: &mut Vec<String>,
    out: &mut Vec<SymbolEntry>,
) {
    match node.kind() {
        "class_declaration"
        | "interface_declaration"
        | "enum_declaration"
        | "record_declaration" => {
            let kind = match node.kind() {
                "class_declaration" => IndexKind::Class,
                "interface_declaration" => IndexKind::Interface,
                "enum_declaration" => IndexKind::Enum,
                _ => IndexKind::Record,
            };
            if let Some(name) = node.child_by_field_name("name") {
                out.push(entry(uri, node, &name, kind, container, text));
                container.push(text[name.byte_range()].to_string());
                // A record's components are declared in its header, not its body.
                if node.kind() == "record_declaration" {
                    collect_record_components(uri, node, text, container, out);
                }
                if let Some(body) = node.child_by_field_name("body") {
                    collect_entries(uri, &body, text, container, out);
                }
                container.pop();
                return;
            }
        }
        "method_declaration" | "constructor_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push(entry(uri, node, &name, IndexKind::Method, container, text));
            }
        }
        "field_declaration" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "variable_declarator" {
                    if let Some(name) = child.child_by_field_name("name") {
                        out.push(entry(uri, &child, &name, IndexKind::Field, container, text));
                    }
                }
            }
        }
        "import_declaration" => {
            let mut cursor = node.walk();
            let target = node
                .children(&mut cursor)
                .find(|child| child.kind() == "scoped_identifier")
                .unwrap_or(*node);
            out.push(SymbolEntry {
                uri: uri.clone(),
                name: import_name(node, text),
                kind: IndexKind::Import,
                package: None,
                container: container.to_vec(),
                full_range: lsp_range(text, node),
                selection_range: lsp_range(text, &target),
                dependency: false,
            });
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.is_named() {
            collect_entries(uri, &child, text, container, out);
        }
    }
}

/// A record's header components, indexed as accessor methods at their declared
/// positions so go-to-definition, references, rename, and `workspace/symbol`
/// can target them.
fn collect_record_components(
    uri: &Url,
    node: &Node,
    text: &str,
    container: &[String],
    out: &mut Vec<SymbolEntry>,
) {
    let Some(parameters) = node.child_by_field_name("parameters") else {
        return;
    };
    let mut cursor = parameters.walk();
    for parameter in parameters.named_children(&mut cursor) {
        if parameter.kind() != "formal_parameter" {
            continue;
        }
        if let Some(name) = parameter.child_by_field_name("name") {
            out.push(entry(
                uri,
                &parameter,
                &name,
                IndexKind::Method,
                container,
                text,
            ));
        }
    }
}

fn entry(
    uri: &Url,
    node: &Node,
    name: &Node,
    kind: IndexKind,
    container: &[String],
    text: &str,
) -> SymbolEntry {
    SymbolEntry {
        uri: uri.clone(),
        name: text[name.byte_range()].to_string(),
        kind,
        package: None,
        container: container.to_vec(),
        full_range: lsp_range(text, node),
        selection_range: lsp_range(text, name),
        dependency: false,
    }
}

/// The dotted path of an import (including a trailing `.*` wildcard and any
/// `static` member), taken from the statement text so grammar details do not
/// matter.
fn import_name(node: &Node, text: &str) -> String {
    let mut raw = text[node.byte_range()].trim();
    raw = raw.strip_prefix("import").map_or(raw, str::trim);
    raw = raw.strip_prefix("static").map_or(raw, str::trim);
    raw.trim_end_matches(';').trim().to_string()
}

/// Warm-up: builds the project model, scans the source roots (or the whole
/// root when no `pom.xml` exists), resolves each module's dependency closure
/// against the local repository, and indexes the resolved jars — all off the
/// request path; flips the warm flag at the end.
pub fn scan_workspace(root: Url, index: WorkspaceIndex) {
    let start = std::time::Instant::now();
    let Some(root_path) = root.to_file_path().ok() else {
        index.set_ready();
        return;
    };

    let mut resolver = Resolver::new(local_repository());
    let model = crate::project::discover(&root_path, &mut resolver);
    index.set_model(model.clone());

    let mut files = Vec::new();
    for source_root in model.source_roots() {
        collect_java_files(&source_root, &mut files);
    }
    files.sort();

    let mut parser = java_parser();
    let mut indexed = 0usize;
    let mut types = crate::types::TypeModel::new();
    for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let Some(tree) = parser.parse(text.as_bytes(), None) else {
            continue;
        };
        let package = crate::types::file_package(&tree, &text);
        types.extend(crate::types::collect_type_infos(
            package.as_deref(),
            &tree,
            &text,
        ));
        if let Some(uri) = Url::from_file_path(path).ok() {
            index.upsert_file(&uri, extract_entries(&uri, &tree, &text));
            indexed += 1;
        }
    }

    // Dependency jars: resolution is pom-level; a jar present in the local
    // repository is indexed even when its pom went missing mid-flight.
    let mut jars = 0usize;
    for module in &model.modules {
        let Some(effective) = &module.effective else {
            continue;
        };
        for (group, artifact, version) in resolve_closure(effective, &mut resolver) {
            let jar_path = resolver.jar_path(&group, &artifact, &version);
            if !jar_path.is_file() {
                continue;
            }
            if let Some((entries, infos)) = crate::classfile::jar_outputs(&jar_path) {
                if let Some(jar_uri) = Url::from_file_path(&jar_path).ok() {
                    types.extend(infos);
                    index.upsert_file(&jar_uri, entries);
                    jars += 1;
                }
            }
        }
    }

    // The standard library: same treatment as dependency jars (offered in
    // completions, filtered from navigation); a missing JDK is a no-op.
    let mut jdk_classes = 0usize;
    if let Some(home) = crate::jdk::locate_jdk() {
        for (archive_uri, entries, infos) in crate::jdk::jdk_entries(&home) {
            jdk_classes += entries.len();
            types.extend(infos);
            index.upsert_file(&archive_uri, entries);
        }
    } else {
        tracing::info!("no usable JDK found; standard library not indexed");
    }

    index.set_types(Arc::new(types));
    index.set_ready();
    tracing::info!(
        root = %root,
        files = indexed,
        jars,
        jdk_classes,
        maven = model.maven,
        elapsed_ms = start.elapsed().as_millis() as u64,
        "workspace index warm-up complete"
    );
}

/// The local Maven repository: `$MAVEN_REPO` if set, else `~/.m2/repository`.
fn local_repository() -> PathBuf {
    if let Ok(override_path) = std::env::var("MAVEN_REPO") {
        return PathBuf::from(override_path);
    }
    std::env::var("HOME")
        .map(|home| PathBuf::from(home).join(".m2").join("repository"))
        .unwrap_or_else(|_| PathBuf::from(".m2/repository"))
}

fn collect_java_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let hidden = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.'));
            if !hidden {
                collect_java_files(&path, out);
            }
        } else if path.extension().is_some_and(|ext| ext == "java") {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::lsp_types::Position;

    fn entry_named<'a>(entries: &'a [SymbolEntry], name: &str) -> &'a SymbolEntry {
        entries
            .iter()
            .find(|entry| entry.name == name)
            .unwrap_or_else(|| panic!("no entry named {name} in {entries:?}"))
    }

    fn parse(text: &str) -> Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .expect("failed to load the Java grammar");
        parser
            .parse(text.as_bytes(), None)
            .expect("parse must succeed")
    }

    fn uri(path: &str) -> Url {
        Url::parse(path).unwrap()
    }

    fn sample_entry(name: &str, line: u32) -> SymbolEntry {
        let position = Position::new(line, 0);
        SymbolEntry {
            name: name.to_string(),
            kind: IndexKind::Class,
            package: Some("demo".to_string()),
            container: Vec::new(),
            full_range: Range::new(position, Position::new(line, 1)),
            selection_range: Range::new(position, Position::new(line, 1)),
            dependency: false,
            uri: uri("file:///src/A.java"),
        }
    }

    #[test]
    fn upsert_replace_and_remove_keep_the_name_index_consistent() {
        let index = WorkspaceIndex::new();
        let file = uri("file:///src/A.java");

        index.upsert_file(&file, vec![sample_entry("Alpha", 0)]);
        assert_eq!(index.query_name("Alpha").len(), 1);

        // Re-upserting the same file replaces its entries, not adds.
        index.upsert_file(&file, vec![sample_entry("Beta", 1)]);
        assert!(index.query_name("Alpha").is_empty());
        assert_eq!(index.query_name("Beta").len(), 1);

        index.remove_file(&file);
        assert!(index.query_name("Beta").is_empty());
        assert_eq!(index.file_count(), 0);
    }

    #[test]
    fn same_name_in_different_files_is_kept_for_both() {
        let index = WorkspaceIndex::new();
        index.upsert_file(&uri("file:///src/A.java"), vec![sample_entry("Dupe", 0)]);
        index.upsert_file(&uri("file:///src/B.java"), vec![sample_entry("Dupe", 3)]);
        assert_eq!(index.query_name("Dupe").len(), 2);

        index.remove_file(&uri("file:///src/A.java"));
        assert_eq!(index.query_name("Dupe").len(), 1);
    }

    #[test]
    fn prefix_lookup_orders_by_name_and_position() {
        let index = WorkspaceIndex::new();
        index.upsert_file(
            &uri("file:///src/A.java"),
            vec![sample_entry("getB", 2), sample_entry("getA", 1)],
        );
        index.upsert_file(&uri("file:///src/B.java"), vec![sample_entry("getA", 0)]);

        let names: Vec<(String, u32)> = index
            .query_prefix("get")
            .into_iter()
            .map(|entry| (entry.name.clone(), entry.full_range.start.line))
            .collect();
        assert_eq!(
            names,
            vec![("getA".into(), 0), ("getA".into(), 1), ("getB".into(), 2)]
        );
    }

    #[test]
    fn ready_flag_starts_false_and_flips_once() {
        let index = WorkspaceIndex::new();
        assert!(!index.ready());
        index.set_ready();
        assert!(index.ready());
    }

    #[test]
    fn source_files_lists_java_files_only() {
        let index = WorkspaceIndex::new();
        let java = uri("file:///src/Widget.java");
        let jar = uri("file:///repo/lib-1.0.jar");
        index.upsert_file(&java, Vec::new());
        index.upsert_file(&jar, Vec::new());
        assert_eq!(index.source_files(), vec![java]);
    }

    #[test]
    fn extraction_covers_all_seven_kinds_and_container_chains() {
        let text = "\
package demo;
import java.util.List;
public class Outer {
    private int field;
    public Outer() {}
    public interface Inner { void run(); }
    public int get() { return field; }
}
enum Color { RED }
record Point(int x, int y) {}
";
        let entries = extract_entries(&uri("file:///Test.java"), &parse(text), text);

        let outer = entry_named(&entries, "Outer");
        assert_eq!(outer.kind, IndexKind::Class);
        assert!(outer.container.is_empty());

        let field = entry_named(&entries, "field");
        assert_eq!(field.kind, IndexKind::Field);
        assert_eq!(field.container, vec!["Outer".to_string()]);

        let constructors: Vec<&SymbolEntry> = entries
            .iter()
            .filter(|entry| entry.name == "Outer" && entry.kind == IndexKind::Method)
            .collect();
        assert_eq!(constructors.len(), 1);
        assert_eq!(constructors[0].container, vec!["Outer".to_string()]);

        let inner = entry_named(&entries, "Inner");
        assert_eq!(inner.kind, IndexKind::Interface);
        assert_eq!(inner.container, vec!["Outer".to_string()]);

        let run = entry_named(&entries, "run");
        assert_eq!(run.kind, IndexKind::Method);
        assert_eq!(
            run.container,
            vec!["Outer".to_string(), "Inner".to_string()]
        );

        let get = entry_named(&entries, "get");
        assert_eq!(get.kind, IndexKind::Method);
        assert_eq!(get.container, vec!["Outer".to_string()]);

        let color = entry_named(&entries, "Color");
        assert_eq!(color.kind, IndexKind::Enum);

        let point = entry_named(&entries, "Point");
        assert_eq!(point.kind, IndexKind::Record);

        let import = entry_named(&entries, "java.util.List");
        assert_eq!(import.kind, IndexKind::Import);

        // Enum constants are intentionally not indexed (seven kinds only).
        assert!(!entries.iter().any(|entry| entry.name == "RED"));
    }

    #[test]
    fn record_components_are_indexed_at_their_header_positions() {
        let text = "record Point(int x, int y) {}\n";
        let entries = extract_entries(&uri("file:///Point.java"), &parse(text), text);

        assert_eq!(entry_named(&entries, "Point").kind, IndexKind::Record);

        let x = entry_named(&entries, "x");
        assert_eq!(x.kind, IndexKind::Method);
        assert_eq!(x.container, vec!["Point".to_string()]);
        // Indexed where it is declared, in the header, not in the (empty) body.
        let declared = text.find("int x").unwrap() as u32;
        assert_eq!(x.full_range.start.line, 0);
        assert_eq!(x.full_range.start.character, declared);
        assert_eq!(x.selection_range.start.character, declared + 4);
        assert_eq!(x.selection_range.end.character, declared + 5);

        let y = entry_named(&entries, "y");
        assert_eq!(y.kind, IndexKind::Method);
        assert_eq!(y.container, vec!["Point".to_string()]);
    }

    #[test]
    fn selection_ranges_point_at_the_declared_name() {
        let text = "class Hello {\n}\n";
        let entries = extract_entries(&uri("file:///x.java"), &parse(text), text);
        let hello = entry_named(&entries, "Hello");

        // Default package: no package on the entries.
        assert_eq!(hello.package, None);
        assert_eq!(hello.selection_range.start.line, 0);
        assert_eq!(hello.selection_range.start.character, 6);
        assert_eq!(hello.selection_range.end.character, 11);
        // The full range spans the whole declaration.
        assert_eq!(hello.full_range.start.character, 0);
    }

    #[test]
    fn entries_carry_their_file_package() {
        let text = "\
package com.example.app;\n\nclass Widget {\n    void run() {}\n}\n";
        let entries = extract_entries(&uri("file:///x.java"), &parse(text), text);
        let class = entry_named(&entries, "Widget");
        let method = entry_named(&entries, "run");
        assert_eq!(class.package.as_deref(), Some("com.example.app"));
        assert_eq!(method.package.as_deref(), Some("com.example.app"));
    }

    #[test]
    fn wildcard_and_static_imports_keep_their_dotted_names() {
        let text = "\
import java.util.*;
import static java.lang.Math.max;
class A {}
";
        let entries = extract_entries(&uri("file:///Test.java"), &parse(text), text);
        assert!(entries.iter().any(|entry| entry.name == "java.util.*"));
        assert!(entries
            .iter()
            .any(|entry| entry.name == "java.lang.Math.max" && entry.kind == IndexKind::Import));
    }

    #[test]
    fn scan_workspace_indexes_a_directory_tree_and_sets_ready() {
        // Opt out of machine-JDK indexing for this fixture scan.
        let _env = crate::jdk::env_lock();
        std::env::set_var("JAVA_LSP_JDK", "/definitely/not/a/jdk");
        let root = std::env::temp_dir().join(format!(
            "java-lsp-scan-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("Top.java"), "public class Top {\n    int x;\n}\n").unwrap();
        std::fs::write(
            root.join("nested").join("Deep.java"),
            "package deep;\npublic class Deep {}\n",
        )
        .unwrap();
        // Hidden directories are skipped, non-Java files too.
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join(".hidden").join("Skip.java"), "class Skip {}\n").unwrap();
        std::fs::write(root.join("notes.txt"), "not java").unwrap();

        let root_url = Url::from_file_path(&root).unwrap();
        let index = WorkspaceIndex::new();
        scan_workspace(root_url, index.clone());

        assert!(index.ready());
        let symbols = index.all_symbols();
        let classes: Vec<&str> = symbols
            .iter()
            .filter(|entry| entry.kind == IndexKind::Class)
            .map(|entry| entry.name.as_str())
            .collect();
        // all_symbols orders by URI: the root's Top.java sorts before
        // nested/Deep.java ('T' < 'n').
        assert_eq!(classes, vec!["Top", "Deep"]);
        assert!(index.query_name("Skip").is_empty());

        // The same warm-up builds the declared-type model.
        let model = index.type_model().expect("type model");
        assert!(!model.find("Top").is_empty());
        assert!(!model.find("Deep").is_empty());

        std::env::remove_var("JAVA_LSP_JDK");
        let _ = std::fs::remove_dir_all(&root);
    }
}
