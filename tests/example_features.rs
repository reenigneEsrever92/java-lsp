//! End-to-end feature check over stdio against the `example/` Maven workspace:
//! `greeting-lib` (`Greeter`, `Data`, `DataType`, `SumType`) and `greeting-app`
//! (`Main`) with a sibling-module dependency. Drives the real binary with raw
//! LSP JSON-RPC — the transport an editor uses — so a change to the index can be
//! checked against real feature answers, not just unit tests.
//!
//! JDK indexing is disabled (`JAVA_LSP_JDK` points nowhere) for speed and
//! determinism, and all assertions are over workspace sources, so the test does
//! not depend on the local Maven repository.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Server {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_java-lsp"))
            .env("JAVA_LSP_OFFLINE", "1")
            .env("JAVA_LSP_JDK", "/definitely/not/a/jdk")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn java-lsp");
        let stdin = Some(child.stdin.take().expect("child stdin"));
        let stdout = BufReader::new(child.stdout.take().expect("child stdout"));
        Self {
            child,
            stdin,
            stdout,
        }
    }

    fn send(&mut self, msg: &Value) {
        let body = serde_json::to_vec(msg).unwrap();
        let stdin = self.stdin.as_mut().expect("child stdin");
        write!(stdin, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
        stdin.write_all(&body).unwrap();
        stdin.flush().unwrap();
    }

    fn read_message(&mut self) -> Value {
        let mut content_length: Option<usize> = None;
        loop {
            let mut line = String::new();
            self.stdout.read_line(&mut line).unwrap();
            let line = line.trim();
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length:") {
                content_length = Some(value.trim().parse().unwrap());
            }
        }
        let len = content_length.expect("missing Content-Length header");
        let mut body = vec![0u8; len];
        self.stdout.read_exact(&mut body).unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn read_response(&mut self, id: i64) -> Value {
        loop {
            let msg = self.read_message();
            if msg.get("method").is_none() && msg["id"] == id {
                return msg;
            }
        }
    }

    fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        self.read_response(id)
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn example_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("example")
}

fn file_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// The LSP position (0-based line, UTF-16 column) of the byte offset of `needle`
/// (plus `into`) in `text`.
fn position_at(text: &str, needle: &str, into: usize) -> (u32, u32) {
    let byte = text.find(needle).expect("needle") + into;
    let before = &text[..byte];
    let line = before.matches('\n').count() as u32;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let character = before[line_start..]
        .chars()
        .map(|c| c.len_utf16() as u32)
        .sum();
    (line, character)
}

#[test]
fn example_features_resolve_over_stdio() {
    let root = example_dir();
    let main_path = root.join("greeting-app/src/main/java/com/example/app/Main.java");
    let greeter_path = root.join("greeting-lib/src/main/java/com/example/greeting/Greeter.java");
    let main = std::fs::read_to_string(&main_path).expect("Main.java");
    let greeter = std::fs::read_to_string(&greeter_path).expect("Greeter.java");
    let main_uri = file_uri(&main_path);
    let greeter_uri = file_uri(&greeter_path);

    let mut server = Server::start();
    server.request(
        1,
        "initialize",
        json!({ "capabilities": {}, "rootUri": file_uri(&root) }),
    );
    server.notify("initialized", json!({}));
    for (uri, text) in [(&main_uri, &main), (&greeter_uri, &greeter)] {
        server.notify(
            "textDocument/didOpen",
            json!({ "textDocument": { "uri": uri, "languageId": "java", "version": 1, "text": text } }),
        );
    }

    // The workspace source scan is asynchronous: poll `definition` on the
    // `Greeter` declaration until the index answers.
    let (line, character) = position_at(&main, "Greeter greeter", 0);
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut definition = Value::Null;
    let mut id = 10;
    while Instant::now() < deadline {
        definition = server.request(
            id,
            "textDocument/definition",
            json!({
                "textDocument": { "uri": main_uri },
                "position": { "line": line, "character": character + 1 },
            }),
        );
        if !definition["result"].is_null() {
            break;
        }
        id += 1;
        std::thread::sleep(Duration::from_millis(200));
    }
    let target = definition["result"]["uri"]
        .as_str()
        .or_else(|| definition["result"][0]["uri"].as_str())
        .unwrap_or_else(|| panic!("no definition for Greeter: {definition}"));
    assert!(target.ends_with("Greeter.java"), "{definition}");

    // documentSymbol names Main.
    let symbols = server.request(
        id + 1,
        "textDocument/documentSymbol",
        json!({ "textDocument": { "uri": main_uri } }),
    );
    let names = symbols["result"].to_string();
    assert!(names.contains("\"Main\""), "{symbols}");

    // hover on Greeter names the type.
    let (hline, hchar) = position_at(&main, "new Greeter", "new ".len() + 1);
    let hover = server.request(
        id + 2,
        "textDocument/hover",
        json!({
            "textDocument": { "uri": main_uri },
            "position": { "line": hline, "character": hchar },
        }),
    );
    assert!(
        hover["result"]["contents"].to_string().contains("Greeter"),
        "{hover}"
    );

    // completion after `greeter.` offers the type's members.
    let (cline, cchar) = position_at(&main, "greeter.getName", "greeter.".len());
    let completion = server.request(
        id + 3,
        "textDocument/completion",
        json!({
            "textDocument": { "uri": main_uri },
            "position": { "line": cline, "character": cchar },
        }),
    );
    let items = completion["result"].to_string();
    assert!(items.contains("getName"), "{completion}");
    assert!(items.contains("greet"), "{completion}");

    // references on the Greeter declaration include Main.java.
    let (rline, rchar) = position_at(&greeter, "class Greeter", "class ".len());
    let references = server.request(
        id + 4,
        "textDocument/references",
        json!({
            "textDocument": { "uri": greeter_uri },
            "position": { "line": rline, "character": rchar },
            "context": { "includeDeclaration": true },
        }),
    );
    assert!(
        references["result"].to_string().contains("Main.java"),
        "{references}"
    );

    // workspace/symbol finds the workspace type.
    let symbols = server.request(id + 5, "workspace/symbol", json!({ "query": "Greeter" }));
    assert!(
        symbols["result"].to_string().contains("Greeter"),
        "{symbols}"
    );
}
