//! Autocomplete: the names offerable at a cursor — Java keywords, the names in
//! scope, and the workspace's indexed symbols.
//!
//! Completion is its own hub module, labelled `completion`. It reads Java — the
//! requested file, for the word being typed and the names in scope — and reads
//! index only through the hub, over the coarse vocabulary
//! [`crate::messages::IndexRequest`] lays out. It answers the shell's `Complete`
//! and `SignatureHelp` requests ([`crate::messages::CompletionRequest`]).
//!
//! It offers Java keywords, the locals, parameters, and fields nameable
//! unqualified at the cursor, and the workspace's indexed symbols whose simple
//! name starts with the typed prefix. A workspace symbol declared outside the
//! file carries an auto-import edit when the file does not already resolve the
//! name. A member access after a `.` resolves the receiver's declared type (the
//! shared receiver step in [`crate::util`], read over the hub) and offers that
//! type's members — inherited ones included, fields and enum constants first,
//! then one item per method overload; a receiver that names a type offers only
//! its static members, and an unresolvable receiver yields nothing rather than a
//! guess at what might be there.
//!
//! Signature help shares that receiver step: for the call the cursor sits in it
//! resolves the callee's overloads through the receiver's declared type and
//! answers with each overload's declared signature and the active parameter
//! counted from the cursor's argument position; a `new T(…)` answers with the
//! created type's constructor overloads. An unresolvable callee or receiver
//! yields no help, and a token the completion path would never guess at is not
//! guessed at here either.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, Position, Range, SignatureHelp, SignatureInformation,
    TextEdit, Url,
};
use tree_sitter::{Node, Tree};

use crate::hub::{Hub, HubClient};
use crate::index::{FileOrigin, SymbolEntry};
use crate::messages::{
    CompletionRequest, CompletionResponse, IndexBatchRequest, IndexBatchResponse, IndexRequest,
    IndexResponse, Message, Request, Response,
};
use crate::model::{Member, SymbolKind, Ty};
use crate::parser::{byte_offset, file_package, java_parser, type_from_node, word_prefix};
use crate::util;

// ---------------------------------------------------------------------------
// The hub client
// ---------------------------------------------------------------------------

/// Starts completion on the hub as the client labelled `completion`, serving the
/// shell's [`CompletionRequest`]s and answering each with a
/// [`CompletionResponse`].
pub fn spawn(hub: &Hub) {
    let hub_client = hub.register("completion");

    tokio::spawn(async move {
        let mut notifications = hub_client.subscribe().await;
        let mut requests = hub_client.serve().await;

        loop {
            tokio::select! {
                // Completion needs no notification, but taking the subscription
                // and draining it keeps the hub's broadcast from backing up on
                // this client.
                Some(_) = notifications.recv() => {}
                Some(envelope) = requests.recv() => {
                    let Message::Request(Request::Completion(request)) = &*envelope.message else {
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

/// Answers one completion request.
async fn serve(client: &HubClient, request: CompletionRequest) -> Response {
    match request {
        CompletionRequest::Complete { uri, position } => Response::Completion(
            CompletionResponse::Items(complete(client, &uri, position).await),
        ),
        CompletionRequest::SignatureHelp { uri, position } => Response::Completion(
            CompletionResponse::SignatureHelp(signature_help(client, &uri, position).await),
        ),
    }
}

// ---------------------------------------------------------------------------
// The index read
// ---------------------------------------------------------------------------

/// Every entry whose simple name starts with `prefix`, in name order; empty when
/// the index is unowned or answers with a different variant.
async fn entries_with_prefix(client: &HubClient, prefix: &str) -> Vec<Arc<SymbolEntry>> {
    let request = IndexRequest::EntriesByPrefix {
        prefix: prefix.to_string(),
    };
    match client.request(Request::Index(request)).await {
        Response::Index(IndexResponse::Entries(entries)) => entries,
        _ => Vec::new(),
    }
}

/// The `MembersOf` members of several declaring types in one batched index
/// read, keyed by `(declaring type name, package)`. Empty when the index is
/// unowned or answers with a different variant; a pair whose read did not answer
/// with members is simply absent from the map.
async fn members_batch(
    client: &HubClient,
    types: Vec<(String, Option<String>)>,
) -> HashMap<(String, Option<String>), Vec<Member>> {
    if types.is_empty() {
        return HashMap::new();
    }
    let requests: Vec<IndexRequest> = types
        .iter()
        .map(|(name, package)| IndexRequest::MembersOf {
            ty: Ty::reference(name.clone()),
            package: package.clone(),
        })
        .collect();
    let responses = match client
        .request(Request::IndexBatch(IndexBatchRequest { requests }))
        .await
    {
        Response::IndexBatch(IndexBatchResponse(responses)) => responses,
        _ => return HashMap::new(),
    };
    types
        .into_iter()
        .zip(responses)
        .filter_map(|(key, response)| match response {
            IndexResponse::Members(members) => Some((key, members)),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Completing
// ---------------------------------------------------------------------------

/// The names to offer at `position` in the file `uri`.
async fn complete(client: &HubClient, uri: &Url, position: Position) -> Vec<CompletionItem> {
    // The open buffer's current text wins over disk, so completion sees unsaved
    // edits — and an untitled buffer it would not find on disk at all.
    let Some(text) = crate::buffers::current_text(client, uri).await else {
        return Vec::new();
    };
    let text = text.as_str();
    let Some(tree) = java_parser().parse(text.as_bytes(), None) else {
        return Vec::new();
    };

    let offset = byte_offset(text, position);
    let prefix = word_prefix(text, offset);
    let before_prefix = offset - prefix.len();
    if text[..before_prefix].ends_with('.') {
        // Member access: the receiver's declared type decides the membership —
        // never a guess at what might be there.
        return member_items(client, text, &tree, offset, prefix).await;
    }
    if prefix.is_empty() {
        // Nothing typed: no suggestions from nothing.
        return Vec::new();
    }

    let mut items = Vec::new();
    let mut seen = HashSet::new();

    // 1. Language keywords.
    for keyword in JAVA_KEYWORDS {
        if keyword.starts_with(prefix) {
            offer(
                &mut items,
                &mut seen,
                (*keyword).to_string(),
                (*keyword).to_string(),
                (*keyword).to_string(),
                (*keyword).to_string(),
                CompletionItemKind::KEYWORD,
                None,
                format!("0{keyword}"),
                Vec::new(),
            );
        }
    }

    // 2. Locals, parameters, and enclosing fields from the requested tree.
    if let Some(cursor) = tree.root_node().descendant_for_byte_range(offset, offset) {
        let mut scoped = Vec::new();
        collect_scoped_names(&cursor, text, &mut scoped);
        for name in scoped {
            if !name.name.starts_with(prefix) {
                continue;
            }
            let sort_text = format!("1{}", name.name);
            offer(
                &mut items,
                &mut seen,
                name.name.clone(),
                name.name.clone(),
                name.name.clone(),
                name.name,
                name.kind,
                Some(name.detail),
                sort_text,
                Vec::new(),
            );
        }
    }

    // 3. Workspace and dependency-jar symbols whose simple name starts with the
    // prefix; items whose symbol lives outside this file get an auto-import edit
    // attached (never-worsen: see `import_edit`). A simple name shared by
    // several symbols stays several items — each with its own import, and each
    // labeled with its owner — so the user can tell them apart.
    let (file_package, package_line) = file_header(&tree, text);
    let imports = collect_imports(&tree.root_node(), text);
    let entries = entries_with_prefix(client, prefix).await;
    let ambiguous = ambiguous_names(&entries);
    // Read the matching method entries' overloads in one batched index request:
    // collect the distinct declaring types (`container.last()` with the entry's
    // own package) of the entries that reach the overload branch below, issue
    // one `MembersOf` per type, and look the members up per entry in the loop.
    let mut overload_types: Vec<(String, Option<String>)> = Vec::new();
    let mut overload_types_seen: HashSet<(String, Option<String>)> = HashSet::new();
    for entry in &entries {
        if entry.kind != SymbolKind::Method || entry.is_constructor() || entry.synthetic {
            continue;
        }
        let Some(declaring) = entry.container.last() else {
            continue;
        };
        let key = (
            declaring.clone(),
            entry.package.as_deref().map(str::to_string),
        );
        if overload_types_seen.insert(key.clone()) {
            overload_types.push(key);
        }
    }
    let overload_members = members_batch(client, overload_types).await;
    for entry in &entries {
        // A constructor is not a callable member, and a Lombok-generated member
        // has no declaration of its own; both stay out of ordinary completion.
        if entry.is_constructor() || entry.synthetic {
            continue;
        }
        let Some(kind) = completion_kind(entry.kind) else {
            continue;
        };
        let name = entry.name.to_string();
        let container = entry.container.join(".");
        let is_member = matches!(
            entry.kind,
            SymbolKind::Method | SymbolKind::Field | SymbolKind::EnumConstant
        ) && !container.is_empty();
        let additional_edits = import_edit(
            uri,
            entry,
            file_package.as_deref(),
            package_line,
            &imports,
            &entries,
        );
        // A method lists each overload of its declaring type, the shape the
        // member path offers, and each item carries the entry's auto-import
        // edit. When the declaring type does not resolve to a same-named member
        // — its container is not in the model, or it declares the name
        // elsewhere — the single name-only item below stands, so no symbol ever
        // disappears.
        if entry.kind == SymbolKind::Method {
            if let Some((declaring, overloads)) = method_overloads(entry, &name, &overload_members)
            {
                for member in &overloads {
                    let signature = member.signature();
                    // Same name + same import is a duplicate; a distinct overload
                    // or a distinct import stays separate.
                    let key = match additional_edits.first() {
                        Some(edit) => format!("{signature}|{}", edit.new_text),
                        None => signature.clone(),
                    };
                    offer_overload(
                        &mut items,
                        &mut seen,
                        member,
                        &declaring,
                        key,
                        format!("2{signature}"),
                        additional_edits.clone(),
                    );
                }
                continue;
            }
        }
        let detail = if ambiguous.contains(&name) {
            format!("{} of {}", kind_word(entry.kind), qualified_owner(entry))
        } else if is_member {
            format!("{} of {container}", kind_word(entry.kind))
        } else {
            kind_word(entry.kind).to_string()
        };
        let label = if is_member {
            format!("{container}.{name}")
        } else {
            name.clone()
        };
        // Same name + same import is a duplicate; same name with a different
        // import is a different symbol and stays separate.
        let key = match additional_edits.first() {
            Some(edit) => format!("{name}|{}", edit.new_text),
            None => name.clone(),
        };
        offer(
            &mut items,
            &mut seen,
            key,
            label,
            name.clone(),
            name,
            kind,
            Some(detail),
            format!("2{}", entry.name),
            additional_edits,
        );
    }

    items
}

// ---------------------------------------------------------------------------
// Member access
// ---------------------------------------------------------------------------

/// Completion items for a member access after `.`: the receiver's declared
/// type's members — inherited ones included — with fields and enum constants
/// first, then one item per method overload. A receiver that names a type offers
/// only its static members; a receiver that does not resolve yields nothing.
async fn member_items(
    client: &HubClient,
    text: &str,
    tree: &Tree,
    offset: usize,
    prefix: &str,
) -> Vec<CompletionItem> {
    let package = file_package(tree, text);
    let Some(receiver) =
        util::receiver_before_dot(client, text, package.as_deref(), tree, offset).await
    else {
        return Vec::new();
    };
    let members = util::members(client, &receiver.ty, package.as_deref()).await;
    // The type whose members these are, as named for the item's `detail`: the
    // `MembersOf` read does not carry an inherited member's own declaring
    // supertype, so the resolved receiver type stands in for it.
    let declaring = receiver.ty.display();
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    for member in members {
        if receiver.is_type && !member.is_static {
            continue; // a type qualifier offers its statics only
        }
        if !member.name.starts_with(prefix) {
            continue;
        }
        if member.kind == SymbolKind::Method {
            // One item per overload: accepting it opens the argument list.
            offer_overload(
                &mut items,
                &mut seen,
                &member,
                &declaring,
                member.signature(),
                format!("1{}", member.signature()),
                Vec::new(),
            );
        } else {
            let kind = if member.kind == SymbolKind::EnumConstant {
                CompletionItemKind::ENUM_MEMBER
            } else {
                CompletionItemKind::FIELD
            };
            offer(
                &mut items,
                &mut seen,
                member.name.clone(),
                member.name.clone(),
                member.name.clone(),
                member.name.clone(),
                kind,
                Some(member.signature()),
                format!("0{}", member.name),
                Vec::new(),
            );
        }
    }
    items
}

/// The same-named methods of a method entry's declaring type, one per overload,
/// with the declaring type's display name — or `None` when it resolves to no
/// such member (its container is not in the model, or declares the name
/// elsewhere), so the caller keeps the single name-only item.
///
/// The overloads come from the pre-fetched `members` batch, keyed by the entry's
/// innermost container and package — exactly the `(declaring type, package)` pair
/// the batched `MembersOf` read was issued for — so no per-entry request is made.
/// The declaring type is the entry's innermost container, read with the entry's
/// own package, exactly as the old engine's `model_overloads` did.
fn method_overloads(
    entry: &SymbolEntry,
    name: &str,
    members: &HashMap<(String, Option<String>), Vec<Member>>,
) -> Option<(String, Vec<Member>)> {
    let declaring = entry.container.last()?;
    let ty = Ty::reference(declaring.clone());
    let overloads: Vec<Member> = members
        .get(&(
            declaring.clone(),
            entry.package.as_deref().map(str::to_string),
        ))?
        .iter()
        .filter(|member| member.kind == SymbolKind::Method && member.name.as_str() == name)
        .cloned()
        .collect();
    (!overloads.is_empty()).then(|| (ty.display(), overloads))
}

// ---------------------------------------------------------------------------
// Signature help
// ---------------------------------------------------------------------------

/// The signature help for the call `position` sits in: the callee's overloads
/// with the active parameter counted from the cursor's argument, or the created
/// type's constructor overloads for a `new T(…)`. `None` when there is no
/// enclosing call, or the callee's receiver cannot be resolved, or the type
/// declares no such member — never a wrong signature (D2).
///
/// A `recv.name(…)` call resolves `recv` through the shared receiver helper, so a
/// type qualifier offers only static overloads; an unqualified `name(…)` is an
/// implicit `this`, its receiver the enclosing type. The methods are read through
/// the overload-preserving `MembersOf`, the constructors through `ConstructorsOf`.
async fn signature_help(
    client: &HubClient,
    uri: &Url,
    position: Position,
) -> Option<SignatureHelp> {
    let text = crate::buffers::current_text(client, uri).await?;
    let text = text.as_str();
    let tree = java_parser().parse(text.as_bytes(), None)?;
    let offset = byte_offset(text, position);
    let package = file_package(&tree, text);

    // A method call: the callee's overloads, its name resolved through the
    // receiver's type.
    if let Some(call) = enclosing_call(&tree, offset) {
        let name_node = call.child_by_field_name("name")?;
        let name = &text[name_node.byte_range()];
        let (receiver, is_type) = match call.child_by_field_name("object") {
            Some(object) => {
                let receiver =
                    util::classify_receiver(client, text, package.as_deref(), &object).await?;
                (receiver.ty, receiver.is_type)
            }
            None => (
                Ty::reference(util::enclosing_type_name(text, &name_node)?),
                false,
            ),
        };
        let overloads: Vec<Member> = util::members(client, &receiver, package.as_deref())
            .await
            .into_iter()
            .filter(|member| member.kind == SymbolKind::Method && member.name == name)
            .filter(|member| !is_type || member.is_static)
            .collect();
        if overloads.is_empty() {
            return None;
        }
        let active = active_parameter(&call, offset);
        return Some(signature_help_of(
            overloads.iter().map(Member::signature),
            active,
        ));
    }

    // A `new T(…)`: the created type's constructor overloads.
    let creation = enclosing_object_creation(&tree, offset)?;
    let type_node = creation.child_by_field_name("type")?;
    let ty = type_from_node(&type_node, text);
    let constructors = constructors(client, &ty, package.as_deref()).await;
    if constructors.is_empty() {
        return None;
    }
    let active = active_parameter(&creation, offset);
    Some(signature_help_of(
        constructors.iter().map(Member::constructor_signature),
        active,
    ))
}

/// A type's constructor overloads, the implicit no-arg one included.
async fn constructors(client: &HubClient, ty: &Ty, package: Option<&str>) -> Vec<Member> {
    let request = IndexRequest::ConstructorsOf {
        ty: ty.clone(),
        package: package.map(str::to_string),
    };
    match client.request(Request::Index(request)).await {
        Response::Index(IndexResponse::Constructors(constructors)) => constructors,
        _ => Vec::new(),
    }
}

/// Builds a [`SignatureHelp`] from the overloads' signatures and the active
/// parameter: each signature carries the active parameter, and the first
/// signature is the active one (D4). The signature text is the whole help — no
/// parameter offsets or labels beyond it (D5).
fn signature_help_of(signatures: impl Iterator<Item = String>, active: u32) -> SignatureHelp {
    let signatures = signatures
        .map(|label| SignatureInformation {
            label,
            documentation: None,
            parameters: None,
            active_parameter: Some(active),
        })
        .collect();
    SignatureHelp {
        signatures,
        active_signature: Some(0),
        active_parameter: None,
    }
}

/// The innermost `method_invocation` whose argument list contains `offset`.
fn enclosing_call(tree: &Tree, offset: usize) -> Option<Node<'_>> {
    let node = tree.root_node().descendant_for_byte_range(offset, offset)?;
    let mut current = Some(node);
    while let Some(candidate) = current {
        if candidate.kind() == "method_invocation" {
            if let Some(arguments) = candidate.child_by_field_name("arguments") {
                if arguments.start_byte() <= offset && offset <= arguments.end_byte() {
                    return Some(candidate);
                }
            }
        }
        current = candidate.parent();
    }
    None
}

/// The innermost `new T(…)` whose argument list contains the cursor, the
/// constructor analog of `enclosing_call`.
fn enclosing_object_creation(tree: &Tree, offset: usize) -> Option<Node<'_>> {
    let node = tree.root_node().descendant_for_byte_range(offset, offset)?;
    let mut current = Some(node);
    while let Some(candidate) = current {
        if candidate.kind() == "object_creation_expression" {
            if let Some(arguments) = candidate.child_by_field_name("arguments") {
                if arguments.start_byte() <= offset && offset <= arguments.end_byte() {
                    return Some(candidate);
                }
            }
        }
        current = candidate.parent();
    }
    None
}

/// The index of the argument the cursor sits in: the number of argument nodes
/// ending before the cursor, clamped to the last argument (D4).
fn active_parameter(call: &Node, offset: usize) -> u32 {
    let Some(arguments) = call.child_by_field_name("arguments") else {
        return 0;
    };
    let mut cursor = arguments.walk();
    let mut index = 0u32;
    let mut count = 0u32;
    for argument in arguments.named_children(&mut cursor) {
        if is_comment_node(argument.kind()) {
            continue;
        }
        count += 1;
        if argument.end_byte() <= offset {
            index += 1;
        }
    }
    index.min(count.saturating_sub(1))
}

/// True for a comment node, which is not an argument.
fn is_comment_node(kind: &str) -> bool {
    matches!(kind, "line_comment" | "block_comment")
}

// ---------------------------------------------------------------------------
// In-scope names
// ---------------------------------------------------------------------------

/// A name offerable unqualified at the cursor, with its presentation.
struct ScopedName {
    name: String,
    kind: CompletionItemKind,
    detail: String,
}

/// Locals and members nameable unqualified at the cursor: the innermost
/// enclosing method/constructor contributes its parameters and local variables,
/// the enclosing type declaration its fields (nameable unqualified inside the
/// type, so this claims no membership the code does not already grant). Locals
/// declared later in the method are included — a known, harmless simplification.
fn collect_scoped_names(node: &Node, text: &str, out: &mut Vec<ScopedName>) {
    const TYPE_DECLS: [&str; 4] = [
        "class_declaration",
        "interface_declaration",
        "enum_declaration",
        "record_declaration",
    ];

    let mut method_done = false;
    let mut type_done = false;
    let mut current = Some(*node);
    while let Some(ancestor) = current {
        match ancestor.kind() {
            "method_declaration" | "constructor_declaration" if !method_done => {
                method_done = true;
                collect_method_scope(&ancestor, text, out);
            }
            kind if TYPE_DECLS.contains(&kind) && !type_done => {
                type_done = true;
                let type_name = ancestor
                    .child_by_field_name("name")
                    .map(|name| text[name.byte_range()].to_string())
                    .unwrap_or_default();
                collect_type_fields(&ancestor, &type_name, text, out);
            }
            _ => {}
        }
        if method_done && type_done {
            break;
        }
        current = ancestor.parent();
    }
}

/// Parameters (including `varargs` spread parameters) and local variables of a
/// method or constructor body.
fn collect_method_scope(method: &Node, text: &str, out: &mut Vec<ScopedName>) {
    if let Some(parameters) = method.child_by_field_name("parameters") {
        let mut cursor = parameters.walk();
        for child in parameters.children(&mut cursor) {
            let name: Option<String> = match child.kind() {
                "formal_parameter" => child
                    .child_by_field_name("name")
                    .map(|name| text[name.byte_range()].to_string()),
                // A varargs parameter wraps its name in a variable_declarator.
                "spread_parameter" => {
                    let mut inner = child.walk();
                    let declarator = child
                        .children(&mut inner)
                        .find(|part| part.kind() == "variable_declarator")
                        .and_then(|declarator| declarator.child_by_field_name("name"))
                        .map(|name| text[name.byte_range()].to_string());
                    declarator
                }
                _ => None,
            };
            if let Some(name) = name {
                out.push(ScopedName {
                    name,
                    kind: CompletionItemKind::VARIABLE,
                    detail: "parameter".to_string(),
                });
            }
        }
    }
    collect_local_declarators(method, text, out);
}

/// `variable_declarator` names under a method body, skipping the parameter list
/// (handled above) and nested type declarations (their members are not in scope
/// here).
fn collect_local_declarators(node: &Node, text: &str, out: &mut Vec<ScopedName>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            continue;
        }
        match child.kind() {
            "formal_parameters"
            | "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration" => {}
            "variable_declarator" => {
                if let Some(name) = child.child_by_field_name("name") {
                    out.push(ScopedName {
                        name: text[name.byte_range()].to_string(),
                        kind: CompletionItemKind::VARIABLE,
                        detail: "local".to_string(),
                    });
                }
            }
            _ => collect_local_declarators(&child, text, out),
        }
    }
}

/// Field names declared inside a type's body (directly, or nested in an enum's
/// declarations section), skipping nested type declarations.
fn collect_type_fields(node: &Node, type_name: &str, text: &str, out: &mut Vec<ScopedName>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            continue;
        }
        match child.kind() {
            "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration" => {}
            "field_declaration" => {
                let mut inner = child.walk();
                for declarator in child.children(&mut inner) {
                    if declarator.kind() != "variable_declarator" {
                        continue;
                    }
                    let Some(name) = declarator.child_by_field_name("name") else {
                        continue;
                    };
                    out.push(ScopedName {
                        name: text[name.byte_range()].to_string(),
                        kind: CompletionItemKind::FIELD,
                        detail: format!("field of {type_name}"),
                    });
                }
            }
            "enum_constant" => {
                if let Some(name) = child.child_by_field_name("name") {
                    out.push(ScopedName {
                        name: text[name.byte_range()].to_string(),
                        kind: CompletionItemKind::ENUM_MEMBER,
                        detail: format!("constant of {type_name}"),
                    });
                }
            }
            _ => collect_type_fields(&child, type_name, text, out),
        }
    }
}

// ---------------------------------------------------------------------------
// Auto-imports
// ---------------------------------------------------------------------------

/// One import statement in the requested document.
struct ExistingImport {
    path: String,
    is_static: bool,
    is_wildcard: bool,
    /// Where a new import line goes: the line right after this one.
    line: u32,
}

fn collect_imports(root: &Node, text: &str) -> Vec<ExistingImport> {
    let mut out = Vec::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() != "import_declaration" {
            continue;
        }
        let mut raw = text[child.byte_range()].trim();
        raw = raw.strip_prefix("import").map_or(raw, str::trim);
        let is_static = raw.starts_with("static");
        raw = raw.strip_prefix("static").map_or(raw, str::trim);
        let path = raw.trim_end_matches(';').trim().to_string();
        let (path, is_wildcard) = match path.strip_suffix(".*") {
            Some(head) => (head.to_string(), true),
            None => (path, false),
        };
        out.push(ExistingImport {
            path,
            is_static,
            is_wildcard,
            line: (child.end_position().row + 1) as u32,
        });
    }
    out
}

/// The dotted name of a file's `package_declaration` (with the line after it, for
/// inserting imports) or `None` for the default package.
fn file_header(tree: &Tree, text: &str) -> (Option<String>, Option<u32>) {
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "package_declaration" {
            let mut inner = child.walk();
            let package = child
                .children(&mut inner)
                .find(|part| part.kind() == "scoped_identifier" || part.kind() == "identifier")
                .map(|part| text[part.byte_range()].to_string());
            return (package, Some((child.end_position().row + 1) as u32));
        }
    }
    (None, None)
}

/// The fully qualified name of the TYPE that must be imported for `entry`: for a
/// type entry its own FQCN, for a member entry its enclosing type's. `None` when
/// there is nothing importable (default package, orphan member).
fn import_target(entry: &SymbolEntry) -> Option<String> {
    let package = entry.package.as_deref()?;
    let mut segments: Vec<String> = entry.container.to_vec();
    if !matches!(
        entry.kind,
        SymbolKind::Method | SymbolKind::Field | SymbolKind::EnumConstant
    ) {
        segments.push(entry.name.to_string());
    }
    if segments.is_empty() {
        return None;
    }
    Some(format!("{package}.{}", segments.join(".")))
}

/// The fully-qualified owner of an entry: its package plus its container chain.
fn qualified_owner(entry: &SymbolEntry) -> String {
    let package = entry
        .package
        .as_deref()
        .unwrap_or("the default package")
        .to_string();
    if entry.container.is_empty() {
        package
    } else {
        format!("{package}.{}", entry.container.join("."))
    }
}

/// An entry's fully-qualified identity: what distinguishes two symbols that share
/// a simple name.
fn symbol_identity(entry: &SymbolEntry) -> String {
    format!("{}.{}", qualified_owner(entry), entry.name)
}

/// The simple names claimed by more than one distinct symbol, so completions can
/// label each such item with its owner.
fn ambiguous_names(entries: &[Arc<SymbolEntry>]) -> HashSet<String> {
    let mut owners: HashMap<String, HashSet<String>> = HashMap::new();
    for entry in entries {
        if entry.kind == SymbolKind::Import {
            continue;
        }
        owners
            .entry(entry.name.to_string())
            .or_default()
            .insert(symbol_identity(entry));
    }
    owners
        .into_iter()
        .filter(|(_, identities)| identities.len() > 1)
        .map(|(name, _)| name)
        .collect()
}

/// The `import ...;` edit for an index-sourced completion item, or none when the
/// never-worsen rules say the file already resolves the name (or must not be
/// touched: conflicts). Returns an empty vec when no edit is needed.
///
/// `candidates` is every entry whose name shares the typed prefix — the entries
/// the item was drawn from — which is enough to spot a same-package clash: any
/// entry with the item's exact simple name starts with the prefix, so it is a
/// candidate.
fn import_edit(
    document_uri: &Url,
    entry: &SymbolEntry,
    document_package: Option<&str>,
    package_line: Option<u32>,
    imports: &[ExistingImport],
    candidates: &[Arc<SymbolEntry>],
) -> Vec<TextEdit> {
    let Some(target) = import_target(entry) else {
        return Vec::new();
    };
    if entry.uri.as_ref() == document_uri {
        return Vec::new(); // same file: the name already resolves
    }
    if entry.package.as_deref() == document_package {
        return Vec::new(); // same package (covers the default package too)
    }
    if entry.package.as_deref() == Some("java.lang") {
        return Vec::new(); // implicitly imported in Java
    }
    let simple = target.rsplit('.').next().unwrap_or("");
    for import in imports {
        if import.is_wildcard && import.path == entry.package.as_deref().unwrap_or("") {
            return Vec::new(); // already covered by `import pkg.*;`
        }
        if import.is_static || import.is_wildcard {
            continue;
        }
        if import.path == target {
            return Vec::new(); // already imported
        }
        if import.path.rsplit('.').next() == Some(simple) {
            return Vec::new(); // conflict: simple name claimed elsewhere
        }
    }
    // Same-package declarations with this simple name also conflict (the name
    // would already resolve in the file — to something else).
    let same_package_clash = candidates.iter().any(|other| {
        other.name.as_ref() == simple
            && other.origin != FileOrigin::Dependency
            && other.package.as_deref() == document_package
            && other.uri != entry.uri
    });
    if same_package_clash {
        return Vec::new();
    }

    // New imports go after the last import; else after the package statement;
    // else at the very top.
    let line = imports
        .last()
        .map(|import| import.line)
        .or(package_line)
        .unwrap_or(0);
    let text = format!("import {target};\n");
    vec![TextEdit {
        range: Range::new(Position::new(line, 0), Position::new(line, 0)),
        new_text: text,
    }]
}

// ---------------------------------------------------------------------------
// Presentation
// ---------------------------------------------------------------------------

/// `SymbolKind` → `CompletionItemKind`; `Import` entries are not nameable and
/// contribute nothing.
fn completion_kind(kind: SymbolKind) -> Option<CompletionItemKind> {
    match kind {
        SymbolKind::Class => Some(CompletionItemKind::CLASS),
        SymbolKind::Interface => Some(CompletionItemKind::INTERFACE),
        SymbolKind::Enum => Some(CompletionItemKind::ENUM),
        SymbolKind::Record => Some(CompletionItemKind::STRUCT),
        SymbolKind::Method => Some(CompletionItemKind::METHOD),
        SymbolKind::Field => Some(CompletionItemKind::FIELD),
        SymbolKind::EnumConstant => Some(CompletionItemKind::ENUM_MEMBER),
        SymbolKind::Import => None,
    }
}

fn kind_word(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Class => "class",
        SymbolKind::Interface => "interface",
        SymbolKind::Enum => "enum",
        SymbolKind::Record => "record",
        SymbolKind::Method => "method",
        SymbolKind::Field => "field",
        SymbolKind::EnumConstant => "enum constant",
        SymbolKind::Import => "import",
    }
}

/// Adds one completion unless its insert text was already offered by a
/// higher-ranked source, so the list never shows the same insert text twice.
#[allow(clippy::too_many_arguments)]
fn offer(
    items: &mut Vec<CompletionItem>,
    seen: &mut HashSet<String>,
    key: String,
    label: String,
    filter_text: String,
    insert_text: String,
    kind: CompletionItemKind,
    detail: Option<String>,
    sort_text: String,
    additional_edits: Vec<TextEdit>,
) {
    if !seen.insert(key) {
        return;
    }
    items.push(CompletionItem {
        label,
        kind: Some(kind),
        detail,
        filter_text: Some(filter_text),
        insert_text: Some(insert_text),
        sort_text: Some(sort_text),
        additional_text_edits: (!additional_edits.is_empty()).then_some(additional_edits),
        ..CompletionItem::default()
    });
}

/// One method-overload completion item — the shape both the member path and the
/// workspace-index path offer: labelled with the member's full signature,
/// filtered by its bare name, inserting `name(` to open the argument list, with
/// the declaring type as the `detail`. `key` dedupes within a listing (the
/// signature, plus the import where the workspace path needs it distinct), and
/// `sort_text` ranks the item.
#[allow(clippy::too_many_arguments)]
fn offer_overload(
    items: &mut Vec<CompletionItem>,
    seen: &mut HashSet<String>,
    member: &Member,
    declaring: &str,
    key: String,
    sort_text: String,
    additional_edits: Vec<TextEdit>,
) {
    offer(
        items,
        seen,
        key,
        member.signature(),
        member.name.clone(),
        format!("{}(", member.name),
        CompletionItemKind::METHOD,
        Some(declaring.to_string()),
        sort_text,
        additional_edits,
    );
}

/// The Java language keywords offered when nothing (or a partial word) precedes
/// the cursor.
const JAVA_KEYWORDS: &[&str] = &[
    "abstract",
    "assert",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extends",
    "false",
    "final",
    "finally",
    "float",
    "for",
    "goto",
    "if",
    "implements",
    "import",
    "instanceof",
    "int",
    "interface",
    "long",
    "native",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "short",
    "static",
    "strictfp",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "true",
    "try",
    "void",
    "volatile",
    "while",
];

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::hub::Hub;
    use crate::index::{scan_sources, set_ready, WorkspaceIndex};
    use crate::messages::Notification;
    use crate::parser::LineIndex;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "java-lsp-completion-{name}-{}-{}",
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

    /// Builds an index by scanning the given files from disk, so completion reads
    /// the same sources it was built from.
    fn index_of(uris: &[Url]) -> WorkspaceIndex {
        let paths: Vec<PathBuf> = uris.iter().map(|uri| uri.to_file_path().unwrap()).collect();
        let mut index = WorkspaceIndex::default();
        let count = scan_sources(&paths, FileOrigin::Workspace, &mut index);
        assert_eq!(count, uris.len(), "every file indexed");
        set_ready(&mut index);
        index
    }

    /// The position of the character right after the first `needle` in `text`.
    fn cursor_after(text: &str, needle: &str) -> Position {
        let offset = text.find(needle).expect("needle present") + needle.len();
        LineIndex::new(text).position(text, offset)
    }

    /// A fresh hub with the index (preloaded from `uris`), the buffers module,
    /// and completion spawned, and a client registered to drive
    /// [`Request::Completion`] through.
    async fn completion_hub(uris: &[Url]) -> HubClient {
        let hub = Hub::new();
        crate::index::spawn_with(&hub, index_of(uris));
        crate::buffers::spawn(&hub);
        spawn(&hub);
        hub.register("test")
    }

    /// Sends a `Complete` and reads its items.
    async fn complete_at(client: &HubClient, uri: &Url, position: Position) -> Vec<CompletionItem> {
        let reply = client
            .request(Request::Completion(CompletionRequest::Complete {
                uri: uri.clone(),
                position,
            }))
            .await;
        match reply {
            Response::Completion(CompletionResponse::Items(items)) => items,
            _ => Vec::new(),
        }
    }

    fn labels(items: &[CompletionItem]) -> Vec<String> {
        items.iter().map(|item| item.label.clone()).collect()
    }

    #[tokio::test]
    async fn completion_reads_the_open_buffer_not_disk() {
        let dir = temp_dir("buffer");
        // On disk the type has no `count` field; the unsaved buffer adds one.
        let disk = "class Sample {\n    void m() {\n        cou\n    }\n}\n";
        let uri = write(&dir, "Sample.java", disk);
        let client = completion_hub(std::slice::from_ref(&uri)).await;

        let buffered =
            "class Sample {\n    int count = 1;\n    void m() {\n        cou\n    }\n}\n";
        client.notify(Notification::DocumentOpened {
            uri: uri.clone(),
            version: 2,
            text: Arc::new(buffered.to_string()),
        });

        let items = complete_at(&client, &uri, cursor_after(buffered, "cou")).await;

        assert!(
            labels(&items).contains(&"count".to_string()),
            "{:?}",
            labels(&items)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn keywords_matching_the_prefix_are_offered() {
        let dir = temp_dir("keywords");
        let text = "class Sample {\n    void m() {\n        fina\n    }\n}\n";
        let uri = write(&dir, "Sample.java", text);
        let client = completion_hub(std::slice::from_ref(&uri)).await;

        let items = complete_at(&client, &uri, cursor_after(text, "fina")).await;

        assert!(
            labels(&items).contains(&"final".to_string()),
            "{:?}",
            labels(&items)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn locals_parameters_and_fields_in_scope_are_offered() {
        let dir = temp_dir("scoped");
        let text = "class Sample {\n    int count = 1;\n    void m(int index) {\n        int local = 2;\n        cou\n        ind\n        loc\n    }\n}\n";
        let uri = write(&dir, "Sample.java", text);
        let client = completion_hub(std::slice::from_ref(&uri)).await;

        let field = complete_at(&client, &uri, cursor_after(text, "\n        cou")).await;
        assert!(
            labels(&field).contains(&"count".to_string()),
            "{:?}",
            labels(&field)
        );

        let parameter = complete_at(&client, &uri, cursor_after(text, "\n        ind")).await;
        assert!(
            labels(&parameter).contains(&"index".to_string()),
            "{:?}",
            labels(&parameter)
        );

        let local = complete_at(&client, &uri, cursor_after(text, "\n        loc")).await;
        assert!(
            labels(&local).contains(&"local".to_string()),
            "{:?}",
            labels(&local)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_workspace_symbol_carries_its_auto_import() {
        let dir = temp_dir("import");
        let widget = write(
            &dir,
            "a/Widget.java",
            "package a;\n\npublic class Widget {}\n",
        );
        let text = "package b;\n\nclass Main {\n    void m() {\n        Wid\n    }\n}\n";
        let main = write(&dir, "b/Main.java", text);
        let client = completion_hub(&[widget, main.clone()]).await;

        let items = complete_at(&client, &main, cursor_after(text, "Wid")).await;
        let widget_item = items
            .iter()
            .find(|item| item.label == "Widget")
            .expect("Widget is offered");

        let edits = widget_item
            .additional_text_edits
            .as_ref()
            .expect("Widget carries an auto-import");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "import a.Widget;\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_bare_method_lists_each_overload_with_its_signature() {
        let dir = temp_dir("index-overloads");
        // The type declares two overloads; a bare `add` offers each with its
        // full signature and inserts `add(` — the shape the member path uses.
        let text = "package a;\nclass Calc {\n    void add(int x) {}\n    void add(int x, int y) {}\n    void m() {\n        add\n    }\n}\n";
        let uri = write(&dir, "a/Calc.java", text);
        let client = completion_hub(std::slice::from_ref(&uri)).await;

        let items = complete_at(&client, &uri, cursor_after(text, "\n        add")).await;
        let names = labels(&items);

        assert!(names.contains(&"void add(int x)".to_string()), "{names:?}");
        assert!(
            names.contains(&"void add(int x, int y)".to_string()),
            "{names:?}"
        );
        // The bare name is not offered as a name-only item, and no third `add`
        // item survives.
        assert!(!names.contains(&"add".to_string()), "{names:?}");
        assert_eq!(names.iter().filter(|name| name.contains("add")).count(), 2);

        let add_x = items
            .iter()
            .find(|item| item.label == "void add(int x)")
            .unwrap();
        assert_eq!(add_x.filter_text.as_deref(), Some("add"));
        assert_eq!(add_x.insert_text.as_deref(), Some("add("));
        assert_eq!(add_x.detail.as_deref(), Some("Calc"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_non_overloaded_bare_method_is_a_single_item() {
        let dir = temp_dir("index-single");
        let text = "package a;\nclass Calc {\n    void greet(String name) {}\n    void m() {\n        gre\n    }\n}\n";
        let uri = write(&dir, "a/Calc.java", text);
        let client = completion_hub(std::slice::from_ref(&uri)).await;

        let items = complete_at(&client, &uri, cursor_after(text, "\n        gre")).await;
        let names = labels(&items);

        assert!(
            names.contains(&"void greet(String name)".to_string()),
            "{names:?}"
        );
        assert_eq!(
            names.iter().filter(|name| name.contains("greet")).count(),
            1
        );
        let greet = items
            .iter()
            .find(|item| item.label == "void greet(String name)")
            .unwrap();
        assert_eq!(greet.filter_text.as_deref(), Some("greet"));
        assert_eq!(greet.insert_text.as_deref(), Some("greet("));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_method_whose_declaring_type_does_not_resolve_keeps_the_name_only_item() {
        let dir = temp_dir("index-unresolved");
        // `run` is declared in an anonymous class body; the index records its
        // container as the enclosing `Main`, which does not declare `run`, so its
        // declaring type yields no overload and the name-only item stands rather
        // than the symbol disappearing.
        let text = "class Main {\n    void m() {\n        Runnable r = new Runnable() {\n            public void run() {}\n        };\n        ru\n    }\n}\n";
        let uri = write(&dir, "Main.java", text);
        let client = completion_hub(std::slice::from_ref(&uri)).await;

        let items = complete_at(&client, &uri, cursor_after(text, "\n        ru")).await;
        let run = items
            .iter()
            .find(|item| item.label == "Main.run")
            .expect("the name-only item is kept");
        // The fallback keeps today's shape: insert the bare name, no `(`.
        assert_eq!(run.insert_text.as_deref(), Some("run"));
        assert!(!labels(&items).contains(&"void run()".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_bare_field_and_type_keep_their_single_item_shape() {
        let dir = temp_dir("index-field-type");
        let widget = write(
            &dir,
            "a/Widget.java",
            "package a;\npublic class Widget {\n    int size = 0;\n}\n",
        );
        let text = "package b;\nclass Main {\n    void m() {\n        si\n    }\n}\n";
        let main = write(&dir, "b/Main.java", text);
        let client = completion_hub(&[widget.clone(), main.clone()]).await;

        // A field keeps its name-only shape (insert the bare name).
        let field = complete_at(&client, &main, cursor_after(text, "\n        si")).await;
        let size = field
            .iter()
            .find(|item| item.label == "Widget.size")
            .expect("the field is offered");
        assert_eq!(size.filter_text.as_deref(), Some("size"));
        assert_eq!(size.insert_text.as_deref(), Some("size"));
        assert_eq!(size.detail.as_deref(), Some("field of Widget"));

        // A type keeps its name-only shape too, with its auto-import.
        let type_text = "package b;\nclass Main {\n    void m() {\n        Wid\n    }\n}\n";
        let type_main = write(&dir, "b/TypeMain.java", type_text);
        let client = completion_hub(&[widget, type_main.clone()]).await;
        let types = complete_at(&client, &type_main, cursor_after(type_text, "Wid")).await;
        let widget_item = types
            .iter()
            .find(|item| item.label == "Widget")
            .expect("the type is offered");
        assert_eq!(widget_item.insert_text.as_deref(), Some("Widget"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_bare_method_overload_carries_its_auto_import() {
        let dir = temp_dir("index-method-import");
        let widget = write(
            &dir,
            "a/Widget.java",
            "package a;\npublic class Widget {\n    public void frob(int x) {}\n    public void frob(int x, int y) {}\n}\n",
        );
        let text = "package b;\nclass Main {\n    void m() {\n        fro\n    }\n}\n";
        let main = write(&dir, "b/Main.java", text);
        let client = completion_hub(&[widget, main.clone()]).await;

        let items = complete_at(&client, &main, cursor_after(text, "\n        fro")).await;
        let names = labels(&items);
        assert!(names.contains(&"void frob(int x)".to_string()), "{names:?}");
        assert!(
            names.contains(&"void frob(int x, int y)".to_string()),
            "{names:?}"
        );

        // Each overload item carries the entry's auto-import edit.
        for label in ["void frob(int x)", "void frob(int x, int y)"] {
            let item = items.iter().find(|item| item.label == label).unwrap();
            assert_eq!(item.insert_text.as_deref(), Some("frob("));
            assert_eq!(item.detail.as_deref(), Some("Widget"));
            let edits = item
                .additional_text_edits
                .as_ref()
                .expect("the overload carries an auto-import");
            assert_eq!(edits.len(), 1);
            assert_eq!(edits[0].new_text, "import a.Widget;\n");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bare_methods_of_two_declaring_types_each_list_their_own_overloads() {
        let dir = temp_dir("index-two-types");
        // Two workspace types declare a same-named method with overloads; a bare
        // name resolves each entry's overloads from its own declaring type, so the
        // batched read returns the right members per `(declaring type, package)`.
        let calc = write(
            &dir,
            "a/Calc.java",
            "package a;\npublic class Calc {\n    public void add(int x) {}\n    public void add(String s) {}\n}\n",
        );
        let counter = write(
            &dir,
            "a/Counter.java",
            "package a;\npublic class Counter {\n    public void add(double d) {}\n}\n",
        );
        let text = "package b;\nclass Main {\n    void m() {\n        ad\n    }\n}\n";
        let main = write(&dir, "b/Main.java", text);
        let client = completion_hub(&[calc, counter, main.clone()]).await;

        let items = complete_at(&client, &main, cursor_after(text, "\n        ad")).await;
        let names = labels(&items);
        assert!(names.contains(&"void add(int x)".to_string()), "{names:?}");
        assert!(
            names.contains(&"void add(String s)".to_string()),
            "{names:?}"
        );
        assert!(
            names.contains(&"void add(double d)".to_string()),
            "{names:?}"
        );

        // Each overload carries its own declaring type as `detail` and its own
        // type's auto-import — the batch's per-type lookup, not a shared one.
        let int_add = items
            .iter()
            .find(|item| item.label == "void add(int x)")
            .unwrap();
        assert_eq!(int_add.detail.as_deref(), Some("Calc"));
        assert_eq!(
            int_add
                .additional_text_edits
                .as_ref()
                .expect("the Calc overload carries an auto-import")[0]
                .new_text,
            "import a.Calc;\n"
        );
        let double_add = items
            .iter()
            .find(|item| item.label == "void add(double d)")
            .unwrap();
        assert_eq!(double_add.detail.as_deref(), Some("Counter"));
        assert_eq!(
            double_add
                .additional_text_edits
                .as_ref()
                .expect("the Counter overload carries an auto-import")[0]
                .new_text,
            "import a.Counter;\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_field_receiver_offers_members_fields_before_methods() {
        let dir = temp_dir("member-field");
        let widget = write(
            &dir,
            "a/Widget.java",
            "package a;\nclass Widget {\n    int size;\n    String label;\n    void run() {}\n    void run(int times) {}\n}\n",
        );
        let text = "package a;\nclass Main {\n    Widget widget;\n    void m() {\n        widget.\n    }\n}\n";
        let main = write(&dir, "a/Main.java", text);
        let client = completion_hub(&[widget, main.clone()]).await;

        let items = complete_at(&client, &main, cursor_after(text, "widget.")).await;
        let names = labels(&items);

        // Fields are single items; each method overload is its own item.
        assert!(names.contains(&"size".to_string()), "{names:?}");
        assert!(names.contains(&"label".to_string()), "{names:?}");
        assert!(names.contains(&"void run()".to_string()), "{names:?}");
        assert!(
            names.contains(&"void run(int times)".to_string()),
            "{names:?}"
        );

        // Fields sort before methods.
        let field_at = names.iter().position(|name| name == "size").unwrap();
        let method_at = names.iter().position(|name| name == "void run()").unwrap();
        assert!(field_at < method_at, "fields before methods: {names:?}");

        // A field keeps its name; a method inserts `name(` and carries the
        // (resolved) declaring type as its detail.
        let size = items.iter().find(|item| item.label == "size").unwrap();
        assert_eq!(size.insert_text.as_deref(), Some("size"));
        let run = items
            .iter()
            .find(|item| item.label == "void run(int times)")
            .unwrap();
        assert_eq!(run.filter_text.as_deref(), Some("run"));
        assert_eq!(run.insert_text.as_deref(), Some("run("));
        assert_eq!(run.detail.as_deref(), Some("Widget"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_partial_name_after_the_dot_filters_the_members() {
        let dir = temp_dir("member-prefix");
        let widget = write(
            &dir,
            "a/Widget.java",
            "package a;\nclass Widget {\n    int size;\n    void run() {}\n    void rest() {}\n}\n",
        );
        let text = "package a;\nclass Main {\n    Widget widget;\n    void m() {\n        widget.r\n    }\n}\n";
        let main = write(&dir, "a/Main.java", text);
        let client = completion_hub(&[widget, main.clone()]).await;

        let items = complete_at(&client, &main, cursor_after(text, "widget.r")).await;
        let names = labels(&items);

        assert!(names.contains(&"void run()".to_string()), "{names:?}");
        assert!(names.contains(&"void rest()".to_string()), "{names:?}");
        assert!(!names.contains(&"size".to_string()), "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_static_qualifier_offers_only_static_members() {
        let dir = temp_dir("member-static");
        let text = "\
package a;
class Widget {
    static int count;
    static void make() {}
    int size;
    void run() {}
}
class Main {
    void m() {
        Widget.
    }
}
";
        let uri = write(&dir, "a/Main.java", text);
        let client = completion_hub(std::slice::from_ref(&uri)).await;

        let items = complete_at(&client, &uri, cursor_after(text, "Widget.")).await;
        let names = labels(&items);

        assert!(names.contains(&"count".to_string()), "{names:?}");
        assert!(
            names.contains(&"static void make()".to_string()),
            "{names:?}"
        );
        assert!(!names.contains(&"size".to_string()), "{names:?}");
        assert!(!names.contains(&"void run()".to_string()), "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn this_receiver_offers_the_enclosing_types_members() {
        let dir = temp_dir("member-this");
        let text = "package a;\nclass Sample {\n    int count;\n    void m() {\n        this.count;\n    }\n}\n";
        let uri = write(&dir, "a/Sample.java", text);
        let client = completion_hub(std::slice::from_ref(&uri)).await;

        let items = complete_at(&client, &uri, cursor_after(text, "this.")).await;

        assert!(
            labels(&items).contains(&"count".to_string()),
            "{:?}",
            labels(&items)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn parameter_and_local_receivers_resolve() {
        let dir = temp_dir("member-scope");
        let widget = write(
            &dir,
            "a/Widget.java",
            "package a;\nclass Widget {\n    void run() {}\n}\n",
        );
        let text = "\
package a;
class Main {
    void m(Widget param) {
        Widget local = null;
        param.run();
        local.run();
    }
}
";
        let main = write(&dir, "a/Main.java", text);
        let client = completion_hub(&[widget, main.clone()]).await;

        let parameter = complete_at(&client, &main, cursor_after(text, "param.")).await;
        assert!(
            labels(&parameter).contains(&"void run()".to_string()),
            "{:?}",
            labels(&parameter)
        );
        let local = complete_at(&client, &main, cursor_after(text, "local.")).await;
        assert!(
            labels(&local).contains(&"void run()".to_string()),
            "{:?}",
            labels(&local)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_new_receiver_resolves_the_created_type() {
        let dir = temp_dir("member-new");
        let widget = write(
            &dir,
            "a/Widget.java",
            "package a;\nclass Widget {\n    void run() {}\n}\n",
        );
        let text = "package a;\nclass Main {\n    void m() {\n        new Widget().\n    }\n}\n";
        let main = write(&dir, "a/Main.java", text);
        let client = completion_hub(&[widget, main.clone()]).await;

        let items = complete_at(&client, &main, cursor_after(text, "new Widget().")).await;

        assert!(
            labels(&items).contains(&"void run()".to_string()),
            "{:?}",
            labels(&items)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_var_local_receiver_infers_its_initializers_type() {
        let dir = temp_dir("member-var");
        let widget = write(
            &dir,
            "a/Widget.java",
            "package a;\nclass Widget {\n    int size;\n    void run() {}\n}\n",
        );
        let text = "\
package a;
class Main {
    void m() {
        var w = new Widget();
        w.
    }
}
";
        let main = write(&dir, "a/Main.java", text);
        let client = completion_hub(&[widget, main.clone()]).await;

        let items = complete_at(&client, &main, cursor_after(text, "w.")).await;
        let names = labels(&items);

        assert!(names.contains(&"size".to_string()), "{names:?}");
        assert!(names.contains(&"void run()".to_string()), "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_var_local_receiver_infers_a_method_calls_return_type() {
        let dir = temp_dir("member-var-call");
        let holder = write(
            &dir,
            "a/Holder.java",
            "package a;\nclass Holder {\n    Widget get() {\n        return null;\n    }\n}\n",
        );
        let widget = write(
            &dir,
            "a/Widget.java",
            "package a;\nclass Widget {\n    int size;\n    void run() {}\n}\n",
        );
        let text = "\
package a;
class Main {
    void m(Holder holder) {
        var w = holder.get();
        w.
    }
}
";
        let main = write(&dir, "a/Main.java", text);
        let client = completion_hub(&[holder, widget, main.clone()]).await;

        let items = complete_at(&client, &main, cursor_after(text, "w.")).await;
        let names = labels(&items);

        assert!(names.contains(&"size".to_string()), "{names:?}");
        assert!(names.contains(&"void run()".to_string()), "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_var_local_with_an_uninferrable_initializer_yields_nothing() {
        let dir = temp_dir("member-var-uninferrable");
        let text = "\
package a;
class Main {
    void m() {
        var f = (Runnable) () -> {};
        var call = missing();
        f.
        call.
    }
}
";
        let uri = write(&dir, "a/Main.java", text);
        let client = completion_hub(std::slice::from_ref(&uri)).await;

        // The initializer `missing()` resolves nowhere and the lambda is not a
        // resolvable receiver shape: both bindings stay untyped, so `.` yields
        // nothing rather than a guess.
        let items = complete_at(&client, &uri, cursor_after(text, "f.")).await;
        assert!(items.is_empty(), "{:?}", labels(&items));
        let unused = complete_at(&client, &uri, cursor_after(text, "call.")).await;
        assert!(unused.is_empty(), "{:?}", labels(&unused));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_enum_receiver_offers_its_constants() {
        let dir = temp_dir("member-enum");
        let text = "\
package a;
enum Color {
    RED,
    GREEN,
}
class Main {
    void m() {
        Color color = Color.RED;
        color.
    }
}
";
        let uri = write(&dir, "a/Main.java", text);
        let client = completion_hub(std::slice::from_ref(&uri)).await;

        let items = complete_at(&client, &uri, cursor_after(text, "color.")).await;
        let names = labels(&items);

        assert!(names.contains(&"RED".to_string()), "{names:?}");
        assert!(names.contains(&"GREEN".to_string()), "{names:?}");
        let red = items.iter().find(|item| item.label == "RED").unwrap();
        assert_eq!(red.kind, Some(CompletionItemKind::ENUM_MEMBER));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_record_receiver_offers_its_component_accessors() {
        let dir = temp_dir("member-record");
        let text = "\
package a;
record Point(int x, int y) {}
class Main {
    void m() {
        Point point = null;
        point.
    }
}
";
        let uri = write(&dir, "a/Main.java", text);
        let client = completion_hub(std::slice::from_ref(&uri)).await;

        let items = complete_at(&client, &uri, cursor_after(text, "point.")).await;
        let names = labels(&items);

        assert!(names.contains(&"int x()".to_string()), "{names:?}");
        assert!(names.contains(&"int y()".to_string()), "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn inherited_members_appear_and_an_override_collapses() {
        let dir = temp_dir("member-inherit");
        let base = write(
            &dir,
            "a/Base.java",
            "package a;\nclass Base {\n    int inherited;\n    void run() {}\n}\n",
        );
        let text = "\
package a;
class Widget extends Base {
    void run() {}
    void run(int times) {}
}
class Main {
    Widget widget;
    void m() {
        widget.
    }
}
";
        let main = write(&dir, "a/Main.java", text);
        let client = completion_hub(&[base, main.clone()]).await;

        let items = complete_at(&client, &main, cursor_after(text, "widget.")).await;
        let names = labels(&items);

        assert!(names.contains(&"inherited".to_string()), "{names:?}");
        assert!(
            names.contains(&"void run(int times)".to_string()),
            "{names:?}"
        );
        assert_eq!(
            names.iter().filter(|name| *name == "void run()").count(),
            1,
            "an override collapses: {names:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_interface_field_receiver_offers_its_members() {
        let dir = temp_dir("member-interface");
        let greeter = write(
            &dir,
            "a/Greeter.java",
            "package a;\ninterface Greeter {\n    void greet();\n}\n",
        );
        let text = "\
package a;
class Main {
    Greeter greeter;
    void m() {
        greeter.
    }
    void n() {
        greeter.gr
    }
}
";
        let main = write(&dir, "a/Main.java", text);
        let client = completion_hub(&[greeter, main.clone()]).await;

        let whole = complete_at(&client, &main, cursor_after(text, "greeter.")).await;
        assert!(
            labels(&whole).contains(&"void greet()".to_string()),
            "{:?}",
            labels(&whole)
        );
        let partial = complete_at(&client, &main, cursor_after(text, "greeter.gr")).await;
        assert!(
            labels(&partial).contains(&"void greet()".to_string()),
            "{:?}",
            labels(&partial)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_unresolved_receiver_yields_nothing() {
        let dir = temp_dir("member-unresolved");
        let text = "class Sample {\n    void m() {\n        obj.fo\n    }\n}\n";
        let uri = write(&dir, "Sample.java", text);
        let client = completion_hub(std::slice::from_ref(&uri)).await;

        let items = complete_at(&client, &uri, cursor_after(text, "obj.fo")).await;

        assert!(items.is_empty(), "{:?}", labels(&items));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_type_variable_receiver_yields_nothing() {
        let dir = temp_dir("member-typevar");
        let text =
            "package a;\nclass Sample<T> {\n    void m(T value) {\n        value.\n    }\n}\n";
        let uri = write(&dir, "a/Sample.java", text);
        let client = completion_hub(std::slice::from_ref(&uri)).await;

        let items = complete_at(&client, &uri, cursor_after(text, "value.")).await;

        assert!(items.is_empty(), "{:?}", labels(&items));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_array_receiver_yields_nothing() {
        let dir = temp_dir("member-array");
        let text =
            "package a;\nclass Sample {\n    void m(int[] values) {\n        values.\n    }\n}\n";
        let uri = write(&dir, "a/Sample.java", text);
        let client = completion_hub(std::slice::from_ref(&uri)).await;

        let items = complete_at(&client, &uri, cursor_after(text, "values.")).await;

        assert!(items.is_empty(), "{:?}", labels(&items));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn nothing_typed_yields_nothing() {
        let dir = temp_dir("empty");
        let text = "class Sample {\n    void m() {\n        \n    }\n}\n";
        let uri = write(&dir, "Sample.java", text);
        let client = completion_hub(std::slice::from_ref(&uri)).await;

        let position = cursor_after(text, "        \n");
        let items = complete_at(&client, &uri, position).await;

        assert!(items.is_empty(), "{:?}", labels(&items));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_chained_call_receiver_completes_on_the_return_type() {
        let dir = temp_dir("member-chain-call");
        // `List.get` returns the element type, so `list.get(0).` resolves to it.
        let list = write(
            &dir,
            "a/List.java",
            "package a;\nclass List {\n    Widget get(int index) {\n        return null;\n    }\n}\n",
        );
        let widget = write(
            &dir,
            "a/Widget.java",
            "package a;\nclass Widget {\n    int size;\n    void run() {}\n}\n",
        );
        let text = "\
package a;
class Main {
    List list;
    void m() {
        list.get(0).foo;
    }
}
";
        let main = write(&dir, "a/Main.java", text);
        let client = completion_hub(&[list, widget, main.clone()]).await;

        let items = complete_at(&client, &main, cursor_after(text, "list.get(0).")).await;
        let names = labels(&items);

        assert!(names.contains(&"size".to_string()), "{names:?}");
        assert!(names.contains(&"void run()".to_string()), "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_longer_chain_resolves_left_to_right() {
        let dir = temp_dir("member-chain-long");
        // `holder.get()` returns a `List`, whose `at(0)` returns the element type:
        // two links, each typed from the receiver before it.
        let text = "\
package a;
class List {
    Widget at(int index) {
        return null;
    }
}
class Holder {
    List get() {
        return null;
    }
}
class Widget {
    int size;
    void run() {}
}
class Main {
    Holder holder;
    void m() {
        holder.get().at(0).foo;
    }
}
";
        let uri = write(&dir, "a/Main.java", text);
        let client = completion_hub(std::slice::from_ref(&uri)).await;

        let items = complete_at(&client, &uri, cursor_after(text, "holder.get().at(0).")).await;
        let names = labels(&items);

        assert!(names.contains(&"size".to_string()), "{names:?}");
        assert!(names.contains(&"void run()".to_string()), "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_chained_static_call_receiver_completes_on_the_return_type() {
        let dir = temp_dir("member-chain-static");
        let widget = write(
            &dir,
            "a/Widget.java",
            "package a;\nclass Widget {\n    int size;\n    void run() {}\n}\n",
        );
        let text = "\
package a;
class Foo {
    static Widget bar() {
        return null;
    }
}
class Main {
    void m() {
        Foo.bar().baz;
    }
}
";
        let main = write(&dir, "a/Main.java", text);
        let client = completion_hub(&[widget, main.clone()]).await;

        let items = complete_at(&client, &main, cursor_after(text, "Foo.bar().")).await;
        let names = labels(&items);

        assert!(names.contains(&"size".to_string()), "{names:?}");
        assert!(names.contains(&"void run()".to_string()), "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_unresolvable_chain_yields_nothing() {
        let dir = temp_dir("member-chain-unresolved");
        let text = "package a;\nclass Main {\n    void m() {\n        obj.foo().bar;\n    }\n}\n";
        let uri = write(&dir, "a/Main.java", text);
        let client = completion_hub(std::slice::from_ref(&uri)).await;

        let items = complete_at(&client, &uri, cursor_after(text, "obj.foo().")).await;

        assert!(items.is_empty(), "{:?}", labels(&items));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_field_access_receiver_completes_on_the_fields_type() {
        let dir = temp_dir("member-field-access");
        let widget = write(
            &dir,
            "a/Widget.java",
            "package a;\nclass Widget {\n    int size;\n    void run() {}\n}\n",
        );
        // `a.field` is a field access, so `foo` completes on `Widget`.
        let text = "\
package a;
class Holder {
    Widget field;
}
class Main {
    Holder a;
    void m() {
        a.field.foo;
    }
}
";
        let main = write(&dir, "a/Main.java", text);
        let client = completion_hub(&[widget, main.clone()]).await;

        let items = complete_at(&client, &main, cursor_after(text, "a.field.")).await;
        let names = labels(&items);

        assert!(names.contains(&"size".to_string()), "{names:?}");
        assert!(names.contains(&"void run()".to_string()), "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn this_field_access_receiver_completes_on_the_fields_type() {
        let dir = temp_dir("member-this-field");
        let text = "\
package a;
class Widget {
    int size;
    void run() {}
}
class Main {
    Widget field;
    void m() {
        this.field.foo;
    }
}
";
        let uri = write(&dir, "a/Main.java", text);
        let client = completion_hub(std::slice::from_ref(&uri)).await;

        let items = complete_at(&client, &uri, cursor_after(text, "this.field.")).await;
        let names = labels(&items);

        assert!(names.contains(&"size".to_string()), "{names:?}");
        assert!(names.contains(&"void run()".to_string()), "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_chain_through_a_field_resolves_left_to_right() {
        let dir = temp_dir("member-chain-field");
        // `a.list` is a field of type `List`, whose `get(0)` returns `Widget`:
        // the field link and the call link each typed from the one before it.
        let text = "\
package a;
class Widget {
    int size;
    void run() {}
}
class List {
    Widget get(int index) {
        return null;
    }
}
class Holder {
    List list;
}
class Main {
    Holder a;
    void m() {
        a.list.get(0).foo;
    }
}
";
        let uri = write(&dir, "a/Main.java", text);
        let client = completion_hub(std::slice::from_ref(&uri)).await;

        let items = complete_at(&client, &uri, cursor_after(text, "a.list.get(0).")).await;
        let names = labels(&items);

        assert!(names.contains(&"size".to_string()), "{names:?}");
        assert!(names.contains(&"void run()".to_string()), "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_unresolvable_field_link_yields_nothing() {
        let dir = temp_dir("member-field-unresolved");
        let text = "package a;\nclass Main {\n    void m() {\n        obj.field.bar;\n    }\n}\n";
        let uri = write(&dir, "a/Main.java", text);
        let client = completion_hub(std::slice::from_ref(&uri)).await;

        let items = complete_at(&client, &uri, cursor_after(text, "obj.field.")).await;

        assert!(items.is_empty(), "{:?}", labels(&items));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Sends a `SignatureHelp` and reads the signatures' labels and the active
    /// parameter the first signature reports; `None` when the module answers no
    /// help.
    async fn signature_at(
        client: &HubClient,
        uri: &Url,
        position: Position,
    ) -> Option<(Vec<String>, u32)> {
        let reply = client
            .request(Request::Completion(CompletionRequest::SignatureHelp {
                uri: uri.clone(),
                position,
            }))
            .await;
        match reply {
            Response::Completion(CompletionResponse::SignatureHelp(Some(help))) => {
                let labels = help
                    .signatures
                    .iter()
                    .map(|signature| signature.label.clone())
                    .collect();
                let active = help
                    .signatures
                    .first()
                    .and_then(|signature| signature.active_parameter)
                    .unwrap_or(0);
                Some((labels, active))
            }
            _ => None,
        }
    }

    #[tokio::test]
    async fn signature_help_lists_overloads_and_follows_the_active_parameter() {
        let dir = temp_dir("signature-overloads");
        let calc = write(
            &dir,
            "a/Calc.java",
            "package a;\nclass Calc {\n    void add(int a) {}\n    void add(int a, int b) {}\n}\n",
        );
        let text = "\
package a;
class Main {
    void m() {
        Calc calc = null;
        calc.add(1, 2);
    }
}
";
        let main = write(&dir, "a/Main.java", text);
        let client = completion_hub(&[calc, main.clone()]).await;

        let (labels, active) = signature_at(&client, &main, cursor_after(text, "calc.add("))
            .await
            .expect("signature help at the open argument list");
        assert!(
            labels.contains(&"void add(int a)".to_string()),
            "{labels:?}"
        );
        assert!(
            labels.contains(&"void add(int a, int b)".to_string()),
            "{labels:?}"
        );
        assert_eq!(active, 0);

        let (_, active) = signature_at(&client, &main, cursor_after(text, "calc.add(1, "))
            .await
            .expect("signature help at the second argument");
        assert_eq!(active, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn signature_help_answers_constructor_overloads_and_the_implicit_no_arg() {
        let dir = temp_dir("signature-constructors");
        let point = write(
            &dir,
            "a/Point.java",
            "package a;\nclass Point {\n    Point(int x, int y) {}\n    Point() {}\n}\n",
        );
        let text = "\
package a;
class Plain {
}
class Main {
    void m() {
        new Point(1, 2);
        new Plain();
    }
}
";
        let main = write(&dir, "a/Main.java", text);
        let client = completion_hub(&[point, main.clone()]).await;

        let (labels, active) = signature_at(&client, &main, cursor_after(text, "new Point("))
            .await
            .expect("signature help for the constructor call");
        assert!(labels.contains(&"Point()".to_string()), "{labels:?}");
        assert!(
            labels.contains(&"Point(int x, int y)".to_string()),
            "{labels:?}"
        );
        assert_eq!(active, 0);

        let (_, active) = signature_at(&client, &main, cursor_after(text, "new Point(1, "))
            .await
            .expect("signature help at the second argument");
        assert_eq!(active, 1);

        let (labels, _) = signature_at(&client, &main, cursor_after(text, "new Plain("))
            .await
            .expect("the implicit no-arg constructor answers");
        assert!(labels.contains(&"Plain()".to_string()), "{labels:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn signature_help_yields_nothing_for_an_unresolvable_callee() {
        let dir = temp_dir("signature-unresolved");
        let text =
            "package a;\nclass Main {\n    void m() {\n        missing.value(1);\n    }\n}\n";
        let uri = write(&dir, "a/Main.java", text);
        let client = completion_hub(std::slice::from_ref(&uri)).await;

        let help = signature_at(&client, &uri, cursor_after(text, "missing.value(")).await;

        assert!(help.is_none(), "{help:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
