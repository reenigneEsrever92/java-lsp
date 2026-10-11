//! The index's data types: the symbol table, the declared-type store, the
//! declared-type model, and the shared identity types.
//!
//! Pure data with its derives; every operation on it lives in [`crate::index`]
//! as a free function. `crate::types` keeps the binding/resolution engine and
//! reads the model through those functions.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::{Range, Url};

use super::is_type_kind;

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

// ---------------------------------------------------------------------------
// The declared-type store
// ---------------------------------------------------------------------------

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

/// A file's line-start byte offsets, built once so many byte offsets convert to
/// LSP positions in `O(log n)` each instead of rescanning the text from the top
/// per position — quadratic, and the whole cost of indexing a large file.
#[derive(Clone)]
pub(crate) struct LineIndex {
    pub(crate) starts: Vec<usize>,
}

// ---------------------------------------------------------------------------
// The declared-type model
// ---------------------------------------------------------------------------

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

impl Prim {
    pub(crate) fn from_keyword(word: &str) -> Option<Prim> {
        Some(match word {
            "boolean" => Prim::Boolean,
            "byte" => Prim::Byte,
            "short" => Prim::Short,
            "int" => Prim::Int,
            "long" => Prim::Long,
            "char" => Prim::Char,
            "float" => Prim::Float,
            "double" => Prim::Double,
            _ => return None,
        })
    }

    fn from_descriptor(byte: u8) -> Option<Prim> {
        Some(match byte {
            b'Z' => Prim::Boolean,
            b'B' => Prim::Byte,
            b'S' => Prim::Short,
            b'I' => Prim::Int,
            b'J' => Prim::Long,
            b'C' => Prim::Char,
            b'F' => Prim::Float,
            b'D' => Prim::Double,
            _ => return None,
        })
    }

    /// The name of this primitive in a binary class file's `ConstantValue`-free
    /// sense: its Java source keyword.
    pub fn name(self) -> &'static str {
        match self {
            Prim::Boolean => "boolean",
            Prim::Byte => "byte",
            Prim::Short => "short",
            Prim::Int => "int",
            Prim::Long => "long",
            Prim::Char => "char",
            Prim::Float => "float",
            Prim::Double => "double",
        }
    }
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

impl Ty {
    /// A reference type from a simple (or dotted) name.
    pub fn reference(name: impl Into<String>) -> Ty {
        Ty::Ref {
            name: name.into(),
            args: Vec::new(),
        }
    }

    /// The simple name to match a reference against the model: the last `.`- or
    /// `$`-separated segment, so the nested `java.util.Map$Entry` yields
    /// `Entry`. `None` for non-reference types.
    pub fn simple_name(&self) -> Option<&str> {
        match self {
            Ty::Ref { name, .. } => Some(name.rsplit(['.', '$']).next().unwrap_or(name)),
            Ty::Var(name) => Some(name),
            _ => None,
        }
    }

    /// The package a qualified reference names (`java.util` for
    /// `java.util.List`, and also for the nested `java.util.Map$Entry`, whose
    /// owner `Map` is not part of the package). `None` when unqualified.
    pub fn qualified_package(&self) -> Option<&str> {
        match self {
            Ty::Ref { name, .. } | Ty::Var(name) => {
                // A `$` introduces the nested owner; the package ends before it.
                let owner = name
                    .rsplit_once('$')
                    .map_or(name.as_str(), |(owner, _)| owner);
                owner.rsplit_once('.').map(|(package, _)| package)
            }
            _ => None,
        }
    }

    /// Replaces every occurrence of a bound type-parameter name with its type,
    /// recursing through generic arguments and array element types. Names that
    /// are not bound are left as written.
    pub fn substitute(&self, bindings: &HashMap<String, Ty>) -> Ty {
        if bindings.is_empty() {
            return self.clone();
        }
        match self {
            Ty::Ref { name, args } => {
                if args.is_empty() {
                    if let Some(bound) = bindings.get(name) {
                        return bound.clone();
                    }
                }
                Ty::Ref {
                    name: name.clone(),
                    args: args.iter().map(|arg| arg.substitute(bindings)).collect(),
                }
            }
            Ty::Var(name) => bindings.get(name).cloned().unwrap_or_else(|| self.clone()),
            Ty::Array(element) => Ty::Array(Box::new(element.substitute(bindings))),
            other => other.clone(),
        }
    }

    /// How the type is written in source-ish form.
    pub fn display(&self) -> String {
        match self {
            Ty::Prim(prim) => prim.name().to_string(),
            Ty::Void => "void".to_string(),
            Ty::Null => "null".to_string(),
            Ty::Ref { name, args } => {
                let simple = name.rsplit(['.', '$']).next().unwrap_or(name);
                if args.is_empty() {
                    simple.to_string()
                } else {
                    let inner: Vec<String> = args.iter().map(Ty::display).collect();
                    format!("{simple}<{}>", inner.join(", "))
                }
            }
            Ty::Array(element) => format!("{}[]", element.display()),
            Ty::Var(name) => name.clone(),
            Ty::Unknown => "?".to_string(),
        }
    }

    /// Parses a JVM field descriptor (`I`, `Ljava/lang/String;`, `[[D`).
    pub fn from_descriptor(descriptor: &str) -> Ty {
        let mut rest = descriptor;
        let ty = parse_descriptor(&mut rest);
        ty
    }

    /// Parses a JVM method descriptor (`(ILjava/lang/String;)V`) into its
    /// parameter types and return type.
    pub fn method_from_descriptor(descriptor: &str) -> (Vec<Ty>, Ty) {
        let mut rest = descriptor;
        let mut params = Vec::new();
        if let Some(after_open) = rest.strip_prefix('(') {
            rest = after_open;
            while !rest.is_empty() && !rest.starts_with(')') {
                let ty = parse_descriptor(&mut rest);
                if ty == Ty::Unknown && !rest.is_empty() {
                    // Unparseable: stop rather than spin.
                    break;
                }
                params.push(ty);
            }
            rest = rest.strip_prefix(')').unwrap_or(rest);
        }
        let ret = parse_descriptor(&mut rest);
        (params, ret)
    }
}

/// Parses one descriptor off the front of `rest`, advancing it.
fn parse_descriptor(rest: &mut &str) -> Ty {
    if rest.is_empty() {
        return Ty::Unknown;
    }
    if let Some(tail) = rest.strip_prefix('[') {
        *rest = tail;
        return Ty::Array(Box::new(parse_descriptor(rest)));
    }
    let first = rest.as_bytes()[0];
    if let Some(prim) = Prim::from_descriptor(first) {
        *rest = &rest[1..];
        return Ty::Prim(prim);
    }
    match first {
        b'V' => {
            *rest = &rest[1..];
            Ty::Void
        }
        b'L' => {
            // `Lcom/example/Lib;` — internal name, slashes to dots.
            match rest[1..].find(';') {
                Some(end) => {
                    let internal = &rest[1..1 + end];
                    *rest = &rest[1 + end + 1..];
                    Ty::reference(internal.replace('/', "."))
                }
                None => {
                    *rest = "";
                    Ty::Unknown
                }
            }
        }
        b'T' => match rest[1..].find(';') {
            Some(end) => {
                let name = &rest[1..1 + end];
                *rest = &rest[1 + end + 1..];
                Ty::Var(name.to_string())
            }
            None => {
                *rest = "";
                Ty::Unknown
            }
        },
        _ => {
            *rest = &rest[1..];
            Ty::Unknown
        }
    }
}

/// One method parameter: its declared type, plus the name where the source
/// provides one. Class-file descriptors carry types only, so parameters of jar
/// and JDK members read from bytecode are name-less.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Param {
    pub name: Option<String>,
    pub ty: Ty,
}

impl Param {
    /// A parameter with no known name, for sources that do not carry one.
    pub fn unnamed(ty: Ty) -> Param {
        Param { name: None, ty }
    }

    /// The parameter as it would be declared, e.g. `int index` or, when the
    /// name is unknown, just `int`.
    pub fn display(&self) -> String {
        match &self.name {
            Some(name) => format!("{} {name}", self.ty.display()),
            None => self.ty.display(),
        }
    }
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

impl Member {
    /// The member as it would be declared, e.g. `static int count` or
    /// `String getName(int index)`.
    pub fn display(&self) -> String {
        let static_prefix = if self.is_static { "static " } else { "" };
        match self.kind {
            IndexKind::Method => {
                let params: Vec<String> = self.params.iter().map(Param::display).collect();
                format!(
                    "{static_prefix}{} {}({})",
                    self.ty.display(),
                    self.name,
                    params.join(", ")
                )
            }
            _ => format!("{static_prefix}{} {}", self.ty.display(), self.name),
        }
    }

    /// Like [`Member::display`], but omits a type the model does not know
    /// (an unreadable descriptor), so hover never renders a `?`.
    pub fn signature(&self) -> String {
        let static_prefix = if self.is_static { "static " } else { "" };
        match self.kind {
            IndexKind::Method => {
                let params: Vec<String> = self.params.iter().map(Param::display).collect();
                let ret = if self.ty == Ty::Unknown {
                    String::new()
                } else {
                    format!("{} ", self.ty.display())
                };
                format!("{static_prefix}{ret}{}({})", self.name, params.join(", "))
            }
            _ => {
                if self.ty == Ty::Unknown {
                    format!("{static_prefix}{}", self.name)
                } else {
                    format!("{static_prefix}{} {}", self.ty.display(), self.name)
                }
            }
        }
    }

    /// The type the member contributes when it is accessed through a receiver:
    /// a field's type, or a method's return type.
    pub fn access_type(&self) -> Ty {
        self.ty.clone()
    }

    /// A constructor as it would be declared, e.g. `Point(int x, int y)`: the
    /// type's name and the parameters, with no return type (which
    /// [`Member::signature`] would render for a method).
    pub fn constructor_signature(&self) -> String {
        let params: Vec<String> = self.params.iter().map(Param::display).collect();
        format!("{}({})", self.name, params.join(", "))
    }

    /// Approximate bytes: the struct plus the owned strings and types.
    fn bytes(&self) -> usize {
        std::mem::size_of::<Member>()
            + self.name.len()
            + self.type_params.iter().map(String::len).sum::<usize>()
            + ty_bytes(&self.ty)
            + self
                .params
                .iter()
                .map(|param| {
                    std::mem::size_of::<Param>()
                        + param.name.as_deref().map_or(0, str::len)
                        + ty_bytes(&param.ty)
                })
                .sum::<usize>()
    }
}

/// Approximate bytes a type occupies, recursing into its arguments.
fn ty_bytes(ty: &Ty) -> usize {
    match ty {
        Ty::Ref { name, args } => {
            std::mem::size_of::<Ty>() + name.len() + args.iter().map(ty_bytes).sum::<usize>()
        }
        Ty::Var(name) => std::mem::size_of::<Ty>() + name.len(),
        Ty::Array(inner) => std::mem::size_of::<Ty>() + ty_bytes(inner),
        _ => std::mem::size_of::<Ty>(),
    }
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

impl TypeInfo {
    pub fn new(name: String, package: Option<String>, kind: IndexKind) -> Self {
        Self {
            name,
            nested: None,
            package,
            kind,
            parameters: Vec::new(),
            supertypes: Vec::new(),
            fields: Vec::new(),
            methods: Vec::new(),
            constructors: Vec::new(),
        }
    }

    pub(crate) fn members(&self) -> impl Iterator<Item = &Member> {
        self.fields.iter().chain(self.methods.iter())
    }

    /// A one-line rendering of the declaration, e.g. `class Widget` or
    /// `interface Greeter`.
    pub fn display(&self) -> String {
        let keyword = match self.kind {
            IndexKind::Interface => "interface",
            IndexKind::Enum => "enum",
            IndexKind::Record => "record",
            _ => "class",
        };
        let params = if self.parameters.is_empty() {
            String::new()
        } else {
            format!("<{}>", self.parameters.join(", "))
        };
        format!("{keyword} {}{params}", self.name)
    }

    /// Approximate bytes: the struct plus the owned strings, types, and members.
    fn bytes(&self) -> usize {
        std::mem::size_of::<TypeInfo>()
            + self.name.len()
            + self.nested.as_deref().map_or(0, str::len)
            + self.package.as_deref().map_or(0, str::len)
            + self.parameters.iter().map(String::len).sum::<usize>()
            + self.supertypes.iter().map(ty_bytes).sum::<usize>()
            + self.fields.iter().map(Member::bytes).sum::<usize>()
            + self.methods.iter().map(Member::bytes).sum::<usize>()
            + self.constructors.iter().map(Member::bytes).sum::<usize>()
    }
}

/// The workspace's declared types, indexed by simple name.
#[derive(Debug, Clone, Default)]
pub struct TypeModel {
    by_name: HashMap<String, Vec<TypeInfo>>,
}

impl TypeModel {
    /// `(types, members, approximate bytes)` across the model, for the index
    /// composition log: the struct sizes plus the owned strings and types.
    pub(crate) fn stats(&self) -> (usize, usize, usize) {
        let mut types = 0usize;
        let mut members = 0usize;
        let mut bytes = 0usize;
        for info in self.by_name.values().flatten() {
            types += 1;
            members += info.fields.len() + info.methods.len() + info.constructors.len();
            bytes += info.bytes();
        }
        (types, members, bytes)
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    pub fn insert(&mut self, info: TypeInfo) {
        let bucket = self.by_name.entry(info.name.clone()).or_default();
        // A later insert for the same (name, package, kind) replaces the
        // earlier one, so source-derived types overlay name-only ones.
        if let Some(existing) = bucket.iter_mut().find(|candidate| {
            candidate.package == info.package
                && candidate.kind == info.kind
                && candidate.nested == info.nested
        }) {
            *existing = info;
        } else {
            bucket.push(info);
        }
    }

    pub fn extend(&mut self, infos: impl IntoIterator<Item = TypeInfo>) {
        for info in infos {
            self.insert(info);
        }
    }

    /// Adds every type of `other`, replacing same-slot entries (by name,
    /// package, kind, and enclosing chain). Used to union per-file overlays.
    pub fn merge(&mut self, other: &TypeModel) {
        for infos in other.by_name.values() {
            for info in infos {
                self.insert(info.clone());
            }
        }
    }

    /// Every type declared with this simple name.
    pub fn find(&self, name: &str) -> &[TypeInfo] {
        self.by_name.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Every declared type in the model, in no particular order.
    pub fn types(&self) -> impl Iterator<Item = &TypeInfo> {
        self.by_name.values().flatten()
    }

    /// Builds a name-only model from index entries: every top-level type
    /// becomes a `TypeInfo`, and each method/field entry is attached to the
    /// type named by the innermost container. Used for jars and the JDK, whose
    /// descriptors the flat index does not carry.
    pub fn from_entries(entries: &[SymbolEntry]) -> TypeModel {
        let mut model = TypeModel::new();
        model.add_entries(entries);
        model
    }

    /// Adds every name-only type and member described by `entries` to this
    /// model (see [`TypeModel::from_entries`]); entries are not kept, so this
    /// can be fed one archive at a time without cloning them first.
    pub fn add_entries(&mut self, entries: &[SymbolEntry]) {
        let mut members: HashMap<String, Vec<Member>> = HashMap::new();
        for entry in entries {
            if !matches!(
                entry.kind,
                IndexKind::Method | IndexKind::Field | IndexKind::EnumConstant
            ) {
                continue;
            }
            let Some(container) = entry.container.last() else {
                continue;
            };
            members.entry(container.clone()).or_default().push(Member {
                name: entry.name.to_string(),
                kind: entry.kind,
                ty: Ty::Unknown,
                params: Vec::new(),
                type_params: Vec::new(),
                is_static: false,
            });
        }
        for entry in entries {
            if !is_type_kind(entry.kind) {
                continue;
            }
            // Nested types are indexed with a container chain; key them by
            // their innermost name so `Outer.Inner` is reachable as `Inner`.
            let name = entry
                .container
                .last()
                .cloned()
                .unwrap_or_else(|| entry.name.to_string());
            let mut info = TypeInfo::new(
                name.clone(),
                entry.package.as_deref().map(str::to_string),
                entry.kind,
            );
            if let Some(declared) = members.get(&name) {
                for member in declared {
                    if member.kind == IndexKind::Method {
                        info.methods.push(member.clone());
                    } else {
                        info.fields.push(member.clone());
                    }
                }
            }
            self.insert(info);
        }
    }
}

/// A stable key for a member's parameter types, for overload deduplication.
pub(crate) fn params_key(params: &[Param]) -> String {
    params
        .iter()
        .map(|param| param.ty.display())
        .collect::<Vec<_>>()
        .join(",")
}

impl TypeModel {
    fn find_unique(&self, name: &str, package: Option<&str>) -> Option<&TypeInfo> {
        let candidates = self.find(name);
        match candidates.len() {
            0 => None,
            1 => Some(&candidates[0]),
            _ => {
                let mut preferred = candidates
                    .iter()
                    .filter(|info| info.package.as_deref() == package);
                let first = preferred.next()?;
                if preferred.next().is_some() {
                    None
                } else {
                    Some(first)
                }
            }
        }
    }

    fn contains(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    fn find_in_package(&self, name: &str, package: Option<&str>) -> Option<&TypeInfo> {
        let mut matches = self
            .find(name)
            .iter()
            .filter(|info| info.package.as_deref() == package);
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }

    fn all_types(&self) -> Vec<&TypeInfo> {
        self.types().collect()
    }
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
    layers: Vec<Arc<TypeModel>>,
    /// The URI of each layer, parallel to `layers`.
    uris: Vec<Url>,
    /// The position of each layer's URI in `layers`, to map a dirty overlay entry
    /// back to the layer it replaces.
    index_of: HashMap<Url, usize>,
    /// The layer positions that declare each name, highest precedence first.
    by_name: HashMap<String, Vec<usize>>,
}

impl SourceLayerIndex {
    /// Builds the index from `(uri, model)` layers already in increasing
    /// precedence (a later layer wins), preserving that order. Used for the
    /// non-source base, whose precedence is the order the producing modules
    /// added their artifacts in.
    pub fn ordered(layers: Vec<(Url, Arc<TypeModel>)>) -> Self {
        let mut models: Vec<Arc<TypeModel>> = Vec::with_capacity(layers.len());
        let mut uris: Vec<Url> = Vec::with_capacity(layers.len());
        let mut index_of: HashMap<Url, usize> = HashMap::with_capacity(layers.len());
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, (uri, model)) in layers.into_iter().enumerate() {
            index_of.insert(uri.clone(), i);
            uris.push(uri);
            models.push(model);
        }
        // Highest precedence first, so each name's layer positions are recorded
        // in precedence order. Within one layer a name's entries are contiguous
        // (one bucket per name), so the last position pushed is that layer.
        for i in (0..models.len()).rev() {
            for info in models[i].types() {
                let positions = by_name.entry(info.name.clone()).or_default();
                if positions.last() != Some(&i) {
                    positions.push(i);
                }
            }
        }
        Self {
            layers: models,
            uris,
            index_of,
            by_name,
        }
    }

    /// Builds the index from `(uri, model)` layers, normalising the order to
    /// URI-ascending (so the caller need not pre-sort). Used for the workspace
    /// source models, whose union is order-independent.
    pub fn new(mut layers: Vec<(Url, Arc<TypeModel>)>) -> Self {
        layers.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        Self::ordered(layers)
    }

    fn uri_of(&self, index: usize) -> &Url {
        &self.uris[index]
    }

    fn contains_uri(&self, uri: &Url) -> bool {
        self.index_of.contains_key(uri)
    }

    /// The layers that declare `name`, highest precedence first.
    fn declaring(&self, name: &str) -> Vec<&TypeModel> {
        let mut out = Vec::new();
        if let Some(positions) = self.by_name.get(name) {
            for &i in positions {
                if let Some(layer) = self.layers.get(i) {
                    if (**layer).contains(name) {
                        out.push(&**layer);
                    }
                }
            }
        }
        out
    }
}

/// The types named `name` across `declaring` (highest precedence first), each
/// slot (`package`, `kind`, enclosing chain) taken from its highest-precedence
/// layer — the same slot-dedup [`ModelLayers`] applies over its own layers.
fn layer_candidates<'a>(declaring: &[&'a TypeModel], name: &str) -> Vec<&'a TypeInfo> {
    let mut seen: HashSet<(Option<&str>, IndexKind, Option<&str>)> = HashSet::new();
    let mut out: Vec<&'a TypeInfo> = Vec::new();
    for layer in declaring {
        for info in layer.find(name) {
            let slot = (info.package.as_deref(), info.kind, info.nested.as_deref());
            if seen.insert(slot) {
                out.push(info);
            }
        }
    }
    out
}

impl SourceLayerIndex {
    pub(crate) fn find_unique(&self, name: &str, package: Option<&str>) -> Option<&TypeInfo> {
        let declaring = self.declaring(name);
        let first = *declaring.first()?;
        if declaring.len() == 1 {
            return first.find_unique(name, package);
        }
        let candidates = layer_candidates(&declaring, name);
        match candidates.len() {
            0 => None,
            1 => Some(candidates[0]),
            _ => {
                let mut preferred = candidates
                    .iter()
                    .filter(|info| info.package.as_deref() == package);
                let first = preferred.next()?;
                if preferred.next().is_some() {
                    None
                } else {
                    Some(*first)
                }
            }
        }
    }

    pub(crate) fn find_in_package(&self, name: &str, package: Option<&str>) -> Option<&TypeInfo> {
        let declaring = self.declaring(name);
        let first = *declaring.first()?;
        if declaring.len() == 1 {
            return first.find_in_package(name, package);
        }
        let mut matches = layer_candidates(&declaring, name)
            .into_iter()
            .filter(|info| info.package.as_deref() == package);
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    pub(crate) fn all_types(&self) -> Vec<&TypeInfo> {
        let mut seen: HashSet<(&str, Option<&str>, IndexKind, Option<&str>)> = HashSet::new();
        let mut out: Vec<&TypeInfo> = Vec::new();
        // Highest precedence first (layers are in increasing precedence), each
        // declared type taken once — the name is part of the identity, so two
        // types sharing a package, kind, and owner both survive.
        for layer in self.layers.iter().rev() {
            for info in layer.types() {
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
        }
        out
    }
}

/// A URI for a layer built with no URI of its own (the `ModelLayers::new` test
/// path). Such a layer can never be overridden, because the dirty overlay has
/// no entry that matches it.
fn synthetic_layer_uri(index: usize) -> Url {
    Url::parse(&format!("java-lsp-layer://{index}")).expect("valid synthetic layer URI")
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
    base: Arc<SourceLayerIndex>,
    dirty: Arc<HashMap<Url, Arc<TypeModel>>>,
}

impl Default for ModelLayers {
    /// An empty view: no layers and no dirty overlay.
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl ModelLayers {
    /// A view over `layers` (increasing precedence) with an empty dirty
    /// overlay, and no URI to override.
    pub fn new(layers: Vec<Arc<TypeModel>>) -> Self {
        let indexed = layers
            .into_iter()
            .enumerate()
            .map(|(i, model)| (synthetic_layer_uri(i), model))
            .collect();
        Self {
            base: Arc::new(SourceLayerIndex::new(indexed)),
            dirty: Arc::new(HashMap::new()),
        }
    }

    /// A view over a cached base and the engine's dirty overlay: a dirty entry
    /// replaces the base layer with the same URI, and a dirty-only URI (a file
    /// created after warm-up) ranks above every base layer.
    pub fn layered(base: Arc<SourceLayerIndex>, dirty: Arc<HashMap<Url, Arc<TypeModel>>>) -> Self {
        Self { base, dirty }
    }

    pub fn is_empty(&self) -> bool {
        let base_empty = self.base.layers.iter().enumerate().all(|(i, layer)| {
            match self.dirty.get(self.base.uri_of(i)) {
                Some(dirty) => dirty.is_empty(),
                None => layer.is_empty(),
            }
        });
        base_empty
            && self
                .dirty
                .iter()
                .filter(|(uri, _)| !self.base.contains_uri(uri))
                .all(|(_, model)| model.is_empty())
    }

    /// Every declared type across the layers, highest precedence first, with
    /// same-slot duplicates collapsed (a later layer's copy wins).
    pub fn types(&self) -> impl Iterator<Item = &TypeInfo> + '_ {
        let mut seen: HashSet<(&str, Option<&str>, IndexKind, Option<&str>)> = HashSet::new();
        let mut out: Vec<&TypeInfo> = Vec::new();
        for layer in self.effective_layers() {
            for info in layer.types() {
                if seen.insert((
                    info.name.as_str(),
                    info.package.as_deref(),
                    info.kind,
                    info.nested.as_deref(),
                )) {
                    out.push(info);
                }
            }
        }
        out.into_iter()
    }

    /// The effective layers in decreasing precedence: the dirty-only layers
    /// (by URI) first, then each base layer with its dirty override in place.
    fn effective_layers(&self) -> Vec<&TypeModel> {
        let mut out: Vec<&TypeModel> = Vec::new();
        for uri in self.dirty_only_uris() {
            if let Some(model) = self.dirty.get(uri) {
                out.push(&**model);
            }
        }
        for i in (0..self.base.layers.len()).rev() {
            out.push(self.effective_base(i));
        }
        out
    }

    /// The model in force for base layer `index`: its dirty override if it has
    /// one, else the warm-up layer.
    fn effective_base(&self, index: usize) -> &TypeModel {
        match self.dirty.get(self.base.uri_of(index)) {
            Some(dirty) => dirty,
            None => &self.base.layers[index],
        }
    }

    /// The dirty overlay's URIs the base does not carry, highest precedence
    /// (URI-descending) first.
    fn dirty_only_uris(&self) -> Vec<&Url> {
        let mut extras: Vec<&Url> = self
            .dirty
            .keys()
            .filter(|uri| !self.base.contains_uri(uri))
            .collect();
        extras.sort_by(|a, b| b.as_str().cmp(a.as_str()));
        extras
    }

    /// The layers that declare `name`, highest precedence first.
    fn declaring_layers(&self, name: &str) -> Vec<&TypeModel> {
        let mut out: Vec<&TypeModel> = Vec::new();
        for uri in self.dirty_only_uris() {
            if let Some(model) = self.dirty.get(uri) {
                if model.contains(name) {
                    out.push(&**model);
                }
            }
        }
        if let Some(positions) = self.base.by_name.get(name) {
            for &i in positions {
                let model = self.effective_base(i);
                if model.contains(name) {
                    out.push(model);
                }
            }
        }
        out
    }

    /// The candidate types named `name` across `declaring` (highest precedence
    /// first), each slot taken from its highest-precedence layer.
    fn candidates<'a>(declaring: &[&'a TypeModel], name: &str) -> Vec<&'a TypeInfo> {
        let mut seen: HashSet<(Option<&str>, IndexKind, Option<&str>)> = HashSet::new();
        let mut out: Vec<&'a TypeInfo> = Vec::new();
        for layer in declaring {
            for info in layer.find(name) {
                let slot = (info.package.as_deref(), info.kind, info.nested.as_deref());
                if seen.insert(slot) {
                    out.push(info);
                }
            }
        }
        out
    }
}

impl ModelLayers {
    pub(crate) fn find_unique(&self, name: &str, package: Option<&str>) -> Option<&TypeInfo> {
        // The common case is one declaring layer, whose own answer is the merged
        // answer — then nothing is allocated at all.
        let declaring = self.declaring_layers(name);
        let first = *declaring.first()?;
        if declaring.len() == 1 {
            return first.find_unique(name, package);
        }
        // Several layers declare the name: reproduce `TypeModel::find_unique`
        // over the slot-deduplicated candidates.
        let candidates = Self::candidates(&declaring, name);
        match candidates.len() {
            0 => None,
            1 => Some(candidates[0]),
            _ => {
                let mut preferred = candidates
                    .iter()
                    .filter(|info| info.package.as_deref() == package);
                let first = preferred.next()?;
                if preferred.next().is_some() {
                    None
                } else {
                    Some(*first)
                }
            }
        }
    }

    pub(crate) fn find_in_package(&self, name: &str, package: Option<&str>) -> Option<&TypeInfo> {
        let declaring = self.declaring_layers(name);
        let first = *declaring.first()?;
        if declaring.len() == 1 {
            return first.find_in_package(name, package);
        }
        let mut matches = Self::candidates(&declaring, name)
            .into_iter()
            .filter(|info| info.package.as_deref() == package);
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        if let Some(positions) = self.base.by_name.get(name) {
            for &i in positions {
                match self.dirty.get(self.base.uri_of(i)) {
                    Some(dirty) => {
                        if dirty.contains(name) {
                            return true;
                        }
                    }
                    None => return true,
                }
            }
        }
        self.dirty
            .iter()
            .any(|(uri, model)| !self.base.contains_uri(uri) && model.contains(name))
    }

    pub(crate) fn all_types(&self) -> Vec<&TypeInfo> {
        self.types().collect()
    }
}
