//! Shared Java declarations and type vocabulary used by parsing, index reads, and consumers.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

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
    pub(crate) fn bytes(&self) -> usize {
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

impl SymbolKind {
    /// Whether this kind names a type declaration (as opposed to a member, an
    /// import, or an enum constant).
    pub fn is_type(&self) -> bool {
        matches!(
            self,
            SymbolKind::Class | SymbolKind::Interface | SymbolKind::Enum | SymbolKind::Record
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Member, SymbolKind, Ty, TypeInfo};

    #[test]
    fn legacy_index_paths_share_the_model_types() {
        let ty: crate::index::types::Ty = Ty::reference("Widget");
        let _: Ty = ty;
        let member: Option<crate::index::types::Member> = None;
        let _: Option<Member> = member;
        let info: Option<crate::index::types::TypeInfo> = None;
        let _: Option<TypeInfo> = info;
        let kind: crate::index::IndexKind = SymbolKind::Class;
        assert_eq!(kind, SymbolKind::Class);
    }
}
