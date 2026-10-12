//! The index's data types: the symbol table, the declared-type store, the
//! declared-type model, and the shared identity types.
//!
//! Pure data with its derives; every operation on it lives in the index's
//! `analyze`, `query`, and `util` modules ([`super::analyze`], [`super::query`],
//! [`super::util`]). `crate::types` keeps the binding/resolution engine and reads
//! the model through those functions.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::{Range, Url};
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

/// A Java primitive type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Prim {
    Boolean,
    Byte,
    Short,
    Int,
    Long,
    Char,
    Float,
    Double,
}

/// A declared type as written. Named reference types are keyed by the simple
/// name they were written with (a qualified name keeps its dots and is matched
/// on its last segment); generic arguments are kept as written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ty {
    Prim(Prim),
    Void,
    Null,
    Ref {
        name: String,
        args: Vec<Ty>,
    },
    Array(Box<Ty>),
    /// A type variable or a name the model could not resolve.
    Var(String),
    Unknown,
}

/// One method parameter: its declared type, plus the name where the source
/// provides one. Class-file descriptors carry types only, so parameters of jar
/// and JDK members read from bytecode are name-less.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Param {
    pub name: Option<String>,
    pub ty: Ty,
}

/// One declared member of a type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub name: String,
    /// `IndexKind::Field` or `IndexKind::Method`.
    pub kind: IndexKind,
    /// The field's type or the method's return type.
    pub ty: Ty,
    /// Parameters; empty for fields.
    pub params: Vec<Param>,
    /// The method's declared type parameter names; empty for fields and for
    /// class-file members, whose signatures are erased.
    pub type_params: Vec<String>,
    pub is_static: bool,
}

/// A declared type and its directly declared members.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeInfo {
    /// The simple name (the last segment of a qualified/indexed name).
    pub name: String,
    /// The enclosing nested-type chain (`Map` for `Map$Entry`), when this is a
    /// nested type; part of the model's identity, so two nested types sharing
    /// an innermost name in one package do not overwrite each other.
    pub nested: Option<String>,
    pub package: Option<String>,
    pub kind: IndexKind,
    /// Type parameter names, treated as resolvable-but-opaque.
    pub parameters: Vec<String>,
    /// `extends`/`implements` targets, as written.
    pub supertypes: Vec<Ty>,
    pub fields: Vec<Member>,
    pub methods: Vec<Member>,
    /// The type's constructors, kept separate from `methods` so a constructor
    /// never appears in a `.`-member listing (`t.T()` is not valid Java).
    pub constructors: Vec<Member>,
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
