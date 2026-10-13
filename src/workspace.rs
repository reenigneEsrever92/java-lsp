//! Workspace symbol search: the workspace's declarations whose simple name
//! matches a query, for the editor's `workspace/symbol`.
//!
//! Search is its own hub module, labelled `workspace`. It holds no state: it
//! reads the index through the hub, over the coarse
//! [`crate::messages::IndexRequest::EntriesByPrefix`] read, and shapes each entry
//! as a [`SymbolInformation`].
//!
//! A search returns the workspace's `Workspace` and `LibrarySource` declarations
//! whose simple name starts with the query — an empty query returns everything —
//! each located at its declared name and named for its innermost enclosing type.
//! A constructor is not an addressable symbol, a Lombok-synthesised member has no
//! declaration of its own, and an import names a path rather than a declaration,
//! so all three are dropped; a `Dependency` class-file entry is dropped too,
//! because a jar's bytecode has no declaration to open.

use std::sync::Arc;

use tower_lsp::lsp_types::{Location, SymbolInformation, SymbolKind as LspSymbolKind};

use crate::hub::{Hub, HubClient};
use crate::index::{FileOrigin, SymbolEntry};
use crate::messages::{
    IndexRequest, IndexResponse, Message, Request, Response, SymbolRequest, SymbolResponse,
};
use crate::model::SymbolKind;

// ---------------------------------------------------------------------------
// The hub client
// ---------------------------------------------------------------------------

/// Starts workspace symbol search on the hub as the client labelled `workspace`,
/// serving the shell's [`SymbolRequest`]s and answering each with a
/// [`SymbolResponse`].
pub fn spawn(hub: &Hub) {
    let hub_client = hub.register("workspace");

    tokio::spawn(async move {
        let mut notifications = hub_client.subscribe().await;
        let mut requests = hub_client.serve().await;

        loop {
            tokio::select! {
                // Search needs no notification, but taking the subscription and
                // draining it keeps the hub's broadcast from backing up on this
                // client.
                Some(_) = notifications.recv() => {}
                Some(envelope) = requests.recv() => {
                    let Message::Request(Request::Symbols(request)) = &*envelope.message else {
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

/// Answers one workspace symbol request.
async fn serve(client: &HubClient, request: SymbolRequest) -> Response {
    match request {
        SymbolRequest::Search { query } => {
            Response::Symbols(SymbolResponse::Symbols(search(client, &query).await))
        }
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

/// The workspace symbols whose simple name starts with `query`, in name order.
async fn search(client: &HubClient, query: &str) -> Vec<SymbolInformation> {
    entries_with_prefix(client, query)
        .await
        .into_iter()
        .filter_map(|entry| symbol_information(&entry))
        .collect()
}

// ---------------------------------------------------------------------------
// Shaping
// ---------------------------------------------------------------------------

/// One entry as a [`SymbolInformation`], or `None` when it names no addressable
/// symbol: an import (no LSP kind), a constructor, a Lombok-synthetic member, or a
/// `Dependency` class-file entry (nothing to open).
fn symbol_information(entry: &SymbolEntry) -> Option<SymbolInformation> {
    if entry.origin == FileOrigin::Dependency || entry.synthetic || entry.is_constructor() {
        return None;
    }
    let kind = lsp_kind(entry.kind)?;
    Some(SymbolInformation {
        name: entry.name.to_string(),
        kind,
        tags: None,
        #[allow(deprecated)] // the field is still required in the struct initializer
        deprecated: None,
        location: Location {
            uri: (*entry.uri).clone(),
            range: entry.selection_range,
        },
        container_name: entry.container.last().cloned(),
    })
}

/// `SymbolKind` → LSP `SymbolKind`; `Import` entries name a path, not a
/// declaration, and contribute nothing.
fn lsp_kind(kind: SymbolKind) -> Option<LspSymbolKind> {
    match kind {
        SymbolKind::Class => Some(LspSymbolKind::CLASS),
        SymbolKind::Interface => Some(LspSymbolKind::INTERFACE),
        SymbolKind::Enum => Some(LspSymbolKind::ENUM),
        SymbolKind::Record => Some(LspSymbolKind::STRUCT),
        SymbolKind::Method => Some(LspSymbolKind::METHOD),
        SymbolKind::Field => Some(LspSymbolKind::FIELD),
        SymbolKind::EnumConstant => Some(LspSymbolKind::ENUM_MEMBER),
        SymbolKind::Import => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::hub::Hub;
    use crate::index::{scan_sources, set_ready, WorkspaceIndex};
    use tower_lsp::lsp_types::{Position, Range, Url};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "java-lsp-workspace-{name}-{}-{}",
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

    fn paths(uris: &[Url]) -> Vec<PathBuf> {
        uris.iter().map(|uri| uri.to_file_path().unwrap()).collect()
    }

    /// An index built by scanning `uris` as workspace sources.
    fn index_of(uris: &[Url]) -> WorkspaceIndex {
        let mut index = WorkspaceIndex::default();
        let count = scan_sources(&paths(uris), FileOrigin::Workspace, &mut index);
        assert_eq!(count, uris.len(), "every file indexed");
        set_ready(&mut index);
        index
    }

    /// An entry of `kind`/`origin`, for exercising the shaping directly — the
    /// index only stamps a batch's origin on the file record, so a scanned file's
    /// entries all read as `Workspace` in the live build.
    fn entry_named(name: &str, kind: SymbolKind, origin: FileOrigin) -> SymbolEntry {
        let zero = Range::new(Position::new(0, 0), Position::new(0, 0));
        SymbolEntry {
            uri: Arc::new(Url::parse("file:///Widget.java").unwrap()),
            name: Arc::from(name),
            kind,
            package: None,
            container: Arc::from(Vec::<String>::new()),
            full_range: zero,
            selection_range: zero,
            origin,
            synthetic: false,
        }
    }

    /// A fresh hub with `index` preloaded and search spawned, and a client
    /// registered to drive [`Request::Symbols`] through.
    async fn hub_with(index: WorkspaceIndex) -> HubClient {
        let hub = Hub::new();
        crate::index::spawn_with(&hub, index);
        spawn(&hub);
        hub.register("test")
    }

    /// A hub over `uris` scanned as workspace sources.
    async fn workspace_hub(uris: &[Url]) -> HubClient {
        hub_with(index_of(uris)).await
    }

    /// Sends a `Search` and reads its symbols.
    async fn search_all(client: &HubClient, query: &str) -> Vec<SymbolInformation> {
        let reply = client
            .request(Request::Symbols(SymbolRequest::Search {
                query: query.to_string(),
            }))
            .await;
        match reply {
            Response::Symbols(SymbolResponse::Symbols(symbols)) => symbols,
            _ => Vec::new(),
        }
    }

    fn names(symbols: &[SymbolInformation]) -> Vec<String> {
        symbols.iter().map(|symbol| symbol.name.clone()).collect()
    }

    #[tokio::test]
    async fn search_matches_by_prefix_and_returns_everything_for_an_empty_query() {
        let dir = temp_dir("prefix");
        let a = write(&dir, "a/Alpha.java", "package a;\nclass Alpha {}\n");
        let b = write(&dir, "b/Beta.java", "package b;\nclass Beta {}\n");
        let client = workspace_hub(&[a, b]).await;

        let prefixed = search_all(&client, "Al").await;
        assert_eq!(names(&prefixed), vec!["Alpha"]);

        let missing = search_all(&client, "Zzz").await;
        assert!(missing.is_empty(), "{:?}", names(&missing));

        // An empty query returns everything, in the index's name order.
        let all = names(&search_all(&client, "").await);
        assert!(all.contains(&"Alpha".to_string()), "{all:?}");
        assert!(all.contains(&"Beta".to_string()), "{all:?}");
        let mut sorted = all.clone();
        sorted.sort();
        assert_eq!(all, sorted, "results are in name order");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_symbol_carries_its_selection_range_and_innermost_container() {
        let dir = temp_dir("shape");
        let uri = write(
            &dir,
            "a/Widget.java",
            "package a;\nclass Widget {\n    void getSize() {}\n}\n",
        );
        let client = workspace_hub(std::slice::from_ref(&uri)).await;

        let symbols = search_all(&client, "getSize").await;
        assert_eq!(symbols.len(), 1, "{:?}", names(&symbols));
        let symbol = &symbols[0];
        assert_eq!(symbol.kind, LspSymbolKind::METHOD);
        assert_eq!(symbol.location.uri, uri);
        // The declared name, not the whole declaration: `getSize` on line 3.
        assert_eq!(symbol.location.range.start.line, 2);
        assert_eq!(symbol.container_name.as_deref(), Some("Widget"));

        // A top-level type has no container.
        let widget = search_all(&client, "Widget").await;
        assert_eq!(widget.len(), 1, "{:?}", names(&widget));
        assert_eq!(widget[0].kind, LspSymbolKind::CLASS);
        assert_eq!(widget[0].container_name, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn every_declaration_kind_maps_to_its_lsp_kind() {
        let dir = temp_dir("kinds");
        let uri = write(
            &dir,
            "a/Mixed.java",
            "package a;\n\
             enum Color { RED }\n\
             class Widget {\n    int size;\n}\n\
             interface Greeter {}\n\
             record Point(int x) {}\n",
        );
        let client = workspace_hub(std::slice::from_ref(&uri)).await;

        let by_name: std::collections::HashMap<String, LspSymbolKind> = search_all(&client, "")
            .await
            .into_iter()
            .map(|symbol| (symbol.name, symbol.kind))
            .collect();

        assert_eq!(by_name.get("Color"), Some(&LspSymbolKind::ENUM));
        assert_eq!(by_name.get("RED"), Some(&LspSymbolKind::ENUM_MEMBER));
        assert_eq!(by_name.get("Widget"), Some(&LspSymbolKind::CLASS));
        assert_eq!(by_name.get("size"), Some(&LspSymbolKind::FIELD));
        assert_eq!(by_name.get("Greeter"), Some(&LspSymbolKind::INTERFACE));
        assert_eq!(by_name.get("Point"), Some(&LspSymbolKind::STRUCT));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn imports_are_not_offered() {
        let dir = temp_dir("imports");
        let uri = write(
            &dir,
            "a/Main.java",
            "package a;\nimport java.util.List;\nclass Main {}\n",
        );
        let client = workspace_hub(std::slice::from_ref(&uri)).await;

        // The import entry's name is its dotted path; the search matches it but
        // drops it for naming no declaration.
        let symbols = search_all(&client, "java.util").await;
        assert!(symbols.is_empty(), "{:?}", names(&symbols));
        // The class itself is still found.
        assert_eq!(names(&search_all(&client, "Main").await), vec!["Main"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn constructors_are_not_offered() {
        let dir = temp_dir("constructors");
        let uri = write(
            &dir,
            "a/Point.java",
            "package a;\nclass Point {\n    Point(int x) {}\n}\n",
        );
        let client = workspace_hub(std::slice::from_ref(&uri)).await;

        // The class and its same-named constructor both match `Point`; only the
        // class is an addressable symbol.
        let symbols = search_all(&client, "Point").await;
        assert_eq!(symbols.len(), 1, "{:?}", names(&symbols));
        assert_eq!(symbols[0].kind, LspSymbolKind::CLASS);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn lombok_synthetic_members_are_not_offered() {
        let dir = temp_dir("lombok");
        let uri = write(
            &dir,
            "a/Widget.java",
            "package a;\nimport lombok.Getter;\n@Getter\nclass Widget {\n    private int size;\n}\n",
        );
        let client = workspace_hub(std::slice::from_ref(&uri)).await;

        // `@Getter` synthesises `getSize`, which has no declaration of its own.
        let synthetic = search_all(&client, "getSize").await;
        assert!(synthetic.is_empty(), "{:?}", names(&synthetic));
        // The field it derives from is real and stays offered.
        assert_eq!(names(&search_all(&client, "size").await), vec!["size"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dependency_entry_is_dropped_while_workspace_and_library_ones_are_kept() {
        // A `Dependency` class-file entry has no declaration to open; a workspace
        // file or an extracted library source is addressable.
        let dependency = entry_named("Widget", SymbolKind::Class, FileOrigin::Dependency);
        assert!(symbol_information(&dependency).is_none());

        for origin in [FileOrigin::Workspace, FileOrigin::LibrarySource] {
            let kept = entry_named("Widget", SymbolKind::Class, origin);
            assert!(symbol_information(&kept).is_some(), "{origin:?} kept");
        }
    }
}
