---
type: ChangeRequest
kind: feature
title: Reintroduce workspace symbol search as a workspace hub module
description: The shell advertises no workspaceSymbolProvider and answers no workspace/symbol, so editors cannot search the workspace by symbol name; restore it as a new workspace module over the index's existing prefix read.
state: done
verified: { by: felix, at: 2026-10-13T17:24:23Z }
priority: medium
tags: [dev, feature, symbols, index]
owner: felix
---

# Problem

The server no longer answers `workspace/symbol`. During the hub rework the
shell's `initialize` dropped `workspace_symbol_provider` and the
`LanguageServer` impl has no `symbol` handler, so an editor never even asks for
workspace symbols — the capability is absent from the handshake. The engine that
computed the answer lives in `src/deprecated/analysis.rs`
(`TreeSitterEngine::workspace_symbols`, ~L1566), which `src/lib.rs` keeps out of
the build (`pub mod analysis;` is commented out at L34). The capability survives
only as dead source and a disabled test: `tests/harness.rs:160` still asserts
`capabilities["workspaceSymbolProvider"] == true`, but the whole file is
`#![cfg(any())]`-disabled (L1) while the modules return.

A developer therefore cannot search the workspace for a type or member by name —
a v0.1 feature (R5, "go-to-definition and workspace symbols backed by it"; UC3,
"query workspace symbols") that the docs still describe as shipping
(`docs/architecture.md:219-224`; the `workspace_symbols` mapping note at
L409-413).

The gap is small, because the index already serves the read this needs.
`IndexRequest::EntriesByPrefix { prefix }` (`src/messages.rs:154`) is answered
by `src/index/read.rs::entries_with_prefix` (~L74), a name-ordered range scan
added for completion (which reads it in `src/completion.rs`). What is missing is
the shell wiring — the capability and the handler — and the entry →
`SymbolInformation` shaping.

# Proposal

Serve `textDocument/workspaceSymbol` from a new `workspace` hub module
(`src/workspace.rs`), labelled `workspace`, which owns the "search the workspace
for a symbol by name" query. The shell advertises `workspaceSymbolProvider` and
routes the request to it as a new `SymbolRequest::Search { query }`, answered by
`SymbolResponse::Symbols(Vec<SymbolInformation>)`.

The search reads the index through the existing `IndexRequest::EntriesByPrefix`
— an empty query returns everything indexed — and maps each matching entry to a
`SymbolInformation`:

- `location` = the entry's declaring file and its `selection_range` (the
  declared name, not the whole declaration);
- `container_name` = the entry's innermost enclosing type (`container.last()`);
- `kind` = the entry's kind as an LSP `SymbolKind`.

Import entries, constructors, and Lombok-synthesised members are dropped (they
name no addressable symbol with an LSP kind), and `FileOrigin::Dependency`
class-file entries are filtered out — a jar's bytecode has no openable
declaration — while `Workspace` and `LibrarySource` entries are kept, matching
the old engine.

# Decisions

- **D1 — Owned by a new `workspace` module.** Workspace symbol search is its own
  LSP method with neither a cursor (`navigate`) nor a typed prefix at a cursor
  (`completion`), so it fits neither existing module's identity. It lands as
  `src/workspace.rs`, a hub client labelled `workspace`. `documentSymbolProvider` —
  the file-outline sibling the old engine served from the same slice — is left
  for a later request and is the natural next occupant of this module (out of
  scope here). A shell→index shortcut (the shell itself sending
  `IndexRequest::EntriesByPrefix`) was rejected: every feature so far is a
  module that owns its hub life, and the shell stays a router.
- **D2 — No new index read.** `EntriesByPrefix` (`src/messages.rs:154`,
  answered in `src/index/read.rs`) already range-scans the name-ordered table —
  the exact query — and an empty prefix returns every entry. Chosen over adding
  a symbol-shaped index read: the index serves light index data, not LSP shapes,
  and the filter/map is a single pass over the returned `Arc<SymbolEntry>`s.
- **D3 — The mapping matches the old engine** (`src/deprecated/analysis.rs:1566-1590`,
  the spec to reintroduce against): `location.range` is the entry's
  `selection_range`, `container_name` is `container.last()`, and `kind` maps
  Class→CLASS, Interface→INTERFACE, Enum→ENUM, Record→STRUCT, Method→METHOD,
  Field→FIELD, EnumConstant→ENUM_MEMBER. Import entries (no LSP kind),
  constructors, and Lombok-synthetic members are dropped; `FileOrigin::Dependency`
  entries are filtered out, while `Workspace` and `LibrarySource` entries stay
  (a fetched-source declaration is openable). Results follow the index's name
  order, as `EntriesByPrefix` returns them.
- **D4 — The constructor test is shared.** `completion.rs` already needs the
  same predicate (`kind == Method && container.last() == name`); it becomes an
  inherent `SymbolEntry::is_constructor()` in `src/index/util.rs` (where index
  value helpers live — `types.rs` is data-only) so `completion` and `workspace`
  share one definition rather than diverge.
- **D5 — The shell wiring is restored.** `initialize` advertises
  `workspace_symbol_provider: Some(OneOf::Left(true))`; a new `symbol` handler
  sends `Request::Symbols(SymbolRequest::Search { query: params.query })` and
  returns the `SymbolResponse::Symbols` list (`None` when empty, matching the
  shell's other handlers). `Request`/`Response` gain the `Symbols` variants,
  `hub::owner_of` routes `Symbols` to `workspace`, `src/lib.rs` declares
  `pub mod workspace;`, and `main.rs::setup` spawns it.
- **D6 — Docs move with the code.** `docs/architecture.md` gains the `workspace`
  module (table row, Layout, data-flow arrow to the index) and its `workspaceSymbols`
  behaviour note, and the shell note that `initialize` advertises workspace
  symbols becomes true; `docs/dev/changelog.md` gets the shipped entry.

# Acceptance criteria

1. `initialize` advertises `workspaceSymbolProvider: true`.
2. `SymbolRequest`, `SymbolResponse`, `Request::Symbols`, and `Response::Symbols`
   exist; `hub::owner_of` routes `Symbols` to `"workspace"`; `src/lib.rs` declares
   `pub mod workspace;`; `main.rs::setup` spawns `workspace`; the shell's `symbol`
   sends `SymbolRequest::Search` and returns `SymbolResponse::Symbols`.
3. The search reads the index through `IndexRequest::EntriesByPrefix` (no new
   index read) — asserted by a `workspace` module test driving a preloaded index
   (`index::spawn_with`).
4. For a non-empty query the result is the workspace symbols whose simple name
   starts with it, in name order; for an empty query, every indexed symbol. Each
   carries the entry's `selection_range` as its location range, its innermost
   container as `container_name`, and the mapped kind (Class→CLASS,
   Interface→INTERFACE, Enum→ENUM, Record→STRUCT, Method→METHOD, Field→FIELD,
   EnumConstant→ENUM_MEMBER).
5. Import entries, constructors, and Lombok-synthetic members are absent;
   `FileOrigin::Dependency` entries are absent; `Workspace` and
   `LibrarySource` entries are present.
6. `cargo build`, `cargo fmt --all -- --check`, and `cargo test` are green.
7. `docs/architecture.md` documents the `workspace` module and the restored
   `workspaceSymbolProvider`; `docs/dev/changelog.md` records the change.

# Docs

- `docs/architecture.md` — add the `workspace` module (module table row, Layout,
  data-flow diagram, an implementation note describing the `EntriesByPrefix`
  read and the entry→`SymbolInformation` mapping), make the shell's
  "`initialize` advertises … workspace symbols" note true, and re-point the
  `workspace_symbols`/`query_prefix` prose that still reads as the engine's.
- `docs/dev/changelog.md` — the shipped entry, once implemented.

# Implementation plan

## Approach

A self-contained addition — vocabulary, routing, a new stateless module, and the
shell wiring — plus moving one predicate onto the index entry type so two
modules share it. It mirrors the `completion` module, the closest precedent (a
hub client that reads `EntriesByPrefix` and shapes LSP items), and needs no new
index read because `EntriesByPrefix` already range-scans the name-ordered table.

1. **Vocabulary** (`messages.rs`): `SymbolRequest::Search { query }` and
   `SymbolResponse::Symbols(Vec<SymbolInformation>)` (its `Default` is the empty
   list), the `Request::Symbols`/`Response::Symbols` variants, and their
   `summary()` arms.
2. **Routing** (`hub.rs`): `owner_of` sends `Request::Symbols` to `workspace`.
3. **Shared predicate** (`index/types.rs`): `SymbolEntry::is_constructor()`;
   `completion.rs` drops its private `is_constructor_entry` and calls the method.
4. **The module** (`src/workspace.rs`, new): `spawn`/`serve`/`search` — a
   stateless hub client labelled `workspace` that reads
   `IndexRequest::EntriesByPrefix` (an empty query returns everything) and maps
   each kept entry to a `SymbolInformation`: `location` = file + `selection_range`,
   `container_name` = `container.last()`, `kind` = the mapped LSP kind. It drops
   import entries, constructors, and Lombok-synthetic members, and filters out
   `FileOrigin::Dependency` entries. Tests drive a preloaded index
   (`index::spawn_with`).
5. **Shell** (`server.rs`): advertise `workspace_symbol_provider:
   Some(OneOf::Left(true))` and add a `symbol` handler that routes
   `SymbolRequest::Search { query: params.query }` and returns
   `Some(Vec<SymbolInformation>)` (empty → `None`).
6. **Wiring**: `lib.rs` declares `pub mod workspace;`; `main.rs::setup` spawns it.
7. **Docs**: `architecture.md` (the `workspace` module row, Layout, data-flow
   arrow to the index, an implementation note, and the shell's `initialize`
   note) and `docs/dev/changelog.md` (the entry).

## Steps

- [x] `messages.rs` — add `SymbolRequest`/`SymbolResponse` and the
      `Request::Symbols`/`Response::Symbols` variants with their `summary()` arms.
- [x] `hub.rs` — route `Request::Symbols` to `"workspace"` in `owner_of`.
- [x] `index/types.rs` — add `SymbolEntry::is_constructor()`; repoint
      `completion.rs` to it and delete its private `is_constructor_entry`.
- [x] `src/workspace.rs` (new) — the module (spawn/serve/search) and its tests:
      prefix match and empty query, the `selection_range`/`container_name`/kind
      mapping, and the dropped import/constructor/synthetic/dependency entries.
- [x] `server.rs` — advertise `workspaceSymbolProvider`; add the `symbol`
      handler routing `SymbolRequest::Search`.
- [x] `lib.rs` + `main.rs` — declare `pub mod workspace;` and spawn `workspace`.
- [x] `docs/architecture.md` — the `workspace` module (table row, Layout,
      data-flow diagram, implementation note) and the shell's `initialize` note.
- [x] `docs/dev/changelog.md` — today's entry for the reintroduction.
- [x] Verify: `cargo build --all-targets`, `cargo fmt --all -- --check`, and
      `cargo test` green.

## Progress

Landed as planned, in one pass.

- **Vocabulary and routing** — `messages.rs` gains `SymbolRequest`/`SymbolResponse`
  and the `Request::Symbols`/`Response::Symbols` variants with their `summary()`
  arms; `hub::owner_of` routes `Symbols` to `workspace`.
- **Shared predicate** — `SymbolEntry::is_constructor()` lives in
  `src/index/util.rs` (not `types.rs`, which stays data-only); `completion.rs`
  drops its private `is_constructor_entry` and calls it.
- **The `workspace` module** — `src/workspace.rs` serves `SymbolRequest::Search`,
  reading `IndexRequest::EntriesByPrefix` and mapping each kept entry to a
  `SymbolInformation`. Seven tests cover the prefix/empty-query match and order,
  the `selection_range`/`container_name`/kind mapping, imports, constructors, and
  Lombok-synthetic members; the `Dependency`-origin filter is asserted at the
  shaping level (`symbol_information`), because the index stamps a batch's origin
  on the file record but leaves each scanned entry's `origin` at `Workspace`.
- **Wiring and shell** — `server.rs` advertises `workspaceSymbolProvider` and adds
  the `symbol` handler; `lib.rs` declares `pub mod workspace;` and `main.rs::setup`
  spawns it.
- **Docs** — `docs/architecture.md` (the rework note, Layout, data-flow diagram,
  module table, and a workspace implementation note), `docs/dev/changelog.md`, and
  `docs/log.md`.

Verified: `cargo build --all-targets` clean (only the eight pre-existing dead-code
warnings), `cargo fmt --all -- --check` clean, and `cargo test` green except the
eight `maven::sources` tests, which fail only because the sandbox blocks their
loopback socket (`TcpListener::bind`) — not from this change. Not committed.
