//! The index subsystem — the workspace's symbols and declared types.
//!
//! The index owns its own warm-up: it walks the project, parses the workspace's
//! `.java` files, and indexes the dependency jars and the JDK itself, rather than
//! being fed by separate scanner driver modules. The only things it is told from
//! outside are the changes it cannot observe on its own — open-buffer edits and
//! watched file events — plus the read requests it answers; that vocabulary lives
//! in `messages.rs`.
//!
//! This is the declaration pass: the two stores ([`SymbolIndex`] for flat
//! declarations and imports, [`TypeStore`] for the layered declared-type model),
//! the shared identity types, and the workspace source scan. The scan is the
//! index's own: [`spawn`] subscribes for the workspace root, walks it, and parses
//! each `.java` file into a file's entries ([`extract_entries`]) and its
//! declared-type model ([`crate::types::collect_type_infos`]), driven by
//! [`scan_sources`]. The read operations, and the jar and JDK passes, arrive as
//! the surrounding modules are reintroduced.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::{Position, Range, Url};
use tree_sitter::{Node, Parser, Tree};

use self::types::{ModelLayers, SourceLayerIndex, TypeModel};
use crate::hub::HubClient;
use crate::messages::{Message, Notification};

pub mod types;

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// What kind of declaration a symbol is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SymbolKind {
    Class,
    Interface,
    Enum,
    Record,
    Method,
    Field,
    /// One `enum` constant — completes as an enum member and is offered by a
    /// receiver like a field, but is not a type and not a plain field.
    EnumConstant,
    Import,
}

/// Transitional alias for the modules that still say `IndexKind`; new code
/// should say [`SymbolKind`].
pub use SymbolKind as IndexKind;

/// Whether a [`SymbolKind`] names a type declaration (as opposed to a member,
/// an import, or an enum constant).
pub fn is_type_kind(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class | SymbolKind::Interface | SymbolKind::Enum | SymbolKind::Record
    )
}

/// Where a file's symbols came from. Replaces a `dependency` + `library_source`
/// pair of flags: one byte, and the predicates the consumer reads name the policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FileOrigin {
    /// A workspace `.java` file: navigable, searchable, renameable.
    Workspace,
    /// A class-file entry from a dependency jar or the JDK: completable, but its
    /// location is not openable, so navigation skips it.
    Dependency,
    /// Extracted library sources in the java-lsp cache: completable and openable,
    /// but never rewritten — rename refuses.
    LibrarySource,
}

/// One indexed declaration or import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolEntry {
    /// The declaring file — shared across the file's entries.
    pub uri: Arc<Url>,
    /// The simple declared name; the `Arc<str>` is shared with the `by_name` key.
    pub name: Arc<str>,
    pub kind: SymbolKind,
    /// The declaring package (`None` for the default package); shared across the
    /// file's entries.
    pub package: Option<Arc<str>>,
    /// Enclosing type names, outermost first.
    pub container: Arc<[String]>,
    pub full_range: Range,
    pub selection_range: Range,
    pub origin: FileOrigin,
    /// A Lombok-generated member: a real member of the source type that has no
    /// declaration of its own, anchored at the field it derives from.
    pub synthetic: bool,
}

// ---------------------------------------------------------------------------
// Store 1 — the symbol index
// ---------------------------------------------------------------------------

/// A flat declaration/import table: name → entries, with the per-file view that
/// keeps incremental updates and the references candidate set cheap.
#[derive(Default)]
pub struct SymbolIndex {
    /// The per-file view, for incremental replace/remove and the candidate set.
    by_file: HashMap<Url, FileRecord>,
    /// Name-ordered, so an exact lookup and a prefix range-scan share one
    /// structure. Keyed by `Arc<str>`, shared with `SymbolEntry::name`.
    by_name: BTreeMap<Arc<str>, Vec<Arc<SymbolEntry>>>,
    /// How many entries belong to each package, so a wildcard import can be
    /// checked in O(1) and a package forgotten exactly when its last entry goes.
    packages: HashMap<Arc<str>, usize>,
}

/// One file's entries and file-level origin. Each entry is shared with
/// `by_name`, so an entry is stored once.
struct FileRecord {
    /// The file's entries, in position order.
    entries: Vec<Arc<SymbolEntry>>,
    /// File-level origin, kept even when `entries` is empty, so a
    /// declaration-free workspace file still counts as a references candidate.
    origin: FileOrigin,
}

impl SymbolIndex {
    /// Replaces `uri`'s entries, keeping the name and package views consistent.
    /// Each entry is allocated once and shared between the per-file and per-name
    /// views, so an entry is stored a single time.
    pub fn upsert_file(&mut self, uri: &Url, origin: FileOrigin, entries: Vec<SymbolEntry>) {
        let entries: Vec<Arc<SymbolEntry>> = entries.into_iter().map(Arc::new).collect();
        if let Some(old) = self.by_file.insert(
            uri.clone(),
            FileRecord {
                entries: entries.clone(),
                origin,
            },
        ) {
            for entry in &old.entries {
                self.remove_from_name(entry);
                self.remove_from_package(entry);
            }
        }
        for entry in entries {
            if let Some(package) = &entry.package {
                *self.packages.entry(Arc::clone(package)).or_default() += 1;
            }
            self.by_name
                .entry(Arc::clone(&entry.name))
                .or_default()
                .push(entry);
        }
    }

    /// Drops `entry` from the name view, forgetting the name when its last entry
    /// goes.
    fn remove_from_name(&mut self, entry: &SymbolEntry) {
        let name = Arc::clone(&entry.name);
        let empty = match self.by_name.get_mut(&name) {
            Some(entries) => {
                entries.retain(|candidate| candidate.as_ref() != entry);
                entries.is_empty()
            }
            None => false,
        };
        if empty {
            self.by_name.remove(&name);
        }
    }

    /// Drops `entry` from the package counts, forgetting the package when its
    /// last entry goes.
    fn remove_from_package(&mut self, entry: &SymbolEntry) {
        let Some(package) = entry.package.clone() else {
            return;
        };
        let gone = match self.packages.get_mut(&package) {
            Some(count) => {
                *count -= 1;
                *count == 0
            }
            None => false,
        };
        if gone {
            self.packages.remove(&package);
        }
    }
}

// ---------------------------------------------------------------------------
// Store 2 — the layered declared-type store
// ---------------------------------------------------------------------------

/// One ordered set of artifact layers with a generation-keyed, name-indexed view.
///
/// A single type serves both halves of [`TypeStore`]; they differ only in what a
/// key means and in whether a layer is ever superseded.
#[derive(Default)]
struct LayerSet {
    /// Layers in increasing precedence (a later layer wins), parallel to `keys`.
    layers: Vec<Arc<TypeModel>>,
    keys: Vec<Url>,
    /// `keys[i] -> i`.
    positions: HashMap<Url, usize>,
    /// Bumped on every mutation; keys `view`.
    generation: u64,
    /// The cached name-indexed view, rebuilt only when `generation` moves.
    view: Option<(u64, Arc<SourceLayerIndex>)>,
    /// Keys superseded by a richer layer: a later put for one is refused, so a
    /// class-file jar and its extracted sources never coexist regardless of the
    /// producers' arrival order. Only the base uses this.
    superseded: HashSet<Url>,
}

impl LayerSet {
    /// Inserts or replaces the layer for `uri`, in place, and invalidates the
    /// cached name-indexed view.
    fn put(&mut self, uri: &Url, model: Arc<TypeModel>) {
        match self.positions.get(uri).copied() {
            Some(i) => self.layers[i] = model,
            None => {
                self.positions.insert(uri.clone(), self.keys.len());
                self.keys.push(uri.clone());
                self.layers.push(model);
            }
        }
        self.generation = self.generation.wrapping_add(1);
    }
}

/// The declared-type store: a non-source base, the per-source-file models, and a
/// small overlay for the files whose current model differs from warm-up.
#[derive(Default)]
struct TypeStore {
    /// Jars, the JDK, and extracted library sources, appended per artifact.
    base: LayerSet,
    /// The warm-up model of every workspace source file.
    source: LayerSet,
    /// Files whose current model differs from warm-up: open buffers, watcher
    /// re-reads, and close re-reads. This *overrides* a file's source layer
    /// rather than ranking as one more layer, and it lives outside `source` so a
    /// keystroke never invalidates the O(workspace) source view.
    dirty: Arc<HashMap<Url, Arc<TypeModel>>>,
}

impl TypeStore {
    /// Records the warm-up declared-type model of one workspace source file.
    fn set_source(&mut self, uri: &Url, model: Arc<TypeModel>) {
        self.source.put(uri, model);
    }
}

/// The two halves of the declared-type model a query reads: the non-source
/// `base` and the workspace `overlay` (source layers + dirty).
#[derive(Clone, Default)]
pub struct TypeContext {
    pub base: Arc<SourceLayerIndex>,
    pub overlay: ModelLayers,
}

/// What the index needs from the project scan. Kept distilled instead of the
/// whole project model.
#[derive(Default)]
struct WorkspaceLayout {
    source_roots: Vec<PathBuf>,
}

// ---------------------------------------------------------------------------
// The index
// ---------------------------------------------------------------------------

/// The workspace index: the symbol table, the declared-type store, the source
/// roots, and the readiness flag. Owned by the subsystem thread.
#[derive(Default)]
pub struct WorkspaceIndex {
    symbols: SymbolIndex,
    types: TypeStore,
    layout: WorkspaceLayout,
    ready: bool,
}

impl WorkspaceIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses one `.java` file and records its index entries and its
    /// declared-type model. `false` when the file cannot be read or parsed.
    fn index_source(&mut self, path: &Path, parser: &mut Parser) -> bool {
        let Ok(text) = std::fs::read_to_string(path) else {
            return false;
        };
        let Some(tree) = parser.parse(text.as_bytes(), None) else {
            return false;
        };
        let Ok(uri) = Url::from_file_path(path) else {
            return false;
        };
        let package = crate::types::file_package(&tree, &text);
        let mut model = TypeModel::new();
        model.extend(crate::types::collect_type_infos(
            package.as_deref(),
            &tree,
            &text,
        ));
        let entries = extract_entries(&uri, &tree, &text);
        self.symbols
            .upsert_file(&uri, FileOrigin::Workspace, entries);
        self.types.set_source(&uri, Arc::new(model));
        true
    }

    /// Marks the warm-up complete.
    fn set_ready(&mut self) {
        self.ready = true;
    }
}

/// A tree-sitter parser for the Java grammar.
pub(crate) fn java_parser() -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .expect("failed to load the Java grammar");
    parser
}

// ---------------------------------------------------------------------------
// Byte offsets to LSP positions
// ---------------------------------------------------------------------------

/// A file's line-start byte offsets, built once so many byte offsets convert to
/// LSP positions in `O(log n)` each instead of rescanning the text from the top
/// per position — quadratic, and the whole cost of indexing a large file.
#[derive(Clone)]
pub(crate) struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    pub(crate) fn new(text: &str) -> Self {
        let mut starts = Vec::with_capacity(text.len() / 32 + 1);
        starts.push(0);
        starts.extend(
            text.bytes()
                .enumerate()
                .filter(|(_, byte)| *byte == b'\n')
                .map(|(i, _)| i + 1),
        );
        Self { starts }
    }

    /// The LSP position (line + UTF-16 code units) of a byte offset.
    pub(crate) fn position(&self, text: &str, byte_offset: usize) -> Position {
        let mut offset = byte_offset.min(text.len());
        while offset > 0 && !text.is_char_boundary(offset) {
            offset -= 1;
        }
        let line = self.starts.partition_point(|&start| start <= offset) - 1;
        let line_start = self.starts[line];
        let character = text[line_start..offset]
            .chars()
            .map(|c| c.len_utf16() as u32)
            .sum();
        Position {
            line: line as u32,
            character,
        }
    }

    fn range(&self, text: &str, node: &Node) -> Range {
        Range {
            start: self.position(text, node.start_byte()),
            end: self.position(text, node.end_byte()),
        }
    }
}

// ---------------------------------------------------------------------------
// Entries extraction
// ---------------------------------------------------------------------------

/// Extracts the indexable declarations and imports from a parsed Java file,
/// attributing every entry to `uri` and to the file's package.
pub fn extract_entries(uri: &Url, tree: &Tree, text: &str) -> Vec<SymbolEntry> {
    let package: Option<Arc<str>> = crate::types::file_package(tree, text).map(Arc::from);
    let uri = Arc::new(uri.clone());
    let lines = LineIndex::new(text);
    let container: Arc<[String]> = Arc::from(Vec::new());
    let mut out = Vec::new();
    collect_entries(&uri, &tree.root_node(), text, &lines, &container, &mut out);
    for entry in &mut out {
        entry.package = package.clone();
    }
    out
}

/// Classes, interfaces, enums, records, methods (constructors included),
/// fields, enum constants, and imports; other nodes are descended into looking
/// for nested declarations (including local and anonymous classes).
fn collect_entries(
    uri: &Arc<Url>,
    node: &Node,
    text: &str,
    lines: &LineIndex,
    container: &Arc<[String]>,
    out: &mut Vec<SymbolEntry>,
) {
    match node.kind() {
        "class_declaration"
        | "interface_declaration"
        | "enum_declaration"
        | "record_declaration" => {
            let kind = match node.kind() {
                "class_declaration" => SymbolKind::Class,
                "interface_declaration" => SymbolKind::Interface,
                "enum_declaration" => SymbolKind::Enum,
                _ => SymbolKind::Record,
            };
            if let Some(name) = node.child_by_field_name("name") {
                let type_name = text[name.byte_range()].to_string();
                out.push(entry(uri, node, &name, kind, container, text, lines));
                // One container chain per type, shared by every entry in it.
                let inner: Arc<[String]> = {
                    let mut chain = container.to_vec();
                    chain.push(type_name.clone());
                    Arc::from(chain)
                };
                // A record's components are declared in its header, not its body.
                if node.kind() == "record_declaration" {
                    collect_record_components(uri, node, text, lines, &inner, out);
                }
                // Lombok-generated members have no declaration of their own, so
                // they are anchored at the field (or the type's name) they derive
                // from, and flagged synthetic.
                collect_lombok_entries(uri, node, text, &type_name, lines, &inner, out);
                if let Some(body) = node.child_by_field_name("body") {
                    collect_entries(uri, &body, text, lines, &inner, out);
                }
                return;
            }
        }
        "method_declaration" | "constructor_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push(entry(
                    uri,
                    node,
                    &name,
                    SymbolKind::Method,
                    container,
                    text,
                    lines,
                ));
            }
        }
        "field_declaration" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "variable_declarator" {
                    if let Some(name) = child.child_by_field_name("name") {
                        out.push(entry(
                            uri,
                            &child,
                            &name,
                            SymbolKind::Field,
                            container,
                            text,
                            lines,
                        ));
                    }
                }
            }
        }
        "enum_constant" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push(entry(
                    uri,
                    node,
                    &name,
                    SymbolKind::EnumConstant,
                    container,
                    text,
                    lines,
                ));
            }
        }
        "import_declaration" => {
            let mut cursor = node.walk();
            let target = node
                .children(&mut cursor)
                .find(|child| child.kind() == "scoped_identifier")
                .unwrap_or(*node);
            out.push(SymbolEntry {
                uri: Arc::clone(uri),
                name: import_name(node, text).into(),
                kind: SymbolKind::Import,
                package: None,
                container: Arc::clone(container),
                full_range: lines.range(text, node),
                selection_range: lines.range(text, &target),
                origin: FileOrigin::Workspace,
                synthetic: false,
            });
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.is_named() {
            collect_entries(uri, &child, text, lines, container, out);
        }
    }
}

/// A record's header components, indexed as accessor methods at their declared
/// positions so go-to-definition, references, rename, and `workspace/symbol`
/// can target them.
fn collect_record_components(
    uri: &Arc<Url>,
    node: &Node,
    text: &str,
    lines: &LineIndex,
    container: &Arc<[String]>,
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
                SymbolKind::Method,
                container,
                text,
                lines,
            ));
        }
    }
}

/// The Lombok-generated members of a type declaration, as synthetic entries
/// anchored at the field (or the type's name) each derives from. A generated
/// constructor is a `Method` entry named after the type, exactly like a
/// declared one.
fn collect_lombok_entries(
    uri: &Arc<Url>,
    node: &Node,
    text: &str,
    type_name: &str,
    lines: &LineIndex,
    container: &Arc<[String]>,
    out: &mut Vec<SymbolEntry>,
) {
    for generated in crate::types::lombok_generated(node, text, type_name) {
        // A builder member is nested in the builder type, itself nested in the
        // declared type (already the last container segment).
        let container = match &generated.container {
            Some(builder) => {
                let mut chain = container.to_vec();
                chain.push(builder.clone());
                Arc::from(chain)
            }
            None => Arc::clone(container),
        };
        out.push(SymbolEntry {
            uri: Arc::clone(uri),
            name: generated.name.into(),
            kind: generated.kind,
            package: None,
            container,
            full_range: lines.range(text, &generated.anchor),
            selection_range: lines.range(text, &generated.anchor_name),
            origin: FileOrigin::Workspace,
            synthetic: true,
        });
    }
}

fn entry(
    uri: &Arc<Url>,
    node: &Node,
    name: &Node,
    kind: SymbolKind,
    container: &Arc<[String]>,
    text: &str,
    lines: &LineIndex,
) -> SymbolEntry {
    SymbolEntry {
        uri: Arc::clone(uri),
        name: text[name.byte_range()].into(),
        kind,
        package: None,
        container: Arc::clone(container),
        full_range: lines.range(text, node),
        selection_range: lines.range(text, name),
        origin: FileOrigin::Workspace,
        synthetic: false,
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

// ---------------------------------------------------------------------------
// The workspace source scan
// ---------------------------------------------------------------------------

/// Parses each workspace `.java` file, recording its index entries and its
/// declared-type model. Returns the number of files indexed.
pub(crate) fn scan_sources(files: &[PathBuf], index: &mut WorkspaceIndex) -> usize {
    let mut parser = java_parser();
    let mut indexed = 0usize;
    for path in files {
        if index.index_source(path, &mut parser) {
            indexed += 1;
        }
    }
    indexed
}

/// Every `.java` file under `root`, hidden and build-output directories skipped,
/// sorted. The Maven source-root walk folds in with the project module.
fn workspace_java_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_java_files(root, &mut files);
    files.sort();
    files
}

fn collect_java_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if is_skipped_dir(&path) {
                continue;
            }
            collect_java_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "java") {
            out.push(path);
        }
    }
}

fn is_skipped_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.') || name == "target" || name == "build")
}

/// Starts the index on the hub: on the workspace root it walks the `.java` files
/// under it and fills the index with their entries and declared-type models.
pub fn spawn(hub: &HubClient) {
    let hub = hub.clone();
    tokio::spawn(async move {
        let mut rx = hub.subscribe().await;
        let mut index = WorkspaceIndex::new();
        while let Some(message) = rx.recv().await {
            let Notification::FolderAdded { uri } = &*message else {
                continue;
            };
            let Ok(root) = uri.to_file_path() else {
                continue;
            };
            index = tokio::task::spawn_blocking(move || {
                let files = workspace_java_files(&root);
                scan_sources(&files, &mut index);
                index.set_ready();
                index
            })
            .await
            .unwrap_or_default();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Tree {
        java_parser()
            .parse(text.as_bytes(), None)
            .expect("a parseable tree")
    }

    fn entry_named(entries: &[SymbolEntry], name: &str) -> SymbolEntry {
        entries
            .iter()
            .find(|entry| &*entry.name == name)
            .unwrap_or_else(|| panic!("no entry named {name}"))
            .clone()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "java-lsp-index-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn extraction_covers_kinds_and_container_chains() {
        let text = "package a.b;\n\nimport java.util.List;\n\npublic class Outer {\n    int field;\n    void run() {}\n    static class Inner {}\n}\n\nenum E { A, B }\n";
        let uri = Url::parse("file:///workspace/Outer.java").unwrap();
        let entries = extract_entries(&uri, &parse(text), text);

        let outer = entry_named(&entries, "Outer");
        assert_eq!(outer.kind, SymbolKind::Class);
        assert_eq!(outer.package.as_deref(), Some("a.b"));
        assert_eq!(outer.origin, FileOrigin::Workspace);
        assert!(outer.container.is_empty());

        let inner = entry_named(&entries, "Inner");
        assert_eq!(inner.container.to_vec(), vec!["Outer".to_string()]);

        let field = entry_named(&entries, "field");
        assert_eq!(field.kind, SymbolKind::Field);
        assert_eq!(field.container.to_vec(), vec!["Outer".to_string()]);

        assert_eq!(entry_named(&entries, "run").kind, SymbolKind::Method);
        assert_eq!(
            entry_named(&entries, "java.util.List").kind,
            SymbolKind::Import
        );

        let variant = entry_named(&entries, "A");
        assert_eq!(variant.kind, SymbolKind::EnumConstant);
        assert_eq!(variant.container.to_vec(), vec!["E".to_string()]);
    }

    #[test]
    fn lombok_annotations_are_indexed_as_synthetic_entries() {
        let text = "package a;\n\nimport lombok.Getter;\n\n@Getter\nclass Widget {\n    private int size;\n}\n";
        let uri = Url::parse("file:///workspace/Widget.java").unwrap();
        let entries = extract_entries(&uri, &parse(text), text);

        let getter = entry_named(&entries, "getSize");
        assert!(getter.synthetic);
        assert_eq!(getter.kind, SymbolKind::Method);
        assert_eq!(getter.container.to_vec(), vec!["Widget".to_string()]);
    }

    #[test]
    fn scan_sources_records_entries_and_models() {
        let dir = temp_dir("scan");
        let file = dir.join("a").join("Widget.java");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            "package a;\n\npublic class Widget {\n    public void run() {}\n}\n",
        )
        .unwrap();

        let mut index = WorkspaceIndex::new();
        let count = scan_sources(std::slice::from_ref(&file), &mut index);

        assert_eq!(count, 1);
        let uri = Url::from_file_path(&file).unwrap();
        assert!(index.symbols.by_file.contains_key(&uri));
        assert!(index.types.source.positions.contains_key(&uri));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_walk_skips_hidden_and_build_trees() {
        let dir = temp_dir("walk");
        for relative in ["Keep.java", ".hidden/Hidden.java", "target/Built.java"] {
            let path = dir.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "class X {}\n").unwrap();
        }

        assert_eq!(workspace_java_files(&dir), vec![dir.join("Keep.java")]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
