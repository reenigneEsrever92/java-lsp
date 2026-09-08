---
type: ChangeRequest
kind: feature
title: Non-blocking workspace symbol index
description: A background, incrementally updated in-memory index of all Java declarations in the workspace.
state: done
priority: high
tags: [dev, index, performance]
owner: felix
verified:
  by: cargo test (31 passed: index/engine unit tests, harness integration test, stdio smoke)
  at: 2026-09-08T18:28:55Z
---

# Problem

Completions and navigation need symbols from across the workspace (R4, R5).
Scanning per request would be O(project) and would block the request path —
directly violating the standing performance requirement R6: project open →
responsive, with the initial scan never blocking text sync or request handling.

# Proposal

A background indexer walks the workspace's `.java` files, extracts
declarations from the tree-sitter AST (classes, interfaces, enums, records,
methods, fields, imports), and maintains an in-memory index. Edits update only
the affected file's entries. Features served by the index report themselves
unavailable until warm-up completes — brief unavailability is agreed to be
acceptable; blocking is not.

# Decisions

- Pure Rust, in process — no semantic engine yet; this is the v0.1 engine.
- Declarations only, no type resolution — type-aware resolution belongs to the
  postponed engine decision (see `type-aware-engine`).
- Built off the request path (background task) so opening a project never
  waits for the scan.

# Acceptance criteria

- Index construction never delays document sync or any LSP request; syntax
  features stay responsive while a large workspace is scanned (verified with
  the `perf-benchmarks` harness).
- Edits update the affected file's symbols incrementally, not by re-scanning
  the workspace.
- Memory stays proportional to workspace size (no unbounded growth).

# Implementation plan

## Approach

The index is a new `src/index.rs` module owned by `TreeSitterEngine`, behind the
existing `SemanticEngine` seam. No new dependencies: the workspace walk uses
`std::fs`, declaration extraction reuses the `tree-sitter-java` parser already
in the engine, and locking uses `std::sync::RwLock`.

- **Data model**: a flat `SymbolEntry { name, kind (class/interface/enum/
  record/method/field/import), container chain (enclosing type names), uri,
  full_range, selection_range }`. Storage is
  `RwLock<IndexState>` where `IndexState` is
  `HashMap<Url, Vec<SymbolEntry>>` (per-file entries) plus a derived
  name -> entries map rebuilt on swap. Entries only — no trees, no text — so
  memory stays proportional to workspace size (AC3).
- **Background scan (AC1)**: the engine gains `set_workspace_root(&Url)` and
  `index_ready() -> bool` on the `SemanticEngine` trait. `set_workspace_root`
  spawns a `tokio` task that recursively walks the root for `*.java`, parses
  each file with its own parser instance, extracts declarations and imports,
  and upserts that file's entries into the index per file (so progress is
  incremental, not one giant swap). It flips an `AtomicBool` warm flag when
  done and logs warm-up completion. The scan never touches the request path;
  readers at worst take a brief read lock per upsert batch.
- **Incremental updates (AC2)**: the engine's existing `open`/`change` hooks
  additionally re-extract the edited document's entries from its already-parsed
  tree — no workspace re-scan. On `close`, a file under the workspace root is
  re-read from disk and re-indexed (so disk truth wins); files outside the root
  drop their entries.
- **Readiness**: `index_ready()` exposes the warm flag so the consumer CRs
  (`completions-v1`, `navigation-v1`) can report their features unavailable or
  degraded during warm-up, per R6. This CR wires the flag; consumers are not
  implemented here.
- **Known v1 limitation**: files on disk are indexed once at scan time; there
  is no file watcher, so out-of-editor disk changes are picked up on close
  re-reads or a future `notify`-based watcher (follow-up CR if needed).
- **Verification**: interim verification with a generated temp-fixture
  integration test (harness drives initialize with `root_uri`, asserts symbols
  appear after warm-up and that syntax features respond *during* warm-up);
  numeric open-to-responsive baselines stay with the `perf-benchmarks` CR,
  which owns the harness.

**Docs touched**: `docs/architecture.md` (new `WorkspaceIndex` component in the
layout tree, the components/data-flow section, and an R6 warm-up note).

## Steps

- [x] Implement `src/index.rs`: `SymbolEntry`, `SymbolKind`, and
      `WorkspaceIndex` with `upsert_file(uri, entries)` / `remove_file(uri)` /
      `query_name(name)` / `query_prefix(prefix)` / `ready()` over the
      `RwLock<IndexState>` snapshot, with unit tests for upsert/replace/remove
      and lookup by name and prefix. (AC2, AC3 — the store itself is bounded
      and per-file)
- [x] Add declaration/import extraction for the index: a tree-walking
      extractor in `src/index.rs` (reusing the tree-sitter-java parser) that
      produces `SymbolEntry` lists — classes, interfaces, enums, records,
      methods (with enclosing type as container), fields, and imports — with
      full and selection ranges; unit tests on a small Java sample covering
      all seven kinds and container chains. (Enables AC2; imports are
      groundwork for `navigation-v1`)
- [x] Extend `SemanticEngine` (`src/engine/mod.rs`) with `set_workspace_root`
      and `index_ready`; implement in `TreeSitterEngine` (owning a
      `WorkspaceIndex`, feeding `open`/`change`/`close` into it, spawning the
      background scan task with a per-task parser and warm-up completion log)
      and as no-ops in `SyntaxOnlyEngine`. (AC1 — scan is off the request
      path)
- [x] Wire the shell (`src/server.rs`): capture `root_uri` (falling back to the
      first workspace folder) in `initialize`, pass it to the engine in
      `initialized`, and expose a test accessor for the engine alongside the
      existing `documents()`; document sync and diagnostics handlers remain
      untouched. (AC1 — open stays responsive regardless of scan)
- [x] Integration test in `tests/harness.rs`: generate a temp fixture project
      (a few `*.java` files), initialize with its `root_uri`, immediately
      issue a document-symbol request (must succeed while warm-up is still
      running — AC1), then poll `index_ready` via the engine accessor and
      assert the fixture's declarations are queryable; then `didChange` a
      class name in an open document and assert only that file's entries
      changed (AC2), and `didClose` re-reads the file from disk.
- [x] Update `docs/architecture.md`: add `src/index.rs` / `WorkspaceIndex` to
      the layout tree, a component paragraph (data model, background scan,
      incremental update policy, readiness flag, no-watcher limitation), and
      extend the data-flow diagram so the engine feeds the index. (Doc step —
      keeps architecture in step with the new component)
