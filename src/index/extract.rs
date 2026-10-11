//! Extraction of the declared-type model from a parsed source file: the
//! `TypeInfo`s for the declarations in one compilation unit, the `Ty`s their
//! signatures mention, and the members Lombok synthesizes from annotations.
//!
//! This is the source-reading half of the model in [`super::types`] — it turns a
//! tree-sitter tree into the model's vocabulary and carries no lookups of its
//! own. `index.rs` runs it per file during the workspace scan.

use tree_sitter::{Node, Tree};

use super::types::{params_key, Member, Param, Prim, Ty, TypeInfo};
use super::IndexKind;

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

/// A type name's simple (last) segment.
fn simple(name: &str) -> &str {
    name.rsplit(['.', '$']).next().unwrap_or(name)
}
