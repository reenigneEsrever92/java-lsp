//! Smoke test: drives the real `java-lsp` binary over stdio with raw LSP
//! JSON-RPC — the exact transport a real editor uses, no tower-lsp helpers.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

const HELLO_URI: &str = "file:///tmp/Hello.java";

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Server {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_java-lsp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
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

    /// Reads the next message; returns notifications as-is.
    fn read_message(&mut self) -> Value {
        loop {
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
            return serde_json::from_slice(&body).unwrap();
        }
    }

    /// Reads messages, skipping server notifications, until the response with
    /// this id arrives.
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

    /// For methods with no params in the LSP spec: sending a params field at
    /// all (even `{}`) makes tower-lsp reject the message with -32602.
    fn request_no_params(&mut self, id: i64, method: &str) -> Value {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method}));
        self.read_response(id)
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn notify_no_params(&mut self, method: &str) {
        self.send(&json!({"jsonrpc": "2.0", "method": method}));
    }

    /// Exits like a real client: sends `exit`, then closes stdin so the
    /// server's blocking stdin reader sees EOF, then reaps the process.
    fn exit_and_wait(&mut self) -> std::process::ExitStatus {
        self.notify_no_params("exit");
        drop(self.stdin.take());
        self.child.wait().expect("wait for java-lsp to exit")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn binary_completes_the_lifecycle_over_stdio() {
    let mut server = Server::start();

    // Handshake: incremental sync, hover, definition, completions.
    let init = server.request(1, "initialize", json!({ "capabilities": {} }));
    let caps = &init["result"]["capabilities"];
    assert_eq!(caps["textDocumentSync"], 2, "2 = incremental, got {caps}");
    assert_eq!(caps["hoverProvider"], true, "{caps}");
    assert_eq!(caps["definitionProvider"], true, "{caps}");
    assert!(caps["completionProvider"]["triggerCharacters"]
        .as_array()
        .unwrap()
        .contains(&json!(".")));

    // didOpen publishes (empty) diagnostics from the engine.
    server.notify(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": HELLO_URI,
                "languageId": "java",
                "version": 1,
                "text": "class Hello {}\n",
            }
        }),
    );
    let diags = server.read_message();
    assert_eq!(
        diags["method"], "textDocument/publishDiagnostics",
        "{diags}"
    );
    assert_eq!(diags["params"]["diagnostics"], json!([]), "{diags}");

    // Incremental didChange: "class Hello {}" -> "class World {}", version 2.
    server.notify(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": HELLO_URI, "version": 2 },
            "contentChanges": [{
                "range": {
                    "start": { "line": 0, "character": 6 },
                    "end": { "line": 0, "character": 11 },
                },
                "text": "World",
            }],
        }),
    );
    let diags = server.read_message();
    assert_eq!(
        diags["method"], "textDocument/publishDiagnostics",
        "{diags}"
    );
    assert_eq!(diags["params"]["version"], 2, "{diags}");

    // Queries respond from the stub engine: null result.
    let hover = server.request(
        2,
        "textDocument/hover",
        json!({
            "textDocument": { "uri": HELLO_URI },
            "position": { "line": 0, "character": 2 },
        }),
    );
    assert!(hover["result"].is_null(), "{hover}");

    // Graceful shutdown: null result, no error, clean exit code.
    let shutdown = server.request_no_params(3, "shutdown");
    assert!(shutdown["result"].is_null(), "{shutdown}");
    assert!(shutdown.get("error").is_none(), "{shutdown}");

    let status = server.exit_and_wait();
    assert!(status.success(), "binary must exit cleanly, got {status}");
}
