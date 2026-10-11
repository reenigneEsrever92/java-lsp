//! The index subsystem — the workspace's symbols and declared types.
//!
//! The index owns its own warm-up: it walks the project, parses the workspace's
//! `.java` files, and indexes the dependency jars and the JDK itself, rather than
//! being fed by separate scanner driver modules. The only things it is told from
//! outside are the changes it cannot observe on its own — open-buffer edits and
//! watched file events — plus the read requests it answers; that vocabulary lives
//! in `messages.rs`.
//!
//! This module is behaviour only: every operation is a free function over the
//! index data in [`types`]. The index's own actions ([`index_source`],
//! [`scan_sources`], [`set_ready`], [`upsert_file`]) and the declared-type
//! queries the engine reads ([`find_unique`], [`lookup`], [`members`], …) all
//! take the index they operate on — [`WorkspaceIndex`] — as their first
//! parameter. The read operations are the declaration pass's counterpart to the
//! jar and JDK passes, which arrive as the surrounding modules are reintroduced.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tower_lsp::lsp_types::{Position, Range, Url};
use tree_sitter::{Node, Parser, Tree};

use self::types::{
    params_key, FileRecord, LayerSet, LineIndex, Member, ModelLayers, SourceLayerIndex, TypeInfo,
    TypeModel,
};
use crate::hub::HubClient;
use crate::messages::Notification;

pub mod extract;
pub mod types;

pub use self::types::{
    FileOrigin, IndexKind, SymbolEntry, SymbolKind, TypeContext, WorkspaceIndex,
};

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// Whether a [`SymbolKind`] names a type declaration (as opposed to a member,
/// an import, or an enum constant).
pub fn is_type_kind(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class | SymbolKind::Interface | SymbolKind::Enum | SymbolKind::Record
    )
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
    let package: Option<Arc<str>> = self::extract::file_package(tree, text).map(Arc::from);
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
    for generated in self::extract::lombok_generated(node, text, type_name) {
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
// The symbol index
// ---------------------------------------------------------------------------

/// Replaces `uri`'s entries, keeping the name and package views consistent.
/// Each entry is allocated once and shared between the per-file and per-name
/// views, so an entry is stored a single time.
pub fn upsert_file(index: &mut WorkspaceIndex, uri: &Url, origin: FileOrigin, entries: Vec<SymbolEntry>) {
    let entries: Vec<Arc<SymbolEntry>> = entries.into_iter().map(Arc::new).collect();
    if let Some(old) = index.symbols.by_file.insert(
        uri.clone(),
        FileRecord {
            entries: entries.clone(),
            origin,
        },
    ) {
        for entry in &old.entries {
            remove_from_name(&mut index.symbols, entry);
            remove_from_package(&mut index.symbols, entry);
        }
    }
    for entry in entries {
        if let Some(package) = &entry.package {
            *index.symbols.packages.entry(Arc::clone(package)).or_default() += 1;
        }
        index
            .symbols
            .by_name
            .entry(Arc::clone(&entry.name))
            .or_default()
            .push(entry);
    }
}

/// Drops `entry` from the name view, forgetting the name when its last entry
/// goes.
fn remove_from_name(symbols: &mut types::SymbolIndex, entry: &SymbolEntry) {
    let name = Arc::clone(&entry.name);
    let empty = match symbols.by_name.get_mut(&name) {
        Some(entries) => {
            entries.retain(|candidate| candidate.as_ref() != entry);
            entries.is_empty()
        }
        None => false,
    };
    if empty {
        symbols.by_name.remove(&name);
    }
}

/// Drops `entry` from the package counts, forgetting the package when its last
/// entry goes.
fn remove_from_package(symbols: &mut types::SymbolIndex, entry: &SymbolEntry) {
    let Some(package) = entry.package.clone() else {
        return;
    };
    let gone = match symbols.packages.get_mut(&package) {
        Some(count) => {
            *count -= 1;
            *count == 0
        }
        None => false,
    };
    if gone {
        symbols.packages.remove(&package);
    }
}

// ---------------------------------------------------------------------------
// The declared-type store
// ---------------------------------------------------------------------------

/// Inserts or replaces the layer for `uri`, in place, and invalidates the
/// cached name-indexed view.
fn put(set: &mut LayerSet, uri: &Url, model: Arc<TypeModel>) {
    match set.positions.get(uri).copied() {
        Some(i) => set.layers[i] = model,
        None => {
            set.positions.insert(uri.clone(), set.keys.len());
            set.keys.push(uri.clone());
            set.layers.push(model);
        }
    }
    set.generation = set.generation.wrapping_add(1);
}

/// Records the warm-up declared-type model of one workspace source file.
pub fn set_source(index: &mut WorkspaceIndex, uri: &Url, model: Arc<TypeModel>) {
    put(&mut index.types.source, uri, model);
}

/// Builds one artifact set's name-indexed [`SourceLayerIndex`] view.
fn source_layer_index(set: &LayerSet) -> Arc<SourceLayerIndex> {
    let layers: Vec<(Url, Arc<TypeModel>)> = set
        .keys
        .iter()
        .cloned()
        .zip(set.layers.iter().cloned())
        .collect();
    Arc::new(SourceLayerIndex::ordered(layers))
}

/// Rebuilds the effective declared-type view a query reads from the store's
/// layers: the non-source `base` and the workspace `overlay` (source layers over
/// the dirty edit overlay).
pub fn rebuild_context(index: &mut WorkspaceIndex) {
    let base = source_layer_index(&index.types.base);
    let source = source_layer_index(&index.types.source);
    let overlay = ModelLayers::layered(source, Arc::clone(&index.types.dirty));
    index.context = TypeContext { base, overlay };
}

// ---------------------------------------------------------------------------
// The declared-type queries
// ---------------------------------------------------------------------------

/// The single type named `name`, preferring `package`; `None` when absent or
/// ambiguous. The workspace overlay wins over the non-source base.
pub fn find_unique<'a>(index: &'a WorkspaceIndex, name: &str, package: Option<&str>) -> Option<&'a TypeInfo> {
    index
        .context
        .overlay
        .find_unique(name, package)
        .or_else(|| index.context.base.find_unique(name, package))
}

/// The single type declared with this simple name in exactly `package`. Unlike
/// [`find_unique`], a name that only matches a type in another package yields
/// `None` rather than a unique-match fallback.
pub fn find_in_package<'a>(
    index: &'a WorkspaceIndex,
    name: &str,
    package: Option<&str>,
) -> Option<&'a TypeInfo> {
    index
        .context
        .overlay
        .find_in_package(name, package)
        .or_else(|| index.context.base.find_in_package(name, package))
}

/// True when any type is declared with this simple name.
pub fn contains(index: &WorkspaceIndex, name: &str) -> bool {
    index.context.overlay.contains(name) || index.context.base.contains(name)
}

/// Every declared type the index holds, overlay first, with same-slot duplicates
/// collapsed. The name-keyed queries cannot express "all types nested in `X`", so
/// callers that need such a scan use this (and [`nested_types`], built on it).
pub fn all_types(index: &WorkspaceIndex) -> Vec<&TypeInfo> {
    let mut seen: HashSet<(&str, Option<&str>, IndexKind, Option<&str>)> = HashSet::new();
    let mut out: Vec<&TypeInfo> = Vec::new();
    for info in index
        .context
        .overlay
        .all_types()
        .into_iter()
        .chain(index.context.base.all_types())
    {
        let slot = (
            info.name.as_str(),
            info.package.as_deref(),
            info.kind,
            info.nested.as_deref(),
        );
        if seen.insert(slot) {
            out.push(info);
        }
    }
    out
}

/// The types directly nested in the type `owner` declared in `package`, in no
/// particular order. A nested type records its enclosing chain in
/// `TypeInfo::nested`, so a direct child's innermost `$`-segment names it.
pub fn nested_types<'a>(index: &'a WorkspaceIndex, owner: &str, package: Option<&str>) -> Vec<&'a TypeInfo> {
    all_types(index)
        .into_iter()
        .filter(|info| {
            info.package.as_deref() == package
                && info
                    .nested
                    .as_deref()
                    .and_then(|chain| chain.rsplit('$').next())
                    == Some(owner)
        })
        .collect()
}

/// The top-level types declared directly in `package`. Like [`nested_types`],
/// this is a scan over [`all_types`] for a query the name index cannot express
/// (a package's members); nested types are excluded, since a package-qualified
/// reference to one names its owner chain, not the package alone.
pub fn types_in_package<'a>(index: &'a WorkspaceIndex, package: &str) -> Vec<&'a TypeInfo> {
    all_types(index)
        .into_iter()
        .filter(|info| info.nested.is_none() && info.package.as_deref() == Some(package))
        .collect()
}

/// The type a reference points at. A dotted name with no `$` is resolved
/// nested-first — its prefix as a type, then the last segment among that type's
/// nested types — falling back to reading the prefix as a package; a name with
/// no `.` resolves with `package` breaking ties.
pub fn lookup<'a>(index: &'a WorkspaceIndex, ty: &types::Ty, package: Option<&str>) -> Option<&'a TypeInfo> {
    match ty {
        types::Ty::Array(element) => lookup(index, element, package),
        types::Ty::Ref { name, .. } | types::Ty::Var(name) => {
            // A dotted name with no `$` may name a nested type (`Outer.Inner`)
            // or a package-qualified one (`java.util.List`); Java gives no
            // syntactic signal, so the in-scope type reading is tried first and
            // the package reading is the fallback.
            if !name.contains('$') {
                if let Some((owner_path, simple)) = name.rsplit_once('.') {
                    if let Some(owner) = lookup(index, &types::Ty::reference(owner_path), package) {
                        if let Some(nested) = nested_types(index, &owner.name, owner.package.as_deref())
                            .into_iter()
                            .find(|info| info.name == simple)
                        {
                            return Some(nested);
                        }
                    }
                    return find_in_package(index, simple, Some(owner_path));
                }
            }
            let simple = name.rsplit(['.', '$']).next().unwrap_or(name);
            match ty.qualified_package() {
                Some(qualifier) => find_in_package(index, simple, Some(qualifier)),
                None => find_unique(index, simple, package),
            }
        }
        _ => None,
    }
}

/// The type's own constructors. Constructors are not inherited, so unlike
/// [`members`] this does not walk the hierarchy.
pub fn constructors(index: &WorkspaceIndex, ty: &types::Ty, package: Option<&str>) -> Vec<Member> {
    lookup(index, ty, package)
        .map(|info| info.constructors.clone())
        .unwrap_or_default()
}

/// The members of `ty`, including inherited ones. Supertype names resolve with
/// the subtype's own package as context; cycles are guarded and the first
/// declaration of a name wins. Same-named overloads collapse to the first — see
/// [`members_with_overloads`] to keep them apart.
pub fn members(index: &WorkspaceIndex, ty: &types::Ty, package: Option<&str>) -> Vec<Member> {
    let mut out = Vec::new();
    let mut seen: HashSet<(String, bool)> = HashSet::new();
    for member in members_with_overloads(index, ty, package) {
        let is_method = member.kind == IndexKind::Method;
        if seen.insert((member.name.clone(), is_method)) {
            out.push(member);
        }
    }
    out
}

/// The members of `ty`, including inherited ones, with same-named overloads kept
/// separate. Unlike [`members`] — which collapses a name to a single member for
/// typing — this deduplicates by name, kind, and parameter types, so an override
/// still collapses but genuinely distinct overloads remain. Supertype resolution
/// and the cycle guard match `members`.
pub fn members_with_overloads(
    index: &WorkspaceIndex,
    ty: &types::Ty,
    package: Option<&str>,
) -> Vec<Member> {
    if matches!(ty, types::Ty::Array(_)) {
        return Vec::new();
    }
    // The root resolves exactly when its name is qualified, and with `package`
    // breaking ties otherwise.
    let Some(root) = lookup(index, ty, package) else {
        return Vec::new();
    };
    let root_name = root.name.clone();
    let root_context = root.package.clone();
    let mut out = Vec::new();
    let mut seen: HashSet<(String, bool, String)> = HashSet::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, Option<String>)> = VecDeque::new();
    queue.push_back((root_name, root_context));
    while let Some((name, context)) = queue.pop_front() {
        if !visited.insert(name.clone()) {
            continue;
        }
        let Some(info) = find_unique(index, &name, context.as_deref()) else {
            continue;
        };
        for member in info.members() {
            let is_method = member.kind == IndexKind::Method;
            let key = (member.name.clone(), is_method, params_key(&member.params));
            if seen.insert(key) {
                out.push(member.clone());
            }
        }
        let super_context = info.package.clone().or(context);
        for supertype in &info.supertypes {
            // A qualified supertype (every class-file edge) resolves in its own
            // package, so a base whose simple name is shared across packages
            // still finds the right type; a source-written simple name falls
            // back to the subtype's own context as before.
            if supertype.qualified_package().is_some() {
                if let Some(resolved) = lookup(index, supertype, None) {
                    queue.push_back((resolved.name.clone(), resolved.package.clone()));
                }
            } else if let Some(super_name) = supertype.simple_name() {
                queue.push_back((super_name.to_string(), super_context.clone()));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The index
// ---------------------------------------------------------------------------

/// Parses one `.java` file and records its index entries and its declared-type
/// model. `false` when the file cannot be read or parsed.
pub(crate) fn index_source(index: &mut WorkspaceIndex, path: &Path, parser: &mut Parser) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Some(tree) = parser.parse(text.as_bytes(), None) else {
        return false;
    };
    let Ok(uri) = Url::from_file_path(path) else {
        return false;
    };
    let package = self::extract::file_package(&tree, &text);
    let mut model = TypeModel::new();
    model.extend(self::extract::collect_type_infos(
        package.as_deref(),
        &tree,
        &text,
    ));
    let entries = extract_entries(&uri, &tree, &text);
    upsert_file(index, &uri, FileOrigin::Workspace, entries);
    set_source(index, &uri, Arc::new(model));
    true
}

/// Marks the warm-up complete and builds the declared-type view a query reads.
pub fn set_ready(index: &mut WorkspaceIndex) {
    index.ready = true;
    rebuild_context(index);
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
        if index_source(index, path, &mut parser) {
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
        let mut index = WorkspaceIndex::default();
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
                set_ready(&mut index);
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

        let mut index = WorkspaceIndex::default();
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
