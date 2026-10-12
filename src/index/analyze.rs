//! Java analysis and extraction — everything that reads Java source and builds
//! the index.
//!
//! This module owns the whole source-reading path: the declared-type model
//! extracted from a parsed tree (including the members Lombok synthesizes), the
//! flat index entries extracted from that same tree, the byte-offset-to-position
//! conversion those entries are anchored with, and the workspace `.java` walk and
//! scan that drive it all. It turns a parsed tree, or a path on disk, into the
//! index's vocabulary and stores it: the symbol table's upsert and removal, the
//! declared-type store's layer updates, and the context rebuild live here too, so
//! [`super::query`] is left with reads alone.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tower_lsp::lsp_types::Url;
use tree_sitter::{Node, Parser, Tree};

use super::types::{
    FileOrigin, FileRecord, IndexKind, LayerSet, Member, ModelLayers, Param, Prim,
    SourceLayerIndex, SymbolEntry, SymbolIndex, SymbolKind, Ty, TypeContext, TypeInfo, TypeModel,
    WorkspaceIndex,
};
use super::util::params_key;
use crate::parser::{file_package, java_parser, type_from_node, LineIndex};
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
        if let Some((mut info, extras)) = type_info_from_declaration(node, text, package.clone()) {
            info.nested = enclosing.clone();
            if let Some(name) = node.child_by_field_name("name") {
                let simple = text[name.byte_range()].to_string();
                child_enclosing = Some(match &enclosing {
                    Some(outer) => format!("{outer}${simple}"),
                    None => simple,
                });
            }
            out.push(info);
            // A synthetic nested type (the `@Builder` builder) is keyed like a
            // source nested one, nested in the type that declared it.
            for mut extra in extras {
                extra.nested = child_enclosing.clone();
                out.push(extra);
            }
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

// ---------------------------------------------------------------------------
// Lombok synthesis
// ---------------------------------------------------------------------------

/// An annotation seen on a declaration: its simple name and its `key = value`
/// arguments (a single unkeyed value is keyed `value`).
#[derive(Clone)]
struct Annotation {
    name: String,
    args: Vec<(String, String)>,
}

impl Annotation {
    /// True when the argument `key` is written `= true`.
    fn flag(&self, key: &str) -> bool {
        self.args.iter().any(|(k, v)| k == key && v == "true")
    }
}

/// One member Lombok generates for a source type, with the node it anchors to:
/// the field it derives from, or the type's own name for a type-level member.
/// Both the type model and the flat index consume this, so their names and
/// shapes cannot drift apart.
pub struct GeneratedMember<'a> {
    pub name: String,
    pub kind: IndexKind,
    /// The nested builder type this member belongs to, if any.
    pub container: Option<String>,
    pub ty: Ty,
    pub params: Vec<Param>,
    pub is_static: bool,
    /// True for a generated constructor (its name is the type's).
    pub constructor: bool,
    pub anchor: Node<'a>,
    pub anchor_name: Node<'a>,
}

/// The annotations in a declaration's `modifiers` child, in source order.
fn annotations_of(node: &Node, text: &str) -> Vec<Annotation> {
    let Some(modifiers) = child_of_kind(node, "modifiers") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = modifiers.walk();
    for child in modifiers.named_children(&mut cursor) {
        if !matches!(child.kind(), "annotation" | "marker_annotation") {
            continue;
        }
        let Some(name) = child.child_by_field_name("name") else {
            continue;
        };
        let mut args = Vec::new();
        if let Some(list) = child.child_by_field_name("arguments") {
            let mut inner = list.walk();
            for element in list.named_children(&mut inner) {
                if element.kind() == "element_value_pair" {
                    let key = element
                        .child_by_field_name("key")
                        .map(|k| text[k.byte_range()].to_string())
                        .unwrap_or_default();
                    let value = element
                        .child_by_field_name("value")
                        .map(|v| text[v.byte_range()].trim().to_string())
                        .unwrap_or_default();
                    args.push((key, value));
                } else {
                    args.push((
                        "value".to_string(),
                        text[element.byte_range()].trim().to_string(),
                    ));
                }
            }
        }
        out.push(Annotation {
            name: simple(&text[name.byte_range()]).to_string(),
            args,
        });
    }
    out
}

fn child_of_kind<'a>(node: &Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    let found = node
        .children(&mut cursor)
        .find(|child| child.kind() == kind);
    found
}

fn has_annotation(annotations: &[Annotation], name: &str) -> bool {
    annotations.iter().any(|annotation| annotation.name == name)
}

/// `@Accessors` state that changes generated accessor names and return types.
#[derive(Default, Clone, Copy)]
struct Accessors {
    fluent: bool,
    chain: bool,
}

fn accessors_of(annotations: &[Annotation]) -> Accessors {
    let mut out = Accessors::default();
    if let Some(annotation) = annotations.iter().find(|a| a.name == "Accessors") {
        out.fluent = annotation.flag("fluent");
        out.chain = annotation.flag("chain");
    }
    out
}

/// One declared field, with what the generators need.
struct Field<'a> {
    name: String,
    ty: Ty,
    declarator: Node<'a>,
    name_node: Node<'a>,
    is_static: bool,
    is_final: bool,
    non_null: bool,
    annotations: Vec<Annotation>,
}

fn lombok_fields<'a>(body: &Node<'a>, text: &str) -> Vec<Field<'a>> {
    let mut out = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() != "field_declaration" {
            continue;
        }
        let Some(ty_node) = child.child_by_field_name("type") else {
            continue;
        };
        let ty = type_from_node(&ty_node, text);
        let is_static = has_modifier(&child, text, "static");
        let is_final = has_modifier(&child, text, "final");
        let annotations = annotations_of(&child, text);
        let non_null = has_annotation(&annotations, "NonNull");
        let mut declarators = child.walk();
        for declarator in child.named_children(&mut declarators) {
            if declarator.kind() != "variable_declarator" {
                continue;
            }
            let Some(name_node) = declarator.child_by_field_name("name") else {
                continue;
            };
            out.push(Field {
                name: text[name_node.byte_range()].to_string(),
                ty: ty.clone(),
                declarator,
                name_node,
                is_static,
                is_final,
                non_null,
                annotations: annotations.clone(),
            });
        }
    }
    out
}

/// The Lombok log-field annotations and the logger type each introduces.
const LOG_ANNOTATIONS: &[(&str, &str)] = &[
    ("Log", "java.util.logging.Logger"),
    ("Slf4j", "org.slf4j.Logger"),
    ("Log4j", "org.apache.log4j.Logger"),
    ("Log4j2", "org.apache.logging.log4j.Logger"),
    ("CommonsLog", "org.apache.commons.logging.Log"),
    ("JBossLog", "org.jboss.logging.Logger"),
    ("Flogger", "com.google.common.flogger.FluentLogger"),
    ("XSlf4j", "org.slf4j.ext.XLogger"),
];

/// The members Lombok would generate for a type declaration, ordered so the
/// model and the index can both consume them: the nested builder type first
/// (so its members can attach to it), then the type's own generated members.
/// Empty when the declaration carries no covering Lombok annotation.
pub fn lombok_generated<'a>(
    node: &Node<'a>,
    text: &str,
    type_name: &str,
) -> Vec<GeneratedMember<'a>> {
    let Some(body) = node.child_by_field_name("body") else {
        return Vec::new();
    };
    let type_annotations = annotations_of(node, text);
    let fields = lombok_fields(&body, text);
    let accessors = accessors_of(&type_annotations);
    let name_node = node.child_by_field_name("name").unwrap_or(*node);

    let class_getter = has_annotation(&type_annotations, "Getter");
    let class_setter = has_annotation(&type_annotations, "Setter");
    let class_with = has_annotation(&type_annotations, "With");
    let data = has_annotation(&type_annotations, "Data");
    let value = has_annotation(&type_annotations, "Value");

    let mut out = Vec::new();

    // `@Builder`: the nested builder type, `builder()` on the type, and the
    // builder's per-field setters plus `build()`.
    if let Some(builder) = type_annotations.iter().find(|a| a.name == "Builder") {
        let builder_name = format!("{type_name}Builder");
        out.push(GeneratedMember {
            name: builder_name.clone(),
            kind: IndexKind::Class,
            container: None,
            ty: Ty::Unknown,
            params: Vec::new(),
            is_static: false,
            constructor: false,
            anchor: name_node,
            anchor_name: name_node,
        });
        out.push(GeneratedMember {
            name: "builder".to_string(),
            kind: IndexKind::Method,
            container: None,
            ty: Ty::reference(builder_name.clone()),
            params: Vec::new(),
            is_static: true,
            constructor: false,
            anchor: name_node,
            anchor_name: name_node,
        });
        if builder.flag("toBuilder") {
            out.push(GeneratedMember {
                name: "toBuilder".to_string(),
                kind: IndexKind::Method,
                container: None,
                ty: Ty::reference(builder_name.clone()),
                params: Vec::new(),
                is_static: false,
                constructor: false,
                anchor: name_node,
                anchor_name: name_node,
            });
        }
        for field in &fields {
            out.push(GeneratedMember {
                name: field.name.clone(),
                kind: IndexKind::Method,
                container: Some(builder_name.clone()),
                ty: Ty::reference(builder_name.clone()),
                params: vec![field_param(field)],
                is_static: false,
                constructor: false,
                anchor: field.declarator,
                anchor_name: field.name_node,
            });
        }
        out.push(GeneratedMember {
            name: "build".to_string(),
            kind: IndexKind::Method,
            container: Some(builder_name),
            ty: Ty::reference(type_name),
            params: Vec::new(),
            is_static: false,
            constructor: false,
            anchor: name_node,
            anchor_name: name_node,
        });
    }

    // Accessors, setters, and `withX`. Class-level annotations skip static
    // fields; a field-level annotation applies regardless.
    for field in &fields {
        let field_getter = has_annotation(&field.annotations, "Getter");
        let field_setter = has_annotation(&field.annotations, "Setter");
        let field_with = has_annotation(&field.annotations, "With");
        let getter = field_getter || ((class_getter || data || value) && !field.is_static);
        let setter =
            field_setter || ((class_setter || data) && !field.is_static && !field.is_final);
        let with = field_with || (class_with && !field.is_static);
        if getter {
            out.push(GeneratedMember {
                name: getter_name(
                    &field.name,
                    field.ty == Ty::Prim(Prim::Boolean),
                    accessors.fluent,
                ),
                kind: IndexKind::Method,
                container: None,
                ty: field.ty.clone(),
                params: Vec::new(),
                is_static: field.is_static,
                constructor: false,
                anchor: field.declarator,
                anchor_name: field.name_node,
            });
        }
        if setter {
            let ret = if accessors.chain {
                Ty::reference(type_name)
            } else {
                Ty::Void
            };
            out.push(GeneratedMember {
                name: setter_name(&field.name, accessors.fluent),
                kind: IndexKind::Method,
                container: None,
                ty: ret,
                params: vec![field_param(field)],
                is_static: field.is_static,
                constructor: false,
                anchor: field.declarator,
                anchor_name: field.name_node,
            });
        }
        if with {
            out.push(GeneratedMember {
                name: with_name(&field.name),
                kind: IndexKind::Method,
                container: None,
                ty: Ty::reference(type_name),
                params: vec![field_param(field)],
                is_static: false,
                constructor: false,
                anchor: field.declarator,
                anchor_name: field.name_node,
            });
        }
    }

    // The log field.
    if let Some((_, logger)) = LOG_ANNOTATIONS
        .iter()
        .find(|(annotation, _)| has_annotation(&type_annotations, annotation))
    {
        out.push(GeneratedMember {
            name: "log".to_string(),
            kind: IndexKind::Field,
            container: None,
            ty: Ty::reference(*logger),
            params: Vec::new(),
            is_static: true,
            constructor: false,
            anchor: name_node,
            anchor_name: name_node,
        });
    }

    // Constructors. `@Builder` also implies an all-args one; `@Data` a
    // required-args one; `@Value` an all-args one.
    let all_args = has_annotation(&type_annotations, "AllArgsConstructor")
        || value
        || has_annotation(&type_annotations, "Builder");
    let required_args = has_annotation(&type_annotations, "RequiredArgsConstructor") || data;
    let no_args = has_annotation(&type_annotations, "NoArgsConstructor");
    if all_args {
        out.push(generated_constructor(
            type_name,
            all_fields(&fields),
            name_node,
        ));
    }
    if required_args {
        let required: Vec<Param> = fields
            .iter()
            .filter(|field| !field.is_static && (field.is_final || field.non_null))
            .map(field_param)
            .collect();
        out.push(generated_constructor(type_name, required, name_node));
    }
    if no_args {
        out.push(generated_constructor(type_name, Vec::new(), name_node));
    }

    out
}

fn field_param(field: &Field) -> Param {
    Param {
        name: Some(field.name.clone()),
        ty: field.ty.clone(),
    }
}

fn all_fields(fields: &[Field]) -> Vec<Param> {
    fields
        .iter()
        .filter(|field| !field.is_static)
        .map(field_param)
        .collect()
}

fn generated_constructor<'a>(
    name: &str,
    params: Vec<Param>,
    anchor: Node<'a>,
) -> GeneratedMember<'a> {
    GeneratedMember {
        name: name.to_string(),
        kind: IndexKind::Method,
        container: None,
        ty: Ty::Void,
        params,
        is_static: false,
        constructor: true,
        anchor,
        anchor_name: anchor,
    }
}

fn getter_name(name: &str, boolean_prim: bool, fluent: bool) -> String {
    if fluent {
        return name.to_string();
    }
    if boolean_prim {
        // `isActive` already reads as a getter; never `isIsActive`.
        if name
            .strip_prefix("is")
            .is_some_and(|rest| rest.chars().next().is_some_and(|c| c.is_ascii_uppercase()))
        {
            return name.to_string();
        }
        return format!("is{}", capitalize(name));
    }
    format!("get{}", capitalize(name))
}

fn setter_name(name: &str, fluent: bool) -> String {
    if fluent {
        name.to_string()
    } else {
        format!("set{}", capitalize(name))
    }
}

fn with_name(name: &str) -> String {
    format!("with{}", capitalize(name))
}

fn capitalize(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn type_info_from_declaration(
    node: &Node,
    text: &str,
    package: Option<String>,
) -> Option<(TypeInfo, Vec<TypeInfo>)> {
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
    // Lombok: append the members the annotations generate, and collect a
    // synthetic nested type (`@Builder`'s `TBuilder`). Constructors go into the
    // `constructors` collection so the implicit no-arg synthesis below is
    // skipped when Lombok already generated one.
    let mut extras: Vec<TypeInfo> = Vec::new();
    for generated in lombok_generated(node, text, &info.name) {
        if generated.kind == IndexKind::Class {
            let mut builder = TypeInfo::new(
                generated.name.clone(),
                info.package.clone(),
                IndexKind::Class,
            );
            builder.nested = Some(info.name.clone());
            extras.push(builder);
            continue;
        }
        let kind = generated.kind;
        let container = generated.container.clone();
        let is_constructor = generated.constructor;
        let member = Member {
            name: generated.name,
            kind,
            ty: generated.ty,
            params: generated.params,
            type_params: Vec::new(),
            is_static: generated.is_static,
        };
        if is_constructor {
            info.constructors.push(member);
        } else if container.is_some() {
            if let Some(builder) = extras.last_mut() {
                match kind {
                    IndexKind::Method => builder.methods.push(member),
                    _ => builder.fields.push(member),
                }
            }
        } else {
            match kind {
                IndexKind::Method => info.methods.push(member),
                _ => info.fields.push(member),
            }
        }
    }
    // Constructors live in their own collection, never in `methods`. A record
    // always has its canonical constructor, synthesized from its components
    // when it declares none of the same shape; a class that declares no
    // constructor has the implicit no-arg one. Interfaces and enums add none.
    if kind == IndexKind::Record {
        let canonical: Vec<Param> = record_components(node, text)
            .into_iter()
            .map(|(name, ty)| Param {
                name: Some(name),
                ty,
            })
            .collect();
        let declared = info
            .constructors
            .iter()
            .any(|ctor| params_key(&ctor.params) == params_key(&canonical));
        if !declared {
            info.constructors.insert(
                0,
                Member {
                    name: info.name.clone(),
                    kind: IndexKind::Method,
                    ty: Ty::Void,
                    params: canonical,
                    type_params: Vec::new(),
                    is_static: false,
                },
            );
        }
    } else if kind == IndexKind::Class && info.constructors.is_empty() {
        info.constructors.push(Member {
            name: info.name.clone(),
            kind: IndexKind::Method,
            ty: Ty::Void,
            params: Vec::new(),
            type_params: Vec::new(),
            is_static: false,
        });
    }
    Some((info, extras))
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
    // A constant's type is the enum that declares it, so a chained access such
    // as `TYPE_1.rank()` resolves through the member's access type.
    let enum_name = info.name.clone();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        match child.kind() {
            // An enum's declared members (fields, methods, constructors) sit in
            // its `enum_body_declarations` after the constants.
            "enum_body_declarations" => collect_members(&child, text, info),
            "enum_constant" => {
                let Some(name) = child.child_by_field_name("name") else {
                    continue;
                };
                info.fields.push(Member {
                    name: text[name.byte_range()].to_string(),
                    kind: IndexKind::EnumConstant,
                    ty: Ty::reference(enum_name.clone()),
                    params: Vec::new(),
                    type_params: Vec::new(),
                    is_static: true,
                });
            }
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
            "constructor_declaration" => {
                let Some(name) = child.child_by_field_name("name") else {
                    continue;
                };
                info.constructors.push(Member {
                    name: text[name.byte_range()].to_string(),
                    kind: IndexKind::Method,
                    ty: Ty::Void,
                    params: parameter_list(&child, text),
                    type_params: method_type_params(&child, text),
                    is_static: false,
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

/// A type name's simple (last) segment.
fn simple(name: &str) -> &str {
    name.rsplit(['.', '$']).next().unwrap_or(name)
}

/// Extracts the indexable declarations and imports from a parsed Java file,
/// attributing every entry to `uri` and to the file's package.
pub fn extract_entries(uri: &Url, tree: &Tree, text: &str) -> Vec<SymbolEntry> {
    let package: Option<Arc<str>> = file_package(tree, text).map(Arc::from);
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
    for generated in lombok_generated(node, text, type_name) {
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
// The index's build side
// ---------------------------------------------------------------------------

/// Replaces `uri`'s entries, keeping the name and package views consistent.
/// Each entry is allocated once and shared between the per-file and per-name
/// views, so an entry is stored a single time.
pub fn upsert_file(
    index: &mut WorkspaceIndex,
    uri: &Url,
    origin: FileOrigin,
    entries: Vec<SymbolEntry>,
) {
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
            *index
                .symbols
                .packages
                .entry(Arc::clone(package))
                .or_default() += 1;
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
fn remove_from_name(symbols: &mut SymbolIndex, entry: &SymbolEntry) {
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
fn remove_from_package(symbols: &mut SymbolIndex, entry: &SymbolEntry) {
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

/// Marks the warm-up complete and builds the declared-type view a query reads.
pub fn set_ready(index: &mut WorkspaceIndex) {
    index.ready = true;
    rebuild_context(index);
}

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
    let package = file_package(&tree, &text);
    let mut model = TypeModel::new();
    model.extend(collect_type_infos(package.as_deref(), &tree, &text));
    let entries = extract_entries(&uri, &tree, &text);
    upsert_file(index, &uri, FileOrigin::Workspace, entries);
    set_source(index, &uri, Arc::new(model));
    true
}

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
pub(crate) fn workspace_java_files(root: &Path) -> Vec<PathBuf> {
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
