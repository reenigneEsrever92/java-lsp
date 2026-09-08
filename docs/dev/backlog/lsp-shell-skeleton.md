---
type: ChangeRequest
kind: feature
title: LSP shell skeleton
description: A tower-lsp server over stdio with lifecycle, incremental text sync, and the SemanticEngine trait seam.
state: done
priority: high
tags: [dev, lsp, shell]
owner: felix
verified:
  by: stdio smoke test + cargo test (12 passed)
  at: 2026-09-08T12:33:38Z
---

# Problem

Nothing exists yet. Every other feature — syntax or semantic — needs a server
that speaks LSP over stdio, tracks document state, and separates the editor
facing layer from whatever analysis backend comes later (R1, R2).

# Proposal

Create the cargo crate and the server skeleton: `tower-lsp` over stdio with
initialize/shutdown lifecycle, incremental `didOpen`/`didChange`/`didClose`
handling storing versioned documents, and `tracing` logging. Define the
`SemanticEngine` trait (open/change/diagnostics/hover/definition/completions)
and a `SyntaxOnlyEngine` stub that returns empty results, so every later
backend slots in without touching the LSP layer.

# Decisions

- Rust with `tower-lsp`, over `lsp-server` (sync loop) — ergonomics first;
  revisit if it gets in the way of cancellation or backpressure control
  (recorded in `requirements.md`).
- stdio transport only — the universal mechanism every LSP client supports.
- The trait seam exists from the first commit — the architecture principle
  agreed during project init.

# Acceptance criteria

- The server starts as a stdio binary and completes the LSP handshake with
  Zed, Neovim, and VS Code.
- Incremental `didChange` events are applied to a versioned document store.
- Hover/definition/completions/diagnostics respond (with empty results) from
  the `SyntaxOnlyEngine` stub.
- Graceful shutdown on `shutdown`/`exit`.
- A test harness can drive the server without a real editor.

# Implementation plan

## Approach

Single cargo crate at the repo root (a workspace split into shell/engine crates
is deliberately deferred — the trait seam gives the same isolation until there
is a second binary or a real second backend):

```
Cargo.toml
src/
  main.rs        — tokio entry point, tracing init, serve(Stdio)
  server.rs      — JavaLanguageServer: LanguageServer impl (the LSP shell)
  document.rs    — DocumentStore: URI -> { version, text }, incremental sync
  engine.rs      — SemanticEngine trait + SyntaxOnlyEngine stub
  tests/harness.rs — integration test driving the server over JSON-RPC
```

- **Dependencies**: `tower-lsp` (latest 0.20.x), `tokio` with
  `rt-multi-thread`/`macros`/`io-std`, `tracing`, `tracing-subscriber`
  (env-filter). No tree-sitter yet — that is `syntax-features`.
- **Logging goes to stderr.** stdout is the LSP transport; any stray stdout
  write corrupts the protocol. `tracing_subscriber::fmt().with_writer(std::io::stderr)`.
- **Data flow**: client -> `LspService<JavaLanguageServer>` over stdio ->
  handlers -> `DocumentStore` (shared as `Arc<RwLock<DocumentStore>>`) for
  text state, and `Arc<RwLock<dyn SemanticEngine>>` for analysis. The shell
  forwards `didOpen`/`didChange`/`didClose` to the engine so a future
  tree-sitter backend can maintain parse trees without touching the LSP layer.
- **Incremental sync**: `initialize` advertises
  `TextDocumentSyncCapability::Kind(Incremental)`; `didChange` applies each
  `TextDocumentContentChangeEvent` in order (full replacement when `range` is
  `None`, spliced by byte range — tree-sitter works on bytes, so the store
  keeps UTF-8 bytes and positions are converted at the LSP boundary later).
- **Test harness without an editor**: `tower_lsp::LspService` implements
  `tower::Service`, so a test can feed JSON-RPC requests (`initialize`,
  `textDocument/didOpen`, `textDocument/didChange`, `shutdown`, `exit`) and
  assert responses and notifications directly — no stdio, no editor.

**Docs touched**: this is the first code, so it also creates
`docs/architecture.md` (crate layout, shell/engine seam, data flow, stderr
logging constraint) and links it from `docs/index.md` and `docs/overview.md`.

## Steps

- [x] Scaffold the crate: `Cargo.toml` with the dependencies above and
      `src/main.rs` that initializes a stderr tracing subscriber, builds the
      document store and `SyntaxOnlyEngine`, and runs
      `tower_lsp::Server::new(Stdio, service)` — `cargo run` then waits on
      stdio for an `initialize` request. (AC: starts as a stdio binary)
- [x] Implement `src/document.rs`: `DocumentStore` with `open`/`change`/
      `close`, storing `{ version, bytes }` per URI and applying full and
      ranged (incremental) `TextDocumentContentChangeEvent`s in order; unit
      tests covering version tracking and a mixed full+incremental edit
      sequence. (AC: incremental didChange applied to a versioned store)
- [x] Define `src/engine.rs`: the `SemanticEngine` trait with `open`,
      `change`, `close`, `diagnostics`, `hover`, `definition`, `completions`
      (LSP types in, `Option`-typed results out) and a `SyntaxOnlyEngine`
      stub returning empty results for every query. (AC: the seam exists;
      later backends slot in without touching the LSP layer)
- [x] Implement `src/server.rs`: `JavaLanguageServer` implementing
      `tower_lsp::LanguageServer` — `initialize` (incremental sync,
      hover/definition/completion capabilities), `initialized`, `shutdown`,
      `didOpen`/`didChange`/`didClose` updating the store and notifying the
      engine, and hover/definition/completion handlers dispatching through
      the engine; publish empty diagnostics on open/change. (AC: handshake
      completes, stub queries respond, versioned sync wired)
- [x] Create `docs/architecture.md` describing the crate layout, the LSP
      shell / `SemanticEngine` seam, the document store data flow, and the
      stderr-only logging constraint; link it from `docs/index.md` and
      `docs/overview.md`. (Doc step — keeps architecture docs in step with
      the skeleton)
- [x] Write `src/tests/harness.rs`: an integration test constructing
      `LspService<JavaLanguageServer>` and driving `initialize` (assert
      capabilities), `didOpen` + incremental `didChange` (assert via the
      store), `hover` (empty), `shutdown`/`exit` — no real editor. (AC:
      test harness drives the server; graceful shutdown)
- [x] Smoke test the handshake with a real client. No editor registration
      was available in this environment, so the equivalent was done: drive
      the actual binary over stdio with raw LSP JSON-RPC through initialize →
      didOpen → didChange → hover → shutdown → exit
      (`scripts/stdio-smoke.py`); result recorded in `docs/log.md`. A Zed/
      Neovim/VS Code registration remains to be verified by hand. (AC: LSP
      handshake with a real client)
