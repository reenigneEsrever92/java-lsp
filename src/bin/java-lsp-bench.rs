//! Performance benchmark harness for `java-lsp`.
//!
//! Generates a fixture Java workspace of configurable size in a temp dir,
//! spawns the **real** `java-lsp` binary and drives it over raw stdio
//! JSON-RPC (the exact transport an editor uses), then measures:
//!
//! - first-response time per feature after `didOpen` (and from process start),
//! - request latency (hover RTT) during index warm-up, detected via the
//!   server's own "workspace index warm-up complete" log line on stderr,
//! - peak memory via `/proc/<pid>/status` `VmHWM` (Linux; `n/a` elsewhere),
//!
//! and prints one report (text table, or JSON with `--json`). std only —
//! one driver thread plus one stderr-reader thread, no async runtime.
//!
//! Usage:
//!
//! ```text
//! cargo run --release --bin java-lsp-bench -- --files 500 --methods-per-class 10
//! ```

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Interval between warm-up latency probes.
const TICK_MS: u64 = 25;

const FEATURES: [&str; 5] = [
    "textDocument/documentSymbol",
    "textDocument/foldingRange",
    "textDocument/semanticTokens/full",
    "textDocument/completion",
    "textDocument/hover",
];

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

struct Args {
    files: usize,
    methods_per_class: usize,
    fields_per_class: usize,
    server: Option<PathBuf>,
    json: bool,
    keep: bool,
    maven: bool,
}

fn usage() -> String {
    "usage: java-lsp-bench [--files N] [--methods-per-class M] [--fields-per-class F] \
     [--server PATH] [--json] [--keep] [--maven]
  --files N              fixture .java files to generate (default 200)
  --methods-per-class M  multi-line methods per fixture class (default 5)
  --fields-per-class F   fields per fixture class (default 3)
  --maven                lay the fixture out as a single-module Maven project
                         (pom.xml + src/main/java) instead of a flat root
  --server PATH          java-lsp binary to drive (default: $JAVA_LSP_BIN,
                         then target/{release,debug}/java-lsp, else an auto
                         `cargo build --bin java-lsp` when run under cargo)
  --json                 print the report as one JSON object
  --keep                 keep the generated fixture directory"
        .to_string()
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut args = Args {
        files: 200,
        methods_per_class: 5,
        fields_per_class: 3,
        server: None,
        json: false,
        keep: false,
        maven: false,
    };
    let mut i = 0;
    while i < argv.len() {
        let (flag, inline) = match argv[i].split_once('=') {
            Some((f, v)) => (f.to_string(), Some(v.to_string())),
            None => (argv[i].clone(), None),
        };
        let mut next_value = |name: &str| -> Result<String, String> {
            if let Some(v) = &inline {
                return Ok(v.clone());
            }
            i += 1;
            argv.get(i)
                .cloned()
                .ok_or_else(|| format!("{name} needs a value\n{}", usage()))
        };
        match flag.as_str() {
            "--files" => {
                args.files = next_value("--files")?
                    .parse()
                    .map_err(|_| "--files needs a number".to_string())?
            }
            "--methods-per-class" => {
                args.methods_per_class = next_value("--methods-per-class")?
                    .parse()
                    .map_err(|_| "--methods-per-class needs a number".to_string())?
            }
            "--fields-per-class" => {
                args.fields_per_class = next_value("--fields-per-class")?
                    .parse()
                    .map_err(|_| "--fields-per-class needs a number".to_string())?
            }
            "--server" => args.server = Some(PathBuf::from(next_value("--server")?)),
            "--json" => args.json = true,
            "--keep" => args.keep = true,
            "--maven" => args.maven = true,
            "--help" | "-h" => return Err(usage()),
            other => return Err(format!("unknown flag `{other}`\n{}", usage())),
        }
        i += 1;
    }
    if args.files == 0 {
        return Err("--files must be at least 1".to_string());
    }
    Ok(args)
}

// ---------------------------------------------------------------------------
// Fixture generator
// ---------------------------------------------------------------------------

struct Fixture {
    root: PathBuf,
    /// Class name of fixture file `i` (`BenchClass00042`).
    class_names: Vec<String>,
    /// `file://` URI of fixture file `i`.
    uris: Vec<String>,
    /// Line/character of the class name inside fixture file `i`, a hover
    /// target with real material to answer from.
    hover_positions: Vec<(u32, u32)>,
}

fn class_name(i: usize) -> String {
    format!("BenchClass{i:05}")
}

/// One fixture file: a unique class with F fields and M multi-line methods,
/// so document symbols, folding ranges, semantic tokens, and index entries
/// all have material.
fn fixture_source(class: &str, methods: usize, fields: usize) -> String {
    let methods = methods.max(1);
    let fields = fields.max(1);
    let mut out = String::new();
    out.push_str("package bench;\n\n");
    out.push_str(&format!("public class {class} {{\n"));
    for f in 0..fields {
        out.push_str(&format!("    private int field{f:02} = {f};\n"));
    }
    out.push('\n');
    for m in 0..methods {
        out.push_str(&format!("    public int method{m:02}(int a) {{\n"));
        out.push_str(&format!(
            "        int sum = a + this.field{:02};\n",
            m % fields
        ));
        out.push_str("        for (int i = 0; i < 10; i++) {\n");
        out.push_str(&format!(
            "            sum += i + method{:02}(i);\n",
            (m + 1) % methods
        ));
        out.push_str("        }\n");
        out.push_str("        return sum;\n");
        out.push_str("    }\n\n");
    }
    out.push_str("}\n");
    out
}

fn generate_fixture(
    files: usize,
    methods: usize,
    fields: usize,
    maven: bool,
) -> std::io::Result<Fixture> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let root =
        std::env::temp_dir().join(format!("java-lsp-bench-{}-{}", std::process::id(), millis));
    fs::create_dir_all(&root)?;

    // Maven mode: a single-module project whose source root is the standard
    // src/main/java; the scanner must then index exactly that root.
    let base = if maven {
        fs::write(
            root.join("pom.xml"),
            "<project>\
             <groupId>bench</groupId>\
             <artifactId>fixture</artifactId>\
             <version>1.0</version>\
             </project>",
        )?;
        let dir = root.join("src").join("main").join("java");
        fs::create_dir_all(&dir)?;
        dir
    } else {
        root.clone()
    };

    let mut class_names = Vec::with_capacity(files);
    let mut uris = Vec::with_capacity(files);
    let mut hover_positions = Vec::with_capacity(files);
    for i in 0..files {
        let class = class_name(i);
        let source = fixture_source(&class, methods, fields);
        fs::write(base.join(format!("{class}.java")), &source)?;
        class_names.push(class.clone());
        uris.push(file_uri(&base.join(format!("{class}.java"))));
        let line = source.lines().position(|l| l.contains(&class)).unwrap_or(0) as u32;
        let character = source
            .lines()
            .nth(line as usize)
            .and_then(|l| l.find(&class))
            .unwrap_or(0) as u32
            + 3;
        hover_positions.push((line, character));
    }
    Ok(Fixture {
        root,
        class_names,
        uris,
        hover_positions,
    })
}

/// Minimal `file://` URI encoding: percent-encode anything outside the URL
/// unreserved/reserved set so odd temp paths survive `Url::parse` server-side.
fn file_uri(path: &Path) -> String {
    let text = path.to_string_lossy();
    let mut uri = String::from("file://");
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || "-._~!$&'()*+,;=:@/".contains(ch) {
            uri.push(ch);
        } else {
            for byte in ch.to_string().as_bytes() {
                uri.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    uri
}

// ---------------------------------------------------------------------------
// Server binary resolution
// ---------------------------------------------------------------------------

fn resolve_server(args: &Args) -> Result<PathBuf, String> {
    if let Some(path) = &args.server {
        if path.is_file() {
            return Ok(path.clone());
        }
        return Err(format!("--server {}: not a file", path.display()));
    }
    if let Some(path) = std::env::var_os("JAVA_LSP_BIN") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!("JAVA_LSP_BIN={}: not a file", path.display()));
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let profiles = ["release", "debug"];
    for profile in profiles {
        let path = manifest.join("target").join(profile).join("java-lsp");
        if path.is_file() {
            return Ok(path);
        }
    }
    // Nothing built yet: under cargo, build it so the single-command shape
    // (`cargo run --release --bin java-lsp-bench`) stays true.
    if std::env::var_os("CARGO").is_some() {
        let release = !cfg!(debug_assertions);
        eprintln!(
            "no java-lsp binary found; running `cargo build{} --bin java-lsp`",
            if release { " --release" } else { "" }
        );
        let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args(if release {
                vec!["build", "--release", "--bin", "java-lsp"]
            } else {
                vec!["build", "--bin", "java-lsp"]
            })
            .current_dir(&manifest)
            .status()
            .map_err(|e| format!("failed to run cargo build: {e}"))?;
        if !status.success() {
            return Err("cargo build --bin java-lsp failed".to_string());
        }
        for profile in profiles {
            let path = manifest.join("target").join(profile).join("java-lsp");
            if path.is_file() {
                return Ok(path);
            }
        }
        return Err("cargo build succeeded but no java-lsp binary was found".to_string());
    }
    Err(format!(
        "no java-lsp binary found; build one first (`cargo build --release --bin java-lsp`) \
         or point --server / JAVA_LSP_BIN at it\n{}",
        usage()
    ))
}

// ---------------------------------------------------------------------------
// Warm-up readiness watcher
// ---------------------------------------------------------------------------

/// The server's own scan stats from the "workspace index warm-up complete"
/// log line, plus the Instant the line was observed.
#[derive(Clone, Copy)]
struct ScanStats {
    files: Option<u64>,
    elapsed_ms: Option<u64>,
    at: Instant,
}

type Readiness = Arc<Mutex<Option<ScanStats>>>;

/// Drains the server's stderr forever (so the server never blocks writing),
/// capturing the first readiness line and its `files=`/`elapsed_ms=` fields.
fn watch_stderr(stderr: impl Read + Send + 'static, readiness: Readiness) {
    let reader = BufReader::new(stderr);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.contains("workspace index warm-up complete") {
            let mut slot = readiness.lock().expect("readiness mutex");
            if slot.is_none() {
                *slot = Some(ScanStats {
                    files: log_field(&line, "files="),
                    elapsed_ms: log_field(&line, "elapsed_ms="),
                    at: Instant::now(),
                });
            }
        }
    }
}

/// Parses a trailing integer field like `files=500` out of a tracing line.
fn log_field(line: &str, key: &str) -> Option<u64> {
    let start = line.find(key)? + key.len();
    let digits: String = line[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

// ---------------------------------------------------------------------------
// stdio JSON-RPC client (framing pattern from tests/stdio_smoke.rs)
// ---------------------------------------------------------------------------

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Server {
    fn spawn(binary: &Path) -> Self {
        let mut child = Command::new(binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("RUST_LOG", "java_lsp=info")
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn {}: {e}", binary.display()));
        let stdin = Some(child.stdin.take().expect("child stdin"));
        let stdout = BufReader::new(child.stdout.take().expect("child stdout"));
        Self {
            child,
            stdin,
            stdout,
        }
    }

    fn send(&mut self, msg: &Value) {
        let body = serde_json::to_vec(msg).expect("serialize message");
        let stdin = self.stdin.as_mut().expect("child stdin");
        write!(stdin, "Content-Length: {}\r\n\r\n", body.len()).expect("write header");
        stdin.write_all(&body).expect("write body");
        stdin.flush().expect("flush");
    }

    /// Reads the next message; returns notifications as-is.
    fn read_message(&mut self) -> Value {
        loop {
            let mut content_length: Option<usize> = None;
            loop {
                let mut line = String::new();
                let n = self
                    .stdout
                    .read_line(&mut line)
                    .expect("read from server stdout");
                if n == 0 {
                    panic!("server closed stdout unexpectedly");
                }
                let line = line.trim_end();
                if line.is_empty() {
                    break;
                }
                if let Some(value) = line.strip_prefix("Content-Length:") {
                    content_length = Some(value.trim().parse().expect("Content-Length value"));
                }
            }
            let len = content_length.expect("missing Content-Length header");
            let mut body = vec![0u8; len];
            self.stdout.read_exact(&mut body).expect("read body");
            return serde_json::from_slice(&body).expect("parse message");
        }
    }

    /// Reads messages, skipping server notifications, until the response with
    /// this id arrives.
    fn read_response(&mut self, id: u64) -> Value {
        loop {
            let msg = self.read_message();
            if msg.get("method").is_none() && msg.get("id") == Some(&json!(id)) {
                return msg;
            }
        }
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        self.read_response(id)
    }

    /// For methods with no params in the LSP spec: sending a params field at
    /// all (even `{}`) makes tower-lsp reject the message with -32602.
    fn request_no_params(&mut self, id: u64, method: &str) -> Value {
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

fn assert_ok(response: &Value, what: &str) {
    assert!(
        response.get("error").is_none() && response.get("result").is_some(),
        "{what} failed: {response}"
    );
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

struct Mem {
    peak_rss_kb: Option<u64>,
    rss_kb: Option<u64>,
}

/// Peak (VmHWM) and current (VmRSS) resident set of the server child, from
/// the kernel-maintained `/proc/<pid>/status`. Linux-specific.
fn read_memory(pid: u32) -> Option<Mem> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    Some(Mem {
        peak_rss_kb: status_field(&status, "VmHWM"),
        rss_kb: status_field(&status, "VmRSS"),
    })
}

fn status_field(status: &str, key: &str) -> Option<u64> {
    status.lines().find_map(|line| {
        let rest = line.strip_prefix(key)?.trim().strip_prefix(':')?;
        rest.trim().split_whitespace().next()?.parse().ok()
    })
}

// ---------------------------------------------------------------------------
// Measurement run
// ---------------------------------------------------------------------------

struct FeatureTiming {
    feature: &'static str,
    from_did_open_ms: f64,
    from_start_ms: f64,
}

struct Report {
    files: usize,
    methods_per_class: usize,
    fields_per_class: usize,
    fixture_lines: usize,
    features: Vec<FeatureTiming>,
    warmup_duration_ms: Option<f64>,
    scan_files: Option<u64>,
    scan_elapsed_ms: Option<u64>,
    samples: usize,
    sample_max_ms: Option<f64>,
    sample_mean_ms: Option<f64>,
    post_warmup_rtt_ms: Option<f64>,
    completion_probe_found: bool,
    completion_probe_ms: f64,
    peak_rss_kb: Option<u64>,
    final_rss_kb: Option<u64>,
    total_ms: f64,
}

fn run_bench(server: &Path, fixture: &Fixture, args: &Args) -> Report {
    let start = Instant::now();
    let mut server = Server::spawn(server);
    let pid = server.child.id();

    // The stderr watcher must be draining before the scan starts, or a fast
    // warm-up could fill the pipe; data written before we take the fd still
    // sits in the pipe buffer, but start early anyway.
    let readiness: Readiness = Arc::new(Mutex::new(None));
    {
        let readiness = Arc::clone(&readiness);
        let stderr = server.child.stderr.take().expect("child stderr");
        thread::spawn(move || watch_stderr(stderr, readiness));
    }

    let mut next_id: u64 = 1;
    let mut id = || {
        next_id += 1;
        next_id - 1
    };

    // Handshake; the fixture root as rootUri starts the background scan on
    // `initialized`.
    let init = server.request(
        id(),
        "initialize",
        json!({ "capabilities": {}, "rootUri": file_uri(&fixture.root) }),
    );
    assert_ok(&init, "initialize");
    let warm_start = Instant::now();
    server.notify("initialized", json!({}));

    // Open fixture file 0, drain its publishDiagnostics, then fire the five
    // feature requests back-to-back (an editor opening a file does exactly
    // this) and time each first response.
    let (open_uri, open_text, probe_prefix, probe_pos) = open_document(fixture, args.maven);
    let did_open_at = Instant::now();
    server.notify(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": open_uri,
                "languageId": "java",
                "version": 1,
                "text": open_text,
            }
        }),
    );
    loop {
        let msg = server.read_message();
        if msg["method"] == "textDocument/publishDiagnostics" {
            break;
        }
    }

    let hover_pos = fixture.hover_positions[0];
    let mut first_responses: Vec<Option<Instant>> = vec![None; FEATURES.len()];
    let mut pending: Vec<(u64, usize)> = Vec::new();
    for (i, feature) in FEATURES.iter().enumerate() {
        let req_id = id();
        let params = feature_params(feature, &open_uri, hover_pos, probe_pos);
        server.send(&json!({"jsonrpc": "2.0", "id": req_id, "method": feature, "params": params}));
        pending.push((req_id, i));
    }
    while !pending.is_empty() {
        let msg = server.read_message();
        if let Some(pos) = pending.iter().position(|(req_id, _)| {
            msg.get("method").is_none() && msg.get("id") == Some(&json!(req_id))
        }) {
            let (req_id, feature_idx) = pending.remove(pos);
            assert_ok(&msg, FEATURES[feature_idx]);
            first_responses[feature_idx] = Some(Instant::now());
            let _ = req_id;
        }
    }

    let features: Vec<FeatureTiming> = FEATURES
        .iter()
        .zip(&first_responses)
        .map(|(feature, at)| FeatureTiming {
            feature,
            from_did_open_ms: ms(did_open_at, *at).expect("first response recorded"),
            from_start_ms: ms(start, *at).expect("first response recorded"),
        })
        .collect();

    // Hover RTT on a fixed tick until the readiness line arrives; every
    // response must succeed (AC3: features respond while indexing runs).
    let mut samples: Vec<f64> = Vec::new();
    loop {
        if readiness.lock().expect("readiness mutex").is_some() {
            break;
        }
        let req_id = id();
        let sent = Instant::now();
        let response = server.request(
            req_id,
            "textDocument/hover",
            json!({
                "textDocument": { "uri": open_uri },
                "position": { "line": hover_pos.0, "character": hover_pos.1 },
            }),
        );
        assert_ok(&response, "hover during warm-up");
        samples.push(ms(sent, Some(Instant::now())).expect("sample recorded"));
        if readiness.lock().expect("readiness mutex").is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(TICK_MS));
    }

    let scan = readiness.lock().expect("readiness mutex").unwrap();
    let warmup_duration_ms = ms(warm_start, Some(scan.at));

    // Sanity check that index data actually reaches a feature: a completion
    // whose prefix matches a fixture-only class must offer it.
    let probe_id = id();
    let probe_sent = Instant::now();
    let probe = server.request(
        probe_id,
        "textDocument/completion",
        json!({
            "textDocument": { "uri": open_uri },
            "position": { "line": probe_pos.0, "character": probe_pos.1 },
        }),
    );
    assert_ok(&probe, "post-warm-up completion probe");
    let probe_ms = ms(probe_sent, Some(Instant::now())).expect("probe recorded");
    let probe_found = completion_offers(&probe, &probe_prefix);

    // Post-warm-up hover RTT: the steady-state number to compare against.
    let post_id = id();
    let post_sent = Instant::now();
    let post = server.request(
        post_id,
        "textDocument/hover",
        json!({
            "textDocument": { "uri": open_uri },
            "position": { "line": hover_pos.0, "character": hover_pos.1 },
        }),
    );
    assert_ok(&post, "post-warm-up hover");
    let post_warmup_rtt_ms = ms(post_sent, Some(Instant::now()));
    let _ = post_id;

    let memory = read_memory(pid);

    // Graceful shutdown, same rules as tests/stdio_smoke.rs.
    let shutdown = server.request_no_params(id(), "shutdown");
    assert_ok(&shutdown, "shutdown");
    let status = server.exit_and_wait();
    assert!(status.success(), "java-lsp exited with {status}");

    // VmHWM is monotonic; one last read in case the peak came late.
    let memory = match (memory, read_memory(pid)) {
        (Some(a), Some(b)) => Some(Mem {
            peak_rss_kb: a.peak_rss_kb.max(b.peak_rss_kb),
            rss_kb: b.rss_kb,
        }),
        (a, b) => a.or(b),
    };

    Report {
        files: args.files,
        methods_per_class: args.methods_per_class,
        fields_per_class: args.fields_per_class,
        fixture_lines: (open_text.lines().count() - 1) * args.files,
        features,
        warmup_duration_ms,
        scan_files: scan.files,
        scan_elapsed_ms: scan.elapsed_ms,
        samples: samples.len(),
        sample_max_ms: samples.iter().cloned().reduce(f64::max),
        sample_mean_ms: if samples.is_empty() {
            None
        } else {
            Some(samples.iter().sum::<f64>() / samples.len() as f64)
        },
        post_warmup_rtt_ms,
        completion_probe_found: probe_found,
        completion_probe_ms: probe_ms,
        peak_rss_kb: memory.as_ref().and_then(|m| m.peak_rss_kb),
        final_rss_kb: memory.as_ref().and_then(|m| m.rss_kb),
        total_ms: start.elapsed().as_secs_f64() * 1000.0,
    }
}

/// The directory holding fixture file `i` (root, or the source root in
/// Maven mode — mirrored from `generate_fixture`).
fn fixture_base(fixture: &Fixture, maven: bool) -> PathBuf {
    if maven {
        fixture.root.join("src").join("main").join("java")
    } else {
        fixture.root.clone()
    }
}

/// The document the bench opens: fixture file 0 plus a trailing comment
/// holding the completion-probe prefix (a fixture-only class name, offered
/// only once the scan has indexed that other file). Returns the URI, the
/// text, the probe class, and the position just past the probe prefix.
fn open_document(fixture: &Fixture, maven: bool) -> (String, String, String, (u32, u32)) {
    let probe_class = fixture.class_names[fixture.class_names.len() - 1].clone();
    let mut text = {
        let path = fixture_base(fixture, maven).join(format!("{}.java", fixture.class_names[0]));
        fs::read_to_string(&path).expect("read fixture file 0")
    };
    text.push_str(&format!("// {probe_class}"));
    let pos = (
        text.lines().count() as u32 - 1,
        text.lines().last().map_or(0, |l| l.chars().count() as u32),
    );
    text.push('\n');
    (fixture.uris[0].clone(), text, probe_class, pos)
}

fn feature_params(feature: &str, uri: &str, hover_pos: (u32, u32), probe_pos: (u32, u32)) -> Value {
    match feature {
        "textDocument/completion" => json!({
            "textDocument": { "uri": uri },
            "position": { "line": probe_pos.0, "character": probe_pos.1 },
        }),
        "textDocument/hover" => json!({
            "textDocument": { "uri": uri },
            "position": { "line": hover_pos.0, "character": hover_pos.1 },
        }),
        _ => json!({ "textDocument": { "uri": uri } }),
    }
}

/// True if the completion response offers an item for the probe class.
fn completion_offers(response: &Value, class: &str) -> bool {
    let items = match &response["result"] {
        Value::Array(items) => items.clone(),
        result => result["items"].as_array().cloned().unwrap_or_default(),
    };
    items.iter().any(|item| {
        item["label"] == *class
            || item["insertText"] == *class
            || item["filterText"] == *class
            || item["sortText"] == *class
    })
}

fn ms(from: Instant, to: Option<Instant>) -> Option<f64> {
    to.map(|to| to.saturating_duration_since(from).as_secs_f64() * 1000.0)
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

fn print_report(report: &Report, server: &Path) {
    println!("java-lsp benchmark report");
    println!("=========================");
    println!(
        "fixture: {} files x {} methods x {} fields ({} lines total)",
        report.files, report.methods_per_class, report.fields_per_class, report.fixture_lines
    );
    println!("server:  {}", server.display());
    println!();
    println!("first response per feature (open to responsive):");
    for f in &report.features {
        println!(
            "  {:<36} from didOpen {:>8.1} ms   from start {:>8.1} ms",
            f.feature, f.from_did_open_ms, f.from_start_ms
        );
    }
    println!();
    println!("index warm-up:");
    match report.warmup_duration_ms {
        Some(d) => println!("  duration (readiness line): {d:.1} ms"),
        None => println!("  duration (readiness line): n/a"),
    }
    println!(
        "  scan (server-reported):    files={} elapsed_ms={}",
        report
            .scan_files
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".into()),
        report
            .scan_elapsed_ms
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".into())
    );
    println!(
        "  hover RTT during warm-up:  {} samples ({} ms tick), max {}, mean {}",
        report.samples,
        TICK_MS,
        report
            .sample_max_ms
            .map(|v| format!("{v:.1} ms"))
            .unwrap_or_else(|| "n/a".into()),
        report
            .sample_mean_ms
            .map(|v| format!("{v:.1} ms"))
            .unwrap_or_else(|| "n/a".into())
    );
    println!(
        "  post-warm-up hover RTT:    {}",
        report
            .post_warmup_rtt_ms
            .map(|v| format!("{v:.1} ms"))
            .unwrap_or_else(|| "n/a".into())
    );
    println!(
        "  completion probe ({}):  {:.1} ms",
        if report.completion_probe_found {
            "fixture class offered"
        } else {
            "FIXTURE CLASS MISSING"
        },
        report.completion_probe_ms
    );
    println!();
    println!("memory:");
    println!(
        "  peak RSS (VmHWM):  {}",
        report
            .peak_rss_kb
            .map(|v| format!("{v} kB"))
            .unwrap_or_else(|| "n/a".into())
    );
    println!(
        "  final RSS (VmRSS): {}",
        report
            .final_rss_kb
            .map(|v| format!("{v} kB"))
            .unwrap_or_else(|| "n/a".into())
    );
    println!();
    println!("total wall clock: {:.1} s", report.total_ms / 1000.0);
}

fn report_json(report: &Report) -> Value {
    json!({
        "fixture": {
            "files": report.files,
            "methods_per_class": report.methods_per_class,
            "fields_per_class": report.fields_per_class,
            "lines": report.fixture_lines,
        },
        "features": report.features.iter().map(|f| json!({
            "feature": f.feature,
            "from_did_open_ms": f.from_did_open_ms,
            "from_start_ms": f.from_start_ms,
        })).collect::<Vec<_>>(),
        "warmup": {
            "duration_ms": report.warmup_duration_ms,
            "scan_files": report.scan_files,
            "scan_elapsed_ms": report.scan_elapsed_ms,
            "samples": report.samples,
            "tick_ms": TICK_MS,
            "max_rtt_ms": report.sample_max_ms,
            "mean_rtt_ms": report.sample_mean_ms,
        },
        "post_warmup_rtt_ms": report.post_warmup_rtt_ms,
        "completion_probe": {
            "found": report.completion_probe_found,
            "ms": report.completion_probe_ms,
        },
        "memory": {
            "peak_rss_kb": report.peak_rss_kb,
            "final_rss_kb": report.final_rss_kb,
        },
        "total_ms": report.total_ms,
    })
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse_args(&argv) {
        Ok(args) => args,
        Err(text) => {
            eprintln!("{text}");
            std::process::exit(if argv.iter().any(|a| a == "--help" || a == "-h") {
                0
            } else {
                2
            });
        }
    };
    let server = match resolve_server(&args) {
        Ok(path) => path,
        Err(text) => {
            eprintln!("error: {text}");
            std::process::exit(2);
        }
    };
    eprintln!("server binary: {}", server.display());

    let fixture = generate_fixture(
        args.files,
        args.methods_per_class,
        args.fields_per_class,
        args.maven,
    )
    .expect("generate fixture");
    eprintln!(
        "fixture: {} files in {}",
        args.files,
        fixture.root.display()
    );

    let report = run_bench(&server, &fixture, &args);

    if !args.keep {
        let _ = fs::remove_dir_all(&fixture.root);
    }

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report_json(&report)).expect("serialize report")
        );
    } else {
        print_report(&report, &server);
    }
}

// ---------------------------------------------------------------------------
// Unit tests: fixture generator shape
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_source_has_class_fields_and_multiline_methods() {
        let source = fixture_source("BenchClass00042", 3, 2);
        assert!(source.starts_with("package bench;\n\n"));
        assert!(source.contains("public class BenchClass00042 {"));
        assert!(source.contains("private int field00 = 0;"));
        assert!(source.contains("private int field01 = 1;"));
        for m in 0..3 {
            assert!(source.contains(&format!("public int method{m:02}(int a) {{")));
        }
        // Methods are multi-line: a body with a for loop and a return.
        assert_eq!(
            source
                .matches("        for (int i = 0; i < 10; i++) {")
                .count(),
            3
        );
        assert_eq!(source.matches("        return sum;").count(), 3);
        assert!(source.trim_end().ends_with('}'));
    }

    #[test]
    fn class_names_are_unique_and_zero_padded() {
        assert_eq!(class_name(0), "BenchClass00000");
        assert_eq!(class_name(42), "BenchClass00042");
        assert_ne!(class_name(41), class_name(42));
    }

    #[test]
    fn generated_fixture_has_expected_files_and_names() {
        let fixture = generate_fixture(4, 2, 2, false).expect("generate fixture");
        assert_eq!(fixture.class_names.len(), 4);
        assert_eq!(fixture.uris.len(), 4);
        assert_eq!(fixture.class_names[0], "BenchClass00000");
        assert_eq!(fixture.class_names[3], "BenchClass00003");
        for (i, uri) in fixture.uris.iter().enumerate() {
            assert!(
                uri.starts_with("file://") && uri.ends_with(&format!("{}.java", class_name(i))),
                "{uri}"
            );
        }
        // Every generated file exists on disk.
        for class in &fixture.class_names {
            assert!(fixture.root.join(format!("{class}.java")).is_file());
        }
        fs::remove_dir_all(&fixture.root).expect("clean up fixture");
    }

    #[test]
    fn parse_args_defaults_flags_and_values() {
        let args = parse_args(&[]).expect("defaults");
        assert_eq!(args.files, 200);
        assert_eq!(args.methods_per_class, 5);
        assert_eq!(args.fields_per_class, 3);
        assert!(args.server.is_none());
        assert!(!args.json);
        assert!(!args.keep);

        let args = parse_args(&[
            "--files".to_string(),
            "7".to_string(),
            "--methods-per-class=9".to_string(),
            "--fields-per-class".to_string(),
            "2".to_string(),
            "--json".to_string(),
            "--keep".to_string(),
            "--server".to_string(),
            "/tmp/java-lsp".to_string(),
        ])
        .expect("overrides");
        assert_eq!(args.files, 7);
        assert_eq!(args.methods_per_class, 9);
        assert_eq!(args.fields_per_class, 2);
        assert!(args.json);
        assert!(args.keep);
        assert_eq!(args.server, Some(PathBuf::from("/tmp/java-lsp")));

        assert!(parse_args(&["--nope".to_string()]).is_err());
        assert!(parse_args(&["--files".to_string(), "0".to_string()]).is_err());
        assert!(parse_args(&["--files".to_string()]).is_err());
    }

    #[test]
    fn file_uri_encodes_spaces() {
        assert_eq!(
            file_uri(Path::new("/tmp/a b/x.java")),
            "file:///tmp/a%20b/x.java"
        );
        assert_eq!(file_uri(Path::new("/tmp/a/x.java")), "file:///tmp/a/x.java");
    }

    #[test]
    fn log_field_parses_trailing_integers() {
        let line = "2026-09-08T00:00:00Z INFO java_lsp::index: workspace index warm-up complete root=file:///tmp/x files=500 elapsed_ms=123";
        assert_eq!(log_field(line, "files="), Some(500));
        assert_eq!(log_field(line, "elapsed_ms="), Some(123));
        assert_eq!(log_field(line, "missing="), None);
    }
}
