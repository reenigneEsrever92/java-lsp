//! Cursor navigation: the declaration the symbol under the cursor names, and
//! every reference to it across the workspace sources.
//!
//! Navigation is its own hub module, labelled `navigate`. It reads Java — the
//! requested file to find the symbol under the cursor, and each candidate source
//! to find its occurrences — and reads the index only through the hub, over the
//! coarse vocabulary [`crate::messages::IndexRequest`] lays out. It answers the
//! shell's `FindDeclaration`, `FindReferences`, and `FindImplementation` requests
//! ([`crate::messages::NavigateRequest`]).
//!
//! Resolution is name-based, matching the index's extraction: the cursor
//! resolves to the declarations of the name it sits on, narrowed by what the node
//! kind says (a type position names a type, a member position a member). A type
//! name is first narrowed to what the cursor's file can see — its own, its
//! package's, or an imported type — so a same-named type elsewhere does not make
//! a visible use ambiguous; what remains must share one file, because a name
//! spread across files is no answer at all — no result beats a wrong result.
//! A cursor on a declaration's own name is the exception: it resolves to that
//! declaration, and only a use keeps the cross-file ambiguity policy.
//! A member cursor is resolved further through the receiver's declared type: the
//! expression before the `.` — `this`, an implicit `this`, a field of the
//! enclosing type, a parameter or local of the enclosing method, `new T(…)`, or a
//! `Type.staticMember` qualifier — is turned into a type through the index's
//! declared-type model (read as [`crate::messages::IndexRequest::MembersOf`] and
//! [`crate::messages::IndexRequest::TypeClosure`]), and the name is narrowed to
//! the nearest declaration the receiver's supertype closure provides (most-derived
//! first, so an override or a concrete-typed receiver resolves). A name with no
//! receiver-resolvable relationship keeps the ambiguity policy.
//! Type references are found soundly: every `Type` occurrence of a type the file
//! can see (its own file, its package, or an import). Member references are found
//! where the receiver's resolved type (or a supertype) declares the target, or,
//! for a bare name, inside the declaring type's own span. A member receiver may
//! itself be a call (`a.foo()`) or a field access (`a.field`), both resolved
//! recursively through the shared receiver step; a chain resolves left to right
//! while every link's own receiver can be typed, and breaks at a link that
//! cannot. Documented limitations: a nested-type or package qualifier, `super`,
//! a generic type argument, and a lambda or anonymous-class scope are not
//! resolved through; a bare field use (a bare identifier that is not an
//! unqualified call) is not narrowed; and a receiver whose declared type is a
//! generic type-variable name is not resolved.
//!
//! Navigation is async: it reads source files with tokio and awaits each index
//! read over the hub. The deep AST walks stay synchronous — an async-recursive
//! walk would need boxing — so occurrence collection runs a synchronous pass that
//! gathers the nodes it would resolve and a flat async loop that resolves each
//! gathered node through the index, batching the distinct receivers' closure reads.

use std::collections::HashSet;
use std::sync::Arc;

use tower_lsp::lsp_types::{Location, Position, Range, Url};
use tree_sitter::{Node, Tree};

use crate::hub::{Hub, HubClient};
use crate::index::{FileOrigin, SymbolEntry};
use crate::messages::{
    IndexBatchRequest, IndexBatchResponse, IndexRequest, IndexResponse, MemberSelector, Message,
    NavigateRequest, NavigateResponse, Request, Response,
};
use crate::model::{Prim, SymbolKind, Ty};
use crate::parser::{
    byte_offset, file_package, java_parser, type_from_node, word_at, word_prefix, LineIndex,
};
use crate::util::{enclosing_type_name, identifier_type, receiver_type_of};

// ---------------------------------------------------------------------------
// The hub client
// ---------------------------------------------------------------------------

/// Starts navigation on the hub as the client labelled `navigate`, serving the
/// shell's [`NavigateRequest`]s and answering each with a [`NavigateResponse`].
pub fn spawn(hub: &Hub) {
    let hub_client = hub.register("navigate");

    tokio::spawn(async move {
        let mut notifications = hub_client.subscribe().await;
        let mut requests = hub_client.serve().await;

        loop {
            tokio::select! {
                // Navigation needs no notification, but taking the subscription
                // and draining it keeps the hub's broadcast from backing up on
                // this client.
                Some(_) = notifications.recv() => {}
                Some(envelope) = requests.recv() => {
                    let Message::Request(Request::Navigate(request)) = &*envelope.message else {
                        continue;
                    };
                    let request = request.clone();
                    let id = envelope.id;
                    let response = serve(&hub_client, request).await;
                    hub_client.respond(id, response);
                }
                else => break,
            }
        }
    });
}

/// Answers one navigation request.
async fn serve(client: &HubClient, request: NavigateRequest) -> Response {
    match request {
        NavigateRequest::FindDeclaration { uri, position } => Response::Navigate(
            NavigateResponse::Declaration(find_declaration(client, &uri, position).await),
        ),
        NavigateRequest::FindReferences {
            uri,
            position,
            include_declaration,
        } => Response::Navigate(NavigateResponse::References(
            find_references(client, &uri, position, include_declaration).await,
        )),
        NavigateRequest::FindImplementation { uri, position } => Response::Navigate(
            NavigateResponse::Implementations(find_implementation(client, &uri, position).await),
        ),
    }
}

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

/// Awaits several index reads in one round-trip, in request order.
async fn read_batch(client: &HubClient, requests: Vec<IndexRequest>) -> Vec<IndexResponse> {
    if requests.is_empty() {
        return Vec::new();
    }
    match client
        .request(Request::IndexBatch(IndexBatchRequest { requests }))
        .await
    {
        Response::IndexBatch(IndexBatchResponse(responses)) => responses,
        _ => Vec::new(),
    }
}

/// Every entry declared with exactly `name`.
async fn named(client: &HubClient, name: &str) -> Vec<Arc<SymbolEntry>> {
    match read(
        client,
        IndexRequest::EntriesByName {
            name: name.to_string(),
        },
    )
    .await
    {
        Some(IndexResponse::Entries(entries)) => entries,
        _ => Vec::new(),
    }
}

/// The `(simple name, package)` identities of a type and its supertype closure.
async fn type_closure(
    client: &HubClient,
    ty: &Ty,
    package: Option<&str>,
) -> Vec<(String, Option<String>)> {
    match read(
        client,
        IndexRequest::TypeClosure {
            ty: ty.clone(),
            package: package.map(str::to_string),
        },
    )
    .await
    {
        Some(IndexResponse::Closure(closure)) => closure,
        _ => Vec::new(),
    }
}

/// Every workspace source file the reference search considers, in index order.
async fn source_files(client: &HubClient) -> Vec<Url> {
    match read(client, IndexRequest::WorkspaceFiles).await {
        Some(IndexResponse::Files(files)) => files,
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// The queries
// ---------------------------------------------------------------------------

/// The declaration the symbol at `position` in `uri` names, or `None` when the
/// cursor names nothing or several files declare the name. A cursor on a
/// declaration's own name resolves to that declaration — except an overriding
/// member, which resolves to the nearest declaration it overrides in its type's
/// supertype closure — not to the cross-file ambiguity policy that governs a use.
async fn find_declaration(client: &HubClient, uri: &Url, position: Position) -> Option<Location> {
    let (text, tree) = read_and_parse(client, uri).await?;
    let offset = byte_offset(&text, position);
    if let Some(location) = own_declaration(client, uri, &text, &tree, offset).await {
        return overridden_declaration(client, uri, &text, &tree, offset)
            .await
            .or(Some(location));
    }
    unique_location(declaration_candidates(client, uri, &text, &tree, offset).await)
}

/// The declaration the cursor at `offset` sits on, when it is the `name` field of
/// an indexed declaration in `uri` itself — a type, method, constructor, field,
/// or enum constant: the entry in the cursor's own file whose selection range is
/// exactly the name node's. `None` when the cursor is on a use, or on a
/// declaration the symbol table does not hold (a local, a parameter, an
/// annotation type), so the caller falls back to the cross-file policy.
async fn own_declaration(
    client: &HubClient,
    uri: &Url,
    text: &str,
    tree: &Tree,
    offset: usize,
) -> Option<Location> {
    let word = word_at(text, offset);
    if word.is_empty() {
        return None;
    }
    let start = offset - word_prefix(text, offset).len();
    let node = tree
        .root_node()
        .descendant_for_byte_range(start, start + word.len())?;
    if !is_indexed_declaration_name(&node) {
        return None;
    }
    let lines = LineIndex::new(text);
    let range = Range {
        start: lines.position(text, node.start_byte()),
        end: lines.position(text, node.end_byte()),
    };
    let Some(IndexResponse::File(Some(entries))) =
        read(client, IndexRequest::FileSymbols { uri: uri.clone() }).await
    else {
        return None;
    };
    entries
        .iter()
        .find(|entry| entry.selection_range == range)
        .map(|entry| Location {
            uri: uri.clone(),
            range: entry.selection_range,
        })
}

/// The nearest declaration of the cursor's member name in the enclosing type's
/// supertype closure — the contract an override implements — or `None` when the
/// cursor is not an overriding method or field declaration, or no openable
/// supertype declares the name. A type, constructor, or local keeps
/// self-resolution; a library/JDK supertype declaration is not an openable target,
/// so it does too.
async fn overridden_declaration(
    client: &HubClient,
    uri: &Url,
    text: &str,
    tree: &Tree,
    offset: usize,
) -> Option<Location> {
    let word = word_at(text, offset);
    if word.is_empty() {
        return None;
    }
    let start = offset - word_prefix(text, offset).len();
    let node = tree
        .root_node()
        .descendant_for_byte_range(start, start + word.len())?;
    let parent = node.parent()?;
    let kind = match parent.kind() {
        "method_declaration" => SymbolKind::Method,
        "variable_declarator" => SymbolKind::Field,
        _ => return None,
    };
    if !parent
        .child_by_field_name("name")
        .is_some_and(|name| name.id() == node.id())
    {
        return None;
    }
    let enclosing = enclosing_type_name(text, &node)?;
    let package = file_package(tree, text);
    let closure = type_closure(
        client,
        &Ty::reference(enclosing.clone()),
        package.as_deref(),
    )
    .await;
    if closure.is_empty() {
        return None;
    }
    let override_params =
        (kind == SymbolKind::Method).then(|| method_parameter_types(&parent, text));
    let entries: Vec<Arc<SymbolEntry>> = named(client, word)
        .await
        .into_iter()
        .filter(|entry| entry.origin != FileOrigin::Dependency && entry.kind == kind)
        .collect();
    // The closure is root first, so the first supertype that declares the name is
    // the nearest overridden declaration.
    for identity in &closure {
        if identity.0 == enclosing && identity.1 == package {
            continue;
        }
        let mut declaring: Vec<Arc<SymbolEntry>> = entries
            .iter()
            .filter(|entry| declared_by(identity, entry))
            .cloned()
            .collect();
        if declaring.is_empty() {
            continue;
        }
        declaring.sort_by(|a, b| {
            a.uri
                .as_str()
                .cmp(b.uri.as_str())
                .then_with(|| {
                    a.selection_range
                        .start
                        .line
                        .cmp(&b.selection_range.start.line)
                })
                .then_with(|| {
                    a.selection_range
                        .start
                        .character
                        .cmp(&b.selection_range.start.character)
                })
        });
        // Prefer the overload whose parameter types match the override's; fall
        // back to the nearest declaration by position.
        let chosen = match &override_params {
            Some(params) => {
                let mut best = None;
                for entry in &declaring {
                    let matches = declaration_parameter_types(client, uri, text, tree, entry)
                        .await
                        .as_ref()
                        == Some(params);
                    if matches {
                        best = Some(Arc::clone(entry));
                        break;
                    }
                }
                best.or_else(|| declaring.first().cloned())
            }
            None => declaring.first().cloned(),
        };
        return chosen.map(|entry| Location {
            uri: (*entry.uri).clone(),
            range: entry.selection_range,
        });
    }
    None
}

/// The declared parameter types of the method `entry` names: read from `uri`'s
/// tree when the entry is in the cursor's own file, otherwise by re-reading the
/// entry's source. `None` when that source cannot be read or declares no method.
async fn declaration_parameter_types(
    client: &HubClient,
    uri: &Url,
    text: &str,
    tree: &Tree,
    entry: &SymbolEntry,
) -> Option<Vec<Ty>> {
    if entry.uri.as_ref() == uri {
        return method_params_at(text, tree, entry.selection_range);
    }
    let (source_text, source_tree) = read_and_parse(client, &entry.uri).await?;
    method_params_at(&source_text, &source_tree, entry.selection_range)
}

/// The declared parameter types of the method declaration enclosing `range` in a
/// parsed file, or `None` when no method declaration does.
fn method_params_at(text: &str, tree: &Tree, range: Range) -> Option<Vec<Ty>> {
    let start = byte_offset(text, range.start);
    let mut current = Some(tree.root_node().descendant_for_byte_range(start, start)?);
    while let Some(node) = current {
        if node.kind() == "method_declaration" {
            return Some(method_parameter_types(&node, text));
        }
        current = node.parent();
    }
    None
}

/// The declared parameter types of a method declaration, as written.
fn method_parameter_types(method: &Node, text: &str) -> Vec<Ty> {
    let Some(parameters) = method.child_by_field_name("parameters") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = parameters.walk();
    for parameter in parameters.named_children(&mut cursor) {
        if !matches!(parameter.kind(), "formal_parameter" | "spread_parameter") {
            continue;
        }
        if let Some(ty) = parameter.child_by_field_name("type") {
            out.push(type_from_node(&ty, text));
        }
    }
    out
}

/// The workspace declarations implementing the contract the symbol at `position`
/// names: for a type, every workspace type whose supertype closure reaches it,
/// transitively; for a member, every workspace subtype that declares an override
/// of it. Empty when the cursor names no contract or nothing implements it.
async fn find_implementation(client: &HubClient, uri: &Url, position: Position) -> Vec<Location> {
    let Some((text, tree)) = read_and_parse(client, uri).await else {
        return Vec::new();
    };
    let offset = byte_offset(&text, position);
    let Some((contract, member)) = implementation_contract(client, &text, &tree, offset).await
    else {
        return Vec::new();
    };
    let package = file_package(&tree, &text);
    let response = read(
        client,
        IndexRequest::ImplementationsOf {
            contract,
            package,
            member,
        },
    )
    .await;
    let Some(IndexResponse::Entries(entries)) = response else {
        return Vec::new();
    };
    let mut out: Vec<Location> = entries
        .iter()
        .map(|entry| Location {
            uri: (*entry.uri).clone(),
            range: entry.selection_range,
        })
        .collect();
    out.sort_by(|a, b| {
        a.uri
            .as_str()
            .cmp(b.uri.as_str())
            .then_with(|| a.range.start.line.cmp(&b.range.start.line))
            .then_with(|| a.range.start.character.cmp(&b.range.start.character))
    });
    out.dedup();
    out
}

/// The contract the cursor at `offset` names — a type, or a member of a type,
/// with the member selector for the latter — or `None` for a cursor that names no
/// contract (a field, constructor, local, or an unresolvable one). Unlike
/// definition, a library or JDK type may be the contract's owner, since every
/// implementation is a workspace declaration.
async fn implementation_contract(
    client: &HubClient,
    text: &str,
    tree: &Tree,
    offset: usize,
) -> Option<(Ty, Option<MemberSelector>)> {
    let node = tree.root_node().descendant_for_byte_range(offset, offset)?;
    // A written type names a contract directly.
    if matches!(node.kind(), "type_identifier" | "scoped_type_identifier") {
        return Some((type_from_node(&node, text), None));
    }
    if node.kind() != "identifier" {
        return None;
    }
    let name = text[node.byte_range()].to_string();
    let parent = node.parent()?;
    // A type declaration's own name names the type itself.
    if matches!(
        parent.kind(),
        "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
    ) && parent
        .child_by_field_name("name")
        .is_some_and(|named| named.id() == node.id())
    {
        return Some((Ty::reference(name), None));
    }
    // A method declaration's own name is a member of its enclosing type.
    if parent.kind() == "method_declaration"
        && parent
            .child_by_field_name("name")
            .is_some_and(|named| named.id() == node.id())
    {
        let owner = enclosing_type_name(text, &node)?;
        let params = method_parameter_types(&parent, text);
        return Some((
            Ty::reference(owner),
            Some(MemberSelector {
                name,
                params: Some(params),
            }),
        ));
    }
    // A member call `recv.name(…)` is a member of the receiver's type.
    if parent.kind() == "method_invocation"
        && parent
            .child_by_field_name("name")
            .is_some_and(|named| named.id() == node.id())
    {
        let package = file_package(tree, text);
        let owner = receiver_type_of(client, text, package.as_deref(), &node).await?;
        let params = call_argument_types(client, text, package.as_deref(), &parent).await;
        return Some((owner, Some(MemberSelector { name, params })));
    }
    None
}

/// The declared types of a call's arguments, when every one resolves — the pinned
/// overload's parameter types. `None` when any argument's type is not resolvable,
/// so the member is matched by name alone.
async fn call_argument_types(
    client: &HubClient,
    text: &str,
    package: Option<&str>,
    call: &Node<'_>,
) -> Option<Vec<Ty>> {
    let arguments = call.child_by_field_name("arguments")?;
    let mut out = Vec::new();
    let mut cursor = arguments.walk();
    for argument in arguments.named_children(&mut cursor) {
        out.push(argument_type(client, text, package, &argument).await?);
    }
    Some(out)
}

/// The declared type of an argument expression, for the shapes a receiver
/// resolves too — an identifier, `this`, a type, `new T(…)`, a cast — plus the
/// literals; `None` for any shape whose type is not resolved.
async fn argument_type(
    client: &HubClient,
    text: &str,
    package: Option<&str>,
    node: &Node<'_>,
) -> Option<Ty> {
    match node.kind() {
        "identifier" => identifier_type(client, text, package, node, 0)
            .await
            .map(|receiver| receiver.ty),
        "this" => enclosing_type_name(text, node).map(Ty::reference),
        "type_identifier" | "scoped_type_identifier" => {
            Some(Ty::reference(text[node.byte_range()].to_string()))
        }
        "object_creation_expression" | "cast_expression" => node
            .child_by_field_name("type")
            .map(|ty| type_from_node(&ty, text)),
        "decimal_integer_literal" | "hex_integer_literal" => Some(Ty::Prim(Prim::Int)),
        "decimal_floating_point_literal" => Some(Ty::Prim(Prim::Double)),
        "string_literal" => Some(Ty::reference("String")),
        "character_literal" => Some(Ty::Prim(Prim::Char)),
        "true" | "false" => Some(Ty::Prim(Prim::Boolean)),
        _ => None,
    }
}

/// Every occurrence of the symbol at `position` in `uri` across the workspace
/// sources, sorted; the declaration sites are included only when
/// `include_declaration`. Empty when the cursor names an ambiguous or unknown
/// target.
async fn find_references(
    client: &HubClient,
    uri: &Url,
    position: Position,
    include_declaration: bool,
) -> Vec<Location> {
    let Some((text, tree)) = read_and_parse(client, uri).await else {
        return Vec::new();
    };
    let offset = byte_offset(&text, position);
    let Some(candidates) =
        unique_candidates(declaration_candidates(client, uri, &text, &tree, offset).await)
    else {
        return Vec::new();
    };
    let Some(target) = Target::from(&candidates) else {
        return Vec::new();
    };

    let mut out: Vec<Location> = Vec::new();
    let mut seen: HashSet<(String, u32, u32)> = HashSet::new();

    let mut sources = source_files(client).await;
    if !sources.iter().any(|source| source == uri) {
        sources.push(uri.clone());
    }
    sources.sort_by(|a, b| a.as_str().cmp(b.as_str()));

    for source in sources {
        let Some((file_text, file_tree)) = read_and_parse(client, &source).await else {
            continue;
        };
        // Cheap prefilter: only a file that mentions the name can hold a hit.
        if !file_text.contains(&target.name) {
            continue;
        }
        // A type occurrence needs the file to be able to see the type. A member
        // occurrence is instead attributed by resolving its receiver in the file's
        // own tree (below) — a stronger, precise test — so the type-visibility
        // prefilter is skipped for members.
        if target.is_type && !target.visible_in(&source, &file_text, &file_tree) {
            continue;
        }
        let lines = LineIndex::new(&file_text);
        if target.is_type {
            collect_type_occurrences(
                &file_tree.root_node(),
                &file_text,
                &target.name,
                &source,
                &lines,
                &mut out,
                &mut seen,
            );
        } else {
            let owner_span = owner_span_in(client, &target, &source, &file_text).await;
            let package = file_package(&file_tree, &file_text);
            collect_member_occurrences(
                client,
                &file_tree.root_node(),
                &file_text,
                &target,
                &source,
                package.as_deref(),
                &lines,
                owner_span,
                &mut out,
                &mut seen,
            )
            .await;
        }
    }

    if include_declaration {
        for entry in &target.declarations {
            push_range(
                (*entry.uri).clone(),
                entry.selection_range,
                &mut out,
                &mut seen,
            );
        }
    }

    out.sort_by(|a, b| {
        a.uri
            .as_str()
            .cmp(b.uri.as_str())
            .then_with(|| a.range.start.line.cmp(&b.range.start.line))
            .then_with(|| a.range.start.character.cmp(&b.range.start.character))
    });
    out
}

/// The target a reference search is centered on: the declaration's name, whether
/// it names a type or a member, the declaring type it belongs to (itself, for a
/// type), its package, and the declaration entries themselves.
struct Target {
    name: String,
    is_type: bool,
    /// The simple name of the type a member belongs to; a type's own name.
    owner: String,
    package: Option<String>,
    declarations: Vec<Arc<SymbolEntry>>,
}

impl Target {
    /// The target the resolved `candidates` name, or `None` when they do not
    /// agree on one declaring type (an overloaded name shared by several types is
    /// no target).
    fn from(candidates: &[Arc<SymbolEntry>]) -> Option<Target> {
        let first = candidates.first()?;
        let is_type = first.kind.is_type();
        let owner = if is_type {
            first.name.to_string()
        } else {
            first.container.last().cloned().unwrap_or_default()
        };
        if owner.is_empty() {
            return None;
        }
        let package = first.package.as_deref().map(str::to_string);
        let container = Arc::clone(&first.container);
        let agrees = candidates.iter().all(|entry| {
            entry.kind.is_type() == is_type
                && entry.container == container
                && entry.package.as_deref() == package.as_deref()
        });
        if !agrees {
            return None;
        }
        Some(Target {
            name: first.name.to_string(),
            is_type,
            owner,
            package,
            declarations: candidates.to_vec(),
        })
    }

    /// Whether a file can name this target: its own file, its package, or a file
    /// that imports the declaring type (exactly or via a package wildcard).
    fn visible_in(&self, uri: &Url, text: &str, tree: &Tree) -> bool {
        if self
            .declarations
            .iter()
            .any(|entry| entry.uri.as_ref() == uri)
        {
            return true;
        }
        if file_package(tree, text) == self.package {
            return true;
        }
        let Some(package) = &self.package else {
            return false;
        };
        let exact = format!("{package}.{}", self.owner);
        let wildcard = format!("{package}.*");
        imports_of(tree, text)
            .into_iter()
            .filter(|import| !import.is_static)
            .any(|import| import.path == exact || import.path == wildcard)
    }
}

/// Whether the cursor's file can name the type `entry`: it is declared in the
/// file itself, in the file's own package, or imported into it — exactly, or via
/// a package wildcard. Only the visibility that pins a simple name to one type
/// is recognised; a miss leaves the candidate set unchanged (the caller falls
/// back), so a nested type or a qualified reference is never dropped.
fn type_visible_in_file(entry: &SymbolEntry, uri: &Url, text: &str, tree: &Tree) -> bool {
    if entry.uri.as_ref() == uri {
        return true;
    }
    if entry.package.as_deref() == file_package(tree, text).as_deref() {
        return true;
    }
    let Some(package) = entry.package.as_deref() else {
        return false;
    };
    let exact = qualified_type_name(package, entry);
    let wildcard = format!("{package}.*");
    imports_of(tree, text)
        .into_iter()
        .filter(|import| !import.is_static)
        .any(|import| import.path == exact || import.path == wildcard)
}

/// The dotted name a type is imported by: its package, its enclosing-type chain,
/// and its own name (`a.Outer.Inner`), matching how an import path is written.
fn qualified_type_name(package: &str, entry: &SymbolEntry) -> String {
    let mut segments: Vec<&str> = Vec::with_capacity(entry.container.len() + 2);
    segments.push(package);
    segments.extend(entry.container.iter().map(String::as_str));
    segments.push(&entry.name);
    segments.join(".")
}

// ---------------------------------------------------------------------------
// Resolving the cursor to its declarations
// ---------------------------------------------------------------------------

/// The declaration entries the cursor at `offset` names: the last segment of an
/// import path, or a plain identifier narrowed by what its node kind says.
/// Dependencies (a class-file jar or JDK location that cannot be opened) are
/// excluded, as are import entries themselves.
async fn declaration_candidates(
    client: &HubClient,
    uri: &Url,
    text: &str,
    tree: &Tree,
    offset: usize,
) -> Vec<Arc<SymbolEntry>> {
    let word = word_at(text, offset);
    if word.is_empty() {
        return Vec::new();
    }
    let Some(node) = tree.root_node().descendant_for_byte_range(offset, offset) else {
        return Vec::new();
    };

    // An import-qualified cursor: only the dotted path's last segment names the
    // target, so a cursor on an earlier segment names nothing.
    let mut ancestor = Some(node);
    while let Some(current) = ancestor {
        if current.kind() == "import_declaration" {
            let (path, is_static) = import_parts(text, &current);
            let simple = path.rsplit('.').next().unwrap_or("");
            if simple.is_empty() || simple == "*" || simple != word {
                return Vec::new();
            }
            return named(client, simple)
                .await
                .into_iter()
                .filter(|entry| entry.origin != FileOrigin::Dependency)
                .filter(|entry| {
                    if is_static {
                        entry.kind != SymbolKind::Import
                    } else {
                        entry.kind.is_type()
                    }
                })
                .collect();
        }
        ancestor = current.parent();
    }

    let constraint = name_constraint(&node);
    let mut candidates: Vec<Arc<SymbolEntry>> = named(client, word)
        .await
        .into_iter()
        .filter(|entry| entry.origin != FileOrigin::Dependency)
        .filter(|entry| entry.kind != SymbolKind::Import)
        .filter(|entry| match constraint {
            NameConstraint::Type => entry.kind.is_type(),
            NameConstraint::Member => matches!(
                entry.kind,
                SymbolKind::Method | SymbolKind::Field | SymbolKind::EnumConstant
            ),
            NameConstraint::Any => true,
        })
        .collect();

    // A type name resolves against what the cursor's file can see. Narrowing the
    // candidates to the types the file can name — its own, its package's, or an
    // import — before the cross-file ambiguity policy runs keeps a same-named
    // type in another package (`b.Widget`) from making a visible use of
    // `a.Widget` ambiguous. When nothing is visible the set is left whole, so a
    // reference the predicate does not recognise (a nested type, a qualified
    // name) is never dropped.
    if matches!(constraint, NameConstraint::Type) {
        let visible: Vec<Arc<SymbolEntry>> = candidates
            .iter()
            .filter(|entry| type_visible_in_file(entry, uri, text, tree))
            .cloned()
            .collect();
        if !visible.is_empty() {
            candidates = visible;
        }
    }

    // A member cursor is resolved through the receiver's declared type (D1): keep
    // only the entries the nearest type in the receiver's supertype closure
    // declares. The walk is root-first, mirroring `members_with_overloads`'s
    // first-wins order, so an override or a concrete-typed receiver resolves to the
    // most-derived declaration rather than spanning the subtype and the supertype
    // (its own overloads all stay). A closure that declares the name nowhere leaves
    // no candidate (no result beats a wrong result); when the receiver does not
    // resolve to a type at all, the whole name-only set stands, so the ambiguity
    // policy still governs it (D4).
    if !matches!(constraint, NameConstraint::Type) {
        let package = file_package(tree, text);
        if let Some(receiver) = receiver_type_of(client, text, package.as_deref(), &node).await {
            let closure = type_closure(client, &receiver, package.as_deref()).await;
            if !closure.is_empty() {
                match closure
                    .iter()
                    .find(|identity| candidates.iter().any(|entry| declared_by(identity, entry)))
                {
                    Some(identity) => {
                        candidates.retain(|entry| declared_by(identity, entry));
                    }
                    None => candidates.clear(),
                }
            }
        }
    }
    candidates
}

/// Sorts candidates into a canonical order and returns them when they share one
/// file — the ambiguity policy made concrete: a single candidate, or several in
/// one file (overloads, same-file repeats), is an answer; a name spread across
/// files is not.
fn unique_candidates(mut candidates: Vec<Arc<SymbolEntry>>) -> Option<Vec<Arc<SymbolEntry>>> {
    candidates.sort_by(|a, b| {
        a.uri
            .as_str()
            .cmp(b.uri.as_str())
            .then_with(|| {
                a.selection_range
                    .start
                    .line
                    .cmp(&b.selection_range.start.line)
            })
            .then_with(|| {
                a.selection_range
                    .start
                    .character
                    .cmp(&b.selection_range.start.character)
            })
    });
    {
        let (first, rest) = candidates.split_first()?;
        if rest.iter().any(|entry| entry.uri != first.uri) {
            return None;
        }
    }
    Some(candidates)
}

/// The single location `candidates` resolve to, or `None` when they are
/// ambiguous ([`unique_candidates`]).
fn unique_location(candidates: Vec<Arc<SymbolEntry>>) -> Option<Location> {
    let first = unique_candidates(candidates)?.first().cloned()?;
    Some(Location {
        uri: (*first.uri).clone(),
        range: first.selection_range,
    })
}

// ---------------------------------------------------------------------------
// Occurrence collection
// ---------------------------------------------------------------------------

/// Records every `Type` occurrence of a type name: a `type_identifier`, or the
/// last segment of an import path. A declaration's own name is not an occurrence.
fn collect_type_occurrences(
    node: &Node,
    text: &str,
    name: &str,
    uri: &Url,
    lines: &LineIndex,
    out: &mut Vec<Location>,
    seen: &mut HashSet<(String, u32, u32)>,
) {
    if &text[node.byte_range()] == name && !is_declaration_name(node) {
        match node.kind() {
            "type_identifier" => push_location(lines, node, text, uri, out, seen),
            "identifier" if has_import_ancestor(node) => {
                push_location(lines, node, text, uri, out, seen)
            }
            _ => {}
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_type_occurrences(&child, text, name, uri, lines, out, seen);
    }
}

/// A synchronous pass over `node` gathering the member-access name nodes whose
/// receiver must be resolved through the index. The bare-name uses — an implicit
/// `this` inside the declaring type's own span — are attributed inline, since
/// that test needs nothing from the index.
#[allow(clippy::too_many_arguments)]
fn collect_member_candidates<'a>(
    node: &Node<'a>,
    text: &str,
    target: &Target,
    uri: &Url,
    lines: &LineIndex,
    owner_span: Option<(usize, usize)>,
    out: &mut Vec<Location>,
    seen: &mut HashSet<(String, u32, u32)>,
    pending: &mut Vec<Node<'a>>,
) {
    if node.kind() == "identifier"
        && &text[node.byte_range()] == target.name.as_str()
        && !is_declaration_name(node)
    {
        if is_member_access_name(node) {
            pending.push(*node);
        } else if owner_span
            .is_some_and(|(start, end)| node.start_byte() >= start && node.end_byte() <= end)
        {
            push_location(lines, node, text, uri, out, seen);
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_member_candidates(
            &child, text, target, uri, lines, owner_span, out, seen, pending,
        );
    }
}

/// Records every occurrence of a member name whose receiver resolves to the
/// declaring type: a `recv.member` whose receiver's declared type or a supertype
/// declares the target, or a bare identifier inside the declaring type's own span
/// (an implicit `this`). A `recv.member` on an unresolved or unrelated receiver is
/// not an occurrence. The synchronous pass gathers the member accesses; the flat
/// loop below resolves each receiver and its supertype closure — the distinct
/// receivers batched into one read — so the AST walk never recurses across an
/// `await`.
#[allow(clippy::too_many_arguments)]
async fn collect_member_occurrences(
    client: &HubClient,
    root: &Node<'_>,
    text: &str,
    target: &Target,
    uri: &Url,
    package: Option<&str>,
    lines: &LineIndex,
    owner_span: Option<(usize, usize)>,
    out: &mut Vec<Location>,
    seen: &mut HashSet<(String, u32, u32)>,
) {
    let mut pending: Vec<Node> = Vec::new();
    collect_member_candidates(
        root,
        text,
        target,
        uri,
        lines,
        owner_span,
        out,
        seen,
        &mut pending,
    );
    if pending.is_empty() {
        return;
    }

    // Resolve each gathered member access's receiver, then fetch every distinct
    // receiver's supertype closure in one batched read.
    let mut receivers: Vec<Option<Ty>> = Vec::with_capacity(pending.len());
    for node in &pending {
        receivers.push(receiver_type_of(client, text, package, node).await);
    }
    let mut distinct: Vec<Ty> = Vec::new();
    for receiver in receivers.iter().flatten() {
        if !distinct.contains(receiver) {
            distinct.push(receiver.clone());
        }
    }
    let closures = read_batch(
        client,
        distinct
            .iter()
            .map(|ty| IndexRequest::TypeClosure {
                ty: ty.clone(),
                package: package.map(str::to_string),
            })
            .collect(),
    )
    .await;

    for (node, receiver) in pending.iter().zip(&receivers) {
        let Some(receiver) = receiver else {
            continue;
        };
        let Some(index) = distinct.iter().position(|ty| ty == receiver) else {
            continue;
        };
        let matches = matches!(
            closures.get(index),
            Some(IndexResponse::Closure(closure))
                if closure
                    .iter()
                    .any(|(name, scope)| name == &target.owner && scope == &target.package)
        );
        if matches {
            push_location(lines, node, text, uri, out, seen);
        }
    }
}

/// The byte span of the declaring type's own declaration in `uri`, when `uri`
/// declares it — the bounds of an implicit `this` for unqualified member uses.
async fn owner_span_in(
    client: &HubClient,
    target: &Target,
    uri: &Url,
    text: &str,
) -> Option<(usize, usize)> {
    let entries = named(client, &target.owner).await;
    let entry = entries.iter().find(|entry| {
        entry.kind.is_type()
            && entry.uri.as_ref() == uri
            && entry.package.as_deref() == target.package.as_deref()
    })?;
    Some((
        byte_offset(text, entry.full_range.start),
        byte_offset(text, entry.full_range.end),
    ))
}

/// Whether `entry` is declared by the `(name, package)` identity — its innermost
/// enclosing type's simple name and its package.
fn declared_by(identity: &(String, Option<String>), entry: &SymbolEntry) -> bool {
    let Some(owner) = entry.container.last() else {
        return false;
    };
    identity.0 == *owner && identity.1.as_deref() == entry.package.as_deref()
}

fn push_location(
    lines: &LineIndex,
    node: &Node,
    text: &str,
    uri: &Url,
    out: &mut Vec<Location>,
    seen: &mut HashSet<(String, u32, u32)>,
) {
    let range = Range {
        start: lines.position(text, node.start_byte()),
        end: lines.position(text, node.end_byte()),
    };
    push_range(uri.clone(), range, out, seen);
}

fn push_range(
    uri: Url,
    range: Range,
    out: &mut Vec<Location>,
    seen: &mut HashSet<(String, u32, u32)>,
) {
    let key = (
        uri.as_str().to_string(),
        range.start.line,
        range.start.character,
    );
    if seen.insert(key) {
        out.push(Location { uri, range });
    }
}

// ---------------------------------------------------------------------------
// Java-reading helpers
// ---------------------------------------------------------------------------

/// Reads and parses the `.java` file `uri` — the open buffer's text when the
/// editor has it open, otherwise the file on disk — or `None` when neither can be
/// read or parsed.
async fn read_and_parse(client: &HubClient, uri: &Url) -> Option<(String, Tree)> {
    let text = crate::buffers::current_text(client, uri).await?;
    let tree = java_parser().parse(text.as_bytes(), None)?;
    Some(((*text).clone(), tree))
}

/// What the node kind at the cursor says about the name it references: a
/// `type_identifier` names a type, a method-invocation or field-access name
/// names a member, anything else is unconstrained.
#[derive(Debug, Clone, Copy)]
enum NameConstraint {
    Type,
    Member,
    Any,
}

fn name_constraint(node: &Node) -> NameConstraint {
    if node.kind() == "type_identifier" {
        return NameConstraint::Type;
    }
    if node.kind() == "identifier" {
        if let Some(parent) = node.parent() {
            let is_named_field = |field: &str| {
                parent
                    .child_by_field_name(field)
                    .is_some_and(|name| name.byte_range() == node.byte_range())
            };
            match parent.kind() {
                "method_invocation" if is_named_field("name") => return NameConstraint::Member,
                "field_access" if is_named_field("field") => return NameConstraint::Member,
                // A declaration's own name is constrained by what it declares, so a
                // same-named member — a class and its constructor share a name —
                // never joins the type's candidate set.
                "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration"
                | "annotation_type_declaration"
                    if is_named_field("name") =>
                {
                    return NameConstraint::Type
                }
                "method_declaration"
                | "constructor_declaration"
                | "enum_constant"
                | "variable_declarator"
                | "formal_parameter"
                | "spread_parameter"
                | "enhanced_for_statement"
                    if is_named_field("name") =>
                {
                    return NameConstraint::Member
                }
                _ => {}
            }
        }
    }
    NameConstraint::Any
}

/// True when `node` is the `name` field of a declaration (a type, method, field,
/// local, parameter, or enhanced-for binding) rather than a use of it, so it is
/// reported only via `include_declaration`.
fn is_declaration_name(node: &Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if !matches!(
        parent.kind(),
        "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
            | "method_declaration"
            | "constructor_declaration"
            | "variable_declarator"
            | "formal_parameter"
            | "spread_parameter"
            | "enhanced_for_statement"
    ) {
        return false;
    }
    parent
        .child_by_field_name("name")
        .is_some_and(|name| name.id() == node.id())
}

/// True when `node` is the `name` of a declaration the symbol table holds. That
/// is [`is_declaration_name`]'s kinds — a type, method, constructor, field, local,
/// or parameter — plus an enum constant, which the shared predicate omits because
/// it also governs references (left unchanged).
fn is_indexed_declaration_name(node: &Node) -> bool {
    if is_declaration_name(node) {
        return true;
    }
    node.parent().is_some_and(|parent| {
        parent.kind() == "enum_constant"
            && parent
                .child_by_field_name("name")
                .is_some_and(|name| name.id() == node.id())
    })
}

fn has_import_ancestor(node: &Node) -> bool {
    let mut current = node.parent();
    while let Some(node) = current {
        if node.kind() == "import_declaration" {
            return true;
        }
        current = node.parent();
    }
    false
}

/// True when the identifier is the member half of `recv.name` / `recv.name()`.
fn is_member_access_name(node: &Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "field_access" => parent
            .child_by_field_name("field")
            .is_some_and(|field| field.id() == node.id()),
        "method_invocation" => {
            if parent.child_by_field_name("object").is_none() {
                return false;
            }
            parent
                .child_by_field_name("name")
                .is_some_and(|named| named.id() == node.id())
        }
        _ => false,
    }
}

/// One import: its dotted path (a trailing `.*` wildcard and any `static` kept
/// apart) and whether it is static.
struct Import {
    path: String,
    is_static: bool,
}

fn imports_of(tree: &Tree, text: &str) -> Vec<Import> {
    let mut out = Vec::new();
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "import_declaration" {
            let (path, is_static) = import_parts(text, &child);
            out.push(Import { path, is_static });
        }
    }
    out
}

/// An import statement's dotted path and whether it is `static`.
fn import_parts(text: &str, node: &Node) -> (String, bool) {
    let mut raw = text[node.byte_range()].trim();
    raw = raw.strip_prefix("import").map_or(raw, str::trim);
    let is_static = raw.strip_prefix("static").is_some();
    raw = raw.strip_prefix("static").map_or(raw, str::trim);
    (raw.trim_end_matches(';').trim().to_string(), is_static)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::hub::Hub;
    use crate::index::{scan_sources, set_ready, WorkspaceIndex};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "java-lsp-navigate-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &std::path::Path, relative: &str, text: &str) -> Url {
        let path = dir.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        Url::from_file_path(&path).unwrap()
    }

    /// Builds an index by scanning the given files from disk, so the query reads
    /// the same sources it was built from.
    fn index_of(uris: &[Url]) -> WorkspaceIndex {
        let paths: Vec<PathBuf> = uris.iter().map(|uri| uri.to_file_path().unwrap()).collect();
        let mut index = WorkspaceIndex::default();
        let count = scan_sources(&paths, FileOrigin::Workspace, &mut index);
        assert_eq!(count, uris.len(), "every file indexed");
        set_ready(&mut index);
        index
    }

    /// The position of the first `needle` in `text`.
    fn position_of(text: &str, needle: &str) -> Position {
        let offset = text.find(needle).expect("needle present");
        LineIndex::new(text).position(text, offset)
    }

    /// A fresh hub with the index (preloaded from `uris`) and navigation spawned,
    /// and a client registered to drive [`Request::Navigate`] through.
    async fn navigate_hub(uris: &[Url]) -> HubClient {
        let hub = Hub::new();
        crate::index::spawn_with(&hub, index_of(uris));
        spawn(&hub);
        hub.register("test")
    }

    /// Sends a `FindDeclaration` and reads its reply.
    async fn declaration(client: &HubClient, uri: &Url, position: Position) -> Option<Location> {
        let reply = client
            .request(Request::Navigate(NavigateRequest::FindDeclaration {
                uri: uri.clone(),
                position,
            }))
            .await;
        match reply {
            Response::Navigate(NavigateResponse::Declaration(location)) => location,
            _ => None,
        }
    }

    /// Sends a `FindReferences` and reads its reply.
    async fn references(
        client: &HubClient,
        uri: &Url,
        position: Position,
        include_declaration: bool,
    ) -> Vec<Location> {
        let reply = client
            .request(Request::Navigate(NavigateRequest::FindReferences {
                uri: uri.clone(),
                position,
                include_declaration,
            }))
            .await;
        match reply {
            Response::Navigate(NavigateResponse::References(locations)) => locations,
            _ => Vec::new(),
        }
    }

    #[tokio::test]
    async fn declaration_resolves_a_same_file_type() {
        let dir = temp_dir("decl");
        let text = "class Widget {\n    int size;\n}\nclass Use {\n    Widget w;\n}\n";
        let uri = write(&dir, "Widget.java", text);
        let client = navigate_hub(std::slice::from_ref(&uri)).await;

        let position = position_of(text, "Widget w");
        let location = declaration(&client, &uri, position)
            .await
            .expect("declaration found");
        assert_eq!(location.uri, uri);
        assert_eq!(location.range.start.line, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn declaration_of_an_ambiguous_name_is_none() {
        let dir = temp_dir("ambig");
        let a = write(&dir, "a/Dup.java", "package a;\nclass Dup {}\n");
        let b = write(&dir, "b/Dup.java", "package b;\nclass Dup {}\n");
        let use_text = "class Use {\n    Dup d;\n}\n";
        let use_uri = write(&dir, "Use.java", use_text);
        let client = navigate_hub(&[a, b, use_uri.clone()]).await;

        let position = position_of(use_text, "Dup d");
        assert!(declaration(&client, &use_uri, position).await.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn declaration_of_an_interface_method_is_found_though_implemented_elsewhere() {
        let dir = temp_dir("iface-method");
        let iface_text = "package p;\n\npublic interface Iface {\n    void solo();\n}\n";
        let iface = write(&dir, "p/Iface.java", iface_text);
        let impl_uri = write(
            &dir,
            "p/Impl.java",
            "package p;\n\npublic class Impl implements Iface {\n    public void solo() {}\n}\n",
        );
        let client = navigate_hub(&[iface.clone(), impl_uri]).await;

        // A cursor on the interface's own method declaration, whose name is also
        // declared by the implementation in another file.
        let position = position_of(iface_text, "solo()");
        let location = declaration(&client, &iface, position).await;
        assert_eq!(location.map(|l| l.uri), Some(iface));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn declaration_of_an_enum_constant_is_found_though_the_name_repeats_elsewhere() {
        let dir = temp_dir("enum-constant");
        let data_text = "package p;\n\npublic enum DataType {\n    TYPE_1, TYPE_2\n}\n";
        let data = write(&dir, "p/DataType.java", data_text);
        let other = write(
            &dir,
            "p/Other.java",
            "package p;\n\npublic enum Other {\n    TYPE_1\n}\n",
        );
        let client = navigate_hub(&[data.clone(), other]).await;

        // A cursor on the enum constant's own declaration, whose simple name is
        // also an enum constant in another file.
        let position = position_of(data_text, "TYPE_1");
        let location = declaration(&client, &data, position).await;
        assert_eq!(location.map(|l| l.uri), Some(data));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn references_find_type_uses_across_files() {
        let dir = temp_dir("refs");
        let widget_text = "package a;\n\npublic class Widget {}\n";
        let widget = write(&dir, "a/Widget.java", widget_text);
        let use_text = "package a;\n\nclass Use {\n    Widget w;\n}\n";
        let use_uri = write(&dir, "a/Use.java", use_text);
        let client = navigate_hub(&[widget.clone(), use_uri.clone()]).await;

        let position = position_of(use_text, "Widget w");
        let locations = references(&client, &use_uri, position, false).await;
        assert_eq!(locations.len(), 1, "{locations:?}");
        assert_eq!(locations[0].uri, use_uri);

        let with_declaration = references(&client, &use_uri, position, true).await;
        assert_eq!(with_declaration.len(), 2, "{with_declaration:?}");
        assert!(with_declaration
            .iter()
            .any(|location| location.uri == widget));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn references_ignore_a_same_named_type_in_another_package() {
        let dir = temp_dir("refs-dup");
        let widget = write(
            &dir,
            "a/Widget.java",
            "package a;\n\npublic class Widget {}\n",
        );
        let other = write(
            &dir,
            "b/Widget.java",
            "package b;\n\npublic class Widget {}\n",
        );
        let use_text = "package a;\n\nclass Use {\n    Widget w;\n}\n";
        let use_uri = write(&dir, "a/Use.java", use_text);
        let client = navigate_hub(&[widget.clone(), other, use_uri.clone()]).await;

        // The same-named type in `b` must not make the `a` use ambiguous.
        let position = position_of(use_text, "Widget w");
        let locations = references(&client, &use_uri, position, true).await;
        assert_eq!(locations.len(), 2, "{locations:?}");
        assert!(locations.iter().any(|location| location.uri == use_uri));
        assert!(locations.iter().any(|location| location.uri == widget));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn declaration_resolves_a_type_use_with_a_same_named_type_elsewhere() {
        let dir = temp_dir("decl-dup-type");
        let widget = write(
            &dir,
            "a/Widget.java",
            "package a;\n\npublic class Widget {}\n",
        );
        let other = write(
            &dir,
            "b/Widget.java",
            "package b;\n\npublic class Widget {}\n",
        );
        let use_text = "package a;\n\nclass Use {\n    Widget w;\n}\n";
        let use_uri = write(&dir, "a/Use.java", use_text);
        let client = navigate_hub(&[widget.clone(), other, use_uri.clone()]).await;

        let position = position_of(use_text, "Widget w");
        let location = declaration(&client, &use_uri, position).await;
        assert_eq!(location.map(|location| location.uri), Some(widget));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_type_reference_follows_an_import_when_the_simple_name_is_shared() {
        let dir = temp_dir("refs-import-dup");
        let widget = write(
            &dir,
            "a/Widget.java",
            "package a;\n\npublic class Widget {}\n",
        );
        let other = write(
            &dir,
            "b/Widget.java",
            "package b;\n\npublic class Widget {}\n",
        );
        let use_text = "package c;\n\nimport a.Widget;\n\nclass Other {\n    Widget field;\n}\n";
        let use_uri = write(&dir, "c/Other.java", use_text);
        let client = navigate_hub(&[widget.clone(), other, use_uri.clone()]).await;

        // The explicit import pins the target to `a.Widget`, never `b.Widget`:
        // the import, the field use, and the declaration (included).
        let position = position_of(use_text, "Widget field");
        let locations = references(&client, &use_uri, position, true).await;
        assert_eq!(locations.len(), 3, "{locations:?}");
        assert!(locations.iter().any(|location| location.uri == widget));
        assert_eq!(
            locations
                .iter()
                .filter(|location| location.uri == use_uri)
                .count(),
            2,
            "the import and the field use: {locations:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn references_find_this_member_access() {
        let dir = temp_dir("members");
        let text = "\
class Calc {
    int total;
    void add(int amount) {
        this.total = amount;
    }
}
";
        let uri = write(&dir, "Calc.java", text);
        let client = navigate_hub(std::slice::from_ref(&uri)).await;

        let position = position_of(text, "total;");
        let locations = references(&client, &uri, position, false).await;
        // `this.total = amount;` only: the declaration is excluded by default.
        assert_eq!(locations.len(), 1, "{locations:?}");
        assert_eq!(locations[0].range.start.line, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_member_access_resolves_through_an_interface_typed_field() {
        let dir = temp_dir("receiver-field");
        let iface_text =
            "package p;\n\npublic interface TestInterface {\n    void doSomething();\n}\n";
        let iface = write(&dir, "p/TestInterface.java", iface_text);
        let impl_uri = write(
            &dir,
            "p/TestImpl.java",
            "package p;\n\npublic class TestImpl implements TestInterface {\n    public void doSomething() {}\n}\n",
        );
        let main_text = "\
package p;

public class Main {
    TestInterface ti = new TestImpl();

    void run() {
        ti.doSomething();
    }
}
";
        let main = write(&dir, "p/Main.java", main_text);
        let client = navigate_hub(&[iface.clone(), impl_uri, main.clone()]).await;

        // `ti`'s declared type is the interface, so the name resolves to the
        // interface method though the implementation declares it too.
        let position = position_of(main_text, "doSomething");
        let location = declaration(&client, &main, position)
            .await
            .expect("declaration found");
        assert_eq!(location.uri, iface);

        let refs = references(&client, &main, position, false).await;
        assert_eq!(refs.len(), 1, "{refs:?}");
        assert_eq!(refs[0].uri, main);

        let with_declaration = references(&client, &main, position, true).await;
        assert_eq!(with_declaration.len(), 2, "{with_declaration:?}");
        assert!(with_declaration
            .iter()
            .any(|location| location.uri == iface));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_member_access_resolves_through_a_parameter_and_a_local() {
        let dir = temp_dir("receiver-param-local");
        let iface_text = "package p;\n\npublic interface TestInterface {\n    void alpha();\n    void beta();\n}\n";
        let iface = write(&dir, "p/TestInterface.java", iface_text);
        let impl_uri = write(
            &dir,
            "p/TestImpl.java",
            "package p;\n\npublic class TestImpl implements TestInterface {\n    public void alpha() {}\n    public void beta() {}\n}\n",
        );
        let use_text = "\
package p;

class Use {
    void run(TestInterface param) {
        param.alpha();
    }

    void other() {
        TestInterface local = null;
        local.beta();
    }
}
";
        let use_uri = write(&dir, "p/Use.java", use_text);
        let client = navigate_hub(&[iface.clone(), impl_uri, use_uri.clone()]).await;

        let param_position = position_of(use_text, "alpha");
        let location = declaration(&client, &use_uri, param_position)
            .await
            .expect("parameter receiver");
        assert_eq!(location.uri, iface);

        let local_position = position_of(use_text, "beta");
        let location = declaration(&client, &use_uri, local_position)
            .await
            .expect("local receiver");
        assert_eq!(location.uri, iface);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_member_access_resolves_through_a_new_receiver() {
        let dir = temp_dir("receiver-new");
        let text = "\
class Use {
    void run() {
        new Widget().resize();
    }
}

class Widget {
    void resize() {}
}
";
        let uri = write(&dir, "Use.java", text);
        let client = navigate_hub(std::slice::from_ref(&uri)).await;

        let position = position_of(text, "resize");
        let location = declaration(&client, &uri, position)
            .await
            .expect("new receiver");
        assert_eq!(location.uri, uri);
        assert_eq!(location.range.start.line, 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_member_access_resolves_through_a_static_qualifier() {
        let dir = temp_dir("receiver-static");
        let text = "\
class Use {
    void run() {
        Widget.create();
    }
}

class Widget {
    static void create() {}
}
";
        let uri = write(&dir, "Use.java", text);
        let client = navigate_hub(std::slice::from_ref(&uri)).await;

        let position = position_of(text, "create");
        let location = declaration(&client, &uri, position)
            .await
            .expect("static qualifier");
        assert_eq!(location.uri, uri);
        assert_eq!(location.range.start.line, 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_member_access_resolves_through_a_chained_receiver() {
        let dir = temp_dir("receiver-chain");
        let iface_text =
            "package p;\n\npublic interface TestInterface {\n    void doSomething();\n}\n";
        let iface = write(&dir, "p/TestInterface.java", iface_text);
        // `holder.get()` returns the interface, so `doSomething` is reached
        // through a chained receiver rather than a plain field/parameter.
        let main_text = "\
package p;

class Holder {
    TestInterface get() {
        return null;
    }
}

class Main {
    Holder holder;

    void run() {
        holder.get().doSomething();
    }
}
";
        let main = write(&dir, "p/Main.java", main_text);
        let client = navigate_hub(&[iface.clone(), main.clone()]).await;

        let position = position_of(main_text, "doSomething");
        let location = declaration(&client, &main, position)
            .await
            .expect("chained receiver declaration");
        assert_eq!(location.uri, iface);

        let refs = references(&client, &main, position, false).await;
        assert_eq!(refs.len(), 1, "{refs:?}");
        assert_eq!(refs[0].uri, main);

        let with_declaration = references(&client, &main, position, true).await;
        assert_eq!(with_declaration.len(), 2, "{with_declaration:?}");
        assert!(with_declaration
            .iter()
            .any(|location| location.uri == iface));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_member_access_resolves_through_a_field_access_receiver() {
        let dir = temp_dir("receiver-field-access");
        let iface_text =
            "package p;\n\npublic interface TestInterface {\n    void doSomething();\n}\n";
        let iface = write(&dir, "p/TestInterface.java", iface_text);
        // `holder.itf` is a field access, so `doSomething` is reached through a
        // field receiver rather than a plain field/parameter.
        let main_text = "\
package p;

class Holder {
    TestInterface itf;
}

class Main {
    Holder holder;

    void run() {
        holder.itf.doSomething();
    }
}
";
        let main = write(&dir, "p/Main.java", main_text);
        let client = navigate_hub(&[iface.clone(), main.clone()]).await;

        let position = position_of(main_text, "doSomething");
        let location = declaration(&client, &main, position)
            .await
            .expect("field-access receiver declaration");
        assert_eq!(location.uri, iface);

        let refs = references(&client, &main, position, false).await;
        assert_eq!(refs.len(), 1, "{refs:?}");
        assert_eq!(refs[0].uri, main);

        let with_declaration = references(&client, &main, position, true).await;
        assert_eq!(with_declaration.len(), 2, "{with_declaration:?}");
        assert!(with_declaration
            .iter()
            .any(|location| location.uri == iface));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_member_access_on_an_unrelated_receiver_is_none() {
        let dir = temp_dir("receiver-unrelated");
        // `C` declares no `ping`, so the name's two unrelated declarations stay
        // ambiguous and no location is returned.
        let text = "\
class Use {
    void run(C c) {
        c.ping();
    }
}

class A {
    void ping() {}
}

class B {
    void ping() {}
}

class C {}
";
        let uri = write(&dir, "Use.java", text);
        let client = navigate_hub(std::slice::from_ref(&uri)).await;

        let position = position_of(text, "ping");
        assert!(declaration(&client, &uri, position).await.is_none());
        assert!(references(&client, &uri, position, true).await.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_concrete_typed_receiver_resolves_the_most_derived_declaration() {
        let dir = temp_dir("receiver-concrete");
        let iface_text =
            "package p;\n\npublic interface TestInterface {\n    void doSomething();\n}\n";
        let iface = write(&dir, "p/TestInterface.java", iface_text);
        let impl_uri = write(
            &dir,
            "p/TestImpl.java",
            "package p;\n\npublic class TestImpl implements TestInterface {\n    public void doSomething() {}\n}\n",
        );
        let main_text = "\
package p;

class Main {
    void run() {
        TestImpl t = new TestImpl();
        t.doSomething();
        new TestImpl().doSomething();
    }
}
";
        let main = write(&dir, "p/Main.java", main_text);
        let client = navigate_hub(&[iface, impl_uri.clone(), main.clone()]).await;

        // `t`'s declared type is the implementation, so the name resolves to
        // `TestImpl.doSomething`, not the interface method it implements.
        let field_position = position_of(main_text, "doSomething();");
        let location = declaration(&client, &main, field_position)
            .await
            .expect("concrete receiver");
        assert_eq!(location.uri, impl_uri);

        // A `new TestImpl()` receiver resolves the same way.
        let first = main_text.find("doSomething").expect("first call");
        let second = main_text[first + 1..]
            .find("doSomething")
            .expect("second call")
            + first
            + 1;
        let new_position = LineIndex::new(main_text).position(main_text, second);
        let location = declaration(&client, &main, new_position)
            .await
            .expect("new receiver");
        assert_eq!(location.uri, impl_uri);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_override_resolves_to_the_most_derived_declaration() {
        let dir = temp_dir("receiver-override");
        let base = write(
            &dir,
            "p/Base.java",
            "package p;\n\npublic class Base {\n    public void run() {}\n}\n",
        );
        let derived = write(
            &dir,
            "p/Derived.java",
            "package p;\n\npublic class Derived extends Base {\n    public void run() {}\n}\n",
        );
        let use_text = "\
package p;

class Use {
    void go(Derived d) {
        d.run();
    }
}
";
        let use_uri = write(&dir, "p/Use.java", use_text);
        let client = navigate_hub(&[base, derived.clone(), use_uri.clone()]).await;

        // The name is declared by both the subtype and its supertype; the
        // receiver resolves it to the most-derived declaration, not both.
        let position = position_of(use_text, "run");
        let location = declaration(&client, &use_uri, position)
            .await
            .expect("override receiver");
        assert_eq!(location.uri, derived);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_subtype_receiver_resolves_a_supertype_only_declaration() {
        let dir = temp_dir("receiver-supertype-only");
        let base = write(
            &dir,
            "p/Base.java",
            "package p;\n\npublic class Base {\n    public void run() {}\n}\n",
        );
        let derived = write(
            &dir,
            "p/Derived.java",
            "package p;\n\npublic class Derived extends Base {}\n",
        );
        let use_text = "\
package p;

class Use {
    void go(Derived d) {
        d.run();
    }
}
";
        let use_uri = write(&dir, "p/Use.java", use_text);
        let client = navigate_hub(&[base.clone(), derived, use_uri.clone()]).await;

        // The subtype does not declare the name, so the nearest declaring type in
        // the closure is the supertype and its declaration is the answer.
        let position = position_of(use_text, "run");
        let location = declaration(&client, &use_uri, position)
            .await
            .expect("supertype declaration");
        assert_eq!(location.uri, base);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_method_reference_name_is_not_an_implicit_this() {
        let dir = temp_dir("receiver-method-ref");
        let strings = write(
            &dir,
            "p/Strings.java",
            "package p;\n\nclass Strings {\n    static String trim(String s) { return s; }\n}\n",
        );
        let use_text = "\
package p;

class Use {
    String trim() {
        return \"\";
    }

    void run() {
        java.util.function.Function<String, String> f = Strings::trim;
    }
}
";
        let use_uri = write(&dir, "p/Use.java", use_text);
        let client = navigate_hub(&[strings, use_uri.clone()]).await;

        // `Strings::trim` is a method-reference name, not an unqualified call: it
        // must not be read as an implicit `this`, so the two declarations stay
        // ambiguous across files and no location is returned.
        let position = position_of(use_text, "trim;");
        assert!(declaration(&client, &use_uri, position).await.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_array_receiver_does_not_resolve() {
        let dir = temp_dir("receiver-array");
        let item = write(
            &dir,
            "p/Item.java",
            "package p;\n\nclass Item {\n    void ping() {}\n}\n",
        );
        let other = write(
            &dir,
            "p/Other.java",
            "package p;\n\nclass Other {\n    void ping() {}\n}\n",
        );
        let use_text = "\
package p;

class Use {
    void run(Item[] items) {
        items.ping();
    }
}
";
        let use_uri = write(&dir, "p/Use.java", use_text);
        let client = navigate_hub(&[item, other, use_uri.clone()]).await;

        // An array receiver names no member: it is not unwrapped to its element
        // type, so the name's two declarations stay ambiguous and yield nothing.
        let position = position_of(use_text, "ping");
        assert!(declaration(&client, &use_uri, position).await.is_none());
        assert!(references(&client, &use_uri, position, true)
            .await
            .is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_type_variable_receiver_is_not_resolved_through_a_same_named_type() {
        let dir = temp_dir("receiver-type-param");
        let t = write(
            &dir,
            "p/T.java",
            "package p;\n\nclass T {\n    void foo() {}\n}\n",
        );
        let other = write(
            &dir,
            "p/Other.java",
            "package p;\n\nclass Other {\n    void foo() {}\n}\n",
        );
        // `t` is typed by the method's type variable `T`, not the class `T`.
        let use_text =
            "package p;\n\nclass Use {\n    <T> void run(T t) {\n        t.foo();\n    }\n}\n";
        let use_uri = write(&dir, "p/Use.java", use_text);
        let client = navigate_hub(&[t, other, use_uri.clone()]).await;

        // The type variable does not name the class `T`, so the receiver does not
        // resolve and `foo`'s two declarations stay ambiguous.
        let position = position_of(use_text, "foo");
        assert!(declaration(&client, &use_uri, position).await.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Sends a `FindImplementation` and reads its reply.
    async fn implementation(client: &HubClient, uri: &Url, position: Position) -> Vec<Location> {
        let reply = client
            .request(Request::Navigate(NavigateRequest::FindImplementation {
                uri: uri.clone(),
                position,
            }))
            .await;
        match reply {
            Response::Navigate(NavigateResponse::Implementations(locations)) => locations,
            _ => Vec::new(),
        }
    }

    #[tokio::test]
    async fn implementation_from_a_type_declaration_lists_the_subtypes() {
        let dir = temp_dir("impl-type");
        let iface_text = "package p;\n\npublic interface Iface {\n    void solo();\n}\n";
        let iface = write(&dir, "p/Iface.java", iface_text);
        let impl_uri = write(
            &dir,
            "p/Impl.java",
            "package p;\n\npublic class Impl implements Iface {\n    public void solo() {}\n}\n",
        );
        // `Inherit` only inherits `solo`, but a type contract lists every subtype.
        let inherit = write(
            &dir,
            "p/Inherit.java",
            "package p;\n\npublic class Inherit implements Iface {}\n",
        );
        let client = navigate_hub(&[iface.clone(), impl_uri.clone(), inherit.clone()]).await;

        // A cursor on the interface's own declaration name names the type.
        let position = position_of(iface_text, "Iface {");
        let locations = implementation(&client, &iface, position).await;
        let uris: Vec<&Url> = locations.iter().map(|location| &location.uri).collect();
        assert!(uris.contains(&&impl_uri), "{locations:?}");
        assert!(uris.contains(&&inherit), "{locations:?}");
        assert!(
            !uris.contains(&&iface),
            "the contract is not its own subtype"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn implementation_from_a_member_contract_lists_only_the_overrides() {
        let dir = temp_dir("impl-member");
        let iface_text = "package p;\n\npublic interface Iface {\n    void solo();\n}\n";
        let iface = write(&dir, "p/Iface.java", iface_text);
        let impl_uri = write(
            &dir,
            "p/Impl.java",
            "package p;\n\npublic class Impl implements Iface {\n    public void solo() {}\n}\n",
        );
        // `Inherit` only inherits `solo`, so it declares no override.
        let inherit = write(
            &dir,
            "p/Inherit.java",
            "package p;\n\npublic class Inherit implements Iface {}\n",
        );
        let client = navigate_hub(&[iface.clone(), impl_uri.clone(), inherit]).await;

        // A cursor on the interface's method declaration names the member.
        let position = position_of(iface_text, "solo()");
        let locations = implementation(&client, &iface, position).await;
        assert_eq!(locations.len(), 1, "{locations:?}");
        assert_eq!(locations[0].uri, impl_uri);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn implementation_resolves_a_member_through_its_receiver() {
        let dir = temp_dir("impl-call");
        let iface = write(
            &dir,
            "p/TestInterface.java",
            "package p;\n\npublic interface TestInterface {\n    void doSomething();\n}\n",
        );
        let impl_uri = write(
            &dir,
            "p/TestImpl.java",
            "package p;\n\npublic class TestImpl implements TestInterface {\n    public void doSomething() {}\n}\n",
        );
        let main_text = "package p;\n\nclass Main {\n    void run() {\n        TestInterface ti = new TestImpl();\n        ti.doSomething();\n    }\n}\n";
        let main = write(&dir, "p/Main.java", main_text);
        let client = navigate_hub(&[iface, impl_uri.clone(), main.clone()]).await;

        // The receiver's declared type (`TestInterface`) names the contract.
        let position = position_of(main_text, "doSomething");
        let locations = implementation(&client, &main, position).await;
        assert_eq!(locations.len(), 1, "{locations:?}");
        assert_eq!(locations[0].uri, impl_uri);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn implementation_of_a_field_or_constructor_is_empty() {
        let dir = temp_dir("impl-none");
        let text = "class Solo {\n    int count;\n\n    Solo() {}\n}\n";
        let uri = write(&dir, "Solo.java", text);
        let client = navigate_hub(std::slice::from_ref(&uri)).await;

        // A field names no contract, and neither does a constructor.
        let field = position_of(text, "count");
        assert!(implementation(&client, &uri, field).await.is_empty());
        let constructor = position_of(text, "Solo()");
        assert!(implementation(&client, &uri, constructor).await.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_override_declaration_resolves_to_the_interface_declaration() {
        let dir = temp_dir("override-decl");
        let iface = write(
            &dir,
            "p/TestInterface.java",
            "package p;\n\npublic interface TestInterface {\n    void doSomething();\n}\n",
        );
        let impl_text = "package p;\n\npublic class TestImpl implements TestInterface {\n    @Override\n    public void doSomething() {}\n}\n";
        let impl_uri = write(&dir, "p/TestImpl.java", impl_text);
        let client = navigate_hub(&[iface.clone(), impl_uri.clone()]).await;

        // A cursor on the implementation's own declaration resolves to the
        // interface method it overrides.
        let position = position_of(impl_text, "doSomething");
        let location = declaration(&client, &impl_uri, position)
            .await
            .expect("overridden declaration");
        assert_eq!(location.uri, iface);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_type_declaration_still_resolves_to_itself() {
        let dir = temp_dir("decl-self");
        let iface = write(
            &dir,
            "p/Iface.java",
            "package p;\n\npublic interface Iface {\n    void solo();\n}\n",
        );
        let impl_text =
            "package p;\n\npublic class Impl implements Iface {\n    public void solo() {}\n}\n";
        let impl_uri = write(&dir, "p/Impl.java", impl_text);
        let client = navigate_hub(&[iface, impl_uri.clone()]).await;

        // A cursor on the class declaration's own name resolves to itself, not to
        // the interface it implements.
        let position = position_of(impl_text, "Impl implements");
        let location = declaration(&client, &impl_uri, position)
            .await
            .expect("declaration found");
        assert_eq!(location.uri, impl_uri);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn references_on_a_class_declaration_survive_a_named_constructor() {
        let dir = temp_dir("refs-ctor");
        let greeter_text = "package com.example.greeting;\n\npublic class Greeter {\n    private final String name;\n\n    public Greeter(String name) {\n        this.name = name;\n    }\n\n    public String greet() {\n        return \"Hello\";\n    }\n}\n";
        let greeter = write(&dir, "com/example/greeting/Greeter.java", greeter_text);
        let main_text = "package com.example.app;\n\nimport com.example.greeting.Greeter;\n\npublic class Main {\n    Greeter greeter = new Greeter(\"world\");\n\n    void m() {\n        greeter.greet();\n        var inner = new Greeter.Inner();\n    }\n}\n";
        let main = write(&dir, "com/example/app/Main.java", main_text);
        let client = navigate_hub(&[greeter.clone(), main.clone()]).await;

        // A class and its constructor share a simple name — the constructor is
        // indexed as a member of the class — so a cursor on the class
        // declaration's own name must still resolve to the class, not to the
        // mixed name group.
        let position = position_of(greeter_text, "Greeter {");
        let with_declaration = references(&client, &greeter, position, true).await;
        assert_eq!(with_declaration.len(), 5, "{with_declaration:?}");
        assert!(with_declaration
            .iter()
            .any(|location| location.uri == greeter));

        let uses = references(&client, &greeter, position, false).await;
        assert_eq!(uses.len(), 4, "{uses:?}");
        assert!(uses.iter().all(|location| location.uri == main));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_override_of_an_unindexed_supertype_resolves_to_itself() {
        let dir = temp_dir("override-lib");
        // `Runnable` is not in the index, so the override has no openable
        // supertype declaration and keeps self-resolution.
        let text =
            "package p;\n\npublic class Task implements Runnable {\n    public void run() {}\n}\n";
        let uri = write(&dir, "p/Task.java", text);
        let client = navigate_hub(std::slice::from_ref(&uri)).await;

        let position = position_of(text, "run()");
        let location = declaration(&client, &uri, position)
            .await
            .expect("declaration found");
        assert_eq!(location.uri, uri);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
