//! The index's coarse read API: the navigation-shaped reads a consumer of the
//! index makes over the hub.
//!
//! The reads are deliberately light — every entry with a name, one file's
//! entries, the workspace's source files, a type's `(name, package)` supertype
//! closure, a type's members, a type's constructor overloads, whether a type
//! resolves, and the workspace types reaching a contract — so no declared-type
//! model crosses the hub and `TypeInfo` stays as stored. [`answer`] maps one
//! [`IndexRequest`] onto the index state; the
//! supertype-closure walk that used to live with navigation lives here with the
//! index it walks.

use std::collections::{HashSet, VecDeque};
use std::ops::Bound;
use std::sync::Arc;

use tower_lsp::lsp_types::Url;

use super::query;
use super::types::{FileOrigin, SymbolEntry, WorkspaceIndex};
use crate::messages::{IndexRequest, IndexResponse, MemberSelector};
use crate::model::{SymbolKind, Ty};

/// Answers one coarse read from the index.
pub(crate) fn answer(index: &WorkspaceIndex, request: &IndexRequest) -> IndexResponse {
    match request {
        IndexRequest::EntriesByName { name } => IndexResponse::Entries(
            index
                .symbols
                .by_name
                .get(name.as_str())
                .cloned()
                .unwrap_or_default(),
        ),
        IndexRequest::EntriesByPrefix { prefix } => {
            IndexResponse::Entries(entries_with_prefix(index, prefix))
        }
        IndexRequest::FileSymbols { uri } => IndexResponse::File(
            index
                .symbols
                .by_file
                .get(uri)
                .map(|record| record.entries.clone()),
        ),
        IndexRequest::WorkspaceFiles => IndexResponse::Files(source_files(index)),
        IndexRequest::TypeClosure { ty, package } => {
            IndexResponse::Closure(type_closure(index, ty, package.as_deref()))
        }
        IndexRequest::MembersOf { ty, package } => {
            IndexResponse::Members(query::members_with_overloads(index, ty, package.as_deref()))
        }
        IndexRequest::ConstructorsOf { ty, package } => {
            IndexResponse::Constructors(query::constructors(index, ty, package.as_deref()))
        }
        IndexRequest::ResolvesType { ty, package } => {
            IndexResponse::Resolves(query::lookup(index, ty, package.as_deref()).is_some())
        }
        IndexRequest::ImplementationsOf {
            contract,
            package,
            member,
        } => IndexResponse::Entries(implementations_of(
            index,
            contract,
            package.as_deref(),
            member.as_ref(),
        )),
    }
}

/// Every entry whose simple name starts with `prefix`, in name order. The symbol
/// table is name-ordered, so the prefix is a single range scan rather than a walk
/// of the whole table.
fn entries_with_prefix(index: &WorkspaceIndex, prefix: &str) -> Vec<Arc<SymbolEntry>> {
    index
        .symbols
        .by_name
        .range::<str, _>((Bound::Included(prefix), Bound::Unbounded))
        .take_while(|(name, _)| name.starts_with(prefix))
        .flat_map(|(_, entries)| entries.iter().cloned())
        .collect()
}

/// Every workspace source file the index holds, in index order; extracted library
/// sources and dependency artifacts are not rewritten, so they are not searched.
fn source_files(index: &WorkspaceIndex) -> Vec<Url> {
    index
        .symbols
        .by_file
        .iter()
        .filter(|(_, record)| record.origin == FileOrigin::Workspace)
        .map(|(uri, _)| uri.clone())
        .collect()
}

/// The receiver type and every type in its `supertypes` closure, each as its
/// `(simple name, package)` identity — the same cycle-guarded walk
/// [`query::members_with_overloads`] uses, so a same-named type elsewhere is
/// never touched.
fn type_closure(
    index: &WorkspaceIndex,
    ty: &Ty,
    package: Option<&str>,
) -> Vec<(String, Option<String>)> {
    let Some(root) = query::lookup(index, ty, package) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, Option<String>)> = VecDeque::new();
    queue.push_back((root.name.clone(), root.package.clone()));
    while let Some((name, context)) = queue.pop_front() {
        if !visited.insert(name.clone()) {
            continue;
        }
        let Some(info) = query::find_unique(index, &name, context.as_deref()) else {
            continue;
        };
        out.push((info.name.clone(), info.package.clone()));
        let super_context = info.package.clone().or(context);
        for supertype in &info.supertypes {
            if supertype.qualified_package().is_some() {
                if let Some(resolved) = query::lookup(index, supertype, None) {
                    queue.push_back((resolved.name.clone(), resolved.package.clone()));
                }
            } else if let Some(super_name) = supertype.simple_name() {
                queue.push_back((super_name.to_string(), super_context.clone()));
            }
        }
    }
    out
}

/// The workspace type entries whose supertype closure reaches the contract
/// `(ty, package)`, excluding the contract itself; with a `member` selector, each
/// subtype's own declared override of that member instead of the type entry. Both
/// the type and the member answers are workspace `SymbolEntry`s, so dependency and
/// JDK declarations are never results.
fn implementations_of(
    index: &WorkspaceIndex,
    contract: &Ty,
    package: Option<&str>,
    member: Option<&MemberSelector>,
) -> Vec<Arc<SymbolEntry>> {
    let Some(info) = query::lookup(index, contract, package) else {
        return Vec::new();
    };
    let contract_identity = (info.name.clone(), info.package.clone());
    let mut out = Vec::new();
    for subtype in workspace_types(index) {
        // The contract is not its own implementation.
        if (
            subtype.name.to_string(),
            subtype.package.as_deref().map(str::to_string),
        ) == contract_identity
        {
            continue;
        }
        let closure = type_closure(
            index,
            &Ty::reference(subtype.name.to_string()),
            subtype.package.as_deref(),
        );
        if !closure.contains(&contract_identity) {
            continue;
        }
        match member {
            None => out.push(subtype),
            Some(selector) => {
                if let Some(declared) = declared_override(index, &subtype, selector) {
                    out.push(declared);
                }
            }
        }
    }
    out
}

/// Every type entry the workspace symbol table holds, in a stable order.
fn workspace_types(index: &WorkspaceIndex) -> Vec<Arc<SymbolEntry>> {
    let mut types: Vec<Arc<SymbolEntry>> = index
        .symbols
        .by_file
        .values()
        .flat_map(|record| record.entries.iter())
        .filter(|entry| entry.origin == FileOrigin::Workspace && entry.kind.is_type())
        .cloned()
        .collect();
    types.sort_by(|a, b| compare_entries(a, b));
    types
}

/// The entry `subtype` declares as its override of the member `selector` — its own
/// declaration, so an inherited-only member contributes nothing — matched by name
/// and, when pinned, parameter types.
fn declared_override(
    index: &WorkspaceIndex,
    subtype: &SymbolEntry,
    selector: &MemberSelector,
) -> Option<Arc<SymbolEntry>> {
    let info = query::lookup(
        index,
        &Ty::reference(subtype.name.to_string()),
        subtype.package.as_deref(),
    )?;
    // The ordinal of the selected overload among the type's same-named methods;
    // the symbol table's same-named entries are in source order too, so the same
    // ordinal picks the declaring entry.
    let mut ordinal = 0usize;
    let mut selected = None;
    for member in &info.methods {
        if member.name != selector.name {
            continue;
        }
        let matches = selector.params.as_ref().is_none_or(|params| {
            member
                .params
                .iter()
                .map(|param| param.ty.clone())
                .collect::<Vec<Ty>>()
                == *params
        });
        if matches {
            selected = Some(ordinal);
            break;
        }
        ordinal += 1;
    }
    let ordinal = selected?;
    let mut entries: Vec<Arc<SymbolEntry>> = index
        .symbols
        .by_name
        .get(selector.name.as_str())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|entry| {
            entry.kind == SymbolKind::Method
                && !entry.synthetic
                && entry.container.last().map(String::as_str) == Some(&*subtype.name)
                && entry.package.as_deref() == subtype.package.as_deref()
        })
        .collect();
    entries.sort_by(|a, b| compare_entries(a, b));
    entries.into_iter().nth(ordinal)
}

/// A stable order: by file, then by position.
fn compare_entries(a: &SymbolEntry, b: &SymbolEntry) -> std::cmp::Ordering {
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
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tower_lsp::lsp_types::Url;

    use super::*;
    use crate::hub::Hub;
    use crate::index::types::Ty;
    use crate::index::{scan_sources, set_ready};
    use crate::messages::{
        IndexBatchRequest, IndexBatchResponse, IndexRequest, IndexResponse, Request, Response,
    };

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "java-lsp-index-read-{name}-{}-{}",
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

    /// Builds an index by scanning the given files from disk.
    fn index_of(uris: &[Url]) -> WorkspaceIndex {
        let paths: Vec<PathBuf> = uris.iter().map(|uri| uri.to_file_path().unwrap()).collect();
        let mut index = WorkspaceIndex::default();
        scan_sources(&paths, FileOrigin::Workspace, &mut index);
        set_ready(&mut index);
        index
    }

    #[test]
    fn entries_by_name_returns_every_declaration_with_that_name() {
        let dir = temp_dir("entries");
        let a = write(&dir, "a/Dup.java", "package a;\nclass Dup {}\n");
        let b = write(&dir, "b/Dup.java", "package b;\nclass Dup {}\n");
        let index = index_of(&[a.clone(), b.clone()]);

        let response = answer(
            &index,
            &IndexRequest::EntriesByName {
                name: "Dup".to_string(),
            },
        );
        let IndexResponse::Entries(entries) = response else {
            panic!("expected entries");
        };
        assert_eq!(entries.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn entries_by_prefix_returns_every_name_starting_with_it() {
        let dir = temp_dir("prefix");
        let alpha = write(&dir, "a/Alpha.java", "package a;\nclass Alpha {}\n");
        let alpha_beta = write(&dir, "a/AlphaBeta.java", "package a;\nclass AlphaBeta {}\n");
        let zed = write(&dir, "a/Zed.java", "package a;\nclass Zed {}\n");
        let index = index_of(&[alpha, alpha_beta, zed]);

        let response = answer(
            &index,
            &IndexRequest::EntriesByPrefix {
                prefix: "Alpha".to_string(),
            },
        );
        let IndexResponse::Entries(entries) = response else {
            panic!("expected entries");
        };
        // Name order, so `Alpha` precedes `AlphaBeta`.
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_ref()).collect();
        assert_eq!(names, vec!["Alpha", "AlphaBeta"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_symbols_returns_one_files_entries_or_none() {
        let dir = temp_dir("file");
        let widget = write(&dir, "a/Widget.java", "package a;\nclass Widget {}\n");
        let index = index_of(std::slice::from_ref(&widget));

        let found = answer(
            &index,
            &IndexRequest::FileSymbols {
                uri: widget.clone(),
            },
        );
        let IndexResponse::File(Some(entries)) = found else {
            panic!("expected the file's entries");
        };
        assert!(entries.iter().any(|entry| entry.name.as_ref() == "Widget"));

        let missing = Url::parse("file:///nowhere/Nothing.java").unwrap();
        let none = answer(&index, &IndexRequest::FileSymbols { uri: missing });
        assert!(matches!(none, IndexResponse::File(None)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspace_files_lists_the_scanned_sources() {
        let dir = temp_dir("files");
        let widget = write(&dir, "a/Widget.java", "package a;\nclass Widget {}\n");
        let use_uri = write(&dir, "a/Use.java", "package a;\nclass Use {}\n");
        let index = index_of(&[widget.clone(), use_uri.clone()]);

        let response = answer(&index, &IndexRequest::WorkspaceFiles);
        let IndexResponse::Files(files) = response else {
            panic!("expected files");
        };
        assert!(files.contains(&widget));
        assert!(files.contains(&use_uri));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn type_closure_walks_the_supertype_chain() {
        let dir = temp_dir("closure");
        let base = write(&dir, "p/Base.java", "package p;\nclass Base {}\n");
        let derived = write(
            &dir,
            "p/Derived.java",
            "package p;\nclass Derived extends Base {}\n",
        );
        let index = index_of(&[base, derived]);

        let response = answer(
            &index,
            &IndexRequest::TypeClosure {
                ty: Ty::reference("Derived"),
                package: Some("p".to_string()),
            },
        );
        let IndexResponse::Closure(closure) = response else {
            panic!("expected a closure");
        };
        assert!(closure
            .iter()
            .any(|(name, package)| name == "Derived" && package.as_deref() == Some("p")));
        assert!(closure
            .iter()
            .any(|(name, package)| name == "Base" && package.as_deref() == Some("p")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn members_of_lists_the_types_members_with_overloads() {
        let dir = temp_dir("members");
        let widget = write(
            &dir,
            "p/Widget.java",
            "package p;\nclass Widget {\n    int size;\n    void run() {}\n    void run(int times) {}\n}\n",
        );
        let index = index_of(std::slice::from_ref(&widget));

        let response = answer(
            &index,
            &IndexRequest::MembersOf {
                ty: Ty::reference("Widget"),
                package: Some("p".to_string()),
            },
        );
        let IndexResponse::Members(members) = response else {
            panic!("expected members");
        };
        assert!(members.iter().any(|member| member.name == "size"));
        // Overloads survive the read: both `run` declarations are answered.
        assert_eq!(
            members.iter().filter(|member| member.name == "run").count(),
            2
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn constructors_of_lists_the_types_constructor_overloads() {
        let dir = temp_dir("constructors");
        let point = write(
            &dir,
            "p/Point.java",
            "package p;\nclass Point {\n    Point() {}\n    Point(int x, int y) {}\n}\n",
        );
        let index = index_of(std::slice::from_ref(&point));

        let response = answer(
            &index,
            &IndexRequest::ConstructorsOf {
                ty: Ty::reference("Point"),
                package: Some("p".to_string()),
            },
        );
        let IndexResponse::Constructors(constructors) = response else {
            panic!("expected constructors");
        };
        // Overloads survive the read: both `Point` declarations are answered.
        assert_eq!(constructors.len(), 2);
        assert!(constructors.iter().all(|ctor| ctor.name == "Point"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolves_type_reports_whether_a_name_resolves() {
        let dir = temp_dir("resolves");
        let widget = write(&dir, "p/Widget.java", "package p;\nclass Widget {}\n");
        let index = index_of(std::slice::from_ref(&widget));

        let found = answer(
            &index,
            &IndexRequest::ResolvesType {
                ty: Ty::reference("Widget"),
                package: Some("p".to_string()),
            },
        );
        assert!(matches!(found, IndexResponse::Resolves(true)));

        let gone = answer(
            &index,
            &IndexRequest::ResolvesType {
                ty: Ty::reference("Nope"),
                package: Some("p".to_string()),
            },
        );
        assert!(matches!(gone, IndexResponse::Resolves(false)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_batch_request_answers_each_read_in_order() {
        let dir = temp_dir("batch");
        let widget = write(&dir, "p/Widget.java", "package p;\nclass Widget {}\n");
        let index = index_of(std::slice::from_ref(&widget));

        let hub = Hub::new();
        crate::index::spawn_with(&hub, index);
        let client = hub.register("test");

        let reply = client
            .request(Request::IndexBatch(IndexBatchRequest {
                requests: vec![
                    IndexRequest::EntriesByName {
                        name: "Widget".to_string(),
                    },
                    IndexRequest::WorkspaceFiles,
                    IndexRequest::ResolvesType {
                        ty: Ty::reference("Widget"),
                        package: Some("p".to_string()),
                    },
                ],
            }))
            .await;

        let Response::IndexBatch(IndexBatchResponse(responses)) = reply else {
            panic!("expected a batch response");
        };
        assert_eq!(responses.len(), 3);
        assert!(matches!(&responses[0], IndexResponse::Entries(entries) if !entries.is_empty()));
        assert!(matches!(&responses[1], IndexResponse::Files(files) if !files.is_empty()));
        assert!(matches!(&responses[2], IndexResponse::Resolves(true)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_type_contract_lists_its_transitive_subtypes() {
        let dir = temp_dir("impl-type");
        let base = write(&dir, "p/Base.java", "package p;\nclass Base {}\n");
        let mid = write(
            &dir,
            "p/Mid.java",
            "package p;\nclass Mid extends Base {}\n",
        );
        let leaf = write(
            &dir,
            "p/Leaf.java",
            "package p;\nclass Leaf extends Mid {}\n",
        );
        let index = index_of(&[base, mid, leaf]);

        let response = answer(
            &index,
            &IndexRequest::ImplementationsOf {
                contract: Ty::reference("Base"),
                package: Some("p".to_string()),
                member: None,
            },
        );
        let IndexResponse::Entries(entries) = response else {
            panic!("expected entries");
        };
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_ref()).collect();
        assert!(names.contains(&"Mid"), "{names:?}");
        assert!(names.contains(&"Leaf"), "{names:?}");
        assert!(
            !names.contains(&"Base"),
            "the contract is not its own subtype"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_contract_with_no_subtype_lists_nothing() {
        let dir = temp_dir("impl-none");
        let lonely = write(&dir, "p/Lonely.java", "package p;\nclass Lonely {}\n");
        let index = index_of(&[lonely]);

        let response = answer(
            &index,
            &IndexRequest::ImplementationsOf {
                contract: Ty::reference("Lonely"),
                package: Some("p".to_string()),
                member: None,
            },
        );
        assert!(matches!(response, IndexResponse::Entries(entries) if entries.is_empty()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_member_contract_lists_only_declaring_subtypes() {
        let dir = temp_dir("impl-member");
        let contract = write(
            &dir,
            "p/Contract.java",
            "package p;\ninterface Contract {\n    void run();\n}\n",
        );
        let impl_uri = write(
            &dir,
            "p/Impl.java",
            "package p;\nclass Impl implements Contract {\n    public void run() {}\n}\n",
        );
        // `Inherit` only inherits `run`, so it contributes nothing.
        let inherit = write(
            &dir,
            "p/Inherit.java",
            "package p;\nclass Inherit implements Contract {}\n",
        );
        let index = index_of(&[contract, impl_uri.clone(), inherit]);

        let response = answer(
            &index,
            &IndexRequest::ImplementationsOf {
                contract: Ty::reference("Contract"),
                package: Some("p".to_string()),
                member: Some(MemberSelector {
                    name: "run".to_string(),
                    params: None,
                }),
            },
        );
        let IndexResponse::Entries(entries) = response else {
            panic!("expected entries");
        };
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].name.as_ref(), "run");
        assert_eq!(entries[0].uri.as_ref(), &impl_uri);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pinned_member_contract_matches_the_parameter_types() {
        let dir = temp_dir("impl-pinned");
        let contract = write(
            &dir,
            "p/Contract.java",
            "package p;\ninterface Contract {\n    void run(int x);\n    void run(String s);\n}\n",
        );
        let impl_uri = write(
            &dir,
            "p/Impl.java",
            "package p;\nclass Impl implements Contract {\n    public void run(int x) {}\n    public void run(String s) {}\n}\n",
        );
        let index = index_of(&[contract, impl_uri]);

        let response = answer(
            &index,
            &IndexRequest::ImplementationsOf {
                contract: Ty::reference("Contract"),
                package: Some("p".to_string()),
                member: Some(MemberSelector {
                    name: "run".to_string(),
                    params: Some(vec![Ty::Prim(crate::index::types::Prim::Int)]),
                }),
            },
        );
        let IndexResponse::Entries(entries) = response else {
            panic!("expected entries");
        };
        assert_eq!(entries.len(), 1, "{entries:?}");
        // The `int` overload is on line 2, the `String` one on line 3.
        assert_eq!(entries[0].selection_range.start.line, 2, "{entries:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_shared_simple_name_in_another_package_is_not_a_subtype() {
        let dir = temp_dir("impl-shared");
        let base = write(&dir, "p/Base.java", "package p;\nclass Base {}\n");
        let other = write(&dir, "q/Base.java", "package q;\nclass Base {}\n");
        let derived = write(
            &dir,
            "p/Derived.java",
            "package p;\nclass Derived extends Base {}\n",
        );
        let index = index_of(&[base, other, derived]);

        let response = answer(
            &index,
            &IndexRequest::ImplementationsOf {
                contract: Ty::reference("Base"),
                package: Some("p".to_string()),
                member: None,
            },
        );
        let IndexResponse::Entries(entries) = response else {
            panic!("expected entries");
        };
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_ref()).collect();
        assert_eq!(names, vec!["Derived"], "{entries:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
