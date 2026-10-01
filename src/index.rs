//! Workspace-wide, in-memory index of Java declarations and imports.
//!
//! Entries are flat [`SymbolEntry`]s (no trees, no text — memory stays
//! proportional to workspace size). The initial scan runs off the request
//! path (the drivers the engine spawns); edits to open documents
//! update only that file's entries. `ready()` gates index-backed features
//! until warm-up completes — they may be briefly unavailable, never blocking.
//!
//! The index is a subsystem: [`IndexHandle`] is its message-based handle, and a
//! dedicated thread owns the [`WorkspaceIndex`] and is its sole reader and
//! writer.

use std::collections::{BTreeMap, HashMap};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;

use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::{Range, Url};
use tree_sitter::{Node, Parser, Tree};

use crate::analysis::LineIndex;
use crate::messages::{Bus, DriverMessage, LogLevel, ProgressUpdate, Request};
use crate::resolve::{resolve_closure, Artifact, Resolver};
use crate::types::{ModelLayers, TypeModel};

/// What kind of declaration an indexed entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum IndexKind {
    Class,
    Interface,
    Enum,
    Record,
    Method,
    Field,
    /// One constant of an `enum`, e.g. `TYPE_1`. Not a type and not a plain
    /// field: it completes as an enum member and is offered by a receiver like
    /// a field.
    EnumConstant,
    Import,
}

/// One indexed declaration or import in one file or dependency jar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolEntry {
    /// The file this entry comes from — how navigation tells same-file
    /// overloads (resolvable) from cross-file ambiguity (not). Shared across
    /// every entry of the file, so the path string is allocated once.
    pub uri: Arc<Url>,
    pub name: String,
    pub kind: IndexKind,
    /// The symbol's package (`None` for the default package). With the
    /// container chain this yields the fully qualified name the completion
    /// pipeline imports from. Shared across the file's entries.
    pub package: Option<Arc<str>>,
    /// Enclosing type names, outermost first.
    pub container: Arc<[String]>,
    pub full_range: Range,
    pub selection_range: Range,
    /// True for entries from dependency jars: offered in completions, but
    /// filtered out of navigation (jar locations are not openable).
    pub dependency: bool,
    /// True for dependency entries backed by a real extracted source file in
    /// the java-lsp cache. `dependency` still marks them library-sourced (so
    /// completions offer them and rename refuses them), but unlike a class-file
    /// entry their location *is* openable, so `definition` admits them.
    pub library_source: bool,
    /// True for a Lombok-generated member: a real member of the source type
    /// that has no declaration of its own, anchored at the field it derives
    /// from. Kept out of `workspace/symbol` and ordinary completion, and
    /// rename refuses it.
    pub synthetic: bool,
}

#[derive(Debug, Default)]
struct IndexState {
    files: HashMap<Url, Vec<Arc<SymbolEntry>>>,
    /// Entries grouped by declared name, in **name order** (`BTreeMap`), so a
    /// prefix query is a range scan over the matching names instead of a scan of
    /// every name. Could be large: one key per distinct simple name in the
    /// workspace plus the jars and the JDK.
    by_name: BTreeMap<String, Vec<Arc<SymbolEntry>>>,
    /// How many entries belong to each package, so a wildcard import can be
    /// checked without scanning every entry.
    packages: HashMap<String, usize>,
}

/// The cached, name-indexed view over a layer set, keyed by the generation it
/// was built from, so a request reuses one `Arc` instead of rebuilding it.
type CachedLayerView = Arc<std::sync::Mutex<Option<(u64, Arc<crate::types::SourceLayerIndex>)>>>;

/// Cheaply cloneable handle to the shared index state and its warm flag.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceIndex {
    state: Arc<RwLock<IndexState>>,
    warm: Arc<AtomicBool>,
    /// The project model discovered by the background scan; drives the
    /// close-policy's "is this file a workspace source" check.
    model: Arc<std::sync::Mutex<Option<crate::project::ProjectModel>>>,
    /// The non-source base (dependency jars, the JDK, and extracted library
    /// sources) as per-artifact layers, in insertion order (a later layer wins),
    /// keyed by artifact URI. Extended append-only as each producer's artifacts
    /// land, so no whole-model clone is ever needed.
    base: Arc<RwLock<BaseLayers>>,
    /// The cached, name-indexed view over `base`, keyed by its generation —
    /// rebuilt only when the base changes, so a request reuses one `Arc`.
    base_view: CachedLayerView,
    /// Per-workspace-source-file declared-type models from the warm-up scan,
    /// keyed by URI. The base `types` excludes these, so deleting a file can
    /// forget its types with a map removal rather than re-scanning the source
    /// roots (`WorkspaceIndex::remove_file`). Empty until warm-up runs.
    source_types: Arc<RwLock<SourceTypes>>,
    /// The cached, name-indexed base layer view over `source_types`, keyed by
    /// the source-model generation it was built from. Rebuilt only when the
    /// generation moves, so a request reuses one `Arc`.
    source_layers: CachedLayerView,
    /// The dirty overlay: the current declared-type model of every file whose
    /// state differs from the warm-up base (open buffers, and files a watcher
    /// event or a close re-read from disk), keyed by URI. Layered over the
    /// source models by [`WorkspaceIndex::type_layers`], so an edit replaces the
    /// file's stale warm-up contribution and cross-file edits are visible.
    dirty: Arc<std::sync::Mutex<Arc<HashMap<Url, Arc<TypeModel>>>>>,
}

/// The per-source-file models plus a generation bumped on every change, so the
/// assembled [`crate::types::SourceLayerIndex`] can be cached and invalidated
/// with no rebuild/write race.
#[derive(Debug, Default)]
struct SourceTypes {
    models: HashMap<Url, Arc<crate::types::TypeModel>>,
    generation: u64,
}

/// The per-artifact non-source base layers plus a generation bumped on every
/// change, so the assembled view can be cached and invalidated race-free.
#[derive(Debug, Default)]
struct BaseLayers {
    /// In increasing precedence (a later layer wins).
    layers: Vec<(Url, Arc<crate::types::TypeModel>)>,
    index_of: HashMap<Url, usize>,
    generation: u64,
    /// Artifact URIs whose class-file layer was superseded by extracted sources.
    /// A later `add_base_layer` for one is dropped, so the class-file and source
    /// declarations never coexist even when the downloader's removal arrives
    /// before the jar producer's add (the producers run concurrently).
    superseded: std::collections::HashSet<Url>,
}

impl WorkspaceIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// A one-line summary of what the index holds, for the composition log: the
    /// flat entries (by kind) and their approximate bytes, the base layers'
    /// types/members and bytes, and the per-file source models.
    pub(crate) fn composition(&self) -> String {
        let mut entries = 0usize;
        let mut names = 0usize;
        let mut files = 0usize;
        let mut entry_bytes = 0usize;
        let (mut types, mut enums, mut methods, mut fields, mut imports) = (0, 0, 0, 0, 0);
        if let Ok(state) = self.state.read() {
            names = state.by_name.len();
            files = state.files.len();
            for entry in state.by_name.values().flatten() {
                entries += 1;
                entry_bytes += std::mem::size_of::<SymbolEntry>()
                    + entry.name.len()
                    + entry.container.iter().map(String::len).sum::<usize>();
                match entry.kind {
                    IndexKind::Enum => enums += 1,
                    IndexKind::Method => methods += 1,
                    IndexKind::Field | IndexKind::EnumConstant => fields += 1,
                    IndexKind::Import => imports += 1,
                    IndexKind::Class | IndexKind::Interface | IndexKind::Record => types += 1,
                }
            }
        }
        let (mut layers, mut base_types, mut members, mut type_bytes) = (0, 0, 0, 0);
        if let Ok(base) = self.base.read() {
            layers = base.layers.len();
            for (_, model) in &base.layers {
                let (t, m, b) = model.stats();
                base_types += t;
                members += m;
                type_bytes += b;
            }
        }
        let (mut source_models, mut source_bytes) = (0, 0);
        if let Ok(sources) = self.source_types.read() {
            source_models = sources.models.len();
            for model in sources.models.values() {
                source_bytes += model.stats().2;
            }
        }
        format!(
            "index composition: entries={entries} (types={types} enums={enums} methods={methods} \
             fields={fields} imports={imports}) names={names} files={files} ~{}KB; base \
             layers={layers} types={base_types} members={members} ~{}KB; source models={source_models} \
             ~{}KB",
            entry_bytes / 1024,
            type_bytes / 1024,
            source_bytes / 1024,
        )
    }

    /// Replaces the entries for `uri`, keeping the name lookup consistent. Each
    /// entry is allocated once and shared between the per-file and per-name
    /// maps, so an entry is stored a single time.
    pub fn upsert_file(&self, uri: &Url, entries: Vec<SymbolEntry>) {
        let Ok(mut state) = self.state.write() else {
            return;
        };
        let entries: Vec<Arc<SymbolEntry>> = entries.into_iter().map(Arc::new).collect();
        if let Some(old) = state.files.insert(uri.clone(), entries.clone()) {
            for old_entry in old {
                remove_from_name_index(&mut state, &old_entry);
                remove_from_package_index(&mut state, &old_entry);
            }
        }
        for entry in entries {
            add_to_package_index(&mut state, &entry);
            state
                .by_name
                .entry(entry.name.clone())
                .or_default()
                .push(entry);
        }
    }

    /// Drops the entries for `uri`, and its declared-type model if it was a
    /// workspace source file — the whole file is forgotten in one call, so the
    /// index and the model can never disagree after a deletion.
    pub fn remove_file(&self, uri: &Url) {
        let Ok(mut state) = self.state.write() else {
            return;
        };
        if let Some(old) = state.files.remove(uri) {
            for old_entry in old {
                remove_from_name_index(&mut state, &old_entry);
                remove_from_package_index(&mut state, &old_entry);
            }
        }
        drop(state);
        if let Ok(mut sources) = self.source_types.write() {
            if sources.models.remove(uri).is_some() {
                sources.generation = sources.generation.wrapping_add(1);
            }
        }
    }

    /// Whether any indexed declaration (a type, member, or import) sits in
    /// `package`, so a wildcard import can be checked against the index.
    pub fn has_package(&self, package: &str) -> bool {
        self.state
            .read()
            .map(|state| state.packages.contains_key(package))
            .unwrap_or(false)
    }

    /// All entries declared with exactly `name`, as shared handles.
    pub fn query_name(&self, name: &str) -> Vec<Arc<SymbolEntry>> {
        self.state
            .read()
            .ok()
            .and_then(|state| state.by_name.get(name).cloned())
            .unwrap_or_default()
    }

    /// All entries whose name starts with `prefix` (case-sensitive), ordered
    /// by name and then position. A `BTreeMap` range scan over the names that
    /// begin with `prefix`, so the cost is the matched names, not every name.
    pub fn query_prefix(&self, prefix: &str) -> Vec<Arc<SymbolEntry>> {
        let Ok(state) = self.state.read() else {
            return Vec::new();
        };
        let mut out: Vec<Arc<SymbolEntry>> = Vec::new();
        for (name, entries) in state
            .by_name
            .range::<str, _>((Bound::Included(prefix), Bound::Unbounded))
        {
            if !name.starts_with(prefix) {
                break;
            }
            out.extend(entries.iter().cloned());
        }
        // Names arrive in order from the map; only the position within a name
        // needs sorting.
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
        let mut files: Vec<(&Url, &Vec<Arc<SymbolEntry>>)> = state.files.iter().collect();
        files.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        let mut out = Vec::new();
        for (_, entries) in files {
            let mut entries: Vec<&Arc<SymbolEntry>> = entries.iter().collect();
            entries.sort_by(|a, b| cmp_position(&a.full_range, &b.full_range));
            out.extend(entries.into_iter().map(|entry| (**entry).clone()));
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

    /// The workspace's non-source declared-type base (jars, the JDK, and
    /// extracted library sources) as a name-indexed layered view, or `None`
    /// before any artifact has landed. Workspace source files are *not* part of
    /// it — the engine layers [`WorkspaceIndex::source_models`] over it, so a
    /// file's types can be removed by a map delete. Every consumer pairs it with
    /// that overlay. The view is cached behind the base generation, so the
    /// append-only extension below never re-merges it per request.
    pub fn type_model(&self) -> Option<Arc<crate::types::SourceLayerIndex>> {
        let base = self.base.read().ok()?;
        if base.layers.is_empty() {
            return None;
        }
        let generation = base.generation;
        if let Ok(cache) = self.base_view.lock() {
            if let Some((cached, view)) = cache.as_ref() {
                if *cached == generation {
                    return Some(Arc::clone(view));
                }
            }
        }
        let layers: Vec<(Url, Arc<crate::types::TypeModel>)> = base
            .layers
            .iter()
            .map(|(uri, model)| (uri.clone(), Arc::clone(model)))
            .collect();
        drop(base);
        let view = Arc::new(crate::types::SourceLayerIndex::ordered(layers));
        if let Ok(mut cache) = self.base_view.lock() {
            *cache = Some((generation, Arc::clone(&view)));
        }
        Some(view)
    }

    /// Adds (or replaces) one non-source base artifact: its index `entries`
    /// under `uri` and its declared `types` as one layer. Append-only, so the
    /// base grows per artifact and no whole-model clone is ever needed. A URI
    /// already superseded by extracted sources is ignored.
    pub fn add_base_layer(
        &self,
        uri: &Url,
        entries: Vec<SymbolEntry>,
        types: Arc<crate::types::TypeModel>,
    ) {
        if self
            .base
            .read()
            .map(|base| base.superseded.contains(uri))
            .unwrap_or(false)
        {
            return;
        }
        self.upsert_file(uri, entries);
        let Ok(mut base) = self.base.write() else {
            return;
        };
        let len = base.layers.len();
        match base.index_of.get(uri).copied() {
            Some(i) => base.layers[i] = (uri.clone(), types),
            None => {
                base.index_of.insert(uri.clone(), len);
                base.layers.push((uri.clone(), types));
            }
        }
        base.generation = base.generation.wrapping_add(1);
    }

    /// Drops a non-source base artifact — its index entries and its layer — and
    /// remembers the URI as superseded, e.g. when a class-file jar is replaced by
    /// its extracted sources. Order-independent: a jar layer added *after* this
    /// removal is dropped, so the two never coexist.
    pub fn remove_base_layer(&self, uri: &Url) {
        if let Ok(mut base) = self.base.write() {
            base.superseded.insert(uri.clone());
        }
        self.remove_file(uri);
        let Ok(mut base) = self.base.write() else {
            return;
        };
        if let Some(i) = base.index_of.remove(uri) {
            base.layers.remove(i);
            let rebuilt: HashMap<Url, usize> = base
                .layers
                .iter()
                .enumerate()
                .map(|(j, (layer_uri, _))| (layer_uri.clone(), j))
                .collect();
            base.index_of = rebuilt;
            base.generation = base.generation.wrapping_add(1);
        }
    }

    /// Replaces the whole base with a single layer. A fixture/test hook; the
    /// scan itself extends the base per artifact via
    /// [`WorkspaceIndex::add_base_layer`].
    pub fn set_types(&self, model: Arc<crate::types::TypeModel>) {
        let Ok(mut base) = self.base.write() else {
            return;
        };
        base.layers.clear();
        base.index_of.clear();
        base.superseded.clear();
        let uri = Url::parse("java-lsp-base://model").expect("valid base URI");
        base.index_of.insert(uri.clone(), 0);
        base.layers.push((uri, model));
        base.generation = base.generation.wrapping_add(1);
    }

    /// Records the warm-up declared-type model for one workspace source file.
    /// Called by the scan alongside `upsert_file`; a later `remove_file` for the
    /// same URI forgets it.
    pub fn set_source_types(&self, uri: &Url, model: Arc<crate::types::TypeModel>) {
        if let Ok(mut sources) = self.source_types.write() {
            sources.models.insert(uri.clone(), model);
            sources.generation = sources.generation.wrapping_add(1);
        }
    }

    /// The warm-up declared-type models of every workspace source file, ordered
    /// by URI so the union is deterministic. The engine layers its dirty overlay
    /// over these (a dirty entry replaces its URI's model).
    pub fn source_models(&self) -> Vec<(Url, Arc<crate::types::TypeModel>)> {
        let Ok(sources) = self.source_types.read() else {
            return Vec::new();
        };
        let mut out: Vec<(Url, Arc<crate::types::TypeModel>)> = sources
            .models
            .iter()
            .map(|(uri, model)| (uri.clone(), Arc::clone(model)))
            .collect();
        out.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        out
    }

    /// The cached, name-indexed base layer view: every warm-up source model,
    /// sorted by URI and indexed by declared name. Rebuilt only when the
    /// source-model generation moves, so a request reuses the same `Arc`
    /// instead of rebuilding an O(workspace) view.
    pub fn source_layer_index(&self) -> Arc<crate::types::SourceLayerIndex> {
        let Ok(sources) = self.source_types.read() else {
            return Arc::new(crate::types::SourceLayerIndex::default());
        };
        let generation = sources.generation;
        if let Ok(cache) = self.source_layers.lock() {
            if let Some((cached, index)) = cache.as_ref() {
                if *cached == generation {
                    return Arc::clone(index);
                }
            }
        }
        // Collect under the read guard, so `generation` and the models are one
        // consistent snapshot (a concurrent write bumps the generation only
        // after updating the map, both under the write guard).
        let mut layers: Vec<(Url, Arc<crate::types::TypeModel>)> = sources
            .models
            .iter()
            .map(|(uri, model)| (uri.clone(), Arc::clone(model)))
            .collect();
        drop(sources);
        layers.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        let index = Arc::new(crate::types::SourceLayerIndex::new(layers));
        if let Ok(mut cache) = self.source_layers.lock() {
            *cache = Some((generation, Arc::clone(&index)));
        }
        index
    }

    /// The workspace's own `.java` source files, sorted — the candidate set for
    /// a references or rename search, requiring no second directory walk. Jar and
    /// JDK archive URIs are excluded, and so are extracted library sources (whose
    /// entries are all `library_source`): a references search must never read the
    /// cache and a rename must never rewrite it.
    pub fn source_files(&self) -> Vec<Url> {
        let Ok(state) = self.state.read() else {
            return Vec::new();
        };
        let mut files: Vec<Url> = state
            .files
            .iter()
            .filter(|(uri, entries)| {
                uri.path().ends_with(".java") && entries.iter().all(|entry| !entry.library_source)
            })
            .map(|(uri, _)| uri.clone())
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

    /// The workspace source layers for a query: every source file's declared
    /// types (the warm-up models), with the dirty overlay layered over them and a
    /// later layer winning. Both halves are behind `Arc`s, so assembling the view
    /// is two `Arc` clones, never a merge.
    pub fn type_layers(&self) -> ModelLayers {
        let dirty = self
            .dirty
            .lock()
            .map(|dirty| Arc::clone(&dirty))
            .unwrap_or_default();
        ModelLayers::layered(self.source_layer_index(), dirty)
    }

    /// Records the current declared-type model of one file in the dirty overlay —
    /// an open buffer, or a file a watcher event or a close re-read from disk —
    /// replacing its warm-up contribution.
    pub fn record_dirty_type(&self, uri: &Url, model: Arc<TypeModel>) {
        if let Ok(mut dirty) = self.dirty.lock() {
            Arc::make_mut(&mut dirty).insert(uri.clone(), model);
        }
    }

    /// Drops a file's dirty overlay entry, so its warm-up model (if any) applies
    /// again.
    pub fn drop_dirty_type(&self, uri: &Url) {
        if let Ok(mut dirty) = self.dirty.lock() {
            Arc::make_mut(&mut dirty).remove(uri);
        }
    }
}

fn remove_from_name_index(state: &mut IndexState, entry: &SymbolEntry) {
    if let Some(list) = state.by_name.get_mut(&entry.name) {
        list.retain(|candidate| candidate.as_ref() != entry);
        if list.is_empty() {
            state.by_name.remove(&entry.name);
        }
    }
}

fn add_to_package_index(state: &mut IndexState, entry: &SymbolEntry) {
    if let Some(package) = &entry.package {
        *state.packages.entry(package.to_string()).or_default() += 1;
    }
}

fn remove_from_package_index(state: &mut IndexState, entry: &SymbolEntry) {
    if let Some(package) = &entry.package {
        if let Some(count) = state.packages.get_mut(package.as_ref()) {
            *count -= 1;
            if *count == 0 {
                state.packages.remove(package.as_ref());
            }
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
    let package: Option<Arc<str>> = file_package(&tree.root_node(), text).map(Arc::from);
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

/// Drops entries no feature reads. Import declarations are indexed for a
/// workspace file but never consumed — `definition` filters `Import` out,
/// `completion_kind`/`symbol_kind` answer `None` for it, and the import helpers
/// read the file's tree, not the index — so a library pass drops them: over a
/// large source tree they are millions of entries.
pub(crate) fn drop_import_entries(entries: &mut Vec<SymbolEntry>) {
    entries.retain(|entry| entry.kind != IndexKind::Import);
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
                "class_declaration" => IndexKind::Class,
                "interface_declaration" => IndexKind::Interface,
                "enum_declaration" => IndexKind::Enum,
                _ => IndexKind::Record,
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
                    IndexKind::Method,
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
                            IndexKind::Field,
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
                    IndexKind::EnumConstant,
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
                name: import_name(node, text),
                kind: IndexKind::Import,
                package: None,
                container: Arc::clone(container),
                full_range: lines.range(text, node),
                selection_range: lines.range(text, &target),
                dependency: false,
                library_source: false,
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
                IndexKind::Method,
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
fn collect_lombok_entries<'a>(
    uri: &Arc<Url>,
    node: &Node<'a>,
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
            name: generated.name,
            kind: generated.kind,
            package: None,
            container,
            full_range: lines.range(text, &generated.anchor),
            selection_range: lines.range(text, &generated.anchor_name),
            dependency: false,
            library_source: false,
            synthetic: true,
        });
    }
}

fn entry(
    uri: &Arc<Url>,
    node: &Node,
    name: &Node,
    kind: IndexKind,
    container: &Arc<[String]>,
    text: &str,
    lines: &LineIndex,
) -> SymbolEntry {
    SymbolEntry {
        uri: Arc::clone(uri),
        name: text[name.byte_range()].to_string(),
        kind,
        package: None,
        container: Arc::clone(container),
        full_range: lines.range(text, node),
        selection_range: lines.range(text, name),
        dependency: false,
        library_source: false,
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

/// Walks the workspace: discovers the Maven model and collects the `.java` files
/// under its source roots. The project driver runs this off the request path.
pub(crate) fn walk_project(root: &Path) -> (crate::project::ProjectModel, Vec<PathBuf>) {
    let mut resolver = Resolver::new(local_repository());
    let model = crate::project::discover(root, &mut resolver);
    let mut files = Vec::new();
    for source_root in model.source_roots() {
        collect_java_files(&source_root, &mut files);
    }
    files.sort();
    (model, files)
}

/// Resolves each module's dependency closure against the local repository,
/// keeping only coordinates whose jar is on disk, deduplicated. The dependency
/// driver runs this off the request path.
pub(crate) fn resolve_artifacts(model: &crate::project::ProjectModel) -> Vec<Artifact> {
    let mut resolver = Resolver::new(local_repository());
    let mut artifacts: Vec<Artifact> = Vec::new();
    let mut seen: std::collections::HashSet<Artifact> = Default::default();
    for module in &model.modules {
        let Some(effective) = &module.effective else {
            continue;
        };
        for (group, artifact_id, version) in resolve_closure(effective, &mut resolver) {
            // Resolution is pom-level; a jar present in the local repository is
            // indexed even when its pom went missing mid-flight.
            let jar_path = resolver.jar_path(&group, &artifact_id, &version);
            if !jar_path.is_file() {
                continue;
            }
            if seen.insert((group.clone(), artifact_id.clone(), version.clone())) {
                artifacts.push((group, artifact_id, version));
            }
        }
    }
    artifacts
}

/// The notice to show when dependency sources will not be fetched: only when
/// `JAVA_LSP_OFFLINE` is set *and* the workspace actually resolved a dependency.
pub(crate) fn offline_notice(artifact_count: usize) -> Option<String> {
    (crate::sources::offline() && artifact_count > 0).then(|| {
        "Dependency sources are disabled (JAVA_LSP_OFFLINE is set): \
         go-to-definition into library code is unavailable."
            .to_string()
    })
}

/// Indexes every resolved dependency jar that is on disk, publishing each as a
/// base artifact (its entries and its declared types as one layer). Returns the
/// number of jars indexed.
///
/// Each archive is served from [`crate::base_cache`] when its identity is
/// unchanged, so a restart re-parses only what changed.
pub(crate) fn index_jars(artifacts: &[Artifact], sink: &mut dyn FnMut(DriverMessage)) -> usize {
    let resolver = Resolver::new(local_repository());
    let store = crate::base_cache::ArchiveStore::new("jars");
    let mut jars = 0usize;
    for (group, artifact_id, version) in artifacts {
        let jar_path = resolver.jar_path(group, artifact_id, version);
        let Some(identity) = crate::base_cache::identity(&jar_path) else {
            continue;
        };
        let output = match store.get::<crate::base_cache::ArchiveOutput>(&identity) {
            Some(output) => output,
            None => {
                let Some((entries, types)) = crate::classfile::jar_outputs(&jar_path) else {
                    continue;
                };
                let output = crate::base_cache::ArchiveOutput { entries, types };
                store.insert(&identity, &output);
                output
            }
        };
        let Ok(jar_uri) = Url::from_file_path(&jar_path) else {
            continue;
        };
        let mut types = crate::types::TypeModel::new();
        types.extend(output.types);
        sink(DriverMessage::BaseArtifact {
            uri: jar_uri,
            entries: Arc::new(output.entries),
            types: Arc::new(types),
        });
        jars += 1;
        sink(DriverMessage::Progress(ProgressUpdate::Update {
            message: format!("Indexed {jars} dependency jars"),
            percentage: None,
        }));
    }
    jars
}

/// Indexes the standard library through the same path as dependency jars
/// (offered in completions, filtered from navigation). A missing JDK is a
/// no-op. Returns the number of classes indexed.
///
/// The whole JDK is served from [`crate::base_cache`] when its home and every
/// archive's identity are unchanged; a JDK upgrade (any archive changed) or a
/// missing archive re-parses it in full.
pub(crate) fn index_jdk(sink: &mut dyn FnMut(DriverMessage)) -> usize {
    let Some(home) = crate::jdk::locate_jdk() else {
        sink(DriverMessage::Log {
            level: LogLevel::Info,
            message: "no usable JDK found; standard library not indexed".to_string(),
        });
        return 0;
    };
    let store = crate::base_cache::ArchiveStore::new("jdk");
    let key = jdk_cache_key(&home);
    let archives = match key
        .as_ref()
        .and_then(|key| store.get::<Vec<crate::base_cache::JdkArchive>>(key))
    {
        Some(archives) => archives,
        None => {
            let archives: Vec<crate::base_cache::JdkArchive> = crate::jdk::jdk_entries(&home)
                .into_iter()
                .map(|(uri, entries, types)| crate::base_cache::JdkArchive {
                    uri,
                    entries,
                    types,
                })
                .collect();
            if let Some(key) = &key {
                store.insert(key, &archives);
            }
            archives
        }
    };
    let mut jdk_classes = 0usize;
    for archive in archives {
        jdk_classes += archive.entries.len();
        let mut types = crate::types::TypeModel::new();
        types.extend(archive.types);
        sink(DriverMessage::BaseArtifact {
            uri: archive.uri,
            entries: Arc::new(archive.entries),
            types: Arc::new(types),
        });
    }
    sink(DriverMessage::Progress(ProgressUpdate::Update {
        message: format!("Indexed {jdk_classes} JDK classes"),
        percentage: None,
    }));
    jdk_classes
}

/// The JDK's cache identity: its home plus every archive's identity, so a JDK
/// upgrade (any archive changed) invalidates the cached parse. `None` when an
/// archive cannot be identified, which forces a full parse.
fn jdk_cache_key(home: &Path) -> Option<String> {
    let archives = crate::jdk::jdk_archive_paths(home);
    if archives.is_empty() {
        return None;
    }
    let mut parts = Vec::with_capacity(archives.len() + 1);
    parts.push(format!("jdk:{}", home.display()));
    for archive in &archives {
        parts.push(crate::base_cache::identity(archive)?);
    }
    Some(parts.join("|"))
}

/// Scans the workspace `.java` files, publishing each file's index entries and
/// its declared-type model. Returns the number of files indexed.
pub(crate) fn scan_sources(files: &[PathBuf], sink: &mut dyn FnMut(DriverMessage)) -> usize {
    let mut parser = java_parser();
    let mut indexed = 0usize;
    for path in files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let Some(tree) = parser.parse(text.as_bytes(), None) else {
            continue;
        };
        let Ok(uri) = Url::from_file_path(path) else {
            continue;
        };
        let package = crate::types::file_package(&tree, &text);
        let mut model = crate::types::TypeModel::new();
        model.extend(crate::types::collect_type_infos(
            package.as_deref(),
            &tree,
            &text,
        ));
        let entries = extract_entries(&uri, &tree, &text);
        sink(DriverMessage::SourceFile {
            uri,
            entries: Arc::new(entries),
            types: Arc::new(model),
        });
        indexed += 1;
    }
    sink(DriverMessage::Progress(ProgressUpdate::Update {
        message: format!("Indexed {indexed} source files"),
        percentage: None,
    }));
    indexed
}

/// The synchronous warm-up for unit tests (no runtime, no engine hub): runs the
/// same producers the drivers run, applying their messages to the index
/// directly. The shell's hub drives the real drivers instead.
#[cfg(test)]
pub(crate) fn warm_up_sync(root: &Url, index: &IndexHandle) {
    index.apply(DriverMessage::Progress(ProgressUpdate::Begin {
        title: "java-lsp".to_string(),
        message: "Indexing workspace".to_string(),
    }));
    let Ok(root_path) = root.to_file_path() else {
        index.apply(DriverMessage::Ready);
        return;
    };
    let (model, files) = walk_project(&root_path);
    let artifacts = resolve_artifacts(&model);
    index.apply(DriverMessage::ProjectModel {
        model: Arc::new(model),
    });
    {
        let mut sink = |message| index.apply(message);
        scan_sources(&files, &mut sink);
    }
    {
        let mut sink = |message| index.apply(message);
        index_jars(&artifacts, &mut sink);
    }
    {
        let mut sink = |message| index.apply(message);
        index_jdk(&mut sink);
    }
    index.apply(DriverMessage::Ready);
}

/// The local Maven repository: `$MAVEN_REPO` if set, else `~/.m2/repository`.
pub(crate) fn local_repository() -> PathBuf {
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

/// The index subsystem handle: a [`crate::bus::BusClient`], so the core and the
/// other subsystems reach the index by message through the hub and never hold its
/// state.
pub type IndexHandle = crate::bus::BusClient;

/// Exact-name lookups against the index, by whatever path the caller reads it:
/// the handle itself (one bus round trip per call), or a cache in front of it
/// such as the diagnostics sweep's.
pub(crate) trait NameLookup {
    fn query_name(&self, name: &str) -> Vec<Arc<SymbolEntry>>;
}

impl NameLookup for IndexHandle {
    /// Blocks for the bus reply: `NameLookup` serves the synchronous analysis
    /// code, which runs off the runtime's workers.
    fn query_name(&self, name: &str) -> Vec<Arc<SymbolEntry>> {
        IndexHandle::query_name(self, name).blocking_recv()
    }
}

/// Starts the index module: a thread that owns the [`WorkspaceIndex`] and serves
/// the bus — it applies the notifications that affect the index and answers the
/// index requests. It exits when the hub drops its sink.
pub fn spawn_module(client: &crate::bus::BusClient) {
    let mut rx = client.serve(crate::bus::Module::Index);
    thread::Builder::new()
        .name("java-lsp-index".to_string())
        .spawn(move || {
            let index = WorkspaceIndex::new();
            while let Some(message) = rx.blocking_recv() {
                match message {
                    Bus::Notify(message) => apply_to_index(&message, &index),
                    Bus::Request(request) => answer(&index, request),
                }
            }
        })
        .expect("spawn the index module thread");
}

/// Answers an index request on the module thread.
fn answer(index: &WorkspaceIndex, request: Request) {
    match request {
        Request::IndexQueryName { name, reply } => {
            reply.send(index.query_name(&name));
        }
        Request::IndexQueryNames { names, reply } => {
            let found: HashMap<String, Vec<Arc<SymbolEntry>>> = names
                .into_iter()
                .map(|name| {
                    let entries = index.query_name(&name);
                    (name, entries)
                })
                .collect();
            reply.send(found);
        }
        Request::IndexQueryPrefix { prefix, reply } => {
            reply.send(index.query_prefix(&prefix));
        }
        Request::IndexAllSymbols { reply } => {
            reply.send(index.all_symbols());
        }
        Request::IndexFileCount { reply } => {
            reply.send(index.file_count());
        }
        Request::IndexReady { reply } => {
            reply.send(index.ready());
        }
        Request::IndexHasPackage { package, reply } => {
            reply.send(index.has_package(&package));
        }
        Request::IndexHasPackages { packages, reply } => {
            let known: std::collections::HashSet<String> = packages
                .into_iter()
                .filter(|package| index.has_package(package))
                .collect();
            reply.send(known);
        }
        Request::IndexTypeModel { reply } => {
            reply.send(index.type_model());
        }
        Request::IndexTypeLayers { reply } => {
            reply.send(index.type_layers());
        }
        Request::IndexSourceFiles { reply } => {
            reply.send(index.source_files());
        }
        Request::IndexSourceRoots { reply } => {
            reply.send(index.source_roots());
        }
        Request::IndexSourceLayerIndex { reply } => {
            reply.send(index.source_layer_index());
        }
        Request::IndexSourceModels { reply } => {
            reply.send(index.source_models());
        }
        _ => {}
    }
}

/// Applies the index-affecting messages to the index. The index subsystem is the
/// only place the warm-up mutates the index.
fn apply_to_index(message: &DriverMessage, index: &WorkspaceIndex) {
    match message {
        DriverMessage::ProjectModel { model } => index.set_model((**model).clone()),
        DriverMessage::SourceFile {
            uri,
            entries,
            types,
        } => {
            index.set_source_types(uri, Arc::clone(types));
            index.upsert_file(uri, (**entries).clone());
        }
        DriverMessage::BaseArtifact {
            uri,
            entries,
            types,
        } => index.add_base_layer(uri, (**entries).clone(), Arc::clone(types)),
        DriverMessage::RemoveBase { uri } => index.remove_base_layer(uri),
        DriverMessage::Ready => {
            index.set_ready();
            tracing::info!(target: "java_lsp::bus", "{}", index.composition());
        }
        DriverMessage::StageDone {
            stage: crate::messages::Stage::Downloads,
            ..
        } => tracing::info!(target: "java_lsp::bus", "{}", index.composition()),
        DriverMessage::SourceEntries { uri, entries } => {
            index.upsert_file(uri, (**entries).clone())
        }
        DriverMessage::SourceRemoved { uri } => index.remove_file(uri),
        DriverMessage::DirtyTypes { uri, model } => index.record_dirty_type(uri, Arc::clone(model)),
        DriverMessage::DirtyTypesDropped { uri } => index.drop_dirty_type(uri),
        DriverMessage::SourceTypes { uri, model } => index.set_source_types(uri, Arc::clone(model)),
        DriverMessage::BaseTypes { model } => index.set_types(Arc::clone(model)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::lsp_types::Position;

    /// The base grows per artifact — append-only, no whole-model remerge — and a
    /// new artifact's types resolve immediately, while `ready` is still false
    /// (so library types are served mid-scan).
    #[test]
    fn the_base_grows_per_artifact_and_resolves_before_ready() {
        let index = WorkspaceIndex::new();
        let jar = Url::parse("file:///lib.jar").unwrap();
        let mut model = crate::types::TypeModel::new();
        model.insert(crate::types::TypeInfo::new(
            "Thing".into(),
            Some("demo".into()),
            IndexKind::Class,
        ));

        assert!(index.type_model().is_none());
        assert!(!index.ready());

        index.add_base_layer(&jar, Vec::new(), Arc::new(model));
        let base = index.type_model().expect("base");
        assert!(crate::types::TypeLookup::contains(base.as_ref(), "Thing"));
        assert!(!index.ready());

        // Removing the artifact drops its layer again.
        index.remove_base_layer(&jar);
        assert!(index.type_model().is_none());
    }

    /// A class-file artifact superseded by extracted sources stays dropped even
    /// when the jar producer's add arrives *after* the downloader's removal (the
    /// two run concurrently), so the two never coexist.
    #[test]
    fn a_superseded_base_artifact_is_not_re_added() {
        let index = WorkspaceIndex::new();
        let jar = Url::parse("file:///lib.jar").unwrap();
        let mut model = crate::types::TypeModel::new();
        model.insert(crate::types::TypeInfo::new(
            "Thing".into(),
            Some("demo".into()),
            IndexKind::Class,
        ));

        index.remove_base_layer(&jar);
        index.add_base_layer(&jar, Vec::new(), Arc::new(model));

        assert!(index.type_model().is_none());
    }

    /// The assembled base layer view is cached: the same `Arc` is handed back
    /// until the source models change, and a change (an insert or a removal)
    /// rebuilds it exactly once.
    #[test]
    fn source_layer_index_is_cached_until_the_sources_change() {
        let index = WorkspaceIndex::new();
        let first = index.source_layer_index();
        assert!(Arc::ptr_eq(&first, &index.source_layer_index()));

        let uri = Url::parse("file:///work/A.java").unwrap();
        index.set_source_types(&uri, Arc::new(crate::types::TypeModel::new()));
        let after_insert = index.source_layer_index();
        assert!(!Arc::ptr_eq(&first, &after_insert));
        assert!(Arc::ptr_eq(&after_insert, &index.source_layer_index()));

        index.remove_file(&uri);
        let after_remove = index.source_layer_index();
        assert!(!Arc::ptr_eq(&after_insert, &after_remove));
        assert!(Arc::ptr_eq(&after_remove, &index.source_layer_index()));
    }

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
            package: Some("demo".into()),
            container: Arc::from(Vec::<String>::new()),
            full_range: Range::new(position, Position::new(line, 1)),
            selection_range: Range::new(position, Position::new(line, 1)),
            dependency: false,
            library_source: false,
            synthetic: false,
            uri: Arc::new(uri("file:///src/A.java")),
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
    fn source_files_excludes_extracted_library_sources() {
        let index = WorkspaceIndex::new();
        let workspace = uri("file:///src/Widget.java");
        index.upsert_file(&workspace, vec![sample_entry("Widget", 0)]);

        // An extracted dependency source is a `.java` file too, but a
        // references search or a rename must never touch it.
        let cache = uri("file:///cache/demo/lib/1.0/demo/Thing.java");
        let mut entry = sample_entry("Thing", 0);
        entry.uri = Arc::new(cache.clone());
        entry.dependency = true;
        entry.library_source = true;
        index.upsert_file(&cache, vec![entry]);

        assert_eq!(index.source_files(), vec![workspace]);
    }

    #[test]
    fn offline_notice_only_when_offline_with_dependencies() {
        let _env = crate::jdk::env_lock();
        std::env::remove_var("JAVA_LSP_OFFLINE");
        assert!(offline_notice(3).is_none());

        std::env::set_var("JAVA_LSP_OFFLINE", "1");
        assert!(
            offline_notice(0).is_none(),
            "nothing to fetch, nothing to say"
        );
        let notice = offline_notice(2).expect("a workspace with dependencies is worth a notice");
        assert!(notice.contains("JAVA_LSP_OFFLINE"), "{notice}");
        std::env::remove_var("JAVA_LSP_OFFLINE");
    }

    #[test]
    fn extraction_covers_every_kind_and_container_chains() {
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
        assert_eq!(field.container.to_vec(), vec!["Outer".to_string()]);

        let constructors: Vec<&SymbolEntry> = entries
            .iter()
            .filter(|entry| entry.name == "Outer" && entry.kind == IndexKind::Method)
            .collect();
        assert_eq!(constructors.len(), 1);
        assert_eq!(
            constructors[0].container.to_vec(),
            vec!["Outer".to_string()]
        );

        let inner = entry_named(&entries, "Inner");
        assert_eq!(inner.kind, IndexKind::Interface);
        assert_eq!(inner.container.to_vec(), vec!["Outer".to_string()]);

        let run = entry_named(&entries, "run");
        assert_eq!(run.kind, IndexKind::Method);
        assert_eq!(
            run.container.to_vec(),
            vec!["Outer".to_string(), "Inner".to_string()]
        );

        let get = entry_named(&entries, "get");
        assert_eq!(get.kind, IndexKind::Method);
        assert_eq!(get.container.to_vec(), vec!["Outer".to_string()]);

        let color = entry_named(&entries, "Color");
        assert_eq!(color.kind, IndexKind::Enum);

        let point = entry_named(&entries, "Point");
        assert_eq!(point.kind, IndexKind::Record);

        let import = entry_named(&entries, "java.util.List");
        assert_eq!(import.kind, IndexKind::Import);

        // Enum constants are indexed as members of the enum.
        let red = entry_named(&entries, "RED");
        assert_eq!(red.kind, IndexKind::EnumConstant);
        assert_eq!(red.container.to_vec(), vec!["Color".to_string()]);
    }

    #[test]
    fn enum_constants_are_indexed_at_their_declared_names() {
        let text = "enum Color {\n    RED,\n    GREEN(1) { },\n    BLUE\n}\n";
        let entries = extract_entries(&uri("file:///Color.java"), &parse(text), text);

        assert_eq!(entry_named(&entries, "Color").kind, IndexKind::Enum);
        for name in ["RED", "GREEN", "BLUE"] {
            let constant = entry_named(&entries, name);
            assert_eq!(constant.kind, IndexKind::EnumConstant, "{name}");
            assert_eq!(constant.container.to_vec(), vec!["Color".to_string()]);
            // The selection range is the constant's own name.
            let at = text.find(name).unwrap();
            let line = text[..at].matches('\n').count() as u32;
            let column = (at - text[..at].rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32;
            assert_eq!(constant.selection_range.start.line, line, "{name}");
            assert_eq!(constant.selection_range.start.character, column, "{name}");
            assert_eq!(
                constant.selection_range.end.character,
                column + name.len() as u32,
                "{name}"
            );
        }
    }

    #[test]
    fn a_nested_enums_constants_carry_the_full_container_chain() {
        let text = "class Outer {\n    enum Color { RED }\n}\n";
        let entries = extract_entries(&uri("file:///Outer.java"), &parse(text), text);
        let red = entry_named(&entries, "RED");
        assert_eq!(red.kind, IndexKind::EnumConstant);
        assert_eq!(
            red.container.to_vec(),
            vec!["Outer".to_string(), "Color".to_string()]
        );
    }

    #[test]
    fn record_components_are_indexed_at_their_header_positions() {
        let text = "record Point(int x, int y) {}\n";
        let entries = extract_entries(&uri("file:///Point.java"), &parse(text), text);

        assert_eq!(entry_named(&entries, "Point").kind, IndexKind::Record);

        let x = entry_named(&entries, "x");
        assert_eq!(x.kind, IndexKind::Method);
        assert_eq!(x.container.to_vec(), vec!["Point".to_string()]);
        // Indexed where it is declared, in the header, not in the (empty) body.
        let declared = text.find("int x").unwrap() as u32;
        assert_eq!(x.full_range.start.line, 0);
        assert_eq!(x.full_range.start.character, declared);
        assert_eq!(x.selection_range.start.character, declared + 4);
        assert_eq!(x.selection_range.end.character, declared + 5);

        let y = entry_named(&entries, "y");
        assert_eq!(y.kind, IndexKind::Method);
        assert_eq!(y.container.to_vec(), vec!["Point".to_string()]);
    }

    #[test]
    fn lombok_members_are_synthetic_entries_anchored_at_the_field() {
        let text = "@Getter class Bean {\n    private int count;\n}\n";
        let entries = extract_entries(&uri("file:///Bean.java"), &parse(text), text);
        let getter = entries
            .iter()
            .find(|entry| entry.name == "getCount" && entry.kind == IndexKind::Method)
            .expect("getCount");
        assert!(getter.synthetic);
        assert_eq!(getter.container.to_vec(), vec!["Bean".to_string()]);
        // Anchored at the field `count` (line 1), not a declaration of its own.
        assert_eq!(getter.selection_range.start.line, 1);
    }

    #[test]
    fn a_class_without_lombok_annotations_indexes_no_synthetic_entries() {
        let text = "class Bean {\n    private int count;\n}\n";
        let entries = extract_entries(&uri("file:///Bean.java"), &parse(text), text);
        assert!(entries.iter().all(|entry| !entry.synthetic));
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
    fn warm_up_indexes_a_directory_tree_and_sets_ready() {
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
        let index = IndexHandle::standalone();
        warm_up_sync(&root_url, &index);

        assert!(index.ready().blocking_recv());
        let symbols = index.all_symbols().blocking_recv();
        let classes: Vec<&str> = symbols
            .iter()
            .filter(|entry| entry.kind == IndexKind::Class)
            .map(|entry| entry.name.as_str())
            .collect();
        // all_symbols orders by URI: the root's Top.java sorts before
        // nested/Deep.java ('T' < 'n').
        assert_eq!(classes, vec!["Top", "Deep"]);
        assert!(index.query_name("Skip").blocking_recv().is_empty());

        // The same warm-up builds the declared-type base: it holds no workspace
        // source types — each source file gets its own per-URI model instead.
        if let Some(base) = index.type_model().blocking_recv() {
            assert!(!crate::types::TypeLookup::contains(base.as_ref(), "Top"));
            assert!(!crate::types::TypeLookup::contains(base.as_ref(), "Deep"));
        }
        let sources = index.source_models().blocking_recv();
        let top_uri = Url::from_file_path(root.join("Top.java")).unwrap();
        let deep_uri = Url::from_file_path(root.join("nested").join("Deep.java")).unwrap();
        let by_uri = |uri: &Url| {
            sources
                .iter()
                .find(|(candidate, _)| candidate == uri)
                .map(|(_, model)| model.clone())
                .unwrap_or_else(|| panic!("no source model for {uri}"))
        };
        assert!(!by_uri(&top_uri).find("Top").is_empty());
        assert!(!by_uri(&deep_uri).find("Deep").is_empty());

        std::env::remove_var("JAVA_LSP_JDK");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A second warm-up over an unchanged JDK re-parses no class files: the whole
    /// JDK is served from the base cache. Proven by emptying the cached archives
    /// under the live key — a cache hit then emits no classes at all.
    #[test]
    fn a_second_jdk_warmup_is_served_from_the_cache() {
        let _env = crate::jdk::env_lock();
        let Some(home) = crate::jdk::locate_jdk() else {
            return; // no JDK here; nothing to cache
        };
        let cache_root = std::env::temp_dir().join(format!(
            "java-lsp-jdk-cache-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::env::set_var("JAVA_LSP_SOURCES_CACHE", &cache_root);

        // First warm-up: parses the JDK and writes the cache.
        let mut sink = |_message| {};
        let classes = index_jdk(&mut sink);
        if classes == 0 {
            // No readable standard-library archive here; nothing to prove.
            std::env::remove_var("JAVA_LSP_SOURCES_CACHE");
            let _ = std::fs::remove_dir_all(&cache_root);
            return;
        }

        // Empty the cached archives under the live key; a hit then emits none,
        // so a zero-class second run proves the cache was used, not re-parsed.
        let store = crate::base_cache::ArchiveStore::new("jdk");
        let key = jdk_cache_key(&home).expect("a JDK key");
        assert!(
            store
                .get::<Vec<crate::base_cache::JdkArchive>>(&key)
                .is_some(),
            "the JDK cache should be written"
        );
        store.insert(&key, &Vec::<crate::base_cache::JdkArchive>::new());

        let mut second = |_message| {};
        assert_eq!(
            index_jdk(&mut second),
            0,
            "the second warm-up must be served from the cache"
        );

        std::env::remove_var("JAVA_LSP_SOURCES_CACHE");
        let _ = std::fs::remove_dir_all(&cache_root);
    }

    #[test]
    fn composition_summarises_the_index() {
        let zero = tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(0, 0),
            tower_lsp::lsp_types::Position::new(0, 0),
        );
        let entry = |name: &str, kind: IndexKind| SymbolEntry {
            uri: Arc::new(Url::parse("file:///W.java").unwrap()),
            name: name.to_string(),
            kind,
            package: Some("demo".into()),
            container: Arc::from(Vec::<String>::new()),
            full_range: zero,
            selection_range: zero,
            dependency: false,
            library_source: false,
            synthetic: false,
        };
        let index = WorkspaceIndex::new();
        index.upsert_file(
            &Url::parse("file:///W.java").unwrap(),
            vec![
                entry("W", IndexKind::Class),
                entry("run", IndexKind::Method),
            ],
        );
        let text = index.composition();
        assert!(text.contains("entries=2"), "{text}");
        assert!(text.contains("methods=1"), "{text}");
        assert!(text.contains("names=2"), "{text}");
        assert!(text.contains("~0KB"), "{text}");
    }
}
