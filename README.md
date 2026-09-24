# java-lsp

A Java language server written in Rust: **no JVM at runtime**, light on
resources, and responsive the moment a project opens. It speaks LSP over stdio,
so any LSP-capable editor can use it — Zed, Neovim, VS Code, Helix, Emacs.

It starts as a syntax-level server built on `tree-sitter-java` and grows toward
full type-aware Java support behind a pluggable `SemanticEngine` interface.

## Why

Java language servers today run on a JVM (Eclipse JDT-LS and friends): a heavy
install, slow project startup, high memory use. `java-lsp` is the alternative —
a single native binary with no JVM, that answers text sync, symbols, and
completions immediately while the workspace index warms up in the background.
The initial scan never blocks the request path.

## Features

Everything below is pure Rust, answered from an in-memory workspace index and a
declared-type model built during warm-up:

- **Syntax** — document symbols, folding ranges, semantic tokens, and
  parse-error diagnostics from tree-sitter (squiggles appear on broken code and
  clear on fix).
- **Completions** — Java keywords, names in scope (parameters, locals, fields),
  and workspace-index symbols; member access after `.` offers the receiver's
  inferred members with signatures; accepted symbols from other packages carry
  an automatic `import` edit.
- **Navigation** — go-to-definition and `workspace/symbol` backed by the symbol
  index.
- **Hover** — a member's signature, a type's declaration, or a local's declared
  type, rendered as Markdown.
- **References and rename** — attribute occurrences with confidence and refuse
  rather than guess (a rename returns `null` for an unresolved or ambiguous
  target, a library declaration, or an invalid identifier).
- **Inlay hints** — variable types (including `var` inference), parameter names
  at call sites, and intermediate chained-call return types, computed for the
  range the client requests.
- **Diagnostics** — parse errors from tree-sitter plus type-aware unresolved
  symbols (types, members, identifiers, imports) reported as errors, each with a
  quick fix: add the missing import, change to a near member, or create the
  missing symbol — a class/interface/enum/record (with the file scaffolded), a
  method or field with its signature inferred from the usage (in the enclosing
  type or a workspace receiver's type), or a local variable. Gated on the model
  vouching for `java.lang` and the file parsing cleanly, so a missing JDK never
  becomes a wall of false positives. Diagnostics refresh when any workspace file
  changes — including a `.java` file created on disk outside the editor, which a
  `**/*.java` file watcher picks up.
- **Project model** — statically parsed Maven `pom.xml` (multi-module, `<build>`
  overrides), offline dependency resolution from the local repository, and
  indexing of dependency jars, the installed JDK's `java.*`/`javax.*`, and (by
  default) downloaded dependency **sources** so go-to-definition can open
  library declarations.

Deliberately out of scope for now: full compiler parity, Gradle project-model
support, and overload resolution by argument types. See the
[requirements](docs/requirements.md) for the numbered list and milestones.

## Requirements

- A Rust toolchain to build from source (stable; developed against 1.98).
- A JDK is **optional**. When one is found, its standard library is indexed so
  `java.lang`/`java.util`/… types complete and resolve. Without one, the server
  runs fine and simply skips that pass.
- Maven is **not** required. The project model reads `pom.xml` and the local
  repository directly; it never invokes `mvn`.

## Install

### From source

```sh
cargo build --release          # → target/release/java-lsp
# or, to put it on your PATH:
cargo install --path .
```

### Zed

`zed-java-lsp/` is a self-contained Zed extension that registers the
tree-sitter-java grammar and the Java language and starts `java-lsp` as the
language server. Install it as a dev extension via `zed: install dev extension`
and select that directory. See [`zed-java-lsp/README.md`](zed-java-lsp/README.md)
for details, including how to point Zed at a specific binary and how to remove
Zed's official `java` extension (which also defines the Java language and pulls
in jdtls — two definitions of the same language conflict).

### Other editors

Point the editor at the `java-lsp` binary as a stdio language server. The
process reads JSON-RPC on stdin and writes it on stdout, and exits when stdin
closes (EOF) after `shutdown`/`exit`. Logs go to **stderr**, so keep them out of
the protocol stream.

A minimal Neovim example:

```lua
local root = vim.fs.root(0, { "pom.xml", ".git" }) or vim.fn.getcwd()
vim.lsp.start({ name = "java-lsp", cmd = { "java-lsp" }, root_dir = root })
```

## Try it

[`example/`](example/README.md) is a small multi-module Maven project (a
sibling-module dependency plus a third-party one) that exercises the project
model. Open it as the workspace root and you should see symbols and folding
immediately, then completions offering workspace types and indexed jar members
as warm-up finishes. Build it once with `mvn install` first if you want the
dependency jars in your local repository; without that, the server scans sources
and skips dependencies with a warning.

## Configuration

`java-lsp` is configured entirely through the environment:

| Variable | Effect |
|----------|--------|
| `RUST_LOG` | Tracing filter on stderr. Default `java_lsp=info`. |
| `MAVEN_REPO` | Local Maven repository to resolve against. Default `~/.m2/repository`. |
| `JAVA_LSP_JDK` | Explicit JDK home to index. Pointing it at an unusable home disables JDK indexing entirely (a deterministic opt-out). |
| `JAVA_HOME` | JDK home used when `JAVA_LSP_JDK` is unset; otherwise common locations (including SDKMAN's `current`) are searched. |
| `SDKMAN_DIR` | SDKMAN root, for locating candidates' JDKs. |
| `JAVA_LSP_OFFLINE` | Any non-empty value disables **all** network work; dependency sources stay class-file-only. |
| `JAVA_LSP_SEMANTIC_DIAGNOSTICS` | `0`/`false` disables the unresolved-symbol diagnostics (parse errors still report). On by default. |
| `JAVA_LSP_MAVEN_CENTRAL_URL` | Base URL for dependency-source downloads. Default Maven Central (`https://repo1.maven.org/maven2`). |
| `JAVA_LSP_SOURCES_CACHE` | Where extracted sources are cached. Default `$XDG_CACHE_HOME/java-lsp/sources`, else `~/.cache/java-lsp/sources`. |

Dependency *resolution* is always offline. Dependency **source** fetching is the
one part that uses the network: for each resolved artifact it reuses a
`-sources.jar` already in the local repository, otherwise downloads it from
Maven Central (writing it back into the local repository) and extracts it into
the cache. Set `JAVA_LSP_OFFLINE=1` to keep everything local.

## Development

```sh
cargo build                     # debug build
cargo test --all-targets        # unit + integration tests
```

Two harnesses drive the server with raw JSON-RPC, both plain `cargo test`:

- `tests/harness.rs` — feeds requests through `tower_lsp::LspService` and
  asserts on responses and document-store state.
- `tests/stdio_smoke.rs` — spawns the real binary and drives the stdio
  transport end to end (initialize → didOpen → didChange → hover → shutdown →
  exit).

### Benchmarks

`src/bin/java-lsp-bench.rs` generates a fixture Java workspace, drives the real
binary over stdio, and reports per-feature first-response time, hover latency
during index warm-up, and peak RSS:

```sh
cargo run --release --bin java-lsp-bench -- --files 500 --methods-per-class 10
```

Flags: `--files N`, `--methods-per-class M`, `--fields-per-class F`, `--maven`
(lay the fixture out as a Maven project), `--server PATH`, `--json`, `--keep`.
Baselines per milestone are recorded in the [changelog](docs/dev/changelog.md).

### Layout

```
src/
  main.rs       — thin stdio binary: tracing (stderr), tokio runtime, tower-lsp server
  lib.rs        — library root so integration tests can drive the shell
  server.rs     — JavaLanguageServer: the LSP shell (all editor-facing handlers)
  document.rs   — DocumentStore: URI -> { version, bytes }, incremental text sync
  index.rs      — WorkspaceIndex: workspace-wide symbol entries, background scan
  types.rs      — declared-type model: types, members, hierarchies, binding
  project.rs    — Maven project model: pom discovery, modules, source roots
  resolve.rs    — static Maven dependency resolution (effective poms, closure)
  sources.rs    — dependency source fetching and indexing
  classfile.rs  — minimal jar (ZIP) + class-file reader for dependency indexing
  jdk.rs        — standard-library indexing: JDK discovery, jmods/src.zip/rt.jar
  engine/       — SemanticEngine trait, SyntaxOnlyEngine stub, TreeSitterEngine
  bin/java-lsp-bench.rs — performance harness (see Benchmarks)
tests/          — integration harnesses (see above)
example/        — sample multi-module Maven project
zed-java-lsp/   — Zed extension
```

### Docs

The project is driven by change requests. `docs/` is an OKF bundle:

- [overview](docs/overview.md) — the problem, the shape, and what is out of scope.
- [requirements](docs/requirements.md) — users, use cases, technologies, and the
  numbered requirement list with milestones.
- [architecture](docs/architecture.md) — crate layout, the LSP shell and
  `SemanticEngine` seam, and the data flow.
- [development](docs/dev/index.md) — the [backlog](docs/dev/backlog/index.md) of
  change requests and the [changelog](docs/dev/changelog.md).

## Status

Early but usable. The syntax server (v0.1), the Maven project model (v0.2), and
the pure-Rust type-aware engine (v0.3) have shipped; type-aware UX — inlay
hints and wider `var` inference (v0.4) — is in place. Gradle support and
overload resolution by argument types remain deferred. See
[docs/requirements.md](docs/requirements.md) for the milestone detail.

## License

No license file is present in this repository yet.
