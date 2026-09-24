//! The pure-Rust type layer (R7): a declared-type model built from the syntax
//! trees and the flat symbol index, plus the binding rules that turn a cursor
//! position into a resolved declaration or receiver type.
//!
//! It is conservative by construction — every query answers `None` or an empty
//! list when no single answer exists, because no result beats a wrong result.
//! Generics are preserved for display but not substituted, overloads are matched
//! by name only, and anything the model cannot see is `Ty::Unknown`.

use std::collections::{HashMap, HashSet, VecDeque};

use tree_sitter::{Node, Tree};

use crate::index::{IndexKind, SymbolEntry};

/// A Java primitive type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    fn from_keyword(word: &str) -> Option<Prim> {
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
}

/// A declared type and its directly declared members.
#[derive(Debug, Clone, PartialEq, Eq)]
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
        }
    }

    fn members(&self) -> impl Iterator<Item = &Member> {
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
}

/// The workspace's declared types, indexed by simple name.
#[derive(Debug, Clone, Default)]
pub struct TypeModel {
    by_name: HashMap<String, Vec<TypeInfo>>,
}

impl TypeModel {
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
            if !matches!(entry.kind, IndexKind::Method | IndexKind::Field) {
                continue;
            }
            let Some(container) = entry.container.last() else {
                continue;
            };
            members.entry(container.clone()).or_default().push(Member {
                name: entry.name.clone(),
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
                .unwrap_or_else(|| entry.name.clone());
            let mut info = TypeInfo::new(name.clone(), entry.package.clone(), entry.kind);
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

/// Read-only access to declared types: the workspace model, optionally with a
/// single open document's types layered over it.
pub trait TypeLookup {
    /// The single type named `name`, preferring `package`; `None` when absent
    /// or ambiguous.
    fn find_unique(&self, name: &str, package: Option<&str>) -> Option<&TypeInfo>;

    /// The single type declared with this simple name in exactly `package`.
    /// Unlike [`TypeLookup::find_unique`], a name that only matches a type in
    /// another package yields `None` rather than a unique-match fallback.
    fn find_in_package(&self, name: &str, package: Option<&str>) -> Option<&TypeInfo>;

    /// True when any type is declared with this simple name.
    fn contains(&self, name: &str) -> bool;

    /// The type a reference points at: a qualified name is resolved in exactly
    /// its own package, otherwise `package` breaks ties.
    fn lookup(&self, ty: &Ty, package: Option<&str>) -> Option<&TypeInfo> {
        match ty {
            Ty::Array(element) => self.lookup(element, package),
            Ty::Ref { name, .. } | Ty::Var(name) => {
                let simple = name.rsplit(['.', '$']).next().unwrap_or(name);
                match ty.qualified_package() {
                    Some(qualifier) => self.find_in_package(simple, Some(qualifier)),
                    None => self.find_unique(simple, package),
                }
            }
            _ => None,
        }
    }

    /// The members of `ty`, including inherited ones. Supertype names resolve
    /// with the subtype's own package as context; cycles are guarded and the
    /// first declaration of a name wins. Same-named overloads collapse to the
    /// first — see [`TypeLookup::members_with_overloads`] to keep them apart.
    fn members(&self, ty: &Ty, package: Option<&str>) -> Vec<Member> {
        let mut out = Vec::new();
        let mut seen: HashSet<(String, bool)> = HashSet::new();
        for member in self.members_with_overloads(ty, package) {
            let is_method = member.kind == IndexKind::Method;
            if seen.insert((member.name.clone(), is_method)) {
                out.push(member);
            }
        }
        out
    }

    /// The members of `ty`, including inherited ones, with same-named overloads
    /// kept separate. Unlike [`TypeLookup::members`] — which collapses a name to
    /// a single member for typing — this deduplicates by name, kind, and
    /// parameter types, so an override still collapses but genuinely distinct
    /// overloads remain. Supertype resolution and the cycle guard match
    /// `members`.
    fn members_with_overloads(&self, ty: &Ty, package: Option<&str>) -> Vec<Member> {
        if matches!(ty, Ty::Array(_)) {
            return Vec::new();
        }
        // The root resolves exactly when its name is qualified, and with
        // `package` breaking ties otherwise.
        let Some(root) = self.lookup(ty, package) else {
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
            let Some(info) = self.find_unique(&name, context.as_deref()) else {
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
                // A qualified supertype (every class-file edge) resolves in its
                // own package, so a base whose simple name is shared across
                // packages still finds the right type; a source-written simple
                // name falls back to the subtype's own context as before.
                if supertype.qualified_package().is_some() {
                    if let Some(resolved) = self.lookup(supertype, None) {
                        queue.push_back((resolved.name.clone(), resolved.package.clone()));
                    }
                } else if let Some(super_name) = supertype.simple_name() {
                    queue.push_back((super_name.to_string(), super_context.clone()));
                }
            }
        }
        out
    }
}

/// A stable key for a member's parameter types, for overload deduplication.
fn params_key(params: &[Param]) -> String {
    params
        .iter()
        .map(|param| param.ty.display())
        .collect::<Vec<_>>()
        .join(",")
}

impl TypeLookup for TypeModel {
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
}

/// The workspace model with one open document's types layered over it: the
/// overlay wins, so what features answer reflects unsaved edits.
pub struct TypeQuery<'a> {
    base: &'a TypeModel,
    overlay: &'a TypeModel,
}

impl<'a> TypeQuery<'a> {
    pub fn new(base: &'a TypeModel, overlay: &'a TypeModel) -> Self {
        Self { base, overlay }
    }
}

impl TypeLookup for TypeQuery<'_> {
    fn find_unique(&self, name: &str, package: Option<&str>) -> Option<&TypeInfo> {
        self.overlay
            .find_unique(name, package)
            .or_else(|| self.base.find_unique(name, package))
    }

    fn find_in_package(&self, name: &str, package: Option<&str>) -> Option<&TypeInfo> {
        self.overlay
            .find_in_package(name, package)
            .or_else(|| self.base.find_in_package(name, package))
    }

    fn contains(&self, name: &str) -> bool {
        self.overlay.contains(name) || self.base.contains(name)
    }
}

/// True for the index kinds that declare a type.
pub fn is_type_kind(kind: IndexKind) -> bool {
    matches!(
        kind,
        IndexKind::Class | IndexKind::Interface | IndexKind::Enum | IndexKind::Record
    )
}

// ---------------------------------------------------------------------------
// Source extraction
// ---------------------------------------------------------------------------

/// Builds `TypeInfo`s for every class/interface/enum/record declared in a
/// parsed document, including nested ones.
pub fn collect_type_infos(package: Option<&str>, tree: &Tree, text: &str) -> Vec<TypeInfo> {
    let mut out = Vec::new();
    collect_types_in(
        &tree.root_node(),
        text,
        package.map(str::to_string),
        None,
        &mut out,
    );
    out
}

fn collect_types_in(
    node: &Node,
    text: &str,
    package: Option<String>,
    enclosing: Option<String>,
    out: &mut Vec<TypeInfo>,
) {
    let mut child_enclosing = enclosing.clone();
    if is_type_decl(node.kind()) {
        if let Some(mut info) = type_info_from_declaration(node, text, package.clone()) {
            info.nested = enclosing.clone();
            if let Some(name) = node.child_by_field_name("name") {
                let simple = text[name.byte_range()].to_string();
                child_enclosing = Some(match &enclosing {
                    Some(outer) => format!("{outer}${simple}"),
                    None => simple,
                });
            }
            out.push(info);
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_types_in(&child, text, package.clone(), child_enclosing.clone(), out);
    }
}

fn is_type_decl(kind: &str) -> bool {
    matches!(
        kind,
        "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
    )
}

fn type_info_from_declaration(
    node: &Node,
    text: &str,
    package: Option<String>,
) -> Option<TypeInfo> {
    let name_node = node.child_by_field_name("name");
    let kind = match node.kind() {
        "interface_declaration" | "annotation_type_declaration" => IndexKind::Interface,
        "enum_declaration" => IndexKind::Enum,
        "record_declaration" => IndexKind::Record,
        _ => IndexKind::Class,
    };
    let mut info = TypeInfo::new(
        name_node.map(|n| text[n.byte_range()].to_string())?,
        package,
        kind,
    );

    if let Some(params) = node.child_by_field_name("type_parameters") {
        let mut cursor = params.walk();
        for parameter in params.named_children(&mut cursor) {
            if parameter.kind() != "type_parameter" {
                continue;
            }
            // `type_parameter` has no fields: its name is the `type_identifier`
            // child (any `type_bound` is ignored).
            let mut inner = parameter.walk();
            let name = parameter
                .named_children(&mut inner)
                .find(|child| child.kind() == "type_identifier")
                .map(|node| text[node.byte_range()].to_string());
            if let Some(name) = name {
                info.parameters.push(name);
            }
        }
    }

    // The `superclass` field wraps the type in a `superclass` node.
    if let Some(superclass) = node.child_by_field_name("superclass") {
        if let Some(inner) = superclass.named_child(0) {
            info.supertypes.push(type_from_node(&inner, text));
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(child.kind(), "super_interfaces" | "extends_interfaces") {
            collect_type_list(&child, text, &mut info.supertypes);
        }
    }

    if let Some(body) = node.child_by_field_name("body") {
        collect_members(&body, text, &mut info);
    }
    // A record's components live in its `parameters` field, not its body. For a
    // client each is the accessor `x()`, so it is modelled as a method (the
    // private backing field is not added, so it is never offered to a receiver).
    if node.kind() == "record_declaration" {
        for (name, ty) in record_components(node, text) {
            info.methods.push(Member {
                name,
                kind: IndexKind::Method,
                ty,
                params: Vec::new(),
                type_params: Vec::new(),
                is_static: false,
            });
        }
    }
    Some(info)
}

/// Collects the type nodes of a `super_interfaces`/`extends_interfaces`
/// clause. Both wrap a single `type_list` child.
fn collect_type_list(node: &Node, text: &str, out: &mut Vec<Ty>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "type_list" {
            let mut inner = child.walk();
            for element in child.named_children(&mut inner) {
                out.push(type_from_node(&element, text));
            }
        }
    }
}

fn collect_members(body: &Node, text: &str, info: &mut TypeInfo) {
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        match child.kind() {
            "field_declaration" | "constant_declaration" => {
                let Some(ty) = child
                    .child_by_field_name("type")
                    .map(|node| type_from_node(&node, text))
                else {
                    continue;
                };
                let is_static = has_modifier(&child, text, "static");
                let mut declarators = child.walk();
                for declarator in child.named_children(&mut declarators) {
                    if declarator.kind() != "variable_declarator" {
                        continue;
                    }
                    let Some(name) = declarator.child_by_field_name("name") else {
                        continue;
                    };
                    info.fields.push(Member {
                        name: text[name.byte_range()].to_string(),
                        kind: IndexKind::Field,
                        ty: ty.clone(),
                        params: Vec::new(),
                        type_params: Vec::new(),
                        is_static,
                    });
                }
            }
            "method_declaration" => {
                let Some(name) = child.child_by_field_name("name") else {
                    continue;
                };
                let ret = child
                    .child_by_field_name("type")
                    .map(|node| type_from_node(&node, text))
                    .unwrap_or(Ty::Unknown);
                info.methods.push(Member {
                    name: text[name.byte_range()].to_string(),
                    kind: IndexKind::Method,
                    ty: ret,
                    params: parameter_list(&child, text),
                    type_params: method_type_params(&child, text),
                    is_static: has_modifier(&child, text, "static"),
                });
            }
            _ => {}
        }
    }
}

/// The type parameter names a method declares, e.g. `["E"]` for
/// `<E> List<E> of(E e1)`.
fn method_type_params(method: &Node, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(params) = method.child_by_field_name("type_parameters") {
        collect_parameter_names(&params, text, &mut out);
    }
    out
}

pub(crate) fn parameter_list(method: &Node, text: &str) -> Vec<Param> {
    let Some(parameters) = method.child_by_field_name("parameters") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = parameters.walk();
    for parameter in parameters.named_children(&mut cursor) {
        if parameter.kind() != "formal_parameter" && parameter.kind() != "spread_parameter" {
            continue;
        }
        let Some(ty) = parameter.child_by_field_name("type") else {
            continue;
        };
        out.push(Param {
            name: parameter_name(&parameter, text),
            ty: type_from_node(&ty, text),
        });
    }
    out
}

/// An integer literal's type: `long` when written with an `L` suffix, else
/// `int`. (A suffix-less literal too large for `int` is still reported as
/// `int`; the compiler's widening is out of scope here.)
fn integer_literal_type(node: &Node, text: &str) -> Ty {
    let literal = &text[node.byte_range()];
    if literal.ends_with('l') || literal.ends_with('L') {
        Ty::Prim(Prim::Long)
    } else {
        Ty::Prim(Prim::Int)
    }
}

/// A floating literal's type: `float` when written with an `f`/`F` suffix, else
/// `double`. The suffix must not be read as a hex digit, so only the final
/// character is checked.
fn floating_literal_type(node: &Node, text: &str) -> Ty {
    let literal = &text[node.byte_range()];
    if literal.ends_with('f') || literal.ends_with('F') {
        Ty::Prim(Prim::Float)
    } else {
        Ty::Prim(Prim::Double)
    }
}

/// The declared name of a `formal_parameter`/`spread_parameter`. A spread
/// parameter has no `name` field; its `variable_declarator` carries it.
fn parameter_name(parameter: &Node, text: &str) -> Option<String> {
    if let Some(name) = parameter.child_by_field_name("name") {
        return Some(text[name.byte_range()].to_string());
    }
    let mut cursor = parameter.walk();
    for child in parameter.named_children(&mut cursor) {
        if child.kind() == "variable_declarator" {
            if let Some(name) = child.child_by_field_name("name") {
                return Some(text[name.byte_range()].to_string());
            }
        }
    }
    None
}

/// A record's header components as `(name, type)` pairs. Unlike a class, a
/// record declares its components in its `parameters` field, not its body.
fn record_components(node: &Node, text: &str) -> Vec<(String, Ty)> {
    let Some(parameters) = node.child_by_field_name("parameters") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = parameters.walk();
    for parameter in parameters.named_children(&mut cursor) {
        if parameter.kind() != "formal_parameter" {
            continue;
        }
        let Some(name) = parameter_name(&parameter, text) else {
            continue;
        };
        let ty = parameter
            .child_by_field_name("type")
            .map(|node| type_from_node(&node, text))
            .unwrap_or(Ty::Unknown);
        out.push((name, ty));
    }
    out
}

fn has_modifier(node: &Node, text: &str, modifier: &str) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifiers" {
            let words = text[child.byte_range()].split_whitespace();
            return words.into_iter().any(|word| word == modifier);
        }
    }
    false
}

/// Converts a type node into a [`Ty`]. Unknown shapes degrade to
/// [`Ty::Unknown`] rather than guessing.
pub fn type_from_node(node: &Node, text: &str) -> Ty {
    match node.kind() {
        "integral_type" | "floating_point_type" | "boolean_type" => {
            Prim::from_keyword(&text[node.byte_range()])
                .map(Ty::Prim)
                .unwrap_or(Ty::Unknown)
        }
        "void_type" => Ty::Void,
        "type_identifier" | "scoped_type_identifier" => {
            Ty::reference(text[node.byte_range()].to_string())
        }
        "generic_type" => {
            // No fields: the base name and `type_arguments` are both children.
            let mut base = String::new();
            let mut args = Vec::new();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                match child.kind() {
                    "type_identifier" | "scoped_type_identifier" if base.is_empty() => {
                        base = text[child.byte_range()].to_string();
                    }
                    "type_arguments" => {
                        let mut inner = child.walk();
                        for argument in child.named_children(&mut inner) {
                            args.push(type_from_node(&argument, text));
                        }
                    }
                    _ => {}
                }
            }
            Ty::Ref { name: base, args }
        }
        "array_type" => {
            let element = node
                .child_by_field_name("element")
                .or_else(|| node.named_child(0));
            match element {
                Some(element) => Ty::Array(Box::new(type_from_node(&element, text))),
                None => Ty::Unknown,
            }
        }
        "annotated_type" => node
            .child_by_field_name("type")
            .or_else(|| node.named_child(0))
            .map(|inner| type_from_node(&inner, text))
            .unwrap_or(Ty::Unknown),
        "wildcard" => Ty::Unknown,
        _ => Ty::Unknown,
    }
}

// ---------------------------------------------------------------------------
// Binding: visible names and receiver types
// ---------------------------------------------------------------------------

/// One `import` statement of a compilation unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    /// The dotted path as written; a wildcard import ends in `*`.
    pub path: String,
    pub is_wildcard: bool,
    pub is_static: bool,
}

impl Import {
    /// The simple name bound by a single-type import; `None` for wildcards.
    pub fn simple_name(&self) -> Option<&str> {
        if self.is_wildcard {
            None
        } else {
            self.path.rsplit('.').next()
        }
    }

    /// The package an import brings names from: the path's package for a
    /// wildcard import, or everything before the last segment for a single-type
    /// import. `None` for a name in the default package.
    pub fn package(&self) -> Option<String> {
        if self.is_wildcard {
            self.path.strip_suffix(".*").map(str::to_string)
        } else {
            self.path
                .rsplit_once('.')
                .map(|(package, _)| package.to_string())
        }
    }
}

/// The `import` statements of a document.
pub fn imports_of(tree: &Tree, text: &str) -> Vec<Import> {
    let root = tree.root_node();
    let mut out = Vec::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() != "import_declaration" {
            continue;
        }
        let mut raw = text[child.byte_range()].trim();
        raw = raw.strip_prefix("import").map_or(raw, str::trim);
        let is_static = raw.strip_prefix("static").is_some();
        raw = raw.strip_prefix("static").map_or(raw, str::trim);
        let path = raw.trim_end_matches(';').trim().to_string();
        let is_wildcard = path.ends_with('*');
        out.push(Import {
            path,
            is_wildcard,
            is_static,
        });
    }
    out
}

/// The dotted name of the file's package, or `None` for the default package.
pub fn file_package(tree: &Tree, text: &str) -> Option<String> {
    let root = tree.root_node();
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

/// The names visible at a position, plus the compilation unit's context.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    pub package: Option<String>,
    pub imports: Vec<Import>,
    /// Locals and parameters in scope, outermost first.
    pub locals: Vec<(String, Ty)>,
    /// Fields of the innermost enclosing type, as declared in the open buffer.
    pub fields: Vec<Member>,
    pub type_params: Vec<String>,
    /// The simple name of the innermost enclosing type, if any.
    pub enclosing_type: Option<String>,
}

/// Builds the scope for a node, walking up through its enclosing methods and
/// types. Locals are limited to those declared before the node. The enclosing
/// type's context is gathered first, so a `var` local's initializer can be
/// resolved against the fields, type parameters, and enclosing type.
pub fn scope_at(node: Node, text: &str, tree: &Tree, model: &dyn TypeLookup) -> Scope {
    let mut scope = Scope {
        package: file_package(tree, text),
        imports: imports_of(tree, text),
        ..Default::default()
    };
    let cursor_byte = node.start_byte();

    // The innermost enclosing method/constructor and type, if any.
    let mut method = None;
    let mut enclosing = None;
    let mut current = Some(node);
    while let Some(current_node) = current {
        match current_node.kind() {
            "method_declaration" | "constructor_declaration" if method.is_none() => {
                method = Some(current_node);
            }
            "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
                if enclosing.is_none() =>
            {
                enclosing = Some(current_node);
            }
            _ => {}
        }
        current = current_node.parent();
    }

    if let Some(type_node) = enclosing {
        scope.enclosing_type = type_node
            .child_by_field_name("name")
            .map(|name| text[name.byte_range()].to_string());
        if let Some(body) = type_node.child_by_field_name("body") {
            collect_field_members(&body, text, &mut scope);
        }
        // A record's components are nameable unqualified inside the record, as
        // the private backing fields would be.
        if type_node.kind() == "record_declaration" {
            for (name, ty) in record_components(&type_node, text) {
                scope.fields.push(Member {
                    name,
                    kind: IndexKind::Field,
                    ty,
                    params: Vec::new(),
                    type_params: Vec::new(),
                    is_static: false,
                });
            }
        }
        if let Some(params) = type_node.child_by_field_name("type_parameters") {
            collect_parameter_names(&params, text, &mut scope.type_params);
        }
    }
    if let Some(method) = method {
        collect_parameters(&method, text, &mut scope);
        if let Some(body) = method.child_by_field_name("body") {
            collect_locals(&body, text, cursor_byte, model, &mut scope);
        }
    }
    scope
}

fn collect_parameters(method: &Node, text: &str, scope: &mut Scope) {
    if let Some(params) = method.child_by_field_name("type_parameters") {
        collect_parameter_names(&params, text, &mut scope.type_params);
    }
    let Some(parameters) = method.child_by_field_name("parameters") else {
        return;
    };
    let mut cursor = parameters.walk();
    for parameter in parameters.named_children(&mut cursor) {
        if !matches!(parameter.kind(), "formal_parameter" | "spread_parameter") {
            continue;
        }
        let ty = parameter
            .child_by_field_name("type")
            .map(|node| type_from_node(&node, text))
            .unwrap_or(Ty::Unknown);
        if let Some(name) = parameter.child_by_field_name("name") {
            scope.locals.push((text[name.byte_range()].to_string(), ty));
        }
    }
}

fn collect_parameter_names(params: &Node, text: &str, out: &mut Vec<String>) {
    let mut cursor = params.walk();
    for parameter in params.named_children(&mut cursor) {
        if parameter.kind() != "type_parameter" {
            continue;
        }
        let mut inner = parameter.walk();
        let name = parameter
            .named_children(&mut inner)
            .find(|child| child.kind() == "type_identifier")
            .map(|node| text[node.byte_range()].to_string());
        if let Some(name) = name {
            out.push(name);
        }
    }
}

fn collect_locals(
    node: &Node,
    text: &str,
    cursor_byte: usize,
    model: &dyn TypeLookup,
    scope: &mut Scope,
) {
    match node.kind() {
        "local_variable_declaration" if node.start_byte() < cursor_byte => {
            let type_node = node.child_by_field_name("type");
            let is_var = type_node
                .as_ref()
                .is_some_and(|ty| &text[ty.byte_range()] == "var");
            let declared = type_node
                .map(|node| type_from_node(&node, text))
                .unwrap_or(Ty::Unknown);
            let mut inner = node.walk();
            for declarator in node.named_children(&mut inner) {
                if declarator.kind() != "variable_declarator" {
                    continue;
                }
                let Some(name) = declarator.child_by_field_name("name") else {
                    continue;
                };
                // `var` takes the initializer's inferred type; the initializer
                // sees the context (fields, type parameters, enclosing type)
                // already gathered on `scope`.
                let ty = if is_var {
                    match declarator.child_by_field_name("value") {
                        Some(value) => receiver_type(&value, text, scope, model),
                        None => Ty::Unknown,
                    }
                } else {
                    declared.clone()
                };
                scope.locals.push((text[name.byte_range()].to_string(), ty));
            }
        }
        "enhanced_for_statement" if node.start_byte() < cursor_byte => {
            // A `var` binding takes the iterable's element type; an explicit
            // type is used as written.
            let ty = if is_var_binding(node, text) {
                node.child_by_field_name("value")
                    .map(|value| element_type(&receiver_type(&value, text, scope, model)))
                    .unwrap_or(Ty::Unknown)
            } else {
                declared_type(node, text)
            };
            if let Some(name) = node.child_by_field_name("name") {
                scope.locals.push((text[name.byte_range()].to_string(), ty));
            }
        }
        // A try-with-resources binding, which declares a name exactly like a
        // local: `try (Widget w = open())` and the `var` form.
        "resource" if node.start_byte() < cursor_byte => {
            let ty = if is_var_binding(node, text) {
                node.child_by_field_name("value")
                    .map(|value| receiver_type(&value, text, scope, model))
                    .unwrap_or(Ty::Unknown)
            } else {
                declared_type(node, text)
            };
            if let Some(name) = node.child_by_field_name("name") {
                scope.locals.push((text[name.byte_range()].to_string(), ty));
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_locals(&child, text, cursor_byte, model, scope);
    }
}

/// True when a declaration-like node (a local, an enhanced-for binding, a
/// try-with-resources resource) is written with `var`.
fn is_var_binding(node: &Node, text: &str) -> bool {
    node.child_by_field_name("type")
        .is_some_and(|ty| &text[ty.byte_range()] == "var")
}

/// A declaration-like node's written type, or `Unknown` when it has none.
fn declared_type(node: &Node, text: &str) -> Ty {
    node.child_by_field_name("type")
        .map(|node| type_from_node(&node, text))
        .unwrap_or(Ty::Unknown)
}

/// The element type of an iterable an enhanced-for binding ranges over: an
/// array's element, or the single type argument of a reference type
/// (`List<Widget>` yields `Widget`). `Unknown` when there is no single answer.
pub fn element_type(ty: &Ty) -> Ty {
    match ty {
        Ty::Array(element) => (**element).clone(),
        Ty::Ref { args, .. } if args.len() == 1 => args[0].clone(),
        _ => Ty::Unknown,
    }
}

/// The directly declared fields of a type body (not nested types' fields).
fn collect_field_members(body: &Node, text: &str, scope: &mut Scope) {
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if !matches!(child.kind(), "field_declaration" | "constant_declaration") {
            continue;
        }
        let Some(ty) = child
            .child_by_field_name("type")
            .map(|node| type_from_node(&node, text))
        else {
            continue;
        };
        let is_static = has_modifier(&child, text, "static");
        let mut inner = child.walk();
        for declarator in child.named_children(&mut inner) {
            if declarator.kind() != "variable_declarator" {
                continue;
            }
            if let Some(name) = declarator.child_by_field_name("name") {
                scope.fields.push(Member {
                    name: text[name.byte_range()].to_string(),
                    kind: IndexKind::Field,
                    ty: ty.clone(),
                    params: Vec::new(),
                    type_params: Vec::new(),
                    is_static,
                });
            }
        }
    }
}

/// What a simple name resolves to at a position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The declared type of the symbol (or the type itself).
    pub ty: Ty,
    /// The member this name is, when it names a field or method.
    pub member: Option<Member>,
    /// True when the name denotes a type rather than a value.
    pub is_type: bool,
}

/// Resolves a simple name against the scope, in Java's precedence order:
/// locals/parameters, the enclosing type's fields (inherited included), then
/// type parameters, then a type in the model.
pub fn resolve_name(name: &str, scope: &Scope, model: &dyn TypeLookup) -> Option<Resolved> {
    if let Some((_, ty)) = scope.locals.iter().rev().find(|(local, _)| local == name) {
        return Some(Resolved {
            ty: ty.clone(),
            member: None,
            is_type: false,
        });
    }
    if let Some(field) = scope.fields.iter().find(|field| field.name == name) {
        return Some(Resolved {
            ty: field.ty.clone(),
            member: Some(field.clone()),
            is_type: false,
        });
    }
    if let Some(enclosing) = &scope.enclosing_type {
        if let Some(member) = member_of(
            &Ty::reference(enclosing.clone()),
            name,
            model,
            scope.package.as_deref(),
        ) {
            let ty = member.access_type();
            return Some(Resolved {
                ty,
                member: Some(member),
                is_type: false,
            });
        }
    }
    if scope.type_params.iter().any(|param| param == name) {
        return Some(Resolved {
            ty: Ty::Var(name.to_string()),
            member: None,
            is_type: true,
        });
    }
    let info = resolve_type_info(name, scope, model)?;
    Some(Resolved {
        ty: qualified_reference(info),
        member: None,
        is_type: true,
    })
}

/// The declared type a simple (or already-qualified) name denotes for this
/// compilation unit, in Java's resolution order: an exact single-type import,
/// then the file's own package, then a wildcard import's package, then a
/// unique model match. A qualified name is looked up in its own package.
fn resolve_type_info<'a>(
    name: &str,
    scope: &Scope,
    model: &'a dyn TypeLookup,
) -> Option<&'a TypeInfo> {
    let simple = name.rsplit('.').next().unwrap_or(name);
    if let Some((package, _)) = name.rsplit_once('.') {
        return model.find_in_package(simple, Some(package));
    }
    for import in scope
        .imports
        .iter()
        .filter(|import| !import.is_wildcard && !import.is_static)
    {
        if import.simple_name() == Some(simple) {
            // The name is bound to this import, so an unindexed target is
            // unresolved rather than a fall-through to some other type.
            return model.find_in_package(simple, import.package().as_deref());
        }
    }
    if let Some(info) = model.find_in_package(simple, scope.package.as_deref()) {
        return Some(info);
    }
    let mut wildcard = None;
    for import in scope
        .imports
        .iter()
        .filter(|import| import.is_wildcard && !import.is_static)
    {
        if let Some(info) = model.find_in_package(simple, import.package().as_deref()) {
            if wildcard.is_some() {
                // Two wildcard imports could supply the name: no single answer.
                return None;
            }
            wildcard = Some(info);
        }
    }
    wildcard.or_else(|| model.find_unique(simple, None))
}

/// `info`'s package-qualified name, or its simple name when it has no package.
fn qualified_name(info: &TypeInfo) -> String {
    match &info.package {
        Some(package) => format!("{package}.{}", info.name),
        None => info.name.clone(),
    }
}

/// A reference to `info`, dotted with its package so later member lookups can
/// disambiguate a simple name shared across packages.
fn qualified_reference(info: &TypeInfo) -> Ty {
    Ty::reference(qualified_name(info))
}

/// The member named `name` on `ty`, or `None` when there is no single answer.
/// Overloads that all share one return type are treated as one answer. When the
/// hierarchy walk finds nothing, `java.lang.Object`'s members are consulted, so
/// an inherited `toString`/`equals`/... resolves for typing and hover without
/// ever appearing in a `.`-completion listing (which reads the hierarchy walk).
pub fn member_of(
    ty: &Ty,
    name: &str,
    model: &dyn TypeLookup,
    package: Option<&str>,
) -> Option<Member> {
    let matches: Vec<Member> = model
        .members(ty, package)
        .into_iter()
        .filter(|member| member.name == name)
        .collect();
    if let Some(member) = single_member(matches) {
        return Some(member);
    }
    if inherits_object(ty) {
        return single_member(object_members(model, package, name));
    }
    None
}

/// The single answer among same-named members, or `None` when there is none or
/// they disagree on return type and kind.
fn single_member(matches: Vec<Member>) -> Option<Member> {
    match matches.len() {
        0 => None,
        1 => Some(matches[0].clone()),
        _ => {
            let first = &matches[0];
            matches
                .iter()
                .all(|member| member.ty == first.ty && member.kind == first.kind)
                .then(|| first.clone())
        }
    }
}

/// True for a receiver that has `java.lang.Object` as an implicit supertype, so
/// `Object`'s members resolve on it even though no type records the edge.
fn inherits_object(ty: &Ty) -> bool {
    matches!(ty, Ty::Ref { .. } | Ty::Var(_) | Ty::Array(_))
}

/// The model's `java.lang.Object`, if the model carries it. The qualified
/// lookup finds the JDK type; the simple-name fallback covers a model that keys
/// it without a package.
fn object_type<'a>(model: &'a dyn TypeLookup, package: Option<&str>) -> Option<&'a TypeInfo> {
    model
        .lookup(&Ty::reference("java.lang.Object"), None)
        .or_else(|| model.find_unique("Object", package))
}

/// `java.lang.Object`'s members named `name`, used only as a resolve-only
/// fallback so `.`-completion listings stay free of `toString`/`equals`/...
fn object_members(model: &dyn TypeLookup, package: Option<&str>, name: &str) -> Vec<Member> {
    object_type(model, package)
        .map(|info| {
            info.members()
                .filter(|member| member.name == name)
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// The member named `name` that a call with `arg_count` arguments most likely
/// targets: the overload whose parameter count matches, when there is a single
/// answer among those (or when every same-arity overload agrees on the return
/// type and kind, as [`member_of`] requires). Falls back to [`member_of`] when
/// no overload matches by arity, so a call that the layer cannot pin down keeps
/// its previous (name-only) answer.
///
/// Arity is used rather than argument types because `List.of`, for instance,
/// declares one overload per count, and [`member_of`] would otherwise pick the
/// parameterless one and lose the type parameters the call could bind.
pub fn member_for_call(
    ty: &Ty,
    name: &str,
    arg_count: usize,
    model: &dyn TypeLookup,
    package: Option<&str>,
) -> Option<Member> {
    // The receiver type's own declaration is read directly: the hierarchy walk
    // in `members` collapses same-named members to the first, which would hide
    // a type's own overloads. Inherited names fall back to that walk.
    let mut candidates: Vec<Member> = match model.lookup(ty, package) {
        Some(info) => info
            .methods
            .iter()
            .filter(|member| member.name == name)
            .cloned()
            .collect(),
        None => Vec::new(),
    };
    if candidates.is_empty() {
        candidates = model
            .members(ty, package)
            .into_iter()
            .filter(|member| member.name == name && member.kind == IndexKind::Method)
            .collect();
    }
    // Inherited `java.lang.Object` methods are resolve-only, so they are not in
    // the hierarchy walk; consult them when nothing else matches.
    if candidates.is_empty() && inherits_object(ty) {
        candidates = object_members(model, package, name)
            .into_iter()
            .filter(|member| member.kind == IndexKind::Method)
            .collect();
    }
    let matching: Vec<Member> = candidates
        .into_iter()
        .filter(|member| member.params.len() == arg_count)
        .collect();
    match matching.len() {
        0 => member_of(ty, name, model, package),
        1 => matching.into_iter().next(),
        _ => {
            let first = &matching[0];
            matching
                .iter()
                .all(|member| member.ty == first.ty && member.kind == first.kind)
                .then(|| first.clone())
        }
    }
}

/// True when a value of type `from` is accepted where `to` is expected, by the
/// conversions Java applies at a call — a conservative approximation covering
/// identity, primitive widening, boxing/unboxing, `null`, subtyping through the
/// model's hierarchy, arrays, and generics by erasure. Anything undecidable (an
/// `Unknown` on either side) is `false`, so an unconfirmable candidate is never
/// preferred over a determinate one.
pub fn assignable(from: &Ty, to: &Ty, model: &dyn TypeLookup, package: Option<&str>) -> bool {
    if from == to {
        return true;
    }
    // A type-variable parameter accepts any argument (its bound is not
    // modelled).
    if matches!(to, Ty::Var(_)) {
        return true;
    }
    if from == &Ty::Unknown || to == &Ty::Unknown {
        return false;
    }
    if from == &Ty::Null {
        return matches!(to, Ty::Ref { .. } | Ty::Array(_) | Ty::Var(_));
    }
    match (from, to) {
        (Ty::Prim(a), Ty::Prim(b)) => widens(*a, *b),
        (Ty::Prim(a), Ty::Ref { name, .. }) => boxed_name(*a) == simple(name),
        (Ty::Ref { name, .. }, Ty::Prim(b)) => {
            unboxed_prim(name).is_some_and(|prim| prim == *b || widens(prim, *b))
        }
        (Ty::Ref { .. }, Ty::Ref { .. }) => ref_assignable(from, to, model, package),
        (Ty::Array(element), Ty::Array(target)) => assignable(element, target, model, package),
        (Ty::Array(_), Ty::Ref { name, .. }) => {
            matches!(simple(name), "Object" | "Cloneable" | "Serializable")
        }
        _ => false,
    }
}

/// A primitive's widening conversions (its own type is handled by equality).
fn widens(from: Prim, to: Prim) -> bool {
    use Prim::*;
    match (from, to) {
        (Byte, Short | Int | Long | Float | Double) => true,
        (Short, Int | Long | Float | Double) => true,
        (Char, Int | Long | Float | Double) => true,
        (Int, Long | Float | Double) => true,
        (Long, Float | Double) => true,
        (Float, Double) => true,
        _ => false,
    }
}

/// The wrapper class a primitive boxes into.
fn boxed_name(prim: Prim) -> &'static str {
    match prim {
        Prim::Boolean => "Boolean",
        Prim::Byte => "Byte",
        Prim::Short => "Short",
        Prim::Int => "Integer",
        Prim::Long => "Long",
        Prim::Char => "Character",
        Prim::Float => "Float",
        Prim::Double => "Double",
    }
}

/// The primitive a wrapper class unboxes to.
fn unboxed_prim(name: &str) -> Option<Prim> {
    Some(match simple(name) {
        "Boolean" => Prim::Boolean,
        "Byte" => Prim::Byte,
        "Short" => Prim::Short,
        "Integer" => Prim::Int,
        "Long" => Prim::Long,
        "Character" => Prim::Char,
        "Float" => Prim::Float,
        "Double" => Prim::Double,
        _ => return None,
    })
}

/// A type name's simple (last) segment.
fn simple(name: &str) -> &str {
    name.rsplit(['.', '$']).next().unwrap_or(name)
}

/// Reference-to-reference assignability: erasure (same simple name), then the
/// implicit `Object`, then a subtype walk over the model's hierarchy.
fn ref_assignable(from: &Ty, to: &Ty, model: &dyn TypeLookup, package: Option<&str>) -> bool {
    let (
        Ty::Ref {
            name: from_name, ..
        },
        Ty::Ref { name: to_name, .. },
    ) = (from, to)
    else {
        return false;
    };
    let to_simple = simple(to_name);
    if to_simple == "Object" || simple(from_name) == to_simple {
        return true;
    }
    subtype_of(from, to_simple, model, package)
}

/// Whether `from` names `target` or a subtype of it, walking the model's
/// supertypes breadth-first and cycle-guarded.
fn subtype_of(from: &Ty, target: &str, model: &dyn TypeLookup, package: Option<&str>) -> bool {
    let Some(root) = model.lookup(from, package) else {
        return false;
    };
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, Option<String>)> = VecDeque::new();
    queue.push_back((root.name.clone(), root.package.clone()));
    while let Some((name, context)) = queue.pop_front() {
        if name == target {
            return true;
        }
        if !visited.insert(name.clone()) {
            continue;
        }
        let Some(info) = model.find_unique(&name, context.as_deref()) else {
            continue;
        };
        let super_context = info.package.clone().or(context);
        for supertype in &info.supertypes {
            if let Some(super_simple) = supertype.simple_name() {
                if super_simple == target {
                    return true;
                }
            }
            if supertype.qualified_package().is_some() {
                if let Some(resolved) = model.lookup(supertype, None) {
                    queue.push_back((resolved.name.clone(), resolved.package.clone()));
                }
            } else if let Some(super_simple) = supertype.simple_name() {
                queue.push_back((super_simple.to_string(), super_context.clone()));
            }
        }
    }
    false
}

/// The same-named methods of `ty` — its own declarations first, then inherited
/// ones, then `java.lang.Object`'s — the candidate set a call-site lookup
/// narrows.
fn call_candidates(
    ty: &Ty,
    name: &str,
    model: &dyn TypeLookup,
    package: Option<&str>,
) -> Vec<Member> {
    let mut candidates: Vec<Member> = match model.lookup(ty, package) {
        Some(info) => info
            .methods
            .iter()
            .filter(|member| member.name == name)
            .cloned()
            .collect(),
        None => Vec::new(),
    };
    if candidates.is_empty() {
        candidates = model
            .members(ty, package)
            .into_iter()
            .filter(|member| member.name == name && member.kind == IndexKind::Method)
            .collect();
    }
    if candidates.is_empty() && inherits_object(ty) {
        candidates = object_members(model, package, name)
            .into_iter()
            .filter(|member| member.kind == IndexKind::Method)
            .collect();
    }
    candidates
}

/// The overload of `name` a call with these argument types selects, but only
/// when the answer is unambiguous: a single type-applicable candidate, or a
/// single same-arity candidate. Several same-arity candidates whose argument
/// types are inconclusive yield `None`, so a caller that must not name the
/// wrong overload (an inlay hint) can refuse. [`member_for_arguments`] adds the
/// looser arity fallback for navigation.
pub fn member_for_arguments_confirmed(
    ty: &Ty,
    name: &str,
    args: &[Ty],
    model: &dyn TypeLookup,
    package: Option<&str>,
) -> Option<Member> {
    let arity: Vec<Member> = call_candidates(ty, name, model, package)
        .into_iter()
        .filter(|member| member.params.len() == args.len())
        .collect();
    if arity.is_empty() {
        return None;
    }
    let applicable: Vec<Member> = arity
        .iter()
        .filter(|member| {
            member
                .params
                .iter()
                .zip(args)
                .all(|(param, arg)| assignable(arg, &param.ty, model, package))
        })
        .cloned()
        .collect();
    match applicable.len() {
        0 if arity.len() == 1 => arity.into_iter().next(),
        0 => None,
        1 => applicable.into_iter().next(),
        _ => most_specific(&applicable, model, package),
    }
}

/// The overload of `name` a call with these argument types most likely targets:
/// the confirmed answer, or — when the types are inconclusive — the arity-level
/// [`member_for_call`] (and thus the name-only [`member_of`] when the arity
/// matches nothing either). Navigation accepts this looser fallback; hints do
/// not.
pub fn member_for_arguments(
    ty: &Ty,
    name: &str,
    args: &[Ty],
    model: &dyn TypeLookup,
    package: Option<&str>,
) -> Option<Member> {
    member_for_arguments_confirmed(ty, name, args, model, package)
        .or_else(|| member_for_call(ty, name, args.len(), model, package))
}

/// The single applicable overload more specific than every other — its
/// parameters are assignable to each rival's. `None` when two incomparable
/// overloads tie, or when two share the same parameters.
fn most_specific(
    applicable: &[Member],
    model: &dyn TypeLookup,
    package: Option<&str>,
) -> Option<Member> {
    let mut best: Option<&Member> = None;
    for candidate in applicable {
        let more_specific = applicable.iter().all(|other| {
            candidate.params.len() == other.params.len()
                && candidate
                    .params
                    .iter()
                    .zip(&other.params)
                    .all(|(mine, theirs)| assignable(&mine.ty, &theirs.ty, model, package))
        });
        if !more_specific {
            continue;
        }
        match best {
            None => best = Some(candidate),
            Some(existing)
                if existing
                    .params
                    .iter()
                    .map(|p| &p.ty)
                    .eq(candidate.params.iter().map(|p| &p.ty)) => {}
            Some(_) => return None,
        }
    }
    best.cloned()
}

/// The argument expressions of a call node, comments excluded.
fn call_arguments<'a>(node: &Node<'a>) -> Vec<Node<'a>> {
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return Vec::new();
    };
    let mut cursor = arguments.walk();
    arguments
        .named_children(&mut cursor)
        .filter(|child| !matches!(child.kind(), "line_comment" | "block_comment"))
        .collect()
}

/// Binds a method's own type parameters from the types of its arguments.
fn bind_method_params(
    member: &Member,
    arguments: &[Node<'_>],
    text: &str,
    scope: &Scope,
    model: &dyn TypeLookup,
    bindings: &mut HashMap<String, Ty>,
) {
    if member.type_params.is_empty() {
        return;
    }
    for (param, argument) in member.params.iter().zip(arguments) {
        let actual = receiver_type(argument, text, scope, model);
        bind_pattern(&param.ty, &actual, &member.type_params, bindings);
    }
}

/// Unifies a declared parameter type (the pattern) with the type of the actual
/// argument, binding any type parameter in `params` to the boxed argument type.
fn bind_pattern(pattern: &Ty, actual: &Ty, params: &[String], bindings: &mut HashMap<String, Ty>) {
    match pattern {
        Ty::Ref { name, args } => {
            if args.is_empty() && params.iter().any(|param| param == name) {
                bindings
                    .entry(name.clone())
                    .or_insert_with(|| boxed(actual.clone()));
                return;
            }
            if let Ty::Ref {
                args: actual_args, ..
            } = actual
            {
                if args.len() == actual_args.len() {
                    for (pattern_arg, actual_arg) in args.iter().zip(actual_args) {
                        bind_pattern(pattern_arg, actual_arg, params, bindings);
                    }
                }
            }
        }
        Ty::Array(element) => {
            if let Ty::Array(actual_element) = actual {
                bind_pattern(element, actual_element, params, bindings);
            }
        }
        _ => {}
    }
}

/// The boxed reference type of a primitive, as Java's inference does when a
/// primitive argument binds a type variable (`List.of(3)` is `List<Integer>`).
fn boxed(ty: Ty) -> Ty {
    let name = match ty {
        Ty::Prim(Prim::Boolean) => "Boolean",
        Ty::Prim(Prim::Byte) => "Byte",
        Ty::Prim(Prim::Short) => "Short",
        Ty::Prim(Prim::Int) => "Integer",
        Ty::Prim(Prim::Long) => "Long",
        Ty::Prim(Prim::Char) => "Character",
        Ty::Prim(Prim::Float) => "Float",
        Ty::Prim(Prim::Double) => "Double",
        other => return other,
    };
    Ty::reference(format!("java.lang.{name}"))
}

/// Bindings for a receiver type's own parameters, taken from its arguments.
/// Empty unless `name` is declared by the receiver's type itself: an inherited
/// member's parameters belong to a supertype whose arguments this layer does not
/// map onto the receiver.
fn receiver_bindings(
    receiver: &Ty,
    name: &str,
    model: &dyn TypeLookup,
    package: Option<&str>,
) -> HashMap<String, Ty> {
    let mut bindings = HashMap::new();
    let Ty::Ref { args, .. } = receiver else {
        return bindings;
    };
    if args.is_empty() {
        return bindings;
    }
    let Some(info) = model.lookup(receiver, package) else {
        return bindings;
    };
    if info.parameters.len() != args.len() {
        return bindings;
    }
    match member_owner(receiver, name, model, package) {
        Some(owner) if owner.name == info.name && owner.package == info.package => {}
        _ => return bindings,
    }
    for (param, arg) in info.parameters.iter().zip(args) {
        bindings.insert(param.clone(), arg.clone());
    }
    bindings
}

/// The type that declares a member: its simple name and package, enough to tell
/// two same-named types in different packages apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    pub name: String,
    pub package: Option<String>,
}

/// The type that declares `name` for `ty`, searching `ty` and then its
/// supertypes breadth-first, so an inherited member resolves to the type that
/// actually declares it (nearest declaration wins). `None` when no type in the
/// chain declares the name.
pub fn member_owner(
    ty: &Ty,
    name: &str,
    model: &dyn TypeLookup,
    package: Option<&str>,
) -> Option<Owner> {
    let root = model.lookup(ty, package)?;
    let root_name = root.name.clone();
    let root_context = root.package.clone();
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, Option<String>)> = VecDeque::new();
    queue.push_back((root_name, root_context));
    while let Some((type_name, context)) = queue.pop_front() {
        if !visited.insert(type_name.clone()) {
            continue;
        }
        let Some(info) = model.find_unique(&type_name, context.as_deref()) else {
            continue;
        };
        if info
            .fields
            .iter()
            .chain(info.methods.iter())
            .any(|member| member.name == name)
        {
            return Some(Owner {
                name: info.name.clone(),
                package: info.package.clone(),
            });
        }
        let super_context = info.package.clone().or(context);
        for supertype in &info.supertypes {
            if supertype.qualified_package().is_some() {
                if let Some(resolved) = model.lookup(supertype, None) {
                    queue.push_back((resolved.name.clone(), resolved.package.clone()));
                }
            } else if let Some(super_name) = supertype.simple_name() {
                queue.push_back((super_name.to_string(), super_context.clone()));
            }
        }
    }
    None
}

/// Infers the type of the expression a `.` is applied to. Every unsupported
/// shape is [`Ty::Unknown`], which callers treat as "no answer".
/// Resolves a receiver's own type name against the compilation unit, so member
/// access on a local declared with an imported simple name (e.g.
/// `List<String> x`) reaches the right type. An already-qualified or
/// unresolvable reference is returned unchanged.
fn qualify(ty: Ty, scope: &Scope, model: &dyn TypeLookup) -> Ty {
    if ty.qualified_package().is_some() {
        return ty;
    }
    match ty {
        Ty::Ref { name, args } => match resolve_type_info(&name, scope, model) {
            Some(info) => Ty::Ref {
                name: qualified_name(info),
                args,
            },
            None => Ty::Ref { name, args },
        },
        other => other,
    }
}

pub fn receiver_type(node: &Node, text: &str, scope: &Scope, model: &dyn TypeLookup) -> Ty {
    qualify(
        receiver_type_unqualified(node, text, scope, model),
        scope,
        model,
    )
}

/// The receiver type before its own name is resolved against the compilation
/// unit; [`receiver_type`] qualifies the result so a member lookup can
/// disambiguate an imported simple name.
fn receiver_type_unqualified(node: &Node, text: &str, scope: &Scope, model: &dyn TypeLookup) -> Ty {
    match node.kind() {
        "identifier" | "type_identifier" => resolve_name(&text[node.byte_range()], scope, model)
            .map(|resolved| resolved.ty)
            .unwrap_or(Ty::Unknown),
        // A receiver the parser read as a dotted type name — an incomplete
        // `receiver.` can end up this way — is a member chain.
        "scoped_identifier" | "scoped_type_identifier" => {
            let Some(scope_node) = node
                .child_by_field_name("scope")
                .or_else(|| node.named_child(0))
            else {
                return Ty::Unknown;
            };
            let base = receiver_type(&scope_node, text, scope, model);
            let Some(name_node) = node
                .child_by_field_name("name")
                .or_else(|| node.named_child(node.named_child_count().saturating_sub(1) as u32))
            else {
                return Ty::Unknown;
            };
            let name = &text[name_node.byte_range()];
            let Some(member) = member_of(&base, name, model, scope.package.as_deref()) else {
                return Ty::Unknown;
            };
            let bindings = receiver_bindings(&base, name, model, scope.package.as_deref());
            member.access_type().substitute(&bindings)
        }
        "this" => scope
            .enclosing_type
            .clone()
            .map(Ty::reference)
            .unwrap_or(Ty::Unknown),
        "super" => {
            let Some(enclosing) = &scope.enclosing_type else {
                return Ty::Unknown;
            };
            model
                .lookup(&Ty::reference(enclosing.clone()), scope.package.as_deref())
                .and_then(|info| info.supertypes.first().cloned())
                .unwrap_or(Ty::Unknown)
        }
        "object_creation_expression" => node
            .child_by_field_name("type")
            .map(|node| type_from_node(&node, text))
            .unwrap_or(Ty::Unknown),
        "field_access" => {
            let Some(object) = node.child_by_field_name("object") else {
                return Ty::Unknown;
            };
            let base = receiver_type(&object, text, scope, model);
            let Some(field) = node.child_by_field_name("field") else {
                return Ty::Unknown;
            };
            let name = &text[field.byte_range()];
            let Some(member) = member_of(&base, name, model, scope.package.as_deref()) else {
                return Ty::Unknown;
            };
            let bindings = receiver_bindings(&base, name, model, scope.package.as_deref());
            member.access_type().substitute(&bindings)
        }
        "method_invocation" => {
            let Some(name) = node.child_by_field_name("name") else {
                return Ty::Unknown;
            };
            let name = &text[name.byte_range()];
            let base = match node.child_by_field_name("object") {
                Some(object) => receiver_type(&object, text, scope, model),
                None => scope
                    .enclosing_type
                    .clone()
                    .map(Ty::reference)
                    .unwrap_or(Ty::Unknown),
            };
            let arguments = call_arguments(node);
            let Some(member) = member_for_call(
                &base,
                name,
                arguments.len(),
                model,
                scope.package.as_deref(),
            ) else {
                return Ty::Unknown;
            };
            let mut bindings = receiver_bindings(&base, name, model, scope.package.as_deref());
            bind_method_params(&member, &arguments, text, scope, model, &mut bindings);
            member.access_type().substitute(&bindings)
        }
        "parenthesized_expression" => node
            .named_child(0)
            .map(|inner| receiver_type(&inner, text, scope, model))
            .unwrap_or(Ty::Unknown),
        "cast_expression" => node
            .child_by_field_name("type")
            .map(|node| type_from_node(&node, text))
            .unwrap_or(Ty::Unknown),
        "array_access" => match node.child_by_field_name("array") {
            Some(array) => match receiver_type(&array, text, scope, model) {
                Ty::Array(element) => *element,
                _ => Ty::Unknown,
            },
            None => Ty::Unknown,
        },
        "string_literal" => Ty::reference("String"),
        "decimal_integer_literal"
        | "hex_integer_literal"
        | "octal_integer_literal"
        | "binary_integer_literal" => integer_literal_type(node, text),
        "decimal_floating_point_literal" | "hex_floating_point_literal" => {
            floating_literal_type(node, text)
        }
        "character_literal" => Ty::Prim(Prim::Char),
        "true" | "false" => Ty::Prim(Prim::Boolean),
        "null_literal" => Ty::Null,
        // A conditional expression takes its branches' common type; a lone
        // `null` yields the other branch.
        "ternary_expression" => {
            let consequence = node
                .child_by_field_name("consequence")
                .map(|node| receiver_type(&node, text, scope, model))
                .unwrap_or(Ty::Unknown);
            let alternative = node
                .child_by_field_name("alternative")
                .map(|node| receiver_type(&node, text, scope, model))
                .unwrap_or(Ty::Unknown);
            unify(consequence, alternative)
        }
        "array_creation_expression" => node
            .child_by_field_name("type")
            .map(|node| Ty::Array(Box::new(type_from_node(&node, text))))
            .unwrap_or(Ty::Unknown),
        "instanceof_expression" => Ty::Prim(Prim::Boolean),
        "switch_expression" => switch_result_type(node, text, scope, model),
        _ => Ty::Unknown,
    }
}

/// The single type two branches agree on: their common type, a lone `null`
/// yielding the other branch, or `Unknown` when there is no single answer. A
/// lambda has no target type here, so it too stays `Unknown`.
fn unify(left: Ty, right: Ty) -> Ty {
    if left == right {
        return left;
    }
    match (left, right) {
        (Ty::Null, other) | (other, Ty::Null) if other != Ty::Unknown => other,
        _ => Ty::Unknown,
    }
}

/// The type a `switch` expression yields: the unification of every result
/// expression — an arrow rule's expression or `block`, or a `yield`ed value.
/// `Unknown` unless they all agree.
fn switch_result_type(node: &Node, text: &str, scope: &Scope, model: &dyn TypeLookup) -> Ty {
    let Some(body) = node.child_by_field_name("body") else {
        return Ty::Unknown;
    };
    let mut results = Vec::new();
    collect_switch_results(&body, &mut results);
    let mut result: Option<Ty> = None;
    for value in results {
        let ty = receiver_type(&value, text, scope, model);
        result = Some(match result {
            Some(previous) => unify(previous, ty),
            None => ty,
        });
    }
    result.unwrap_or(Ty::Unknown)
}

/// Collects a `switch` block's result expressions: an arrow rule's expression
/// body and every `yield`ed value, descending into blocks.
fn collect_switch_results<'a>(node: &Node<'a>, out: &mut Vec<Node<'a>>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "switch_rule" => {
                let mut inner = child.walk();
                for part in child.named_children(&mut inner) {
                    match part.kind() {
                        "expression_statement" => {
                            if let Some(value) = part.named_child(0) {
                                out.push(value);
                            }
                        }
                        "block" => collect_switch_results(&part, out),
                        _ => {}
                    }
                }
            }
            "switch_block_statement_group" | "block" => collect_switch_results(&child, out),
            "yield_statement" => {
                if let Some(value) = child.named_child(0) {
                    out.push(value);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::java_parser;

    fn model_of(text: &str) -> TypeModel {
        let mut parser = java_parser();
        let tree = parser.parse(text.as_bytes(), None).expect("parse");
        let mut model = TypeModel::new();
        model.extend(collect_type_infos(None, &tree, text));
        model
    }

    #[test]
    fn descriptors_parse_fields_and_methods() {
        assert_eq!(Ty::from_descriptor("I"), Ty::Prim(Prim::Int));
        assert_eq!(
            Ty::from_descriptor("Ljava/lang/String;"),
            Ty::reference("java.lang.String")
        );
        assert_eq!(
            Ty::from_descriptor("[[D"),
            Ty::Array(Box::new(Ty::Array(Box::new(Ty::Prim(Prim::Double)))))
        );
        let (params, ret) = Ty::method_from_descriptor("(ILjava/lang/String;)Z");
        assert_eq!(
            params,
            vec![Ty::Prim(Prim::Int), Ty::reference("java.lang.String")]
        );
        assert_eq!(ret, Ty::Prim(Prim::Boolean));
        let (none, void) = Ty::method_from_descriptor("()V");
        assert!(none.is_empty());
        assert_eq!(void, Ty::Void);
    }

    #[test]
    fn types_render_in_source_form() {
        assert_eq!(Ty::Prim(Prim::Int).display(), "int");
        assert_eq!(Ty::reference("java.lang.String").display(), "String");
        let list = Ty::Ref {
            name: "List".to_string(),
            args: vec![Ty::reference("String")],
        };
        assert_eq!(list.display(), "List<String>");
        assert_eq!(Ty::Array(Box::new(Ty::Prim(Prim::Int))).display(), "int[]");
    }

    #[test]
    fn source_types_carry_members_and_supertypes() {
        let text = "\
package demo;

class Base {
    int base;
    void run() {}
}

class Widget extends Base implements Runnable {
    static String label;
    private int size;

    public int getSize(int extra) {
        return size;
    }
}
";
        let mut parser = java_parser();
        let tree = parser.parse(text.as_bytes(), None).unwrap();
        let mut model = TypeModel::new();
        model.extend(collect_type_infos(Some("demo"), &tree, text));

        let widget = model.find_unique("Widget", Some("demo")).expect("Widget");
        let field_names: Vec<&str> = widget.fields.iter().map(|m| m.name.as_str()).collect();
        assert!(field_names.contains(&"size"), "{field_names:?}");
        assert!(field_names.contains(&"label"));
        assert!(widget.supertypes.contains(&Ty::reference("Base")));
        assert!(widget.supertypes.contains(&Ty::reference("Runnable")));
        let get_size = widget
            .methods
            .iter()
            .find(|m| m.name == "getSize")
            .expect("getSize");
        assert_eq!(get_size.ty, Ty::Prim(Prim::Int));
        assert_eq!(
            get_size.params,
            vec![Param {
                name: Some("extra".to_string()),
                ty: Ty::Prim(Prim::Int),
            }]
        );
        assert_eq!(get_size.display(), "int getSize(int extra)");
    }

    #[test]
    fn record_components_become_accessor_members() {
        let model = model_of("record Point(int x, int y) {}\n");
        let point = model.find_unique("Point", None).expect("Point");
        assert_eq!(point.kind, IndexKind::Record);

        // To a client a component is the accessor `x()`: a method with the
        // component's type and no parameters, and no private backing field.
        let members = model.members(&Ty::reference("Point"), None);
        let x = members.iter().find(|member| member.name == "x").expect("x");
        assert_eq!(x.kind, IndexKind::Method);
        assert_eq!(x.ty, Ty::Prim(Prim::Int));
        assert!(x.params.is_empty());
        assert_eq!(x.display(), "int x()");
        assert!(members.iter().any(|member| member.name == "y"));
        assert!(!point.fields.iter().any(|field| field.name == "x"));
    }

    #[test]
    fn a_record_without_components_adds_no_members() {
        let model = model_of("record Empty() {}\nrecord Marker() {\n    void run() {}\n}\n");
        assert!(model.members(&Ty::reference("Empty"), None).is_empty());

        // A record's explicitly declared methods are still offered as before.
        let members = model.members(&Ty::reference("Marker"), None);
        let names: Vec<&str> = members.iter().map(|member| member.name.as_str()).collect();
        assert_eq!(names, ["run"], "{names:?}");
    }

    #[test]
    fn a_bare_component_resolves_inside_the_record() {
        let text = "\
record Point(int x, int y) {
    int sum() {
        return x + y;
    }
}
";
        let tree = parse(text);
        let node = node_in(&tree, text, "return x");
        let mut model = TypeModel::new();
        model.extend(collect_type_infos(None, &tree, text));
        let scope = scope_at(node, text, &tree, &model);

        let resolved = resolve_name("x", &scope, &model).expect("the component should resolve");
        assert!(!resolved.is_type);
        assert_eq!(resolved.ty, Ty::Prim(Prim::Int));
        assert_eq!(
            resolved.member.map(|member| member.kind),
            Some(IndexKind::Field)
        );
    }

    #[test]
    fn assignable_covers_the_common_conversions() {
        let mut model = TypeModel::new();
        model.insert(TypeInfo::new("Base".to_string(), None, IndexKind::Class));
        let mut derived = TypeInfo::new("Derived".to_string(), None, IndexKind::Class);
        derived.supertypes.push(Ty::reference("Base"));
        model.insert(derived);
        let int = Ty::Prim(Prim::Int);
        let long = Ty::Prim(Prim::Long);
        assert!(assignable(&int, &long, &model, None), "int widens to long");
        assert!(
            !assignable(&long, &int, &model, None),
            "long does not narrow"
        );
        assert!(
            assignable(&int, &Ty::reference("Integer"), &model, None),
            "boxing"
        );
        assert!(
            assignable(&Ty::reference("Integer"), &int, &model, None),
            "unboxing"
        );
        assert!(
            assignable(&Ty::Null, &Ty::reference("Base"), &model, None),
            "null to reference"
        );
        assert!(
            assignable(
                &Ty::reference("Derived"),
                &Ty::reference("Base"),
                &model,
                None
            ),
            "subtype"
        );
        assert!(
            !assignable(
                &Ty::reference("Base"),
                &Ty::reference("Derived"),
                &model,
                None
            ),
            "supertype is not assignable to subtype"
        );
        assert!(
            !assignable(&Ty::Unknown, &int, &model, None),
            "an unknown argument type is not confirmed"
        );
    }

    #[test]
    fn member_for_arguments_prefers_the_type_matching_overload() {
        let mut model = TypeModel::new();
        let mut calc = TypeInfo::new("Calc".to_string(), None, IndexKind::Class);
        calc.methods.push(Member {
            name: "add".to_string(),
            kind: IndexKind::Method,
            ty: Ty::Prim(Prim::Int),
            params: vec![Param::unnamed(Ty::Prim(Prim::Int))],
            type_params: Vec::new(),
            is_static: false,
        });
        calc.methods.push(Member {
            name: "add".to_string(),
            kind: IndexKind::Method,
            ty: Ty::reference("String"),
            params: vec![Param::unnamed(Ty::reference("String"))],
            type_params: Vec::new(),
            is_static: false,
        });
        model.insert(calc);
        let ty = Ty::reference("Calc");

        let picked = member_for_arguments(&ty, "add", &[Ty::Prim(Prim::Int)], &model, None)
            .expect("the int overload");
        assert_eq!(picked.ty, Ty::Prim(Prim::Int));
        let picked = member_for_arguments(&ty, "add", &[Ty::reference("String")], &model, None)
            .expect("the String overload");
        assert_eq!(picked.ty, Ty::reference("String"));

        // An unknown argument type cannot be confirmed, so arity alone decides;
        // two same-arity overloads disagreeing on return type refuse.
        assert!(member_for_arguments(&ty, "add", &[Ty::Unknown], &model, None).is_none());
    }

    #[test]
    fn floating_literals_take_their_suffix_type() {
        let text = "class Sample { void m() { float a = 2.5f; double b = 3.5; } }";
        let tree = parse(text);
        let model = TypeModel::new();
        let scope = Scope::default();
        let float_literal = node_in(&tree, text, "2.5f");
        let double_literal = node_in(&tree, text, "3.5");
        assert_eq!(
            receiver_type(&float_literal, text, &scope, &model),
            Ty::Prim(Prim::Float)
        );
        assert_eq!(
            receiver_type(&double_literal, text, &scope, &model),
            Ty::Prim(Prim::Double)
        );
    }

    #[test]
    fn confirmed_selection_refuses_an_unpinned_overload() {
        let mut model = TypeModel::new();
        let mut calc = TypeInfo::new("Calc".to_string(), None, IndexKind::Class);
        for param in [Ty::Prim(Prim::Int), Ty::Prim(Prim::Double)] {
            calc.methods.push(Member {
                name: "add".to_string(),
                kind: IndexKind::Method,
                ty: Ty::Void,
                params: vec![Param::unnamed(param)],
                type_params: Vec::new(),
                is_static: false,
            });
        }
        model.insert(calc);
        let ty = Ty::reference("Calc");

        // A float widens to double, so `add(double)` is the single answer.
        let picked =
            member_for_arguments_confirmed(&ty, "add", &[Ty::Prim(Prim::Float)], &model, None)
                .expect("float widens to double");
        assert_eq!(picked.ty, Ty::Void);
        assert_eq!(picked.params[0].ty, Ty::Prim(Prim::Double));

        // An unknown argument leaves two same-arity candidates inconclusive:
        // nothing is confirmed, though navigation's looser lookup still guesses.
        assert!(member_for_arguments_confirmed(&ty, "add", &[Ty::Unknown], &model, None).is_none());
        assert!(member_for_arguments(&ty, "add", &[Ty::Unknown], &model, None).is_some());
    }

    #[test]
    fn members_with_overloads_keeps_overloads_apart() {
        let text = "\
class Calc {
    int add(int a) { return a; }
    int add(int a, int b) { return a + b; }
}
";
        let model = model_of(text);
        let ty = Ty::reference("Calc");
        let overloads = model.members_with_overloads(&ty, None);
        assert_eq!(overloads.len(), 2, "{overloads:?}");
        assert_eq!(
            model.members(&ty, None).len(),
            1,
            "members still collapses a name to one member"
        );
    }

    #[test]
    fn members_include_inherited_ones() {
        let text = "\
class Base {
    int base;
    void run() {}
}

class Widget extends Base {
    int own;
}
";
        let model = model_of(text);
        let members = model.members(&Ty::reference("Widget"), None);
        let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"own"), "{names:?}");
        assert!(
            names.contains(&"base"),
            "inherited field missing: {names:?}"
        );
        assert!(
            names.contains(&"run"),
            "inherited method missing: {names:?}"
        );
    }

    #[test]
    fn inheritance_cycles_terminate() {
        let model = model_of("class A extends B {}\nclass B extends A {}\n");
        let members = model.members(&Ty::reference("A"), None);
        assert!(members.is_empty());
    }

    #[test]
    fn from_entries_builds_name_only_types() {
        let entries = vec![
            entry("Lib", IndexKind::Class, None, &[]),
            entry("getName", IndexKind::Method, None, &["Lib"]),
            entry("count", IndexKind::Field, None, &["Lib"]),
        ];
        let model = TypeModel::from_entries(&entries);
        let lib = model.find_unique("Lib", None).expect("Lib");
        assert_eq!(lib.methods.len(), 1);
        assert_eq!(lib.fields.len(), 1);
        assert_eq!(lib.methods[0].name, "getName");
        assert_eq!(lib.methods[0].ty, Ty::Unknown);
    }

    #[test]
    fn find_unique_prefers_the_context_package() {
        let mut model = TypeModel::new();
        model.insert(TypeInfo::new(
            "Thing".to_string(),
            Some("a".to_string()),
            IndexKind::Class,
        ));
        model.insert(TypeInfo::new(
            "Thing".to_string(),
            Some("b".to_string()),
            IndexKind::Class,
        ));
        assert!(model.find_unique("Thing", None).is_none());
        assert_eq!(
            model.find_unique("Thing", Some("a")).map(|t| &t.package),
            Some(&Some("a".to_string()))
        );
    }

    #[test]
    fn scope_collects_locals_parameters_and_fields() {
        let text = "\
class Widget {
    private String label;
    void run(int count) {
        String local = null;
        // cursor
    }
}
";
        let tree = parse(text);
        let node = node_in(&tree, text, "// cursor");
        let scope = scope_at(node, text, &tree, &TypeModel::new());
        assert_eq!(scope.enclosing_type.as_deref(), Some("Widget"));
        assert_eq!(type_of_local(&scope, "count"), Some(Ty::Prim(Prim::Int)));
        assert_eq!(
            type_of_local(&scope, "local"),
            Some(Ty::reference("String"))
        );
        assert_eq!(
            type_of_local(&scope, "label"),
            Some(Ty::reference("String"))
        );
    }

    #[test]
    fn binding_resolves_names_in_precedence_order() {
        let text = "\
class Widget {
    Widget self_field;
    void run(Widget self_field) {
        // cursor
    }
}
";
        let tree = parse(text);
        let node = node_in(&tree, text, "// cursor");
        let model = TypeModel::from_entries(&[]);
        let scope = scope_at(node, text, &tree, &model);
        // The parameter shadows the field, but both have the same type here.
        let resolved = resolve_name("self_field", &scope, &model).expect("self_field");
        assert_eq!(resolved.ty, Ty::reference("Widget"));
        assert!(!resolved.is_type);
    }

    #[test]
    fn receiver_type_follows_new_this_and_chains() {
        let text = "\
class Base {
    void inherited() {}
}
class Widget extends Base {
    Base base;
    void m() {
        this.base.inherited();
    }
}
";
        let tree = parse(text);
        let node = node_in(&tree, text, "this.base");
        let access = ancestor_of_kind(node, "field_access").expect("field_access");
        let this_node = access.child_by_field_name("object").expect("this");
        assert_eq!(this_node.kind(), "this");
        let mut model = TypeModel::new();
        model.extend(collect_type_infos(None, &tree, text));
        let scope = scope_at(this_node, text, &tree, &model);

        assert_eq!(
            receiver_type(&this_node, text, &scope, &model),
            Ty::reference("Widget")
        );

        // `this.base` is a Base (a field), and `inherited` comes from Base.
        assert_eq!(
            receiver_type(&access, text, &scope, &model),
            Ty::reference("Base")
        );
        let invocation = ancestor_of_kind(node, "method_invocation").expect("invocation");
        assert_eq!(receiver_type(&invocation, text, &scope, &model), Ty::Void);
    }

    /// `List` in two packages, each with a distinct member, so the simple name
    /// alone is ambiguous.
    fn ambiguous_list_model() -> TypeModel {
        let mut model = TypeModel::new();
        let mut a = TypeInfo::new("List".to_string(), Some("a".to_string()), IndexKind::Class);
        a.methods.push(Member {
            name: "size".to_string(),
            kind: IndexKind::Method,
            ty: Ty::Prim(Prim::Int),
            params: Vec::new(),
            type_params: Vec::new(),
            is_static: false,
        });
        model.insert(a);
        let mut b = TypeInfo::new("List".to_string(), Some("b".to_string()), IndexKind::Class);
        b.methods.push(Member {
            name: "other".to_string(),
            kind: IndexKind::Method,
            ty: Ty::Void,
            params: Vec::new(),
            type_params: Vec::new(),
            is_static: false,
        });
        model.insert(b);
        model
    }

    fn scope_with_cursor(text: &str, model: &TypeModel) -> Scope {
        let tree = parse(text);
        let node = node_in(&tree, text, "// cursor");
        scope_at(node, text, &tree, model)
    }

    #[test]
    fn an_exact_import_disambiguates_a_shared_simple_name() {
        let model = ambiguous_list_model();
        let scope = scope_with_cursor(
            "package demo;\nimport a.List;\nclass C {\n    // cursor\n}\n",
            &model,
        );
        let resolved = resolve_name("List", &scope, &model).expect("the import should resolve");
        assert!(resolved.is_type);
        assert_eq!(resolved.ty, Ty::reference("a.List"));
    }

    #[test]
    fn a_wildcard_import_disambiguates_a_shared_simple_name() {
        let model = ambiguous_list_model();
        let scope = scope_with_cursor(
            "package demo;\nimport a.*;\nclass C {\n    // cursor\n}\n",
            &model,
        );
        let resolved = resolve_name("List", &scope, &model).expect("the wildcard should resolve");
        assert_eq!(resolved.ty, Ty::reference("a.List"));
    }

    #[test]
    fn an_ambiguous_name_without_an_import_stays_unresolved() {
        let model = ambiguous_list_model();
        let scope = scope_with_cursor("package demo;\nclass C {\n    // cursor\n}\n", &model);
        assert!(resolve_name("List", &scope, &model).is_none());
    }

    #[test]
    fn an_exact_import_does_not_fall_through_to_another_type() {
        // `a.List` is not indexed, but `b.List` is the only candidate: the
        // import binds the name, so nothing else may be substituted for it.
        let mut model = TypeModel::new();
        model.insert(TypeInfo::new(
            "List".to_string(),
            Some("b".to_string()),
            IndexKind::Class,
        ));
        let scope = scope_with_cursor(
            "package demo;\nimport a.List;\nclass C {\n    // cursor\n}\n",
            &model,
        );
        assert!(resolve_name("List", &scope, &model).is_none());
    }

    #[test]
    fn a_qualified_reference_resolves_in_its_own_package() {
        let model = ambiguous_list_model();
        assert!(member_of(&Ty::reference("a.List"), "size", &model, None).is_some());
        assert!(member_of(&Ty::reference("b.List"), "other", &model, None).is_some());
        // Unqualified, the same name is ambiguous, so no member is claimed.
        assert!(member_of(&Ty::reference("List"), "size", &model, None).is_none());
    }

    #[test]
    fn receiver_type_qualifies_an_imported_local() {
        let model = ambiguous_list_model();
        let text = "\
package demo;
import a.List;
class C {
    void m() {
        List x = null;
        x.size();
    }
}
";
        let tree = parse(text);
        let invocation = ancestor_of_kind(node_in(&tree, text, "x.size"), "method_invocation")
            .expect("method_invocation");
        let object = invocation
            .child_by_field_name("object")
            .expect("receiver x");
        let scope = scope_at(object, text, &tree, &model);
        let ty = receiver_type(&object, text, &scope, &model);
        assert_eq!(ty, Ty::reference("a.List"));
        assert!(member_of(&ty, "size", &model, scope.package.as_deref()).is_some());
    }

    /// The inferred type of the call containing `needle`.
    fn call_type(text: &str, needle: &str) -> Ty {
        let tree = parse(text);
        let mut model = TypeModel::new();
        model.extend(collect_type_infos(None, &tree, text));
        let node = ancestor_of_kind(node_in(&tree, text, needle), "method_invocation")
            .expect("method_invocation");
        let scope = scope_at(node, text, &tree, &model);
        receiver_type(&node, text, &scope, &model)
    }

    #[test]
    fn a_generic_factory_call_binds_its_type_parameter() {
        let text = "\
class Box {
    static <E> Holder<E> of(E e) { return null; }
}
class Holder<T> {}
class C {
    void m() {
        Object a = Box.of(3);
        Object b = Box.of(\"a\");
    }
}
";
        // A primitive argument is boxed, as Java's inference does.
        assert_eq!(
            call_type(text, "Box.of(3)"),
            Ty::Ref {
                name: "Holder".to_string(),
                args: vec![Ty::reference("java.lang.Integer")],
            }
        );
        assert_eq!(
            call_type(text, "Box.of(\"a\")"),
            Ty::Ref {
                name: "Holder".to_string(),
                args: vec![Ty::reference("String")],
            }
        );
    }

    #[test]
    fn a_generic_receivers_argument_substitutes_into_the_result() {
        let text = "\
interface List<E> {
    E get(int index);
}
class C {
    void m() {
        List<String> names = null;
        String first = names.get(0);
    }
}
";
        assert_eq!(call_type(text, "names.get(0)"), Ty::reference("String"));
    }

    #[test]
    fn a_raw_receiver_leaves_the_type_variable_written() {
        let text = "\
interface List<E> {
    E get(int index);
}
class C {
    void m() {
        List raw = null;
        Object first = raw.get(0);
    }
}
";
        // Nothing to infer from: the written form is kept rather than guessed.
        assert_eq!(call_type(text, "raw.get(0)"), Ty::reference("E"));
    }

    #[test]
    fn call_lookup_prefers_the_overload_with_matching_arity() {
        let mut model = TypeModel::new();
        let mut info = TypeInfo::new("T".to_string(), None, IndexKind::Class);
        for (arity, ty) in [
            (0usize, Ty::Void),
            (1, Ty::Prim(Prim::Int)),
            (2, Ty::Prim(Prim::Long)),
        ] {
            info.methods.push(Member {
                name: "over".to_string(),
                kind: IndexKind::Method,
                ty,
                params: (0..arity)
                    .map(|_| Param {
                        name: None,
                        ty: Ty::Prim(Prim::Int),
                    })
                    .collect(),
                type_params: Vec::new(),
                is_static: false,
            });
        }
        model.insert(info);
        let target = Ty::reference("T");

        assert_eq!(
            member_for_call(&target, "over", 1, &model, None).map(|member| member.ty),
            Some(Ty::Prim(Prim::Int))
        );
        assert_eq!(
            member_for_call(&target, "over", 2, &model, None).map(|member| member.ty),
            Some(Ty::Prim(Prim::Long))
        );
        // No arity matches, so the lookup falls back to the name-only answer
        // (here the first `over`, whose parameter list the hierarchy walk
        // collapsed to) rather than guessing an overload.
        assert_eq!(
            member_for_call(&target, "over", 3, &model, None).map(|member| member.ty),
            Some(Ty::Void)
        );
    }

    #[test]
    fn var_enhanced_for_takes_the_element_type() {
        let text = "\
class Widget {}
class C {
    void m(Widget[] ws) {
        for (var w : ws) {
            // cursor
        }
    }
}
";
        let model = model_of(text);
        let scope = scope_with_cursor(text, &model);
        assert_eq!(type_of_local(&scope, "w"), Some(Ty::reference("Widget")));
    }

    #[test]
    fn var_enhanced_for_reads_a_generic_element_type() {
        let text = "\
class Widget {}
class C {
    void m(List<Widget> ws) {
        for (var w : ws) {
            // cursor
        }
    }
}
";
        let model = model_of(text);
        let scope = scope_with_cursor(text, &model);
        assert_eq!(type_of_local(&scope, "w"), Some(Ty::reference("Widget")));
    }

    #[test]
    fn an_explicit_enhanced_for_binding_is_unchanged() {
        let text = "\
class Widget {}
class C {
    void m(Widget[] ws) {
        for (Widget w : ws) {
            // cursor
        }
    }
}
";
        let model = model_of(text);
        let scope = scope_with_cursor(text, &model);
        assert_eq!(type_of_local(&scope, "w"), Some(Ty::reference("Widget")));
    }

    #[test]
    fn try_with_resources_bindings_are_collected() {
        let explicit = "\
class Widget {
    static Widget open() { return null; }
}
class C {
    void m() {
        try (Widget w = Widget.open()) {
            // cursor
        } catch (Exception e) {
        }
    }
}
";
        let model = model_of(explicit);
        let scope = scope_with_cursor(explicit, &model);
        assert_eq!(type_of_local(&scope, "w"), Some(Ty::reference("Widget")));

        let inferred = explicit.replace("Widget w = Widget.open()", "var w = Widget.open()");
        let model = model_of(&inferred);
        let scope = scope_with_cursor(&inferred, &model);
        assert_eq!(type_of_local(&scope, "w"), Some(Ty::reference("Widget")));
    }

    #[test]
    fn var_takes_a_ternary_type_with_a_null_branch() {
        let text = "\
class Widget {}
class C {
    void m(boolean c) {
        var w = c ? new Widget() : null;
        // cursor
    }
}
";
        let model = model_of(text);
        let scope = scope_with_cursor(text, &model);
        assert_eq!(type_of_local(&scope, "w"), Some(Ty::reference("Widget")));
    }

    #[test]
    fn var_takes_an_array_creation_type() {
        let text = "\
class Widget {}
class C {
    void m() {
        var a = new Widget[3];
        // cursor
    }
}
";
        let model = model_of(text);
        let scope = scope_with_cursor(text, &model);
        assert_eq!(
            type_of_local(&scope, "a"),
            Some(Ty::Array(Box::new(Ty::reference("Widget"))))
        );
    }

    #[test]
    fn var_takes_an_instanceof_and_a_switch_result() {
        let text = "\
class Widget {}
class C {
    void m(boolean c, Object o) {
        var b = o instanceof Widget;
        var e = switch (c) { case true -> new Widget(); default -> null; };
        // cursor
    }
}
";
        let model = model_of(text);
        let scope = scope_with_cursor(text, &model);
        assert_eq!(type_of_local(&scope, "b"), Some(Ty::Prim(Prim::Boolean)));
        assert_eq!(type_of_local(&scope, "e"), Some(Ty::reference("Widget")));
    }

    #[test]
    fn a_switch_whose_results_disagree_infers_nothing() {
        let text = "\
class Widget {}
class C {
    void m(boolean c) {
        var e = switch (c) { case true -> new Widget(); default -> 1; };
        // cursor
    }
}
";
        let model = model_of(text);
        let scope = scope_with_cursor(text, &model);
        assert_eq!(type_of_local(&scope, "e"), Some(Ty::Unknown));
    }

    /// A model with a qualified `java.lang.Object` and a member-less `Gson`, as
    /// a JDK-indexed workspace would have.
    fn object_model() -> TypeModel {
        let mut model = TypeModel::new();
        let mut object = TypeInfo::new(
            "Object".to_string(),
            Some("java.lang".to_string()),
            IndexKind::Class,
        );
        object.methods.push(Member {
            name: "toString".to_string(),
            kind: IndexKind::Method,
            ty: Ty::reference("String"),
            params: Vec::new(),
            type_params: Vec::new(),
            is_static: false,
        });
        model.insert(object);
        model.insert(TypeInfo::new("Gson".to_string(), None, IndexKind::Class));
        model
    }

    #[test]
    fn an_inherited_object_method_resolves_but_stays_out_of_member_listing() {
        let model = object_model();
        let gson = Ty::reference("Gson");
        // Resolution finds the inherited method ...
        assert_eq!(
            member_of(&gson, "toString", &model, None).map(|member| member.ty),
            Some(Ty::reference("String"))
        );
        assert_eq!(
            member_for_call(&gson, "toString", 0, &model, None).map(|member| member.ty),
            Some(Ty::reference("String"))
        );
        // ... but the hierarchy walk — what `.`-completion reads — does not.
        assert!(model
            .members(&gson, None)
            .iter()
            .all(|member| member.name != "toString"));
        // An unknown receiver gains nothing.
        assert!(member_of(&Ty::Unknown, "toString", &model, None).is_none());
    }

    #[test]
    fn a_call_to_an_object_method_infers_its_result() {
        let text = "\
package java.lang;
class Object {
    public String toString() { return null; }
}
class Gson {}
class C {
    void m(Gson gson) {
        var s = gson.toString();
        // cursor
    }
}
";
        let model = model_of(text);
        let scope = scope_with_cursor(text, &model);
        assert_eq!(type_of_local(&scope, "s"), Some(Ty::reference("String")));
    }

    fn type_of_local(scope: &Scope, name: &str) -> Option<Ty> {
        scope
            .locals
            .iter()
            .rev()
            .find(|(local, _)| local == name)
            .map(|(_, ty)| ty.clone())
            .or_else(|| {
                scope
                    .fields
                    .iter()
                    .find(|m| m.name == name)
                    .map(|m| m.ty.clone())
            })
    }

    fn parse(text: &str) -> Tree {
        let mut parser = java_parser();
        parser.parse(text.as_bytes(), None).expect("parse")
    }

    fn node_in<'a>(tree: &'a Tree, text: &str, needle: &str) -> Node<'a> {
        let offset = text.find(needle).expect("needle") + needle.len() - 1;
        tree.root_node()
            .descendant_for_byte_range(offset, offset)
            .expect("node")
    }

    fn ancestor_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
        let mut current = Some(node);
        while let Some(node) = current {
            if node.kind() == kind {
                return Some(node);
            }
            current = node.parent();
        }
        None
    }

    #[test]
    fn nested_type_references_use_their_simple_name() {
        let nested = Ty::from_descriptor("Ljava/util/Map$Entry;");
        assert_eq!(nested.simple_name(), Some("Entry"));
        assert_eq!(nested.qualified_package(), Some("java.util"));
        assert_eq!(nested.display(), "Entry");

        // The reference reaches the innermost-keyed model type.
        let mut model = TypeModel::new();
        let mut map_entry = TypeInfo::new(
            "Entry".to_string(),
            Some("java.util".to_string()),
            IndexKind::Interface,
        );
        map_entry.nested = Some("Map".to_string());
        map_entry.methods.push(Member {
            name: "getKey".to_string(),
            kind: IndexKind::Method,
            ty: Ty::Unknown,
            params: Vec::new(),
            type_params: Vec::new(),
            is_static: false,
        });
        model.insert(map_entry);
        let members = model.members(&nested, None);
        assert!(
            members.iter().any(|member| member.name == "getKey"),
            "{members:?}"
        );
    }

    #[test]
    fn nested_types_sharing_an_innermost_name_coexist() {
        let mut model = TypeModel::new();
        let mut first = TypeInfo::new("Inner".to_string(), Some("p".to_string()), IndexKind::Class);
        first.nested = Some("Outer".to_string());
        model.insert(first);
        let mut second =
            TypeInfo::new("Inner".to_string(), Some("p".to_string()), IndexKind::Class);
        second.nested = Some("Other".to_string());
        model.insert(second);
        assert_eq!(model.find("Inner").len(), 2);
    }

    #[test]
    fn source_nested_types_record_their_enclosing_chain() {
        let model = model_of("class Outer {\n    class Inner {}\n}\n");
        let inner = model.find("Inner");
        assert_eq!(inner.len(), 1);
        assert_eq!(inner[0].nested.as_deref(), Some("Outer"));
    }

    #[test]
    fn a_qualified_supertype_resolves_in_its_own_package() {
        // `List` exists in two packages; a subtype in a third package inherits
        // from the qualified `java.util.List`.
        let mut model = ambiguous_list_model();
        let mut util = TypeInfo::new(
            "List".to_string(),
            Some("java.util".to_string()),
            IndexKind::Interface,
        );
        util.methods.push(Member {
            name: "get".to_string(),
            kind: IndexKind::Method,
            ty: Ty::Prim(Prim::Int),
            params: Vec::new(),
            type_params: Vec::new(),
            is_static: false,
        });
        model.insert(util);
        let mut sub = TypeInfo::new(
            "Sub".to_string(),
            Some("demo".to_string()),
            IndexKind::Class,
        );
        sub.supertypes.push(Ty::reference("java.util.List"));
        model.insert(sub);

        let members = model.members(&Ty::reference("demo.Sub"), None);
        assert!(
            members.iter().any(|member| member.name == "get"),
            "{members:?}"
        );
    }

    #[test]
    fn member_owner_reports_the_declaring_package() {
        let mut model = TypeModel::new();
        for package in ["a", "b"] {
            let mut widget = TypeInfo::new(
                "Widget".to_string(),
                Some(package.to_string()),
                IndexKind::Class,
            );
            widget.methods.push(Member {
                name: "run".to_string(),
                kind: IndexKind::Method,
                ty: Ty::Void,
                params: Vec::new(),
                type_params: Vec::new(),
                is_static: false,
            });
            model.insert(widget);
        }
        let a = member_owner(&Ty::reference("a.Widget"), "run", &model, None).expect("a owner");
        let b = member_owner(&Ty::reference("b.Widget"), "run", &model, None).expect("b owner");
        assert_eq!(a.name, "Widget");
        assert_eq!(a.package.as_deref(), Some("a"));
        assert_eq!(b.package.as_deref(), Some("b"));
        assert_ne!(a, b);
    }

    fn entry(
        name: &str,
        kind: IndexKind,
        package: Option<&str>,
        container: &[&str],
    ) -> SymbolEntry {
        use tower_lsp::lsp_types::{Position, Range, Url};
        let zero = Range::new(Position::new(0, 0), Position::new(0, 0));
        SymbolEntry {
            uri: Url::parse("file:///x.jar").unwrap(),
            name: name.to_string(),
            kind,
            package: package.map(str::to_string),
            container: container.iter().map(|s| s.to_string()).collect(),
            full_range: zero,
            selection_range: zero,
            dependency: true,
            library_source: false,
        }
    }
}
