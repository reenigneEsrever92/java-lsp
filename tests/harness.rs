//! Drives the server over JSON-RPC with `LspService` — no editor, no stdio.

use java_lsp::engine::EngineHandle;
use java_lsp::index::IndexKind;
use java_lsp::server::JavaLanguageServer;
use serde_json::{json, Value};
use tower::{Service, ServiceExt};
use tower_lsp::jsonrpc::{Id, Request};
use tower_lsp::lsp_types::{TextDocumentSyncKind, Url};
use tower_lsp::LspService;

const HELLO_URI: &str = "file:///src/Hello.java";
const SAMPLE: &str = "\
public class Sample {
    private int count = 1;

    public int getCount() {
        return count;
    }
}
";

const COMPLETION_SAMPLE: &str = "\
public class Sample {
    private int count = 1;

    public int add(int amount) {
        int named = count + amount;
        return na;
    }
}
";

/// Cursor at the end of the partial word `na` inside `add`'s body.
const COMPLETION_POSITION: (u32, u32) = (5, 17);

fn service() -> LspService<JavaLanguageServer> {
    let (service, _socket) = LspService::new(JavaLanguageServer::new);
    service
}

async fn respond(service: &mut LspService<JavaLanguageServer>, request: Request) -> Option<Value> {
    let response = service.ready().await.unwrap().call(request).await.unwrap();
    response.and_then(|r| r.result().cloned())
}

async fn initialize(service: &mut LspService<JavaLanguageServer>) -> Value {
    respond(
        service,
        Request::build("initialize")
            .id(Id::Number(1))
            .params(json!({ "capabilities": {} }))
            .finish(),
    )
    .await
    .expect("initialize must respond")
}

/// Holds the JDK environment lock and restores `JAVA_LSP_JDK` on drop, so
/// scan-driven tests neither race each other nor leak their JDK setting. Also
/// forces `JAVA_LSP_OFFLINE`, so no test ever fetches dependency sources.
struct JdkEnv {
    _lock: std::sync::MutexGuard<'static, ()>,
    previous: Option<std::ffi::OsString>,
    previous_offline: Option<std::ffi::OsString>,
}

impl JdkEnv {
    /// Pins the JDK to a non-existent path, opting out of machine-JDK indexing.
    fn none() -> JdkEnv {
        JdkEnv::set("/definitely/not/a/jdk")
    }

    fn set(value: &str) -> JdkEnv {
        let lock = java_lsp::jdk::env_lock();
        let previous = std::env::var_os("JAVA_LSP_JDK");
        std::env::set_var("JAVA_LSP_JDK", value);
        let previous_offline = std::env::var_os("JAVA_LSP_OFFLINE");
        std::env::set_var("JAVA_LSP_OFFLINE", "1");
        JdkEnv {
            _lock: lock,
            previous,
            previous_offline,
        }
    }
}

impl Drop for JdkEnv {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var("JAVA_LSP_JDK", value),
            None => std::env::remove_var("JAVA_LSP_JDK"),
        }
        match &self.previous_offline {
            Some(value) => std::env::set_var("JAVA_LSP_OFFLINE", value),
            None => std::env::remove_var("JAVA_LSP_OFFLINE"),
        }
    }
}

/// Sets an environment variable and restores its previous value on drop.
struct EnvVar {
    name: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVar {
    fn set(name: &'static str, value: impl AsRef<std::ffi::OsStr>) -> EnvVar {
        let previous = std::env::var_os(name);
        std::env::set_var(name, value);
        EnvVar { name, previous }
    }
}

impl Drop for EnvVar {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(self.name, value),
            None => std::env::remove_var(self.name),
        }
    }
}

#[tokio::test]
async fn initialize_advertises_incremental_sync_and_language_capabilities() {
    let mut service = service();
    let result = initialize(&mut service).await;

    let capabilities = &result["capabilities"];
    assert_eq!(
        capabilities["textDocumentSync"],
        json!(TextDocumentSyncKind::INCREMENTAL)
    );
    assert_eq!(capabilities["hoverProvider"], true);
    assert_eq!(capabilities["definitionProvider"], true);
    assert!(capabilities["completionProvider"]["triggerCharacters"]
        .as_array()
        .unwrap()
        .contains(&json!(".")));
    assert!(capabilities["signatureHelpProvider"]["triggerCharacters"]
        .as_array()
        .unwrap()
        .contains(&json!("(")));
    assert_eq!(capabilities["documentSymbolProvider"], true);
    assert_eq!(capabilities["workspaceSymbolProvider"], true);
    assert_eq!(capabilities["referencesProvider"], true);
    assert_eq!(capabilities["renameProvider"], true);
    assert_eq!(
        capabilities["codeActionProvider"]["codeActionKinds"][0],
        json!("quickfix")
    );
    assert_eq!(capabilities["inlayHintProvider"], true);
    assert_eq!(capabilities["foldingRangeProvider"], true);
    assert!(
        capabilities["semanticTokensProvider"]["legend"]["tokenTypes"]
            .as_array()
            .unwrap()
            .len()
            >= 10
    );
}

#[tokio::test]
async fn did_open_and_incremental_did_change_update_the_store() {
    let mut service = service();
    initialize(&mut service).await;

    respond(
        &mut service,
        Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": HELLO_URI,
                    "languageId": "java",
                    "version": 1,
                    "text": "class Hello {\n}\n",
                }
            }))
            .finish(),
    )
    .await;

    respond(
        &mut service,
        Request::build("textDocument/didChange")
            .params(json!({
                "textDocument": { "uri": HELLO_URI, "version": 2 },
                "contentChanges": [{
                    "range": {
                        "start": { "line": 0, "character": 6 },
                        "end": { "line": 0, "character": 11 },
                    },
                    "text": "World",
                }],
            }))
            .finish(),
    )
    .await;

    let server = service.inner();
    let docs = server.documents();
    let doc = docs.read().await;
    let document = doc
        .get(&HELLO_URI.parse().unwrap())
        .expect("document must be open");
    assert_eq!(document.version, 2);
    assert_eq!(document.bytes, b"class World {\n}\n");
}

#[tokio::test]
async fn queries_on_an_unopened_document_answer_empty() {
    let mut service = service();
    initialize(&mut service).await;

    let hover = respond(
        &mut service,
        Request::build("textDocument/hover")
            .id(Id::Number(2))
            .params(json!({
                "textDocument": { "uri": HELLO_URI },
                "position": { "line": 0, "character": 2 },
            }))
            .finish(),
    )
    .await
    .expect("hover must respond");
    assert!(
        hover.is_null(),
        "hover on an unopened document must be null, got {hover}"
    );

    let completions = respond(
        &mut service,
        Request::build("textDocument/completion")
            .id(Id::Number(3))
            .params(json!({
                "textDocument": { "uri": HELLO_URI },
                "position": { "line": 0, "character": 2 },
            }))
            .finish(),
    )
    .await
    .expect("completion must respond");
    assert!(
        completions.is_null(),
        "completions on an unopened document must be null"
    );
}

#[tokio::test]
async fn syntax_features_answer_for_an_opened_document() {
    let mut service = service();
    initialize(&mut service).await;

    respond(
        &mut service,
        Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": HELLO_URI,
                    "languageId": "java",
                    "version": 1,
                    "text": SAMPLE,
                }
            }))
            .finish(),
    )
    .await;

    let symbols = respond(
        &mut service,
        Request::build("textDocument/documentSymbol")
            .id(Id::Number(2))
            .params(json!({ "textDocument": { "uri": HELLO_URI } }))
            .finish(),
    )
    .await
    .expect("documentSymbol must respond");
    assert_eq!(symbols[0]["name"], "Sample");
    assert_eq!(symbols[0]["kind"], 5); // SymbolKind::CLASS
    let children = symbols[0]["children"].as_array().unwrap();
    assert!(children.iter().any(|s| s["name"] == "getCount"));

    let folds = respond(
        &mut service,
        Request::build("textDocument/foldingRange")
            .id(Id::Number(3))
            .params(json!({ "textDocument": { "uri": HELLO_URI } }))
            .finish(),
    )
    .await
    .expect("foldingRange must respond");
    assert!(folds.as_array().unwrap().len() >= 2, "{folds}");

    let tokens = respond(
        &mut service,
        Request::build("textDocument/semanticTokens/full")
            .id(Id::Number(4))
            .params(json!({ "textDocument": { "uri": HELLO_URI } }))
            .finish(),
    )
    .await
    .expect("semanticTokens must respond");
    let data = tokens["data"].as_array().unwrap();
    assert_eq!(data.len() % 5, 0);
    assert!(!data.is_empty());
}

#[tokio::test]
async fn published_diagnostics_reflect_parse_errors_and_clear_when_fixed() {
    let (mut service, mut socket) = LspService::new(JavaLanguageServer::new);
    initialize(&mut service).await;

    respond(
        &mut service,
        Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": HELLO_URI,
                    "languageId": "java",
                    "version": 1,
                    "text": "class Broken {\n    int x\n}\n",
                }
            }))
            .finish(),
    )
    .await;

    let diagnostics = next_diagnostics_for(&mut socket, HELLO_URI).await;
    assert!(
        diagnostics.as_array().map_or(false, |d| !d.is_empty()),
        "missing semicolon must be reported, got {diagnostics}"
    );

    respond(
        &mut service,
        Request::build("textDocument/didChange")
            .params(json!({
                "textDocument": { "uri": HELLO_URI, "version": 2 },
                "contentChanges": [{
                    "text": "class Broken {\n    int x;\n}\n",
                }],
            }))
            .finish(),
    )
    .await;

    let diagnostics = next_diagnostics_for(&mut socket, HELLO_URI).await;
    assert!(
        diagnostics.as_array().map_or(false, |d| d.is_empty()),
        "fixed file must clear, got {diagnostics}"
    );
}

/// Reads server-to-client messages until the next publishDiagnostics for
/// `uri` arrives and returns its diagnostics array.
async fn next_diagnostics_for(socket: &mut tower_lsp::ClientSocket, uri: &str) -> Value {
    use futures::StreamExt;
    for _ in 0..16 {
        if let Some(request) = socket.next().await {
            if request.method() != "textDocument/publishDiagnostics" {
                continue;
            }
            if let Some(params) = request.params() {
                if params["uri"].as_str() == Some(uri) {
                    return params["diagnostics"].clone();
                }
            }
        }
    }
    panic!("no publishDiagnostics notification arrived for {uri}");
}

/// Reads server-to-client messages until a request with `method` arrives and
/// returns its params.
async fn next_request_with_method(socket: &mut tower_lsp::ClientSocket, method: &str) -> Value {
    use futures::StreamExt;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "no {method} request arrived"
        );
        let next = tokio::time::timeout(std::time::Duration::from_millis(500), socket.next()).await;
        let Ok(Some(request)) = next else { continue };
        if request.method() == method {
            return request.params().cloned().unwrap_or(Value::Null);
        }
    }
}

#[tokio::test]
async fn workspace_index_scans_in_background_and_updates_incrementally() {
    // Serializes env-var mutation across scan-driven tests.
    let _env = java_lsp::jdk::env_lock();
    std::env::set_var("JAVA_LSP_JDK", "/definitely/not/a/jdk");
    let _offline = EnvVar::set("JAVA_LSP_OFFLINE", "1");
    // Fixture project on disk, so the scan has something to find.
    let root = std::env::temp_dir().join(format!(
        "java-lsp-harness-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let greet_path = root.join("Greet.java");
    let util_path = root.join("Util.java");
    std::fs::write(
        &greet_path,
        "package demo;\n\npublic class Greet {\n    private String name;\n    public String getName() { return name; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        &util_path,
        "package demo;\n\npublic interface Util {\n    int answer();\n}\n",
    )
    .unwrap();

    let (mut service, mut socket) = LspService::new(JavaLanguageServer::new);
    // Diagnostics publications go through a capacity-1 channel; drain it so
    // handlers never block on an unread client socket.
    tokio::spawn(async move {
        use futures::StreamExt;
        while socket.next().await.is_some() {}
    });
    let root_uri = Url::from_file_path(&root).unwrap();
    respond(
        &mut service,
        Request::build("initialize")
            .id(Id::Number(1))
            .params(json!({ "capabilities": {}, "rootUri": root_uri.as_str() }))
            .finish(),
    )
    .await
    .expect("initialize must respond");
    respond(
        &mut service,
        Request::build("initialized").params(json!({})).finish(),
    )
    .await;

    // While warm-up may still be running, syntax features must respond (R6):
    // open an in-memory document and query its symbols immediately.
    respond(
        &mut service,
        Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": HELLO_URI,
                    "languageId": "java",
                    "version": 1,
                    "text": SAMPLE,
                }
            }))
            .finish(),
    )
    .await;
    let symbols = respond(
        &mut service,
        Request::build("textDocument/documentSymbol")
            .id(Id::Number(2))
            .params(json!({ "textDocument": { "uri": HELLO_URI } }))
            .finish(),
    )
    .await
    .expect("documentSymbol must respond during warm-up");
    assert_eq!(symbols[0]["name"], "Sample");

    // The scan finishes and indexes the fixture's declarations.
    let engine = service.inner().engine();
    wait_for_index(&engine, |entries, ready| {
        ready
            && entries
                .iter()
                .any(|e| e.name == "Greet" && e.kind == IndexKind::Class)
    })
    .await;
    let entries = engine.indexed_symbols().await;
    assert!(entries.iter().any(|e| e.name == "getName"
        && e.kind == IndexKind::Method
        && e.container == vec!["Greet".to_string()]));
    assert!(entries
        .iter()
        .any(|e| e.name == "name" && e.kind == IndexKind::Field));
    assert!(entries
        .iter()
        .any(|e| e.name == "Util" && e.kind == IndexKind::Interface));
    assert!(entries
        .iter()
        .any(|e| e.name == "answer" && e.kind == IndexKind::Method));

    // An edit updates only the edited file's entries, not by re-scanning.
    respond(
        &mut service,
        Request::build("textDocument/didChange")
            .params(json!({
                "textDocument": { "uri": HELLO_URI, "version": 2 },
                "contentChanges": [{ "text": "class Renamed {\n}\n" }],
            }))
            .finish(),
    )
    .await;
    wait_for_index(&engine, |entries, _| {
        entries
            .iter()
            .any(|e| e.name == "Renamed" && e.kind == IndexKind::Class)
    })
    .await;
    let entries = engine.indexed_symbols().await;
    assert!(!entries.iter().any(|e| e.name == "Sample"), "{entries:?}");

    // Closing a file outside the workspace root drops its entries.
    respond(
        &mut service,
        Request::build("textDocument/didClose")
            .params(json!({ "textDocument": { "uri": HELLO_URI } }))
            .finish(),
    )
    .await;
    wait_for_index(&engine, |entries, _| {
        !entries.iter().any(|e| e.name == "Renamed")
    })
    .await;

    // Closing a file under the workspace root re-reads it from disk: the
    // in-memory rename below is replaced by the on-disk `Greet`.
    let greet_uri = Url::from_file_path(&greet_path).unwrap();
    respond(
        &mut service,
        Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": greet_uri.as_str(),
                    "languageId": "java",
                    "version": 1,
                    "text": "class GreetModified {\n}\n",
                }
            }))
            .finish(),
    )
    .await;
    wait_for_index(&engine, |entries, _| {
        entries.iter().any(|e| e.name == "GreetModified")
    })
    .await;
    respond(
        &mut service,
        Request::build("textDocument/didClose")
            .params(json!({ "textDocument": { "uri": greet_uri.as_str() } }))
            .finish(),
    )
    .await;
    wait_for_index(&engine, |entries, _| {
        !entries.iter().any(|e| e.name == "GreetModified")
            && entries
                .iter()
                .any(|e| e.name == "Greet" && e.kind == IndexKind::Class)
    })
    .await;

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn maven_project_roots_and_dependency_jars_feed_the_index() {
    // Serializes env-var mutation across scan-driven tests.
    let _env = java_lsp::jdk::env_lock();
    // A fake JDK here would slow the scan; opt out — JDK indexing is covered
    // by its own test.
    std::env::set_var("JAVA_LSP_JDK", "/definitely/not/a/jdk");
    // Never fetch dependency sources: the fixture's repository is synthetic.
    let _offline = EnvVar::set("JAVA_LSP_OFFLINE", "1");
    // Fixture: a two-level Maven project plus a fake local repository
    // containing one dependency whose jar we build by hand.
    let root = temp_dir("maven-fixture");
    let repo = temp_dir("maven-repo");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    let write_file = |path: std::path::PathBuf, content: &str| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    };
    // The scan reads MAVEN_REPO at warm-up; no other test consumes it.
    let _maven = EnvVar::set("MAVEN_REPO", &repo);

    write_file(
        root.join("pom.xml"),
        "<project><groupId>demo</groupId><artifactId>root</artifactId><version>1.0</version><packaging>pom</packaging><modules><module>app</module></modules></project>",
    );
    write_file(
        root.join("app").join("pom.xml"),
        "<project><artifactId>app</artifactId><parent><groupId>demo</groupId><artifactId>root</artifactId><version>1.0</version></parent><dependencies><dependency><groupId>com.example</groupId><artifactId>lib</artifactId><version>1.0</version></dependency></dependencies></project>",
    );
    std::fs::create_dir_all(
        root.join("app")
            .join("src")
            .join("main")
            .join("java")
            .join("com")
            .join("example"),
    )
    .unwrap();
    std::fs::write(
        root.join("app").join("src").join("main").join("java").join("com").join("example").join("App.java"),
        "\
package com.example;\n\npublic class App {\n    private Lib lib;\n\n    public String name() {\n        String s = getNa\n        Other o;\n        return s;\n    }\n}\n",
    )
    .unwrap();
    // A second package in the same module: cross-package workspace symbol.
    std::fs::create_dir_all(
        root.join("app")
            .join("src")
            .join("main")
            .join("java")
            .join("com")
            .join("other"),
    )
    .unwrap();
    std::fs::write(
        root.join("app")
            .join("src")
            .join("main")
            .join("java")
            .join("com")
            .join("other")
            .join("Other.java"),
        "package com.other;\n\npublic class Other {\n}\n",
    )
    .unwrap();
    // Must NOT be indexed: target/ is a build output; the root pom file sits
    // outside any source root.
    write_file(
        root.join("app").join("target").join("Junk.java"),
        "class Junk {}\n",
    );
    write_file(
        root.join("StaysUnindexed.java"),
        "class StaysUnindexed {}\n",
    );

    // The dependency: pom + hand-built jar with one class (Lib.getName).
    let lib_dir = repo.join("com").join("example").join("lib").join("1.0");
    std::fs::create_dir_all(&lib_dir).unwrap();
    write_file(
        lib_dir.join("lib-1.0.pom"),
        "<project><groupId>com.example</groupId><artifactId>lib</artifactId><version>1.0</version></project>",
    );
    let lib_class = test_class_bytes(
        "com/example/lib/Lib",
        0x0021,
        Some("java/lang/Object"),
        &[],
        &["getName"],
    );
    std::fs::write(
        lib_dir.join("lib-1.0.jar"),
        test_stored_zip(&[("com/example/lib/Lib.class", &lib_class)]),
    )
    .unwrap();

    let (mut service, mut socket) = LspService::new(JavaLanguageServer::new);
    tokio::spawn(async move {
        use futures::StreamExt;
        while socket.next().await.is_some() {}
    });
    let root_uri = Url::from_file_path(&root).unwrap();
    respond(
        &mut service,
        Request::build("initialize")
            .id(Id::Number(1))
            .params(json!({ "capabilities": {}, "rootUri": root_uri.as_str() }))
            .finish(),
    )
    .await
    .expect("initialize must respond");
    respond(
        &mut service,
        Request::build("initialized").params(json!({})).finish(),
    )
    .await;

    let engine = service.inner().engine();
    wait_for_index(&engine, |entries, ready| {
        ready
            && entries.iter().any(|e| e.name == "App" && e.kind == IndexKind::Class)
            && entries
                .iter()
                .any(|e| e.name == "Lib" && e.kind == IndexKind::Class && e.dependency)
            && entries.iter().any(|e| e.name == "getName" && e.dependency)
            // Source-root scoping: build outputs and non-root files excluded.
            && !entries.iter().any(|e| e.name == "Junk")
            && !entries.iter().any(|e| e.name == "StaysUnindexed")
    })
    .await;

    // A dependency type is offered in completions (container label, simple
    // insert text) for a prefix inside a method body.
    let app_text = std::fs::read_to_string(app_uri(&root).to_file_path().unwrap()).unwrap();
    respond(
        &mut service,
        Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": app_uri(&root),
                    "languageId": "java",
                    "version": 1,
                    "text": app_text,
                }
            }))
            .finish(),
    )
    .await;
    let completions = respond(
        &mut service,
        Request::build("textDocument/completion")
            .id(Id::Number(2))
            .params(json!({
                "textDocument": { "uri": app_uri(&root) },
                "position": { "line": 6, "character": 24 },
            }))
            .finish(),
    )
    .await
    .expect("completion must respond");
    let get_name = completions
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["filterText"] == "getName")
        .expect("dependency method offered");
    assert_eq!(get_name["insertText"], "getName(");
    assert_eq!(get_name["kind"], 2); // CompletionItemKind::METHOD
                                     // Auto-import: the item adds the dependency type's import; with no
                                     // imports in the file it goes right after the package statement
                                     // (line 1, char 0).
    let lib_import = &get_name["additionalTextEdits"]
        .as_array()
        .expect("import edit on dependency completion")[0];
    assert_eq!(lib_import["newText"], "import com.example.lib.Lib;\n");
    assert_eq!(lib_import["range"]["start"]["line"], 1);
    assert_eq!(lib_import["range"]["start"]["character"], 0);

    // A workspace symbol from another package of the same module imports the
    // same way.
    let completions = respond(
        &mut service,
        Request::build("textDocument/completion")
            .id(Id::Number(5))
            .params(json!({
                "textDocument": { "uri": app_uri(&root) },
                "position": { "line": 7, "character": 13 },
            }))
            .finish(),
    )
    .await
    .expect("second completion must respond");
    let other = completions
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["label"] == "Other")
        .expect("cross-package workspace type offered");
    let other_import = &other["additionalTextEdits"]
        .as_array()
        .expect("import edit on workspace completion")[0];
    assert_eq!(other_import["newText"], "import com.other.Other;\n");
    assert_eq!(other_import["range"]["start"]["line"], 1);

    // Definition on a dependency type is honestly None (jar locations are
    // not openable), and workspace symbols stay source-only.
    let definition = respond(
        &mut service,
        Request::build("textDocument/definition")
            .id(Id::Number(3))
            .params(json!({
                "textDocument": { "uri": app_uri(&root) },
                "position": { "line": 3, "character": 13 },
            }))
            .finish(),
    )
    .await
    .expect("definition must respond");
    assert!(
        definition.is_null(),
        "dependency definition must be null, got {definition}"
    );
    let symbols = respond(
        &mut service,
        Request::build("workspace/symbol")
            .id(Id::Number(4))
            .params(json!({ "query": "Lib" }))
            .finish(),
    )
    .await
    .expect("workspace/symbol must respond");
    assert!(
        symbols.as_array().map_or(true, |items| items.is_empty()),
        "dependency types must not appear in workspace symbols, got {symbols}"
    );

    // Closing a source-root file re-reads it from disk (disk truth wins).
    respond(
        &mut service,
        Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": app_uri(&root).as_str(),
                    "languageId": "java",
                    "version": 1,
                    "text": "class Modified {\n}\n",
                }
            }))
            .finish(),
    )
    .await;
    wait_for_index(&engine, |entries, _| {
        entries.iter().any(|e| e.name == "Modified")
    })
    .await;
    respond(
        &mut service,
        Request::build("textDocument/didClose")
            .params(json!({ "textDocument": { "uri": app_uri(&root).as_str() } }))
            .finish(),
    )
    .await;
    wait_for_index(&engine, |entries, _| {
        !entries.iter().any(|e| e.name == "Modified")
            && entries
                .iter()
                .any(|e| e.name == "App" && e.kind == IndexKind::Class)
    })
    .await;

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&repo);
}

/// A tiny HTTP server over fixed routes, so a sources download can be driven
/// end to end without touching the real network.
struct SourceServer {
    address: std::net::SocketAddr,
}

impl SourceServer {
    fn start(routes: std::collections::HashMap<String, Vec<u8>>) -> SourceServer {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let routes = routes.clone();
                std::thread::spawn(move || {
                    let Ok(clone) = stream.try_clone() else {
                        return;
                    };
                    let mut stream = stream;
                    let mut reader = BufReader::new(clone);
                    let mut request_line = String::new();
                    if reader.read_line(&mut request_line).is_err() {
                        return;
                    }
                    loop {
                        let mut line = String::new();
                        match reader.read_line(&mut line) {
                            Ok(0) => break,
                            Ok(_) if line == "\r\n" || line == "\n" => break,
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                    let path = request_line.split_whitespace().nth(1).unwrap_or("/");
                    let response = match routes.get(path) {
                        Some(body) => {
                            let mut out = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            )
                            .into_bytes();
                            out.extend_from_slice(body);
                            out
                        }
                        None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec(),
                    };
                    let _ = stream.write_all(&response);
                    let _ = stream.flush();
                });
            }
        });
        SourceServer { address }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }
}

/// The headline path: a dependency's sources are downloaded from a repository,
/// extracted, indexed, and then reachable by go-to-definition.
#[tokio::test]
async fn library_sources_are_fetched_and_definition_reaches_them() {
    // Serializes env-var mutation across scan-driven tests.
    let _env = java_lsp::jdk::env_lock();
    std::env::set_var("JAVA_LSP_JDK", "/definitely/not/a/jdk");
    // This is the one harness test that must be online: unset the offline
    // switch and point the fetch at a local server.
    std::env::remove_var("JAVA_LSP_OFFLINE");

    let root = temp_dir("sources-workspace");
    let repo = temp_dir("sources-repo");
    let cache = temp_dir("sources-cache");
    let write_file = |path: std::path::PathBuf, content: &str| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    };
    let _maven = EnvVar::set("MAVEN_REPO", &repo);
    let _cache = EnvVar::set("JAVA_LSP_SOURCES_CACHE", &cache);

    write_file(
        root.join("pom.xml"),
        "<project><groupId>demo</groupId><artifactId>root</artifactId><version>1.0</version><packaging>pom</packaging><modules><module>app</module></modules></project>",
    );
    write_file(
        root.join("app").join("pom.xml"),
        "<project><artifactId>app</artifactId><parent><groupId>demo</groupId><artifactId>root</artifactId><version>1.0</version></parent><dependencies><dependency><groupId>com.example</groupId><artifactId>lib</artifactId><version>1.0</version></dependency></dependencies></project>",
    );
    let app_source = "package com.example;\n\npublic class App {\n    private Lib lib;\n}\n";
    write_file(
        root.join("app")
            .join("src")
            .join("main")
            .join("java")
            .join("com")
            .join("example")
            .join("App.java"),
        app_source,
    );

    // The dependency's class jar is present locally; only its sources are
    // missing and must be fetched.
    let lib_dir = repo.join("com").join("example").join("lib").join("1.0");
    std::fs::create_dir_all(&lib_dir).unwrap();
    write_file(
        lib_dir.join("lib-1.0.pom"),
        "<project><groupId>com.example</groupId><artifactId>lib</artifactId><version>1.0</version></project>",
    );
    let lib_class = test_class_bytes(
        "com/example/lib/Lib",
        0x0021,
        Some("java/lang/Object"),
        &[],
        &["getName"],
    );
    std::fs::write(
        lib_dir.join("lib-1.0.jar"),
        test_stored_zip(&[("com/example/lib/Lib.class", &lib_class)]),
    )
    .unwrap();

    // The repository serves the sources jar for the dependency.
    let lib_source = b"package com.example.lib;\n\npublic class Lib {\n    public String getName() {\n        return null;\n    }\n}\n";
    let mut routes = std::collections::HashMap::new();
    routes.insert(
        "/com/example/lib/1.0/lib-1.0-sources.jar".to_string(),
        test_stored_zip(&[("com/example/lib/Lib.java", lib_source)]),
    );
    let server = SourceServer::start(routes);
    let _central = EnvVar::set("JAVA_LSP_MAVEN_CENTRAL_URL", server.base_url());

    let (mut service, mut socket) = LspService::new(JavaLanguageServer::new);
    tokio::spawn(async move {
        use futures::StreamExt;
        while socket.next().await.is_some() {}
    });
    let root_uri = Url::from_file_path(&root).unwrap();
    respond(
        &mut service,
        Request::build("initialize")
            .id(Id::Number(1))
            .params(json!({ "capabilities": {}, "rootUri": root_uri.as_str() }))
            .finish(),
    )
    .await
    .expect("initialize must respond");
    respond(
        &mut service,
        Request::build("initialized").params(json!({})).finish(),
    )
    .await;

    let engine = service.inner().engine();
    wait_for_index(&engine, |entries, _| {
        entries.iter().any(|e| e.name == "Lib" && e.library_source)
    })
    .await;

    // The jar landed in the repository and the source was extracted.
    let extracted = cache
        .join("com.example")
        .join("lib")
        .join("1.0")
        .join("com")
        .join("example")
        .join("lib")
        .join("Lib.java");
    assert!(lib_dir.join("lib-1.0-sources.jar").is_file());
    assert!(extracted.is_file());

    let app_url = app_uri(&root);
    respond(
        &mut service,
        Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": app_url,
                    "languageId": "java",
                    "version": 1,
                    "text": app_source,
                }
            }))
            .finish(),
    )
    .await;

    // Go-to-definition on `Lib` opens the extracted source.
    let (line, character) = position_in(app_source, "Lib lib;");
    let definition = respond(
        &mut service,
        Request::build("textDocument/definition")
            .id(Id::Number(2))
            .params(json!({
                "textDocument": { "uri": app_url },
                "position": { "line": line, "character": character },
            }))
            .finish(),
    )
    .await
    .expect("definition must respond");
    assert_eq!(
        definition["uri"],
        json!(Url::from_file_path(&extracted).unwrap().as_str())
    );

    // The cache is not a workspace source: workspace symbols stay source-only.
    let symbols = respond(
        &mut service,
        Request::build("workspace/symbol")
            .id(Id::Number(3))
            .params(json!({ "query": "Lib" }))
            .finish(),
    )
    .await
    .expect("workspace/symbol must respond");
    assert!(
        symbols.as_array().map_or(true, |items| items.is_empty()),
        "library sources must not appear in workspace symbols, got {symbols}"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&cache);
}

#[tokio::test]
async fn jdk_types_are_offered_with_imports_but_stay_out_of_navigation() {
    // A fake JDK whose java.base jmod contains List and String.
    let jdk = temp_dir("jdk");
    std::fs::create_dir_all(jdk.join("jmods")).unwrap();
    let _env = JdkEnv::set(&jdk.display().to_string());
    let list_class = test_class_bytes("java/util/List", 0x0601, Some("java/lang/Object"), &[], &[]);
    let string_class = test_class_bytes(
        "java/lang/String",
        0x0021,
        Some("java/lang/Object"),
        &[],
        &[],
    );
    std::fs::write(
        jdk.join("jmods").join("java.base.jmod"),
        test_stored_zip(&[
            ("classes/java/util/List.class", &list_class),
            ("classes/java/lang/String.class", &string_class),
        ]),
    )
    .unwrap();

    // A plain (non-Maven) workspace whose code references List and String.
    let root = temp_dir("jdk-workspace");
    std::fs::create_dir_all(&root).unwrap();
    let doc_uri = Url::from_file_path(root.join("Main.java")).unwrap();
    std::fs::write(
        root.join("Main.java"),
        "\
class Main {\n    List names;\n    String greeting;\n}\n",
    )
    .unwrap();

    let (mut service, mut socket) = LspService::new(JavaLanguageServer::new);
    tokio::spawn(async move {
        use futures::StreamExt;
        while socket.next().await.is_some() {}
    });
    respond(
        &mut service,
        Request::build("initialize")
            .id(Id::Number(1))
            .params(json!({ "capabilities": {}, "rootUri": Url::from_file_path(&root).unwrap().as_str() }))
            .finish(),
    )
    .await
    .expect("initialize must respond");
    respond(
        &mut service,
        Request::build("initialized").params(json!({})).finish(),
    )
    .await;

    let engine = service.inner().engine();
    wait_for_index(&engine, |entries, ready| {
        ready
            && entries
                .iter()
                .any(|e| e.name == "List" && e.package.as_deref() == Some("java.util"))
            && entries
                .iter()
                .any(|e| e.name == "String" && e.package.as_deref() == Some("java.lang"))
    })
    .await;

    // `List` is offered with an import edit; `String` (java.lang) without one.
    let app_text = std::fs::read_to_string(doc_uri.to_file_path().unwrap()).unwrap();
    respond(
        &mut service,
        Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": doc_uri.as_str(),
                    "languageId": "java",
                    "version": 1,
                    "text": app_text,
                }
            }))
            .finish(),
    )
    .await;

    let completions = respond(
        &mut service,
        Request::build("textDocument/completion")
            .id(Id::Number(2))
            .params(json!({
                "textDocument": { "uri": doc_uri.as_str() },
                "position": { "line": 1, "character": 8 },
            }))
            .finish(),
    )
    .await
    .expect("completion must respond");
    let list = completions
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["label"] == "List")
        .expect("List offered");
    assert_eq!(list["insertText"], "List");
    let list_import = &list["additionalTextEdits"]
        .as_array()
        .expect("import edit on List")[0];
    assert_eq!(list_import["newText"], "import java.util.List;\n");

    let string_completions = respond(
        &mut service,
        Request::build("textDocument/completion")
            .id(Id::Number(3))
            .params(json!({
                "textDocument": { "uri": doc_uri.as_str() },
                "position": { "line": 2, "character": 10 },
            }))
            .finish(),
    )
    .await
    .expect("String completion must respond");
    let string_item = string_completions
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["label"] == "String")
        .expect("String offered");
    assert!(
        string_item["additionalTextEdits"].is_null(),
        "java.lang needs no import, got {string_item}"
    );

    // JDK symbols stay out of navigation.
    let definition = respond(
        &mut service,
        Request::build("textDocument/definition")
            .id(Id::Number(4))
            .params(json!({
                "textDocument": { "uri": doc_uri.as_str() },
                "position": { "line": 1, "character": 8 },
            }))
            .finish(),
    )
    .await
    .expect("definition must respond");
    assert!(
        definition.is_null(),
        "JDK definition must be null, got {definition}"
    );
    let symbols = respond(
        &mut service,
        Request::build("workspace/symbol")
            .id(Id::Number(5))
            .params(json!({ "query": "List" }))
            .finish(),
    )
    .await
    .expect("workspace/symbol must respond");
    assert!(
        symbols.as_array().map_or(true, |items| items.is_empty()),
        "JDK types must not appear in workspace symbols, got {symbols}"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&jdk);
}

#[tokio::test]
async fn unresolved_symbol_diagnostics_are_published_and_fixed_by_a_code_action() {
    // A fake JDK providing java.lang.Object and java.lang.String, so the
    // semantic-diagnostics gate holds.
    let jdk = temp_dir("diag-jdk");
    std::fs::create_dir_all(jdk.join("jmods")).unwrap();
    let _env = JdkEnv::set(&jdk.display().to_string());
    let object_class = test_class_bytes("java/lang/Object", 0x0021, None, &[], &[]);
    let string_class = test_class_bytes(
        "java/lang/String",
        0x0021,
        Some("java/lang/Object"),
        &[],
        &[],
    );
    std::fs::write(
        jdk.join("jmods").join("java.base.jmod"),
        test_stored_zip(&[
            ("classes/java/lang/Object.class", &object_class),
            ("classes/java/lang/String.class", &string_class),
        ]),
    )
    .unwrap();

    // A workspace holding Widget in com.b, plus a file that uses it unimported.
    let root = temp_dir("diag-workspace");
    std::fs::create_dir_all(root.join("com").join("b")).unwrap();
    std::fs::write(
        root.join("com").join("b").join("Widget.java"),
        "package com.b;\n\npublic class Widget {\n}\n",
    )
    .unwrap();
    let main_uri = Url::from_file_path(root.join("Main.java")).unwrap();
    let main_text = "class Main {\n    Widget field;\n    Missing other;\n}\n";
    std::fs::write(root.join("Main.java"), main_text).unwrap();

    let (mut service, mut socket) = LspService::new(JavaLanguageServer::new);
    respond(
        &mut service,
        Request::build("initialize")
            .id(Id::Number(1))
            .params(json!({
                "capabilities": {
                    "workspace": { "workspaceEdit": { "resourceOperations": ["create"] } }
                },
                "rootUri": Url::from_file_path(&root).unwrap().as_str(),
            }))
            .finish(),
    )
    .await
    .expect("initialize must respond");
    respond(
        &mut service,
        Request::build("initialized").params(json!({})).finish(),
    )
    .await;

    let engine = service.inner().engine();
    wait_for_index(&engine, |entries, ready| {
        ready
            && entries
                .iter()
                .any(|entry| entry.name == "Widget" && entry.package.as_deref() == Some("com.b"))
    })
    .await;

    respond(
        &mut service,
        Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": main_uri.as_str(),
                    "languageId": "java",
                    "version": 1,
                    "text": main_text,
                }
            }))
            .finish(),
    )
    .await;

    let diagnostics = next_diagnostics_for(&mut socket, main_uri.as_str()).await;
    let widget = diagnostics
        .as_array()
        .unwrap()
        .iter()
        .find(|diagnostic| {
            diagnostic["message"]
                .as_str()
                .is_some_and(|message| message.contains("Widget"))
        })
        .expect("Widget should be flagged as an unresolved type");
    assert_eq!(
        widget["code"], "unresolved-type",
        "expected an unresolved type, got {diagnostics}"
    );
    assert_eq!(
        widget["severity"], 1,
        "expected an error, got {diagnostics}"
    );

    // The quick fix adds the import the index already knows.
    let actions = respond(
        &mut service,
        Request::build("textDocument/codeAction")
            .id(Id::Number(2))
            .params(json!({
                "textDocument": { "uri": main_uri.as_str() },
                "range": widget["range"].clone(),
                "context": { "diagnostics": [widget] },
            }))
            .finish(),
    )
    .await
    .expect("codeAction must respond");
    let action = &actions.as_array().unwrap()[0];
    assert_eq!(action["title"], "Add import `com.b.Widget`");
    assert_eq!(
        action["edit"]["changes"][main_uri.as_str()][0]["newText"],
        "import com.b.Widget;\n"
    );

    // A type nowhere in the index offers the four create-type actions.
    let missing = diagnostics
        .as_array()
        .unwrap()
        .iter()
        .find(|diagnostic| {
            diagnostic["message"]
                .as_str()
                .is_some_and(|message| message.contains("Missing"))
        })
        .expect("Missing should be flagged");
    let create_actions = respond(
        &mut service,
        Request::build("textDocument/codeAction")
            .id(Id::Number(3))
            .params(json!({
                "textDocument": { "uri": main_uri.as_str() },
                "range": missing["range"].clone(),
                "context": { "diagnostics": [missing] },
            }))
            .finish(),
    )
    .await
    .expect("codeAction must respond");
    let titles: Vec<String> = create_actions
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|action| action["title"].as_str().map(str::to_string))
        .collect();
    for kind in ["class", "interface", "enum", "record"] {
        assert!(
            titles.contains(&format!("Create {kind} `Missing`")),
            "missing create-{kind} action, got {titles:?}"
        );
    }

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&jdk);
}

#[tokio::test]
async fn watched_file_changes_refresh_a_referring_document() {
    // A fake JDK providing java.lang.Object and java.lang.String, so the
    // semantic-diagnostics gate holds.
    let jdk = temp_dir("watch-jdk");
    std::fs::create_dir_all(jdk.join("jmods")).unwrap();
    let _env = JdkEnv::set(&jdk.display().to_string());
    let object_class = test_class_bytes("java/lang/Object", 0x0021, None, &[], &[]);
    let string_class = test_class_bytes(
        "java/lang/String",
        0x0021,
        Some("java/lang/Object"),
        &[],
        &[],
    );
    std::fs::write(
        jdk.join("jmods").join("java.base.jmod"),
        test_stored_zip(&[
            ("classes/java/lang/Object.class", &object_class),
            ("classes/java/lang/String.class", &string_class),
        ]),
    )
    .unwrap();

    // A workspace whose Main.java uses a class that does not exist yet.
    let root = temp_dir("watch-workspace");
    std::fs::create_dir_all(&root).unwrap();
    let main_uri = Url::from_file_path(root.join("Main.java")).unwrap();
    let main_text = "package demo;\n\nclass Main {\n    XY field;\n}\n";
    std::fs::write(root.join("Main.java"), main_text).unwrap();

    let (mut service, mut socket) = LspService::new(JavaLanguageServer::new);
    respond(
        &mut service,
        Request::build("initialize")
            .id(Id::Number(1))
            .params(json!({
                "capabilities": {
                    "workspace": {
                        "didChangeWatchedFiles": { "dynamicRegistration": true }
                    }
                },
                "rootUri": Url::from_file_path(&root).unwrap().as_str(),
            }))
            .finish(),
    )
    .await
    .expect("initialize must respond");
    respond(
        &mut service,
        Request::build("initialized").params(json!({})).finish(),
    )
    .await;

    // The server asks the client to watch `**/*.java` (D1).
    let registration = next_request_with_method(&mut socket, "client/registerCapability").await;
    let watched = registration["registrations"]
        .as_array()
        .expect("registrations")
        .iter()
        .find(|registration| registration["method"] == "workspace/didChangeWatchedFiles")
        .expect("a watched-file registration");
    assert_eq!(
        watched["registerOptions"]["watchers"][0]["globPattern"],
        "**/*.java"
    );

    let engine = service.inner().engine();
    wait_for_index(&engine, |_, ready| ready).await;

    respond(
        &mut service,
        Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": main_uri.as_str(),
                    "languageId": "java",
                    "version": 1,
                    "text": main_text,
                }
            }))
            .finish(),
    )
    .await;
    let diagnostics = next_diagnostics_for(&mut socket, main_uri.as_str()).await;
    assert!(
        diagnostics.as_array().unwrap().iter().any(|diagnostic| {
            diagnostic["message"]
                .as_str()
                .is_some_and(|message| message.contains("XY"))
        }),
        "XY should be unresolved before the file exists: {diagnostics}"
    );

    // The file is created on disk and reported through the watcher; it is never
    // opened in the editor.
    std::fs::write(
        root.join("XY.java"),
        "package demo;\n\npublic class XY {\n}\n",
    )
    .unwrap();
    let xy_uri = Url::from_file_path(root.join("XY.java")).unwrap();
    respond(
        &mut service,
        Request::build("workspace/didChangeWatchedFiles")
            .params(json!({ "changes": [{ "uri": xy_uri.as_str(), "type": 1 }] }))
            .finish(),
    )
    .await;

    // `Main.java` was never edited, yet its diagnostic clears (D3).
    let diagnostics = next_diagnostics_for(&mut socket, main_uri.as_str()).await;
    assert!(
        diagnostics.as_array().unwrap().is_empty(),
        "the referring document must refresh: {diagnostics}"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&jdk);
}

#[tokio::test]
async fn watched_files_are_not_registered_without_the_client_capability() {
    use futures::StreamExt;
    let _env = java_lsp::jdk::env_lock();
    std::env::set_var("JAVA_LSP_JDK", "/definitely/not/a/jdk");
    let _offline = EnvVar::set("JAVA_LSP_OFFLINE", "1");

    let root = temp_dir("no-watcher");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("Greet.java"),
        "package demo;\n\npublic class Greet {\n}\n",
    )
    .unwrap();

    let (mut service, mut socket) = LspService::new(JavaLanguageServer::new);
    // No `workspace.didChangeWatchedFiles`: the watcher must be skipped (D6).
    respond(
        &mut service,
        Request::build("initialize")
            .id(Id::Number(1))
            .params(json!({
                "capabilities": {},
                "rootUri": Url::from_file_path(&root).unwrap().as_str(),
            }))
            .finish(),
    )
    .await
    .expect("initialize must respond");
    respond(
        &mut service,
        Request::build("initialized").params(json!({})).finish(),
    )
    .await;

    // Wait for the warm-up, then drain for a bounded window: no registration.
    let engine = service.inner().engine();
    wait_for_index(&engine, |_, ready| ready).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    while std::time::Instant::now() < deadline {
        let next = tokio::time::timeout(std::time::Duration::from_millis(100), socket.next()).await;
        if let Ok(Some(request)) = next {
            assert_ne!(
                request.method(),
                "client/registerCapability",
                "no watcher registration without the capability: {request:?}"
            );
        }
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn inlay_hints_resolve_an_imported_name_shared_across_packages() {
    let _env = java_lsp::jdk::env_lock();
    std::env::set_var("JAVA_LSP_JDK", "/definitely/not/a/jdk");
    let _offline = EnvVar::set("JAVA_LSP_OFFLINE", "1");

    // Two `List` types in different packages make the simple name ambiguous, so
    // only a resolved import can pin `List.of(3)` down.
    let root = temp_dir("ambiguous-import");
    std::fs::create_dir_all(root.join("a")).unwrap();
    std::fs::create_dir_all(root.join("b")).unwrap();
    std::fs::write(
        root.join("a").join("List.java"),
        "package a;\n\npublic class List {\n    public static List of(int n) { return null; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("b").join("List.java"),
        "package b;\n\npublic class List {\n}\n",
    )
    .unwrap();

    let imported_uri = Url::from_file_path(root.join("Imported.java")).unwrap();
    let imported = "package demo;\n\nimport a.List;\n\nclass Imported {\n    void m() {\n        var x = List.of(3);\n    }\n}\n";
    std::fs::write(root.join("Imported.java"), imported).unwrap();
    let bare_uri = Url::from_file_path(root.join("Bare.java")).unwrap();
    let bare =
        "package demo;\n\nclass Bare {\n    void m() {\n        var x = List.of(3);\n    }\n}\n";

    let (mut service, mut socket) = LspService::new(JavaLanguageServer::new);
    tokio::spawn(async move {
        use futures::StreamExt;
        while socket.next().await.is_some() {}
    });
    respond(
        &mut service,
        Request::build("initialize")
            .id(Id::Number(1))
            .params(json!({ "capabilities": {}, "rootUri": Url::from_file_path(&root).unwrap().as_str() }))
            .finish(),
    )
    .await
    .expect("initialize must respond");
    respond(
        &mut service,
        Request::build("initialized").params(json!({})).finish(),
    )
    .await;
    let engine = service.inner().engine();
    wait_for_index(&engine, |entries, ready| {
        ready && entries.iter().any(|e| e.name == "of")
    })
    .await;

    for (uri, text, expect_hint) in [(&imported_uri, imported, true), (&bare_uri, bare, false)] {
        respond(
            &mut service,
            Request::build("textDocument/didOpen")
                .params(json!({
                    "textDocument": {
                        "uri": uri.as_str(),
                        "languageId": "java",
                        "version": 1,
                        "text": text,
                    }
                }))
                .finish(),
        )
        .await;
        let hints = respond(
            &mut service,
            Request::build("textDocument/inlayHint")
                .id(Id::Number(2))
                .params(json!({
                    "textDocument": { "uri": uri.as_str() },
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 20, "character": 0},
                    },
                }))
                .finish(),
        )
        .await
        .expect("inlay hint must respond");
        let labels: Vec<&str> = hints
            .as_array()
            .expect("inlay hints must be an array")
            .iter()
            .filter_map(|hint| hint["label"].as_str())
            .collect();
        if expect_hint {
            assert!(
                labels.contains(&": List"),
                "the import should resolve List.of: {labels:?}"
            );
        } else {
            assert!(
                labels.is_empty(),
                "an ambiguous name must not be guessed: {labels:?}"
            );
        }
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn inlay_hints_resolve_a_jdk_static_factory() {
    // A fake JDK sourced from `lib/src.zip`, with `List` in two packages so the
    // simple name is ambiguous unless the import pins it down.
    let jdk = temp_dir("srczip-hints");
    std::fs::create_dir_all(jdk.join("lib")).unwrap();
    let list_source = b"package java.util;\n\npublic interface List<E> {\n    static <E> List<E> of() { return null; }\n    static <E> List<E> of(E e1) { return null; }\n    static <E> List<E> of(E... elements) { return null; }\n}\n";
    let awt_source = b"package java.awt;\n\npublic class List {\n}\n";
    std::fs::write(
        jdk.join("lib").join("src.zip"),
        test_stored_zip(&[
            ("java.base/java/util/List.java", list_source),
            ("java.desktop/java/awt/List.java", awt_source),
        ]),
    )
    .unwrap();
    let _env = JdkEnv::set(&jdk.display().to_string());

    let root = temp_dir("srczip-hints-workspace");
    std::fs::create_dir_all(&root).unwrap();
    let uri = Url::from_file_path(root.join("Main.java")).unwrap();
    let text = "package demo;\n\nimport java.util.List;\n\nclass Main {\n    void m() {\n        var list = List.of(5);\n    }\n}\n";
    std::fs::write(root.join("Main.java"), text).unwrap();

    let (mut service, mut socket) = LspService::new(JavaLanguageServer::new);
    tokio::spawn(async move {
        use futures::StreamExt;
        while socket.next().await.is_some() {}
    });
    respond(
        &mut service,
        Request::build("initialize")
            .id(Id::Number(1))
            .params(json!({ "capabilities": {}, "rootUri": Url::from_file_path(&root).unwrap().as_str() }))
            .finish(),
    )
    .await
    .expect("initialize must respond");
    respond(
        &mut service,
        Request::build("initialized").params(json!({})).finish(),
    )
    .await;
    let engine = service.inner().engine();
    wait_for_index(&engine, |entries, ready| {
        ready
            && entries
                .iter()
                .any(|e| e.name == "of" && e.package.as_deref() == Some("java.util"))
    })
    .await;

    respond(
        &mut service,
        Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": uri.as_str(),
                    "languageId": "java",
                    "version": 1,
                    "text": text,
                }
            }))
            .finish(),
    )
    .await;

    let hints = respond(
        &mut service,
        Request::build("textDocument/inlayHint")
            .id(Id::Number(2))
            .params(json!({
                "textDocument": { "uri": uri.as_str() },
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 20, "character": 0},
                },
            }))
            .finish(),
    )
    .await
    .expect("inlay hint must respond");
    let labels: Vec<&str> = hints
        .as_array()
        .expect("inlay hints must be an array")
        .iter()
        .filter_map(|hint| hint["label"].as_str())
        .collect();
    assert!(
        labels.contains(&": List<Integer>"),
        "the generic factory call should resolve to its element type: {labels:?}"
    );
    assert!(
        labels.contains(&"e1:"),
        "the arity-matched overload's parameter name: {labels:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&jdk);
}

fn app_uri(root: &std::path::Path) -> Url {
    Url::from_file_path(
        root.join("app")
            .join("src")
            .join("main")
            .join("java")
            .join("com")
            .join("example")
            .join("App.java"),
    )
    .unwrap()
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "java-lsp-harness-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// Minimal class-file builder for fixtures: one Utf8+Class pair for the
/// class and its superclass, one Utf8 per member; all members public.
fn test_class_bytes(
    internal: &str,
    flags: u16,
    super_name: Option<&str>,
    fields: &[&str],
    methods: &[&str],
) -> Vec<u8> {
    let mut pool: Vec<(u8, Vec<u8>)> = Vec::new();
    let utf8 = |text: &str, pool: &mut Vec<(u8, Vec<u8>)>| -> u16 {
        pool.push((
            1,
            [
                (text.len() as u16).to_be_bytes().to_vec(),
                text.as_bytes().to_vec(),
            ]
            .concat(),
        ));
        pool.len() as u16
    };
    let name_slot = utf8(internal, &mut pool);
    pool.push((7, name_slot.to_be_bytes().to_vec()));
    let this_class = pool.len() as u16;
    let super_class = super_name.map_or(0, |name| {
        let slot = utf8(name, &mut pool);
        pool.push((7, slot.to_be_bytes().to_vec()));
        pool.len() as u16
    });
    let mut field_slots = Vec::new();
    for name in fields {
        field_slots.push(utf8(name, &mut pool));
    }
    let mut method_slots = Vec::new();
    for name in methods {
        method_slots.push(utf8(name, &mut pool));
    }

    let mut out = Vec::new();
    out.extend_from_slice(&0xCAFEBABEu32.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&(52u16).to_be_bytes());
    out.extend_from_slice(&((pool.len() + 1) as u16).to_be_bytes());
    for (tag, operands) in &pool {
        out.push(*tag);
        out.extend_from_slice(operands);
    }
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&this_class.to_be_bytes());
    out.extend_from_slice(&super_class.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    for slots in [&field_slots, &method_slots] {
        out.extend_from_slice(&(slots.len() as u16).to_be_bytes());
        for slot in slots {
            out.extend_from_slice(&0x0001u16.to_be_bytes());
            out.extend_from_slice(&slot.to_be_bytes());
            out.extend_from_slice(&0u16.to_be_bytes());
            out.extend_from_slice(&0u16.to_be_bytes());
        }
    }
    out.extend_from_slice(&0u16.to_be_bytes());
    out
}

/// Minimal STORED-entry zip writer for fixtures (std has none).
fn test_stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    fn crc32(data: &[u8]) -> u32 {
        let mut table = [0u32; 256];
        for (i, slot) in table.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB88320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *slot = c;
        }
        let mut crc = 0xFFFF_FFFFu32;
        for &byte in data {
            crc = table[((crc ^ byte as u32) & 0xFF) as usize] ^ (crc >> 8);
        }
        crc ^ 0xFFFF_FFFF
    }

    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let local_offset = out.len() as u32;
        let crc = crc32(data);
        out.extend_from_slice(&[0x50, 0x4b, 0x03, 0x04]);
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // stored
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);

        central.extend_from_slice(&[0x50, 0x4b, 0x01, 0x02]);
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&local_offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let central_offset = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06]);
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}
async fn wait_for_index(
    engine: &EngineHandle,
    mut pred: impl FnMut(&[java_lsp::index::SymbolEntry], bool) -> bool,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let entries = engine.indexed_symbols().await;
        let ready = engine.index_ready().await;
        if pred(&entries, ready) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "index condition not met in time; entries: {entries:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn inlay_hints_render_types_parameters_and_chains() {
    let mut service = service();
    initialize(&mut service).await;

    // Self-contained: the open buffer's own type model supplies everything, so
    // no workspace scan is needed.
    let text = "\
package demo;

class Widget {
    private int size;

    int getSize() {
        return size;
    }

    Widget self() {
        return this;
    }

    void run(int amount, String label) {
        var w = new Widget();
        int total = w.self().getSize();
        w.run(2, \"x\");
    }
}
";
    respond(
        &mut service,
        Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": HELLO_URI,
                    "languageId": "java",
                    "version": 1,
                    "text": text,
                }
            }))
            .finish(),
    )
    .await;

    let hints = respond(
        &mut service,
        Request::build("textDocument/inlayHint")
            .id(Id::Number(2))
            .params(json!({
                "textDocument": { "uri": HELLO_URI },
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 200, "character": 0 },
                },
            }))
            .finish(),
    )
    .await
    .expect("inlay hint must respond");

    let labels: Vec<&str> = hints
        .as_array()
        .expect("inlay hints must be an array")
        .iter()
        .filter_map(|hint| hint["label"].as_str())
        .collect();
    assert!(labels.contains(&": Widget"), "{labels:?}");
    assert!(labels.contains(&": int"), "{labels:?}");
    assert!(labels.contains(&"amount:"), "{labels:?}");
    assert!(labels.contains(&"label:"), "{labels:?}");
    // `: Widget` comes from both the `var w` variable hint and the `w.self()`
    // chain hint, so the chain family is only covered when both appear.
    assert_eq!(
        labels.iter().filter(|label| **label == ": Widget").count(),
        2,
        "the chain link must be hinted too: {labels:?}"
    );
}

#[tokio::test]
async fn hover_and_member_completion_use_the_receiver_type() {
    // Serializes env-var mutation across scan-driven tests, and opts out of
    // machine-JDK indexing (covered by its own test).
    let _env = java_lsp::jdk::env_lock();
    std::env::set_var("JAVA_LSP_JDK", "/definitely/not/a/jdk");
    let _offline = EnvVar::set("JAVA_LSP_OFFLINE", "1");
    let root = std::env::temp_dir().join(format!(
        "java-lsp-harness-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("Widget.java"),
        "package demo;\n\npublic class Widget {\n    private int size;\n    public int getSize() { return size; }\n}\n",
    )
    .unwrap();

    let (mut service, mut socket) = LspService::new(JavaLanguageServer::new);
    tokio::spawn(async move {
        use futures::StreamExt;
        while socket.next().await.is_some() {}
    });
    let root_uri = Url::from_file_path(&root).unwrap();
    respond(
        &mut service,
        Request::build("initialize")
            .id(Id::Number(1))
            .params(json!({ "capabilities": {}, "rootUri": root_uri.as_str() }))
            .finish(),
    )
    .await
    .expect("initialize must respond");
    respond(
        &mut service,
        Request::build("initialized").params(json!({})).finish(),
    )
    .await;

    // `Use.java` is never written to disk: the open buffer is what features
    // answer from, with the scanned `Widget` reachable through the model.
    let uri = "file:///src/Use.java";
    let text = "package demo;\n\npublic class Use {\n    void m() {\n        Widget w = null;\n        w.getSize();\n    }\n}\n";
    respond(
        &mut service,
        Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "java",
                    "version": 1,
                    "text": text,
                }
            }))
            .finish(),
    )
    .await;

    let engine = service.inner().engine();
    wait_for_index(&engine, |entries, ready| {
        ready && entries.iter().any(|entry| entry.name == "getSize")
    })
    .await;

    // Hover inside `getSize` resolves through the receiver's declared type.
    let hover = respond(
        &mut service,
        Request::build("textDocument/hover")
            .id(Id::Number(2))
            .params(json!({
                "textDocument": { "uri": uri },
                "position": { "line": 5, "character": 12 },
            }))
            .finish(),
    )
    .await
    .expect("hover must respond");
    let value = hover["contents"]["value"].as_str().unwrap_or_default();
    assert!(value.contains("int getSize()"), "{value}");
    assert!(value.contains("of `Widget`"), "{value}");

    // A completion right after `w.` offers that type's members.
    let completion = respond(
        &mut service,
        Request::build("textDocument/completion")
            .id(Id::Number(3))
            .params(json!({
                "textDocument": { "uri": uri },
                "position": { "line": 5, "character": 10 },
            }))
            .finish(),
    )
    .await
    .expect("completion must respond");
    let labels: Vec<&str> = completion
        .as_array()
        .expect("completion must return an array")
        .iter()
        .filter_map(|item| item["label"].as_str())
        .collect();
    assert!(
        labels.iter().any(|label| label.starts_with("int getSize")),
        "{labels:?}"
    );
    assert!(labels.contains(&"size"), "{labels:?}");
}

#[tokio::test]
async fn references_and_rename_span_the_workspace() {
    let _env = java_lsp::jdk::env_lock();
    std::env::set_var("JAVA_LSP_JDK", "/definitely/not/a/jdk");
    let _offline = EnvVar::set("JAVA_LSP_OFFLINE", "1");
    let root = std::env::temp_dir().join(format!(
        "java-lsp-harness-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(root.join("a")).unwrap();
    std::fs::write(
        root.join("a/Widget.java"),
        "package a;\n\npublic class Widget {\n}\n",
    )
    .unwrap();
    let use_uri = Url::from_file_path(root.join("a/Use.java")).unwrap();

    let (mut service, mut socket) = LspService::new(JavaLanguageServer::new);
    tokio::spawn(async move {
        use futures::StreamExt;
        while socket.next().await.is_some() {}
    });
    let root_uri = Url::from_file_path(&root).unwrap();
    respond(
        &mut service,
        Request::build("initialize")
            .id(Id::Number(1))
            .params(json!({ "capabilities": {}, "rootUri": root_uri.as_str() }))
            .finish(),
    )
    .await
    .expect("initialize must respond");
    respond(
        &mut service,
        Request::build("initialized").params(json!({})).finish(),
    )
    .await;

    let text =
        "package a;\n\npublic class Use {\n    void m() {\n        Widget w = null;\n    }\n}\n";
    respond(
        &mut service,
        Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": use_uri.as_str(),
                    "languageId": "java",
                    "version": 1,
                    "text": text,
                }
            }))
            .finish(),
    )
    .await;

    let engine = service.inner().engine();
    wait_for_index(&engine, |entries, ready| {
        ready && entries.iter().any(|entry| entry.name == "Widget")
    })
    .await;

    // `Widget` is written on line 4 (`        Widget w = null;`).
    let position = json!({ "line": 4, "character": 9 });
    let references = respond(
        &mut service,
        Request::build("textDocument/references")
            .id(Id::Number(2))
            .params(json!({
                "textDocument": { "uri": use_uri.as_str() },
                "position": position,
                "context": { "includeDeclaration": true },
            }))
            .finish(),
    )
    .await
    .expect("references must respond");
    let locations = references.as_array().expect("references array");
    assert_eq!(locations.len(), 2, "{references}");

    let rename = respond(
        &mut service,
        Request::build("textDocument/rename")
            .id(Id::Number(3))
            .params(json!({
                "textDocument": { "uri": use_uri.as_str() },
                "position": position,
                "newName": "Gadget",
            }))
            .finish(),
    )
    .await
    .expect("rename must respond");
    let changes = rename["changes"].as_object().expect("changes object");
    assert_eq!(changes.len(), 2, "{rename}");
    for edits in changes.values() {
        for edit in edits.as_array().expect("edits array") {
            assert_eq!(edit["newText"], "Gadget");
        }
    }
    // A library/nonexistent rename attempt is refused with null.
    let refused = respond(
        &mut service,
        Request::build("textDocument/rename")
            .id(Id::Number(4))
            .params(json!({
                "textDocument": { "uri": use_uri.as_str() },
                "position": position,
                "newName": "class",
            }))
            .finish(),
    )
    .await
    .expect("rename must respond");
    assert!(refused.is_null(), "{refused}");
}

#[tokio::test]
async fn completion_offers_keywords_locals_and_workspace_symbols() {
    // Serializes env-var mutation across scan-driven tests, and opts out of
    // machine-JDK indexing (covered by its own test).
    let _env = java_lsp::jdk::env_lock();
    std::env::set_var("JAVA_LSP_JDK", "/definitely/not/a/jdk");
    let _offline = EnvVar::set("JAVA_LSP_OFFLINE", "1");
    // Fixture project so the workspace index has symbols to offer.
    let root = std::env::temp_dir().join(format!(
        "java-lsp-harness-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("Greet.java"),
        "package demo;\n\npublic class Greet {\n    private String name;\n    public String getName() { return name; }\n}\n",
    )
    .unwrap();

    let (mut service, mut socket) = LspService::new(JavaLanguageServer::new);
    // Diagnostics publications go through a capacity-1 channel; drain it so
    // handlers never block on an unread client socket.
    tokio::spawn(async move {
        use futures::StreamExt;
        while socket.next().await.is_some() {}
    });
    let root_uri = Url::from_file_path(&root).unwrap();
    respond(
        &mut service,
        Request::build("initialize")
            .id(Id::Number(1))
            .params(json!({ "capabilities": {}, "rootUri": root_uri.as_str() }))
            .finish(),
    )
    .await
    .expect("initialize must respond");
    respond(
        &mut service,
        Request::build("initialized").params(json!({})).finish(),
    )
    .await;

    respond(
        &mut service,
        Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": HELLO_URI,
                    "languageId": "java",
                    "version": 1,
                    "text": COMPLETION_SAMPLE,
                }
            }))
            .finish(),
    )
    .await;

    // Wait for the fixture's declarations before expecting a workspace hit.
    let engine = service.inner().engine();
    wait_for_index(&engine, |entries, ready| {
        ready
            && entries
                .iter()
                .any(|e| e.name == "name" && e.kind == IndexKind::Field)
    })
    .await;

    let (line, character) = COMPLETION_POSITION;
    let completions = respond(
        &mut service,
        Request::build("textDocument/completion")
            .id(Id::Number(2))
            .params(json!({
                "textDocument": { "uri": HELLO_URI },
                "position": { "line": line, "character": character },
            }))
            .finish(),
    )
    .await
    .expect("completion must respond");
    let items = completions
        .as_array()
        .expect("completion must return an array");

    // Rank 0: a Java keyword.
    let keyword = items
        .iter()
        .find(|i| i["insertText"].as_str() == Some("native"))
        .expect("keyword must be offered");
    assert_eq!(keyword["kind"], 14, "CompletionItemKind::KEYWORD");
    assert_eq!(keyword["sortText"], "0native");

    // Rank 1: a local in scope at the cursor.
    let local = items
        .iter()
        .find(|i| i["insertText"].as_str() == Some("named"))
        .expect("local must be offered");
    assert_eq!(local["kind"], 6, "CompletionItemKind::VARIABLE");
    assert_eq!(local["detail"], "local");
    assert_eq!(local["sortText"], "1named");

    // Rank 2: a fixture workspace symbol — container-prefixed label, simple
    // insert text.
    let member = items
        .iter()
        .find(|i| i["insertText"].as_str() == Some("name"))
        .expect("workspace symbol must be offered");
    assert_eq!(member["label"], "Greet.name");
    assert_eq!(member["filterText"], "name");
    assert_eq!(member["kind"], 5, "CompletionItemKind::FIELD");
    assert_eq!(member["detail"], "field of Greet");
    assert_eq!(member["sortText"], "2name");

    // After a dot: an empty list, claiming no membership (AC3).
    respond(
        &mut service,
        Request::build("textDocument/didChange")
            .params(json!({
                "textDocument": { "uri": HELLO_URI, "version": 2 },
                "contentChanges": [{
                    "text": COMPLETION_SAMPLE.replace("return na;", "return count.na;"),
                }],
            }))
            .finish(),
    )
    .await;
    let completions = respond(
        &mut service,
        Request::build("textDocument/completion")
            .id(Id::Number(3))
            .params(json!({
                "textDocument": { "uri": HELLO_URI },
                "position": { "line": line, "character": 23 },
            }))
            .finish(),
    )
    .await
    .expect("completion after a dot must respond");
    assert!(
        completions.as_array().expect("array").is_empty(),
        "{completions}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn completion_without_workspace_root_still_serves_syntax_sources() {
    let mut service = service();
    initialize(&mut service).await; // no rootUri — nothing to warm up

    respond(
        &mut service,
        Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": HELLO_URI,
                    "languageId": "java",
                    "version": 1,
                    "text": COMPLETION_SAMPLE,
                }
            }))
            .finish(),
    )
    .await;

    // The response must arrive immediately, without any index warm-up (R6).
    let (line, character) = COMPLETION_POSITION;
    let completions = respond(
        &mut service,
        Request::build("textDocument/completion")
            .id(Id::Number(2))
            .params(json!({
                "textDocument": { "uri": HELLO_URI },
                "position": { "line": line, "character": character },
            }))
            .finish(),
    )
    .await
    .expect("completion must respond without a workspace root");
    let items = completions
        .as_array()
        .expect("completion must return an array");
    assert!(
        items
            .iter()
            .any(|i| i["insertText"].as_str() == Some("native") && i["kind"] == 14),
        "keyword must be offered, got {items:?}"
    );
    assert!(
        items
            .iter()
            .any(|i| i["insertText"].as_str() == Some("named") && i["kind"] == 6),
        "local must be offered, got {items:?}"
    );
    assert!(
        !items
            .iter()
            .any(|i| i["label"].as_str().is_some_and(|label| label.contains('.'))),
        "no container-prefixed members without a workspace, got {items:?}"
    );
}

#[tokio::test]
async fn signature_help_lists_a_callees_overloads() {
    let mut service = service();
    initialize(&mut service).await;

    let text = "\
class Calc {
    int add(int a) { return a; }
    int add(int a, int b) { return a + b; }
    void use() {
        add(1, 2);
    }
}
";
    respond(
        &mut service,
        Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": HELLO_URI,
                    "languageId": "java",
                    "version": 1,
                    "text": text,
                }
            }))
            .finish(),
    )
    .await;

    // The cursor sits at the start of the second argument.
    let (line, character) = position_in(text, "1, 2");
    let help = respond(
        &mut service,
        Request::build("textDocument/signatureHelp")
            .id(Id::Number(2))
            .params(json!({
                "textDocument": { "uri": HELLO_URI },
                "position": { "line": line, "character": character + 3 },
            }))
            .finish(),
    )
    .await
    .expect("signatureHelp must respond");

    let signatures = help["signatures"].as_array().unwrap();
    let labels: Vec<&str> = signatures
        .iter()
        .filter_map(|signature| signature["label"].as_str())
        .collect();
    assert!(labels.contains(&"int add(int a)"), "{help}");
    assert!(labels.contains(&"int add(int a, int b)"), "{help}");
    assert_eq!(signatures[0]["activeParameter"], 1, "{help}");
}

#[tokio::test]
async fn definition_respects_the_callees_overloads() {
    let mut service = service();
    initialize(&mut service).await;

    let text = "\
class Calc {
    int add(int a) { return a; }
    int add(String s) { return 0; }
    void use() {
        add(\"x\");
    }
}
";
    respond(
        &mut service,
        Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": HELLO_URI,
                    "languageId": "java",
                    "version": 1,
                    "text": text,
                }
            }))
            .finish(),
    )
    .await;

    // Go-to-definition on `add("x")` selects the `String` overload on line 2,
    // not the first-declared `int` overload on line 1.
    let (line, character) = position_in(text, "add(\"x\")");
    let definition = respond(
        &mut service,
        Request::build("textDocument/definition")
            .id(Id::Number(2))
            .params(json!({
                "textDocument": { "uri": HELLO_URI },
                "position": { "line": line, "character": character + 1 },
            }))
            .finish(),
    )
    .await
    .expect("definition must respond");
    assert_eq!(definition["range"]["start"]["line"], 2, "{definition}");
}

/// Line/character of the first occurrence of `needle` (ASCII fixtures only,
/// so UTF-16 units equal characters).
fn position_in(text: &str, needle: &str) -> (u32, u32) {
    let offset = text.find(needle).expect("needle must be present");
    let line = text[..offset].matches('\n').count() as u32;
    let line_start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
    (line, (offset - line_start) as u32)
}

const GREET_FIXTURE: &str = "\
package demo;

public class Greet {
    private String name;
    public String getName() { return name; }
}
";

#[tokio::test]
async fn definition_resolves_usages_imports_and_reports_ambiguity() {
    let _env = JdkEnv::none();
    // Fixture project on disk, so the scan has targets to resolve to.
    let root = std::env::temp_dir().join(format!(
        "java-lsp-harness-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let greet_path = root.join("Greet.java");
    std::fs::write(&greet_path, GREET_FIXTURE).unwrap();
    // The same declaration in two files: deliberately ambiguous.
    std::fs::write(
        root.join("One.java"),
        "package demo;\n\npublic class Same {}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Two.java"),
        "package demo;\n\npublic class Same {}\n",
    )
    .unwrap();

    let (mut service, mut socket) = LspService::new(JavaLanguageServer::new);
    // Diagnostics publications go through a capacity-1 channel; drain it so
    // handlers never block on an unread client socket.
    tokio::spawn(async move {
        use futures::StreamExt;
        while socket.next().await.is_some() {}
    });
    let root_uri = Url::from_file_path(&root).unwrap();
    respond(
        &mut service,
        Request::build("initialize")
            .id(Id::Number(1))
            .params(json!({ "capabilities": {}, "rootUri": root_uri.as_str() }))
            .finish(),
    )
    .await
    .expect("initialize must respond");
    respond(
        &mut service,
        Request::build("initialized").params(json!({})).finish(),
    )
    .await;

    let usage = "\
import demo.Greet;

class Hello {
    Greet greet;
    Same same;
}
";
    respond(
        &mut service,
        Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": HELLO_URI,
                    "languageId": "java",
                    "version": 1,
                    "text": usage,
                }
            }))
            .finish(),
    )
    .await;

    // The scanned fixture files provide the navigation targets.
    let engine = service.inner().engine();
    wait_for_index(&engine, |entries, ready| {
        ready
            && entries
                .iter()
                .any(|e| e.name == "Greet" && e.kind == IndexKind::Class)
            && entries.iter().filter(|e| e.name == "Same").count() == 2
    })
    .await;

    // `public class Greet` in GreetFixture: line 2, characters 13..18.
    let greet_location = json!({
        "uri": Url::from_file_path(&greet_path).unwrap().as_str(),
        "range": {
            "start": { "line": 2, "character": 13 },
            "end": { "line": 2, "character": 18 },
        },
    });

    // A type usage in the open file resolves to the scanned file.
    let (line, character) = position_in(usage, "Greet greet;");
    let definition = respond(
        &mut service,
        Request::build("textDocument/definition")
            .id(Id::Number(2))
            .params(json!({
                "textDocument": { "uri": HELLO_URI },
                "position": { "line": line, "character": character },
            }))
            .finish(),
    )
    .await
    .expect("definition must respond");
    assert_eq!(definition, greet_location, "{definition}");

    // An import target resolves through the dotted path's last segment.
    let (line, character) = position_in(usage, "demo.Greet;");
    let definition = respond(
        &mut service,
        Request::build("textDocument/definition")
            .id(Id::Number(3))
            .params(json!({
                "textDocument": { "uri": HELLO_URI },
                "position": { "line": line, "character": character },
            }))
            .finish(),
    )
    .await
    .expect("definition must respond");
    assert_eq!(definition, greet_location, "{definition}");

    // A reference with same-name declarations in two files: no result beats
    // a wrong result.
    let (line, character) = position_in(usage, "Same same;");
    let definition = respond(
        &mut service,
        Request::build("textDocument/definition")
            .id(Id::Number(4))
            .params(json!({
                "textDocument": { "uri": HELLO_URI },
                "position": { "line": line, "character": character },
            }))
            .finish(),
    )
    .await
    .expect("definition must respond");
    assert!(definition.is_null(), "{definition}");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn workspace_symbol_finds_scanned_files_without_opening_them() {
    let _env = JdkEnv::none();
    // Fixture project on disk; Greet.java is never opened in the editor.
    let root = std::env::temp_dir().join(format!(
        "java-lsp-harness-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let greet_path = root.join("Greet.java");
    std::fs::write(&greet_path, GREET_FIXTURE).unwrap();

    let (mut service, mut socket) = LspService::new(JavaLanguageServer::new);
    // Diagnostics publications go through a capacity-1 channel; drain it so
    // handlers never block on an unread client socket.
    tokio::spawn(async move {
        use futures::StreamExt;
        while socket.next().await.is_some() {}
    });
    let root_uri = Url::from_file_path(&root).unwrap();
    respond(
        &mut service,
        Request::build("initialize")
            .id(Id::Number(1))
            .params(json!({ "capabilities": {}, "rootUri": root_uri.as_str() }))
            .finish(),
    )
    .await
    .expect("initialize must respond");
    respond(
        &mut service,
        Request::build("initialized").params(json!({})).finish(),
    )
    .await;

    let engine = service.inner().engine();
    wait_for_index(&engine, |entries, ready| {
        ready
            && entries
                .iter()
                .any(|e| e.name == "getName" && e.kind == IndexKind::Method)
    })
    .await;

    let greet_uri = Url::from_file_path(&greet_path).unwrap();
    let symbols = respond(
        &mut service,
        Request::build("workspace/symbol")
            .id(Id::Number(2))
            .params(json!({ "query": "Gre" }))
            .finish(),
    )
    .await
    .expect("workspace/symbol must respond");
    let matches = symbols.as_array().expect("an array of symbols");
    assert_eq!(matches.len(), 1, "{symbols}");
    assert_eq!(matches[0]["name"], "Greet");
    assert_eq!(matches[0]["kind"], 5, "SymbolKind::CLASS");
    assert_eq!(matches[0]["location"]["uri"], greet_uri.as_str());
    // The declaration's selection range, not its full range.
    assert_eq!(
        matches[0]["location"]["range"]["start"],
        json!({ "line": 2, "character": 13 })
    );
    assert!(matches[0]["containerName"].is_null());

    // Members carry their container; the scanned file was never opened.
    let symbols = respond(
        &mut service,
        Request::build("workspace/symbol")
            .id(Id::Number(3))
            .params(json!({ "query": "getNa" }))
            .finish(),
    )
    .await
    .expect("workspace/symbol must respond");
    let matches = symbols.as_array().expect("an array of symbols");
    assert_eq!(matches.len(), 1, "{symbols}");
    assert_eq!(matches[0]["name"], "getName");
    assert_eq!(matches[0]["kind"], 6, "SymbolKind::METHOD");
    assert_eq!(matches[0]["containerName"], "Greet");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn shutdown_and_exit_complete_the_lifecycle() {
    let mut service = service();
    initialize(&mut service).await;

    let shutdown = respond(
        &mut service,
        Request::build("shutdown").id(Id::Number(2)).finish(),
    )
    .await
    .expect("shutdown must respond");
    assert!(shutdown.is_null(), "shutdown result must be null");

    respond(&mut service, Request::build("exit").finish()).await;

    // After exit the service stops polling: readiness fails with ExitedError.
    let after_exit = service.ready().await;
    assert!(
        after_exit.is_err(),
        "the service must stop being ready after exit, got {after_exit:?}"
    );
}

/// A workspace fixture for the progress tests: one file, no Maven, no JDK.
fn progress_fixture(name: &str) -> std::path::PathBuf {
    let root = temp_dir(name);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("Greet.java"),
        "package demo;\n\npublic class Greet {\n}\n",
    )
    .unwrap();
    root
}

#[tokio::test]
async fn warmup_progress_is_reported_when_the_client_supports_it() {
    use futures::StreamExt;
    let _env = java_lsp::jdk::env_lock();
    std::env::set_var("JAVA_LSP_JDK", "/definitely/not/a/jdk");
    let _offline = EnvVar::set("JAVA_LSP_OFFLINE", "1");

    let root = progress_fixture("progress");
    let (mut service, mut socket) = LspService::new(JavaLanguageServer::new);
    let root_uri = Url::from_file_path(&root).unwrap();
    respond(
        &mut service,
        Request::build("initialize")
            .id(Id::Number(1))
            .params(json!({
                "capabilities": { "window": { "workDoneProgress": true } },
                "rootUri": root_uri.as_str(),
            }))
            .finish(),
    )
    .await
    .expect("initialize must respond");
    respond(
        &mut service,
        Request::build("initialized").params(json!({})).finish(),
    )
    .await;

    // Collect the server's messages until the progress item has begun and ended.
    let mut created = false;
    let mut begin: Option<Value> = None;
    let mut reports = 0usize;
    let mut ended = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline && !(created && begin.is_some() && ended) {
        let next = tokio::time::timeout(std::time::Duration::from_millis(500), socket.next()).await;
        let Ok(Some(request)) = next else { continue };
        match request.method() {
            "window/workDoneProgress/create" => {
                assert_eq!(
                    request.params().map(|params| params["token"].clone()),
                    Some(json!("java-lsp/warm-up")),
                    "expected the well-known progress token"
                );
                created = true;
            }
            "$/progress" => {
                let params = request.params().cloned().unwrap_or(Value::Null);
                match params["value"]["kind"].as_str() {
                    Some("begin") => begin = Some(params["value"].clone()),
                    Some("report") => reports += 1,
                    Some("end") => ended = true,
                    other => panic!("unexpected progress kind: {other:?} ({params})"),
                }
            }
            other => panic!("unexpected server message: {other}"),
        }
    }

    assert!(created, "expected a window/workDoneProgress/create request");
    let begin = begin.expect("expected a $/progress begin");
    assert_eq!(begin["title"], "java-lsp");
    assert!(
        begin["message"]
            .as_str()
            .is_some_and(|text| !text.is_empty()),
        "the begin must name the phase, got {begin}"
    );
    assert!(reports >= 1, "expected at least one report update");
    assert!(ended, "expected a $/progress end");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn progress_is_not_sent_without_the_client_capability() {
    use futures::StreamExt;
    let _env = java_lsp::jdk::env_lock();
    std::env::set_var("JAVA_LSP_JDK", "/definitely/not/a/jdk");
    let _offline = EnvVar::set("JAVA_LSP_OFFLINE", "1");

    let root = progress_fixture("no-progress");
    let (mut service, mut socket) = LspService::new(JavaLanguageServer::new);
    let root_uri = Url::from_file_path(&root).unwrap();
    // No `window.workDoneProgress`: progress must be suppressed entirely.
    respond(
        &mut service,
        Request::build("initialize")
            .id(Id::Number(1))
            .params(json!({ "capabilities": {}, "rootUri": root_uri.as_str() }))
            .finish(),
    )
    .await
    .expect("initialize must respond");
    respond(
        &mut service,
        Request::build("initialized").params(json!({})).finish(),
    )
    .await;

    // Wait for the warm-up to actually finish, so the check is not vacuous.
    let engine = service.inner().engine();
    wait_for_index(&engine, |_, ready| ready).await;

    // Then drain for a bounded window: nothing progress-like may appear.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    while std::time::Instant::now() < deadline {
        let next = tokio::time::timeout(std::time::Duration::from_millis(100), socket.next()).await;
        if let Ok(Some(request)) = next {
            assert!(
                !matches!(
                    request.method(),
                    "$/progress" | "window/workDoneProgress/create"
                ),
                "progress must not be sent without the capability: {request:?}"
            );
        }
    }

    let _ = std::fs::remove_dir_all(&root);
}
