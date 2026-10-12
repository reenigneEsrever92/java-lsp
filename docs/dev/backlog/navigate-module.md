---
type: ChangeRequest
kind: refactor
title: Make navigation its own module that reads the index over the hub
description: Move cursor navigation out of the index into its own hub module (`navigate`) that reads source files itself and reads the index through a new coarse, batched read API over the hub.
state: done
verified: { by: felix, at: 2026-10-12T21:00:00Z }
priority: medium
tags: [dev, refactor, navigation, index]
owner: felix
---

# Problem

`src/index/navigate.rs` (~1600 lines) is welded into the index. It resolves a
cursor to a declaration, or to its references, by reading the `WorkspaceIndex`
in-process — the symbol table (`symbols.by_name`/`symbols.by_file`) and the
declared-type model through `query::{lookup, find_unique, members}` — from inside
the index's own task. The index therefore owns two unrelated jobs: keeping the
workspace index, and Java cursor navigation (finding the symbol under the cursor,
resolving a receiver through its declared type, collecting occurrences, and
reading each candidate source from disk). The code that reads the index and the
code that navigates it are the same code, so the boundary between "the index" and
"a consumer of the index" is invisible: navigation cannot be tested, changed, or
toggled apart from the index.

This also contradicts the architecture's own rule for every other consumer of the
index — they reach it only through messages and never hold its state — and it is
why the index's task both scans and serves.

# Proposal

Extract navigation into a top-level module, `src/navigate.rs`: a hub client
labelled `navigate` that serves the shell's navigation requests itself. It keeps
everything that is not index state — reading and parsing the cursor's file and
each candidate source from disk, the cursor/AST/scope resolution, and occurrence
collection — and reaches the index only through hub requests. The index keeps its
state, stops owning navigation, and answers a small read vocabulary instead.

The read vocabulary is **coarse and navigation-shaped**, not a mirror of the
index's internals. Navigation never needs a whole `TypeInfo`; it asks only whether
a type resolves, for a type's members, and for a type's supertype closure — so the
reads return light, owned values and no model type has to cross the hub:

| read | answers |
| --- | --- |
| `EntriesByName { name }` | every entry declared with that name |
| `FileSymbols { uri }` | one file's origin and its entries |
| `WorkspaceFiles` | every workspace source file the index holds |
| `TypeClosure { ty, package }` | the `(name, package)` identities of a type and its supertype closure |
| `MembersOf { ty, package }` | a type's members (inherited included) |
| `ResolvesType { ty, package }` | whether a type name resolves |

An `IndexBatchRequest { requests }` → `Vec<IndexResponse>` lets one resolution
fetch several reads at once instead of a round-trip each.

Navigation becomes async: it reads files with tokio and awaits each index `Reply`.
The shell's `textDocument/definition`, `textDocument/declaration`, and
`textDocument/references` route to it as `NavigateRequest`s.

The vocabulary splits by owner. Navigation's queries move from
`IndexRequest`/`IndexResponse` to `NavigateRequest`/`NavigateResponse`, freeing the
`IndexRequest` name for the index's own reads — which gain `IndexBatchRequest`.
`hub::owner_of` routes `Navigate` to `navigate` and the index reads to `index`, and
the unowned-request default becomes a neutral `Response::Empty`.

Because navigate now reads Java (the cursor's file and every candidate source),
the shared Java-reading helpers move out of the index into a neutral module, so
neither module owns the other's parsing.

# Decisions

- **D1 — Navigation is its own hub module.** `src/index/navigate.rs` becomes
  `src/navigate.rs`, a hub client labelled `navigate` that serves navigation. The
  index neither contains nor serves navigation any more. Chosen for separation of
  concerns; it reverses the architecture's "navigation is the index's own"
  decision, which is rewritten (D8).
- **D2 — The index read API is coarse and batched.** The six reads above plus
  `IndexBatchRequest`. Payloads stay light (identities, members, `Arc<SymbolEntry>`
  entries, URIs), so the declared-type model is never cloned across the hub and
  `TypeInfo` stays as stored. Chosen over a broad mirror of `query::*` (chatty per
  read) and over a per-query read snapshot (which would share internal structures
  across modules).
- **D3 — Requests split by owner.** `NavigateRequest`/`NavigateResponse` are
  navigation's (renamed from today's `IndexRequest`/`IndexResponse`);
  `IndexRequest` (single reads) and `IndexBatchRequest` (a batch) are the index's,
  each with a response. `owner_of` routes each family to its owner;
  `Response::default()` becomes the neutral `Response::Empty`, which every
  requester already treats as no answer. `server.rs` sends `Request::Navigate`;
  `main.rs::setup` spawns `navigate`.
- **D4 — Source text stays in navigate.** Navigation keeps reading `.java` files
  from disk itself; there is no document module yet, so this keeps the change
  self-contained. When the document module returns, open-buffer text should come
  from it.
- **D5 — The shared Java-reading helpers move out of the index.** `java_parser`,
  `LineIndex`, `type_from_node`, and `file_package` move from the index into a
  neutral `src/parser.rs` that both the index and navigate use, and `is_type_kind`
  becomes `SymbolKind::is_type()`. The index imports them from there, so navigate
  depends on the index only for its data types and the read API, not for its
  parsing.
- **D6 — Navigation is async.** It uses tokio file I/O and awaits its index
  `Reply`s directly; there is no synchronous facade and no read trait. Resolution
  runs on the runtime (whose worker threads already carry `RUNTIME_STACK_SIZE`), so
  the deep AST walks are safe. This is the main implementation risk: the reference
  search resolves a receiver per member occurrence, interleaving index reads with
  AST walking — the batch request is how those reads are fetched as a set rather
  than one round-trip at a time.
- **D7 — Behaviour may change where maintainability improves.** The refactor need
  not be a strict move: navigation's answers for the cases the tests cover must
  stay equivalent, but the async and read-API reshaping may alter edge behaviour
  (for example, the order or breadth of occurrence resolution), and that is
  acceptable.
- **D8 — Docs move with the code.** `docs/architecture.md`'s "Navigation is the
  index's own" decision, the navigation sentence in the "index is its own
  subsystem" decision, the Layout, the module table's index/shell rows, the
  data-flow diagram, and the shared-parsing text are rewritten; navigation prose
  that reads as "the index's answer" is reworded. `docs/dev/changelog.md` gets the
  shipped entry.

# Acceptance criteria

1. `src/index/navigate.rs` is deleted; `src/navigate.rs` exists, labels itself
   `navigate`, serves `NavigateRequest`, reads source files itself, and reads the
   index only through hub requests.
2. The index no longer serves or contains navigation; it answers `IndexRequest`
   (the six coarse reads) and `IndexBatchRequest` from the `WorkspaceIndex` it
   owns.
3. `Request::Navigate`/`Response::Navigate`, `IndexRequest`, and
   `IndexBatchRequest` exist; `owner_of` routes each to its owner;
   `Response::default()` is `Response::Empty`; `server.rs` sends
   `Request::Navigate(...)`; `main.rs::setup` spawns `navigate`.
4. `java_parser`, `LineIndex`, `type_from_node`, and `file_package` live in
   `src/parser.rs` and are used by both the index and navigate; `is_type_kind` is
   gone, replaced by a `SymbolKind::is_type()` where needed.
5. `cargo build`, `cargo fmt --check`, and `cargo test` are green, including the
   moved navigation tests and `tests/example_features.rs`; definition,
   declaration, and references answers are unchanged for the covered cases.
6. `docs/architecture.md` describes navigation as its own module reading the index
   over the hub and the Java helpers' new home; `docs/dev/changelog.md` records the
   change.

# Docs

- `docs/architecture.md` — the "Navigation is the index's own" decision
  (rewrite), the "index is its own subsystem" decision (its navigation sentence),
  the Layout (`index/navigate.rs` gone; `navigate.rs` and `parser.rs` added), the
  module table (index, shell, and a new navigate row), the data-flow diagram (a
  navigate arrow to the index), and the shared-parsing text (the helpers move out
  of the index).
- `docs/dev/changelog.md` — the shipped entry, once implemented.

# Implementation plan

## Approach

Built bottom-up in four passes, each of which compiles on its own so it can be
verified in isolation.

**1 — The shared Java helpers move out of the index.** A new `src/parser.rs` takes
`java_parser` (from `index/util.rs`), `LineIndex` (struct from `index/types.rs`,
`new`/`position`/`range` from `index/analyze.rs`), `type_from_node` and
`file_package` (from `index/analyze.rs`). `is_type_kind` becomes
`SymbolKind::is_type()` (in `index/util.rs`). The index imports the helpers from
`crate::parser`, and `index/mod.rs` keeps the old `crate::index::…` re-exports
alive. After this, `index/navigate.rs` no longer reaches into the index for its
parsing.

**2 — The hub vocabulary splits by owner** (`messages.rs`). Navigation's queries
become `NavigateRequest`/`NavigateResponse` (today's `IndexRequest`/
`IndexResponse`). The index gains a coarse read vocabulary —
`IndexRequest::{EntriesByName, FileSymbols, WorkspaceFiles, TypeClosure,
MembersOf, ResolvesType}` with an `IndexResponse` — plus
`IndexBatchRequest { requests }` / `IndexBatchResponse(Vec<IndexResponse>)` so a
resolution can fetch several reads in one round-trip. `Response` gains
`Navigate`, `Index`, `IndexBatch`, and a neutral `Empty` (the new `Default`, so
an unowned request resolves to a variant no requester reads as an answer).
`hub::owner_of` routes `Navigate → "navigate"` and `Index | IndexBatch →
"index"`. The index answers its reads from the `WorkspaceIndex` it owns; a new
`index/read.rs` holds the coarse-read implementation, including the supertype
closure walk moved out of navigate. A `#[cfg(test)] pub fn spawn_with(hub,
WorkspaceIndex)` serves reads from a preloaded index, so tests do not have to
wait on a scan.

**3 — Navigation becomes its own async module** (`src/navigate.rs`). The
resolution in `index/navigate.rs` moves there, replacing its `&WorkspaceIndex`
with a `HubClient`: it reads files with `tokio::fs` and awaits each index read
through the hub. The deep AST walks stay synchronous — an async-recursive walk
would need boxing — so the recurrence is split: a recursive pass collects the
nodes it would resolve and a flat async loop resolves each collected node
through the index. `find_declaration` is a short async sequence of reads;
`find_references` collects each file's occurrences synchronously, then resolves
each member occurrence's receiver in a flat loop (`MembersOf` for a field
receiver, `TypeClosure` for the receiver type), using the batch request where a
set of reads is known at once. The index stops serving navigation; `server.rs`
sends `Request::Navigate`; `main.rs::setup` spawns `navigate`. The navigation
tests spawn the index (via `spawn_with`) and navigate on a test hub and drive
requests.

**4 — Docs move with the code.**

## Steps

- [x] Create `src/parser.rs` with `java_parser`, `LineIndex`, `type_from_node`, and
      `file_package`; remove them from `index/util.rs`, `index/types.rs`, and
      `index/analyze.rs`; declare `pub mod parser;` in `lib.rs`; repoint
      `index/mod.rs`'s re-exports and every call site.
- [x] Replace `is_type_kind(kind)` with `SymbolKind::is_type()` in
      `index/util.rs` and update its call sites (`index/analyze.rs`,
      `index/navigate.rs`). Verify pass 1: `cargo build --all-targets`,
      `cargo fmt --all -- --check`, `cargo test` green.
- [x] `messages.rs` — add `NavigateRequest`/`NavigateResponse` (moved from
      `IndexRequest`/`IndexResponse`), `IndexRequest` (the six reads),
      `IndexResponse`, `IndexBatchRequest`, `IndexBatchResponse`, and
      `Response::{Navigate, Index, IndexBatch, Empty}` with `Empty` as the
      `Default`.
- [x] `hub.rs` — route by owner in `owner_of`; update the hub tests for the new
      request/response variants and the `Empty` default.
- [x] `index/read.rs` (new) + `index/mod.rs` — answer `IndexRequest` and
      `IndexBatchRequest` from the owned `WorkspaceIndex` (the closure walk moves
      here); add `#[cfg(test)] spawn_with`.
- [x] `server.rs` — send `Request::Navigate(NavigateRequest::…)`; the shell's
      definition/declaration/references handlers read `Response::Navigate`.
- [x] `src/navigate.rs` (new) — the async `spawn`/`serve` and the resolution
      moved from `index/navigate.rs`, reading the index over the hub; delete
      `src/index/navigate.rs`; drop navigation from `index`'s `serve`; declare
      `pub mod navigate;` in `lib.rs`; spawn it in `main.rs::setup`.
- [x] Move the navigation tests into `src/navigate.rs`, driving a test hub
      (`index::spawn_with` + `navigate::spawn` + a registered client) instead of
      a bare `WorkspaceIndex`; keep them covering the same cases.
- [x] `docs/architecture.md` — rewrite the "Navigation is the index's own"
      decision, the navigation sentence in the "index is its own subsystem"
      decision, the Layout, the module table (index, shell, new navigate row),
      the data-flow diagram, and the shared-parsing text.
- [x] `docs/dev/changelog.md` — add today's entry for the extraction.
- [x] Verify pass 2: `cargo build --all-targets`, `cargo fmt --all -- --check`,
      `cargo test` (library, harness, `example_features`, stdio smoke) green.

## Progress

Every step landed, in three verified passes.

- **Java helpers** — `src/parser.rs` holds `java_parser`, `LineIndex`,
  `type_from_node`, and `file_package`; `is_type_kind` is gone, replaced by
  `SymbolKind::is_type()`.
- **Vocabulary and read API** — `src/messages.rs` splits the requests by owner
  (`NavigateRequest`/`NavigateResponse`; `IndexRequest`/`IndexResponse`;
  `IndexBatchRequest`/`IndexBatchResponse`; `Response::Empty` as the default);
  `hub::owner_of` routes each family; `src/index/read.rs` answers the six coarse
  reads from the owned `WorkspaceIndex` (the supertype-closure walk moved here)
  with `#[cfg(test)] index::spawn_with` for tests.
- **The `navigate` module** — `src/index/navigate.rs` deleted; `src/navigate.rs`
  is an async hub module (`navigate`) serving `NavigateRequest`, reading files
  with `tokio::fs` and the index only through the coarse reads (the recursive AST
  walks stay sync; a flat async loop resolves the collected nodes, batching the
  receiver closures). `server.rs` routes to `Request::Navigate`; `main.rs::setup`
  spawns it; its tests drive a test hub.
- **Docs** — `docs/architecture.md` (rework note, Layout, module table,
  data-flow diagram, the navigation and index implementation notes, and the
  "index is its own subsystem" decision), `docs/dev/changelog.md` (the entry),
  and `docs/log.md` (the reconciliation).

Verified: `cargo build --all-targets` and `cargo fmt --all -- --check` clean
(only the eight pre-existing dead-code warnings), `cargo test` green — 38
library tests and 7 bench tests, including the moved navigation tests and the
new read-API tests. `tests/example_features.rs`, `tests/harness.rs`, and
`tests/stdio_smoke.rs` are `#![cfg(any())]`-disabled during the rework and so
contribute no tests (unchanged by this refactor) — the reason acceptance
criterion 5's `example_features` mention cannot be exercised today.

Implemented via sub-agents; not committed.
