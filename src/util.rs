//! Shared Java reading that more than one hub module needs: turning the
//! expression before a `.` into the type it names.
//!
//! Navigation resolves a member cursor through the receiver's declared type;
//! completion resolves the receiver of a `recv.` so it can offer the receiver's
//! members. Both read the same declared-type model over the hub, so the machinery
//! lives here once rather than in either module. It is a pure function of the
//! index reads and the parsed file: the two reads it makes
//! ([`IndexRequest::MembersOf`], [`IndexRequest::ResolvesType`]) already existed
//! for navigation, so no new hub message family is introduced.

use tree_sitter::{Node, Tree};

use crate::hub::HubClient;
use crate::messages::{IndexRequest, IndexResponse, Request, Response};
use crate::model::{Member, SymbolKind, Ty};
use crate::parser::{type_from_node, word_prefix};

// ---------------------------------------------------------------------------
// The index reads
// ---------------------------------------------------------------------------

/// Awaits one index read. `None` when the index is unowned or answers with a
/// different variant.
async fn read(client: &HubClient, request: IndexRequest) -> Option<IndexResponse> {
    match client.request(Request::Index(request)).await {
        Response::Index(response) => Some(response),
        _ => None,
    }
}

/// A type's members, inherited ones included, one entry per overload.
pub(crate) async fn members(client: &HubClient, ty: &Ty, package: Option<&str>) -> Vec<Member> {
    match read(
        client,
        IndexRequest::MembersOf {
            ty: ty.clone(),
            package: package.map(str::to_string),
        },
    )
    .await
    {
        Some(IndexResponse::Members(members)) => members,
        _ => Vec::new(),
    }
}

/// Whether a type name resolves.
async fn resolves_type(client: &HubClient, ty: &Ty, package: Option<&str>) -> bool {
    matches!(
        read(
            client,
            IndexRequest::ResolvesType {
                ty: ty.clone(),
                package: package.map(str::to_string),
            },
        )
        .await,
        Some(IndexResponse::Resolves(true))
    )
}

// ---------------------------------------------------------------------------
// Receiver-typed member resolution
// ---------------------------------------------------------------------------

/// A receiver resolved to a type: the type itself, and whether the receiver
/// *names* that type (a `Type.staticMember` qualifier) rather than holding a value
/// of it. Completion offers only static members for a type receiver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Receiver {
    pub(crate) ty: Ty,
    pub(crate) is_type: bool,
}

/// The type a member cursor resolves its receiver through, or `None` when the
/// receiver is one of the documented out-of-scope shapes (`super`, an array
/// access, a lambda) or does not resolve.
///
/// A `recv.name`/`recv.name()` cursor resolves `recv`: `this` names the enclosing
/// type; an `identifier` is resolved in the enclosing scope (a parameter, then an
/// in-scope local, then a field of the enclosing type, then a type name for
/// `Type.staticMember`); a `type_identifier`/`scoped_type_identifier` is the
/// written type; a `new T(…)` is `T`. An unqualified call `name(...)` the
/// enclosing type declares is an implicit `this`.
pub(crate) async fn receiver_type_of(
    client: &HubClient,
    text: &str,
    package: Option<&str>,
    node: &Node<'_>,
) -> Option<Ty> {
    if let Some(object) = member_access_object(node) {
        return classify_receiver(client, text, package, &object)
            .await
            .map(|receiver| receiver.ty);
    }
    // An implicit `this`: only an unqualified call `name(...)` — a genuine
    // unqualified member use — whose name the enclosing type declares. A
    // method-reference name (`Strings::trim`), an annotation name, a declaration
    // name, and any other bare identifier are not receivers, so the name-only
    // ambiguity policy stands for them (D4).
    if !is_implicit_this_use(node) {
        return None;
    }
    let name = &text[node.byte_range()];
    let enclosing = enclosing_type_name(text, node)?;
    let ty = Ty::reference(enclosing);
    members(client, &ty, package)
        .await
        .iter()
        .any(|member| member.name == name)
        .then_some(ty)
}

/// The receiver of the member access the `.` at `offset` introduces, resolved to
/// its type and whether it names a type. The relevant dot is the one before the
/// typed prefix — not necessarily before the cursor — so a partial name the
/// parser reads as a type still resolves its qualifier rather than the partial
/// name itself. `None` when there is no dot before the prefix or the receiver
/// does not resolve.
pub(crate) async fn receiver_before_dot(
    client: &HubClient,
    text: &str,
    package: Option<&str>,
    tree: &Tree,
    offset: usize,
) -> Option<Receiver> {
    let prefix = word_prefix(text, offset);
    let dot = offset.checked_sub(prefix.len())?.checked_sub(1)?;
    if text.as_bytes().get(dot) != Some(&b'.') {
        return None;
    }
    let object = formed_member_object(tree, dot)
        .or_else(|| receiver_before_dot_from_text(tree, text, dot))?;
    classify_receiver(client, text, package, &object).await
}

/// The accessed object of a formed member access — the parser accepted the
/// partial name as a value member — whose receiver ends at `dot`.
fn formed_member_object<'a>(tree: &'a Tree, dot: usize) -> Option<Node<'a>> {
    let node = tree.root_node().descendant_for_byte_range(dot, dot)?;
    let mut current = Some(node);
    while let Some(node) = current {
        if matches!(node.kind(), "field_access" | "method_invocation") {
            if let Some(object) = node.child_by_field_name("object") {
                if object.end_byte() == dot {
                    return Some(object);
                }
            }
        }
        current = node.parent();
    }
    None
}

/// Recovers the receiver of an incomplete `receiver.` from the source when the
/// tree has not formed a member access, given the byte index of the dot. A dot at
/// the end of a line can be absorbed into the following token — a `var` line makes
/// the parser read `gson.var` as a scoped type identifier — so the receiver is the
/// outermost expression ending at the last non-whitespace byte before the dot. The
/// node is a real node of the same tree, so callers can still locate its enclosing
/// scope.
fn receiver_before_dot_from_text<'a>(tree: &'a Tree, text: &str, dot: usize) -> Option<Node<'a>> {
    let bytes = text.as_bytes();
    // `end` is the receiver's exclusive end: the dot, backed up over any
    // whitespace before it (`gson .` still resolves `gson`).
    let mut end = dot;
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    if end == 0 {
        return None;
    }
    let mut node = tree
        .root_node()
        .descendant_for_byte_range(end - 1, end - 1)?;
    let mut best = is_receiver_kind(node.kind()).then_some(node);
    // Climb to the outermost expression that still ends at the receiver, so a
    // call or parenthesized receiver is taken whole (`list.get(0).`, `(x).`).
    while let Some(parent) = node.parent() {
        if parent.end_byte() != end {
            break;
        }
        node = parent;
        if is_receiver_kind(node.kind()) {
            best = Some(node);
        }
    }
    best
}

/// True for the node kinds [`classify_receiver`] can type — the shapes an
/// expression receiver may take (an otherwise unhandled kind resolves nothing,
/// so it is not typed).
fn is_receiver_kind(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "type_identifier"
            | "scoped_identifier"
            | "scoped_type_identifier"
            | "this"
            | "super"
            | "object_creation_expression"
            | "array_creation_expression"
            | "field_access"
            | "method_invocation"
            | "parenthesized_expression"
            | "cast_expression"
            | "array_access"
            | "ternary_expression"
            | "instanceof_expression"
            | "switch_expression"
            | "string_literal"
            | "decimal_integer_literal"
            | "hex_integer_literal"
            | "octal_integer_literal"
            | "binary_integer_literal"
            | "decimal_floating_point_literal"
            | "hex_floating_point_literal"
            | "character_literal"
            | "true"
            | "false"
            | "null_literal"
    )
}

/// The type a receiver node names, and whether it names a *type*. `this`, a
/// value-typed identifier, and `new T(…)` are value receivers; a
/// `type_identifier`/`scoped_type_identifier` — and an identifier whose fallback
/// resolves to a type — are type qualifiers. `None` for a shape that names no
/// resolvable type.
pub(crate) async fn classify_receiver(
    client: &HubClient,
    text: &str,
    package: Option<&str>,
    object: &Node<'_>,
) -> Option<Receiver> {
    classify_receiver_at(client, text, package, object, 0).await
}

/// The deepest receiver chain [`classify_receiver`] follows before giving up, so
/// `a.b().c().` terminates rather than recursing without end.
const MAX_RECEIVER_DEPTH: usize = 8;

/// [`classify_receiver`] with the chain depth threaded through, so a receiver
/// that is itself a call — `a.b().c().` resolving `c`'s receiver `a.b()`, whose
/// own receiver `a` is resolved in turn — recurses with the guard applied.
async fn classify_receiver_at(
    client: &HubClient,
    text: &str,
    package: Option<&str>,
    object: &Node<'_>,
    depth: usize,
) -> Option<Receiver> {
    if depth > MAX_RECEIVER_DEPTH {
        return None;
    }
    match object.kind() {
        "this" => enclosing_type_name(text, object).map(|name| Receiver {
            ty: Ty::reference(name),
            is_type: false,
        }),
        "identifier" | "type_identifier" => {
            identifier_type(client, text, package, object, depth).await
        }
        "scoped_type_identifier" => Some(Receiver {
            ty: Ty::reference(text[object.byte_range()].to_string()),
            is_type: true,
        }),
        "object_creation_expression" => object.child_by_field_name("type").map(|ty| Receiver {
            ty: type_from_node(&ty, text),
            is_type: false,
        }),
        "method_invocation" => {
            method_invocation_receiver(client, text, package, object, depth).await
        }
        "field_access" => field_access_receiver(client, text, package, object, depth).await,
        _ => None,
    }
}

/// The result type of a `recv.name(args)` invocation: the receiver before the
/// callee — `recv`, or the enclosing type for an unqualified `name(args)`
/// (implicit `this`) — resolved to its type, then the named method's declared
/// return type. `None` when that receiver does not resolve or names no method:
/// the fail-safe that leaves a chain with a dead link unresolved (D4).
async fn method_invocation_receiver(
    client: &HubClient,
    text: &str,
    package: Option<&str>,
    call: &Node<'_>,
    depth: usize,
) -> Option<Receiver> {
    let name = &text[call.child_by_field_name("name")?.byte_range()];
    let receiver = match call.child_by_field_name("object") {
        Some(object) => {
            // The recursion is boxed to break the `classify_receiver_at` ↔
            // `method_invocation_receiver` cycle into an indirection.
            Box::pin(classify_receiver_at(
                client,
                text,
                package,
                &object,
                depth + 1,
            ))
            .await?
            .ty
        }
        None => Ty::reference(enclosing_type_name(text, call)?),
    };
    members(client, &receiver, package)
        .await
        .into_iter()
        .find(|member| member.kind == SymbolKind::Method && member.name == name)
        .map(|member| Receiver {
            ty: member.access_type(),
            is_type: false,
        })
}

/// The declared type of `recv.name` field access: the accessed object — `recv` —
/// resolved to its type, then the named field's declared type (an enum constant's
/// type is its enum, so `TYPE.rank()` composes). `None` when the object does not
/// resolve or its type declares no such field: the fail-safe that leaves a chain
/// with a dead link unresolved (D3), symmetric to `method_invocation_receiver`.
async fn field_access_receiver(
    client: &HubClient,
    text: &str,
    package: Option<&str>,
    access: &Node<'_>,
    depth: usize,
) -> Option<Receiver> {
    let name = &text[access.child_by_field_name("field")?.byte_range()];
    let object = access.child_by_field_name("object")?;
    // The recursion is boxed to break the `classify_receiver_at` ↔
    // `field_access_receiver` cycle into an indirection.
    let receiver = Box::pin(classify_receiver_at(
        client,
        text,
        package,
        &object,
        depth + 1,
    ))
    .await?
    .ty;
    members(client, &receiver, package)
        .await
        .into_iter()
        .find(|member| {
            matches!(member.kind, SymbolKind::Field | SymbolKind::EnumConstant)
                && member.name == name
        })
        .map(|member| Receiver {
            ty: member.access_type(),
            is_type: false,
        })
}

/// True when `node` is the name of an unqualified `method_invocation` — a bare
/// call `name(...)` with no `object`, the shape an implicit `this` receiver is.
fn is_implicit_this_use(node: &Node) -> bool {
    if node.kind() != "identifier" {
        return false;
    }
    node.parent().is_some_and(|parent| {
        parent.kind() == "method_invocation"
            && parent.child_by_field_name("object").is_none()
            && parent
                .child_by_field_name("name")
                .is_some_and(|name| name.id() == node.id())
    })
}

/// The `object` of the `recv.name`/`recv.name()` a name node belongs to.
fn member_access_object<'a>(node: &Node<'a>) -> Option<Node<'a>> {
    let parent = node.parent()?;
    match parent.kind() {
        "field_access"
            if parent
                .child_by_field_name("field")
                .is_some_and(|field| field.id() == node.id()) =>
        {
            parent.child_by_field_name("object")
        }
        "method_invocation"
            if parent
                .child_by_field_name("name")
                .is_some_and(|named| named.id() == node.id()) =>
        {
            parent.child_by_field_name("object")
        }
        _ => None,
    }
}

/// The type a bare identifier receiver names, resolved in the enclosing scope:
/// the enclosing method's parameter of that name, then an in-scope local (its
/// declared type, or — for a `var` binding — its initializer's inferred type),
/// then a field of the enclosing type (read from the index), then — if the name
/// is a type — the type itself (`Type.staticMember`), flagged as a type receiver
/// so a caller can offer statics only.
pub(crate) async fn identifier_type(
    client: &HubClient,
    text: &str,
    package: Option<&str>,
    node: &Node<'_>,
    depth: usize,
) -> Option<Receiver> {
    let name = &text[node.byte_range()];
    if let Some(ty) = parameter_type(text, node, name) {
        // A parameter typed by a type variable names an unknown type, not the
        // same-named class the model might hold.
        return resolvable(ty)
            .filter(|ty| !is_type_parameter_ty(node, text, ty))
            .map(|ty| Receiver { ty, is_type: false });
    }
    if let Some(local) = local_type(text, node, name) {
        return match local {
            LocalType::Declared(ty) => resolvable(ty)
                .filter(|ty| !is_type_parameter_ty(node, text, ty))
                .map(|ty| Receiver { ty, is_type: false }),
            // A `var` binding takes its initializer's inferred type, resolved
            // exactly as the initializer expression itself would be; a `var`
            // holds a value, so even an initializer the classifier reads as a
            // type name stays a value receiver.
            LocalType::Var(initializer) => {
                // The recursion is boxed to break the `classify_receiver_at` ↔
                // `identifier_type` cycle into an indirection.
                Box::pin(classify_receiver_at(
                    client,
                    text,
                    package,
                    &initializer,
                    depth + 1,
                ))
                .await
                .map(|receiver| Receiver {
                    ty: receiver.ty,
                    is_type: false,
                })
            }
        };
    }
    if let Some(ty) = field_type(client, text, node, package, name).await {
        return resolvable(ty)
            .filter(|ty| !is_type_parameter_ty(node, text, ty))
            .map(|ty| Receiver { ty, is_type: false });
    }
    if is_type_parameter(node, text, name) {
        return None;
    }
    let ty = Ty::reference(name);
    resolves_type(client, &ty, package)
        .await
        .then_some(Receiver { ty, is_type: true })
}

/// True when `ty` names a type parameter of an enclosing method or type rather
/// than a declared type, so it resolves nothing (D1's generic limitation).
fn is_type_parameter_ty(node: &Node, text: &str, ty: &Ty) -> bool {
    match ty {
        Ty::Ref { name, args } if args.is_empty() => is_type_parameter(node, text, name),
        Ty::Var(name) => is_type_parameter(node, text, name),
        _ => false,
    }
}

/// True when `name` is a type parameter of `node`'s nearest enclosing method or
/// type, walking outward through nested types.
fn is_type_parameter(node: &Node, text: &str, name: &str) -> bool {
    let mut current = node.parent();
    while let Some(ancestor) = current {
        if matches!(
            ancestor.kind(),
            "method_declaration"
                | "constructor_declaration"
                | "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration"
        ) {
            if let Some(params) = ancestor.child_by_field_name("type_parameters") {
                let mut cursor = params.walk();
                for parameter in params.named_children(&mut cursor) {
                    if parameter.kind() != "type_parameter" {
                        continue;
                    }
                    // `type_parameter` has no fields: its name is the
                    // `type_identifier` child (any `type_bound` is ignored).
                    let mut inner = parameter.walk();
                    let named = parameter.named_children(&mut inner).any(|child| {
                        child.kind() == "type_identifier" && &text[child.byte_range()] == name
                    });
                    if named {
                        return true;
                    }
                }
            }
        }
        current = ancestor.parent();
    }
    false
}

/// A [`Ty`] a receiver can resolve through: a reference or a type variable (a
/// primitive, `void`, `null`, an array, or an unknown type names no member — an
/// array's element type is not the receiver, matching
/// [`crate::index::query::members_with_overloads`]'s array guard).
fn resolvable(ty: Ty) -> Option<Ty> {
    matches!(ty, Ty::Ref { .. } | Ty::Var(_)).then_some(ty)
}

/// The declared type of the enclosing method's parameter named `name`.
fn parameter_type(text: &str, node: &Node, name: &str) -> Option<Ty> {
    let method = enclosing_ancestor(node, &["method_declaration", "constructor_declaration"])?;
    let params = method.child_by_field_name("parameters")?;
    let mut cursor = params.walk();
    for param in params.named_children(&mut cursor) {
        if !matches!(param.kind(), "formal_parameter" | "spread_parameter") {
            continue;
        }
        let is_named = param
            .child_by_field_name("name")
            .is_some_and(|node_name| &text[node_name.byte_range()] == name);
        if is_named {
            return param
                .child_by_field_name("type")
                .map(|ty| type_from_node(&ty, text));
        }
    }
    None
}

/// What an in-scope local resolves its type from: a written declared type, or —
/// for a `var` binding — the initializer's expression node, whose type the
/// caller infers through the receiver machinery.
enum LocalType<'a> {
    Declared(Ty),
    Var(Node<'a>),
}

/// The type an in-scope local named `name` declared before `node` resolves from —
/// a `local_variable_declaration` (its written type, or — with `var` — the named
/// declarator's initializer) or an `enhanced_for_statement` binding.
fn local_type<'a>(text: &str, node: &Node<'a>, name: &str) -> Option<LocalType<'a>> {
    let method = enclosing_ancestor(
        node,
        &[
            "method_declaration",
            "constructor_declaration",
            "compact_constructor_declaration",
        ],
    )?;
    let mut stack = vec![method];
    while let Some(current) = stack.pop() {
        if current.start_byte() < node.start_byte() {
            match current.kind() {
                "local_variable_declaration" => {
                    let is_var = current
                        .child_by_field_name("type")
                        .is_some_and(|ty| &text[ty.byte_range()] == "var");
                    let ty = current
                        .child_by_field_name("type")
                        .map(|ty| type_from_node(&ty, text));
                    let mut cursor = current.walk();
                    for declarator in current.named_children(&mut cursor) {
                        if declarator.kind() != "variable_declarator" {
                            continue;
                        }
                        let Some(node_name) = declarator.child_by_field_name("name") else {
                            continue;
                        };
                        if &text[node_name.byte_range()] != name {
                            continue;
                        }
                        if is_var {
                            // `var` takes the initializer's inferred type; a
                            // binding without one (invalid Java) resolves nothing.
                            let value = declarator.child_by_field_name("value")?;
                            return Some(LocalType::Var(value));
                        }
                        return ty.map(LocalType::Declared);
                    }
                }
                "enhanced_for_statement" => {
                    let is_named = current
                        .child_by_field_name("name")
                        .is_some_and(|node_name| &text[node_name.byte_range()] == name);
                    if is_named {
                        return current
                            .child_by_field_name("type")
                            .map(|ty| LocalType::Declared(type_from_node(&ty, text)));
                    }
                }
                _ => {}
            }
        }
        let mut cursor = current.walk();
        for child in current.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    None
}

/// The declared type of a field named `name` on the enclosing type, read from the
/// index (inherited fields included).
async fn field_type(
    client: &HubClient,
    text: &str,
    node: &Node<'_>,
    package: Option<&str>,
    name: &str,
) -> Option<Ty> {
    let enclosing = enclosing_type_name(text, node)?;
    members(client, &Ty::reference(enclosing), package)
        .await
        .into_iter()
        .find(|member| member.kind == SymbolKind::Field && member.name == name)
        .map(|member| member.ty)
}

/// The innermost enclosing type declaration's name.
pub(crate) fn enclosing_type_name(text: &str, node: &Node) -> Option<String> {
    let declaration = enclosing_ancestor(
        node,
        &[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
            "record_declaration",
            "annotation_type_declaration",
        ],
    )?;
    declaration
        .child_by_field_name("name")
        .map(|name| text[name.byte_range()].to_string())
}

/// The nearest ancestor of one of `kinds`.
fn enclosing_ancestor<'a>(node: &Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    let mut current = node.parent();
    while let Some(candidate) = current {
        if kinds.contains(&candidate.kind()) {
            return Some(candidate);
        }
        current = candidate.parent();
    }
    None
}
