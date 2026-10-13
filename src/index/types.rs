//! Index-owned storage: the symbol table, the declared-type store and its
//! layered read view. Shared Java declarations live in [`crate::model`] and
//! are re-exported here for callers using the older index paths.
//!
//! Operations on the store live in [`super::analyze`] and [`super::query`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::{Range, Url};

pub use crate::model::{IndexKind, Member, Param, Prim, SymbolKind, Ty, TypeInfo};

/// Where a file's symbols came from. Replaces a `dependency` + `library_source`
/// pair of flags: one byte, and the predicates the consumer reads name the
/// policy.
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

/// A flat declaration/import table: name → entries, with the per-file view that
/// keeps incremental updates and the references candidate set cheap.
#[derive(Default)]
pub struct SymbolIndex {
    /// The per-file view, for incremental replace/remove and the candidate set.
    pub(crate) by_file: HashMap<Url, FileRecord>,
    /// Name-ordered, so an exact lookup and a prefix range-scan share one
    /// structure. Keyed by `Arc<str>`, shared with `SymbolEntry::name`.
    pub(crate) by_name: BTreeMap<Arc<str>, Vec<Arc<SymbolEntry>>>,
    /// How many entries belong to each package, so a wildcard import can be
    /// checked in O(1) and a package forgotten exactly when its last entry goes.
    pub(crate) packages: HashMap<Arc<str>, usize>,
}

/// One file's entries and file-level origin. Each entry is shared with
/// `by_name`, so an entry is stored once.
pub(crate) struct FileRecord {
    /// The file's entries, in position order.
    pub(crate) entries: Vec<Arc<SymbolEntry>>,
    /// File-level origin, kept even when `entries` is empty, so a
    /// declaration-free workspace file still counts as a references candidate.
    pub(crate) origin: FileOrigin,
}

/// One ordered set of artifact layers with a generation-keyed, name-indexed view.
///
/// A single type serves both halves of [`TypeStore`]; they differ only in what a
/// key means and in whether a layer is ever superseded.
#[derive(Default)]
pub(crate) struct LayerSet {
    /// Layers in increasing precedence (a later layer wins), parallel to `keys`.
    pub(crate) layers: Vec<Arc<TypeModel>>,
    pub(crate) keys: Vec<Url>,
    /// `keys[i] -> i`.
    pub(crate) positions: HashMap<Url, usize>,
    /// Bumped on every mutation; keys `view`.
    pub(crate) generation: u64,
    /// The cached name-indexed view, rebuilt only when `generation` moves.
    pub(crate) view: Option<(u64, Arc<SourceLayerIndex>)>,
    /// Keys superseded by a richer layer: a later put for one is refused, so a
    /// class-file jar and its extracted sources never coexist regardless of the
    /// producers' arrival order. Only the base uses this.
    pub(crate) superseded: HashSet<Url>,
}

/// The declared-type store: a non-source base, the per-source-file models, and a
/// small overlay for the files whose current model differs from warm-up.
#[derive(Default)]
pub(crate) struct TypeStore {
    /// Jars, the JDK, and extracted library sources, appended per artifact.
    pub(crate) base: LayerSet,
    /// The warm-up model of every workspace source file.
    pub(crate) source: LayerSet,
    /// Files whose current model differs from warm-up: open buffers, watcher
    /// re-reads, and close re-reads. This *overrides* a file's source layer
    /// rather than ranking as one more layer, and it lives outside `source` so a
    /// keystroke never invalidates the O(workspace) source view.
    pub(crate) dirty: Arc<HashMap<Url, Arc<TypeModel>>>,
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
pub(crate) struct WorkspaceLayout {
    pub(crate) source_roots: Vec<PathBuf>,
}

/// The workspace index: the symbol table, the declared-type store, the source
/// roots, the declared-type context a query reads, and the readiness flag.
/// Owned by the subsystem thread.
#[derive(Default)]
pub struct WorkspaceIndex {
    pub(crate) symbols: SymbolIndex,
    pub(crate) types: TypeStore,
    pub(crate) layout: WorkspaceLayout,
    /// The effective declared-type view, built from `types` when the index is
    /// made ready.
    pub(crate) context: TypeContext,
    pub(crate) ready: bool,
}

/// The workspace's declared types, indexed by simple name.
#[derive(Debug, Clone, Default)]
pub struct TypeModel {
    pub(crate) by_name: HashMap<String, Vec<TypeInfo>>,
}

/// A sorted, name-indexed set of base type layers: the workspace source models
/// without the engine's dirty overlay, in the order [`ModelLayers`] would
/// assemble them (layers in increasing precedence, ordered by URI). Built once
/// behind the index's source-model generation and shared behind an `Arc`, so a
/// request never rebuilds it, and the per-name index keeps a lookup from
/// scanning every layer.
#[derive(Debug, Default)]
pub struct SourceLayerIndex {
    /// Base layers in increasing precedence (a later layer wins), ordered by URI.
    pub(crate) layers: Vec<Arc<TypeModel>>,
    /// The URI of each layer, parallel to `layers`.
    pub(crate) uris: Vec<Url>,
    /// The position of each layer's URI in `layers`, to map a dirty overlay entry
    /// back to the layer it replaces.
    pub(crate) index_of: HashMap<Url, usize>,
    /// The layer positions that declare each name, highest precedence first.
    pub(crate) by_name: HashMap<String, Vec<usize>>,
}

/// A read-only view over several [`TypeModel`] layers, consulted in precedence
/// order: a later layer wins a slot (`name`, `package`, `kind`, enclosing
/// chain), so a file's dirty model shadows its warm-up model and a file created
/// after warm-up is visible. **No data is copied** — the layers already live
/// behind their `Arc`s, so the view holds the `Arc`s and returns references
/// straight into them.
///
/// The view is the index's cached, name-indexed base ([`SourceLayerIndex`],
/// shared behind an `Arc` and rebuilt only when the source models change) plus
/// the engine's small **dirty** overlay (also an `Arc`). Assembling a view is
/// two `Arc` clones, and a lookup consults only the layers that declare the
/// name — via the base's `by_name` and a scan of the small dirty overlay —
/// instead of scanning every layer. This is why the workspace model is layered
/// per file rather than re-merged into one model per request (see
/// `large-project-memory`): a per-file split is what lets a deleted file be
/// forgotten by a single map removal.
#[derive(Clone)]
pub struct ModelLayers {
    pub(crate) base: Arc<SourceLayerIndex>,
    pub(crate) dirty: Arc<HashMap<Url, Arc<TypeModel>>>,
}
