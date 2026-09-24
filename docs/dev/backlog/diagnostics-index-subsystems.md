---
type: ChangeRequest
kind: refactor
title: Make the index and the diagnostics engine their own subsystems
description: Lift the workspace symbol index and the diagnostics engine out of the analysis core and the hub, each into its own message-driven subsystem, leaving the engine a hub that binds them together.
state: done
priority: medium
tags: [dev, refactor, messaging, indexing, diagnostics]
owner: felix
verified:
  by: cargo test --all-targets — lib 284 passed / 4 failed, harness 28 passed / 1 failed, stdio 1 passed, bins 7 passed; the 5 failures are the known sandbox loopback binds (the `sources` `TestServer` and the harness `SourceServer`), not regressions
  at: 2026-09-29T00:00:00Z
---

# Problem

The engine documents one message boundary (`driver-message-bus`), but two of its
heaviest concerns still sit outside it, tangled into the analysis core.

- **The index has no subsystem of its own.** `WorkspaceIndex` (`index.rs`) is
  owned directly by `TreeSitterEngine` (`analysis.rs`) and mutated from two
  places: the hub task applies the drivers' warm-up messages to it
  (`engine.rs::apply_to_index`), while the core upserts and removes entries on
  every open / change / close / watched-file event. Every query in `analysis.rs`
  reaches it as `self.index...`, and a dozen free functions take `&WorkspaceIndex`
  (`declaration_target`, `member_declarations`, `declared_in_workspace`,
  `workspace_type_entry`, `import_edit`, `semantic_diagnostics`, `SemanticCheck`,
  …). Nothing but "hold the same shared handle" is offered to a subsystem that
  needs a symbol.
- **The diagnostics engine is not a subsystem.** Diagnostics are computed by the
  core (`TreeSitterEngine::diagnostics` → `semantic_diagnostics` /
  `SemanticCheck`) and merely _scheduled_ by `DiagnosticsPublisher` in
  `engine.rs`, which holds the core and calls into it rather than exchanging
  messages.

Cost: the "one boundary, one hub" claim holds for the warm-up drivers but not for
the two components the request path leans on hardest; a new consumer of the index
means a new direct coupling to the core; and the engine is still a hub _plus_ two
domain stores rather than the hub that binds subsystems together.

# Proposal

Give the index — and, next, the diagnostics engine — each their own
message-driven subsystem, and leave the engine as the hub that binds them.

- **The index subsystem** (`index.rs`) owns the `WorkspaceIndex` and is
  its sole reader and writer. Every other component reaches it through
  `IndexHandle`, a cheap, cloneable handle: each call is one message to the
  subsystem, with a reply for a query. The hub hands it the index-affecting
  `DriverMessage`s instead of applying them itself; the analysis core uses the
  handle for every symbol lookup and edit.
- **The diagnostics subsystem** (`diagnostics.rs`) owns its own source parsing
  and reports diagnostics to the hub as messages, rather than being scheduled by
  the hub and computing inside the core. It reads the symbol index and the
  declared-type layer from the index subsystem through `IndexHandle`.

# Decisions

- **D1 — The index is a subsystem on its own dedicated thread.** `IndexHandle`
  owns a `std::sync::mpsc` sender to a subsystem thread that owns the single
  `WorkspaceIndex`. A dedicated thread (not a `tokio` task) is required because
  the hub task itself issues synchronous index calls while dispatching
  (`open`/`change`/`close`), and blocking a runtime worker on a task that must run
  on the same runtime would deadlock on a single-worker runtime. Each method blocks
  for the reply, preserving today's "the index reflects the call on return"
  semantics. Accepted tradeoff: every index access is now a channel round-trip;
  queries return shared `Arc`s, so the cost is the hop, not a deep copy.
- **D2 — `IndexHandle` is the index's message API.** Every `WorkspaceIndex`
  method the core or a driver needs is mirrored on the handle (`upsert_file`,
  `remove_file`, `query_name`, `query_prefix`, `type_model`,
  `source_layer_index`, `source_files`, `source_roots`, `ready`, …), so call
  sites change type only, not shape.
- **D3 — Queries are messages to the subsystem, not round-trips through the hub.**
  Routing a query through the hub task would deadlock, because the hub task makes
  synchronous index calls during dispatch. The hub owns the wiring and hands
  subsystems the handle; the messages go straight to the owning subsystem. (The
  hub remains the sole applier of the _bus_ messages it receives.)
- **D4 — The hub stops touching the index.** `engine.rs::apply_to_index` moves
  into the subsystem as `IndexHandle::apply`; the hub calls it for each message
  and no longer holds the index at all.
- **D5 — Behaviour is preserved.** The same index results, the same warm-up
  ordering, and the same diagnostics gating; the existing tests pass unchanged
  (apart from the boundary's own tests).
- **D6 — Two subsystems, one boundary.** Both the index and the diagnostics
  engine are now their own subsystems. Each parses its own source text (the index
  parses files for entries; diagnostics parses the open buffers for its checks),
  rather than sharing the core's parse. The declared-type overlay the checks need
  lives with the index subsystem (it is index/type-layer state), so the
  diagnostics subsystem reads it through the same handle and the core no longer
  owns it.

# Acceptance criteria

Index subsystem (met):

- The `WorkspaceIndex` is owned by the index subsystem (`index.rs`); no
  `WorkspaceIndex` field remains in the core or the hub.
- The core and the hub reach the index only through `IndexHandle`; there is no
  `apply_to_index` in `engine.rs`.
- The existing test suite is green apart from the known sandbox loopback-bind
  failures (`sources` `TestServer`, harness `SourceServer`).

Diagnostics subsystem (met):

- Diagnostics are computed by a diagnostics subsystem (`diagnostics.rs`) that
  parses each open buffer itself and reports them to the hub as messages; the
  `DiagnosticsPublisher` is gone and nothing in `engine.rs` reaches into the core
  for diagnostics.
- The declared-type overlay lives in the index subsystem and is read through
  `IndexHandle`; the core no longer owns a `dirty` map.

# Implementation plan

## Approach

The index subsystem is a thread that owns the `WorkspaceIndex` and serves jobs
sent over a single channel. `IndexHandle` packages each call as one job: a query
carries a reply channel inside the job and blocks for the result; a mutation does
not. `WorkspaceIndex` is already a cheaply-cloneable, internally-synchronized
value, so no index data is copied across the boundary — only the calls are.

The core (`TreeSitterEngine`) holds an `IndexHandle` instead of a
`WorkspaceIndex`; `with_index` lets the hub hand it the same subsystem the hub
routes to. The free functions that took `&WorkspaceIndex` take `&IndexHandle`.
The hub builds the subsystem, gives the core its handle, and replaces
`apply_to_index(message, index)` with `index.apply(message.clone())`.

The diagnostics subsystem (`src/diagnostics.rs`) owns a parser and the open
buffers' text. The hub forwards each open/change (text + version) and each close;
a watched-file event asks for a re-sweep. A coalescing sweep parses each open
buffer itself and computes the pass with `diagnostics_for`, reading the index and
the declared-type layer through `IndexHandle`, and reports one
`DriverMessage::Diagnostics` per document that the hub translates to
`EngineEvent::Diagnostics`. The declared-type overlay moves from the core into
the index subsystem (it is index/type-layer state), exposed as
`IndexHandle::type_layers`; the core delegates `record_types`/`drop_types`/
`type_layers` to it. `TreeSitterEngine::diagnostics` is kept as a thin delegator
for the core's own tests.

## Steps

- [x] Fold the index subsystem into `src/index.rs`: `IndexHandle`, the
      subsystem thread, and the moved `apply_to_index`. (AC: the index is a
      subsystem)
- [x] Rework `src/analysis.rs` onto `IndexHandle`: the field, `new`/`with_index`,
      the `index()` accessor, and the free functions' `&WorkspaceIndex` →
      `&IndexHandle` parameters; `set_workspace_root`'s test warm-up follows.
- [x] Rework `src/engine.rs`: build the subsystem in `spawn`, give the core its
      handle, and replace `apply_to_index` in `apply` with `IndexHandle::apply`;
      update the boundary test.
- [x] Rework `src/index.rs`'s test warm-up (`warm_up_sync`) and `src/sources.rs`'
      download test onto the handle.
- [x] Update `docs/architecture.md` (layout, data-flow diagram, the
      `WorkspaceIndex` bullet, the hub paragraph, and the decisions).
- [x] Move the declared-type overlay into the index subsystem (`type_layers`,
      `record_dirty_type`, `drop_dirty_type`); the core delegates to it. (AC: the
      overlay is index state)
- [x] Move the diagnostics computation (syntax errors and the unresolved-symbol
      checks) into `src/diagnostics.rs`, and run the diagnostics subsystem: its
      own parser and open-document state, fed by the hub, reporting
      `DriverMessage::Diagnostics`. Retire `DiagnosticsPublisher`. (AC: diagnostics
      is a subsystem)
- [x] Append the changelog entry, update `docs/architecture.md`, and mark this
      request `done`.
