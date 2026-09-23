---
type: ChangeRequest
kind: refactor
title: Message-based engine boundary
description: Replace the SemanticEngine trait and its locked handle with command/event queues between the shell and the analysis core.
state: done
priority: high
tags: [dev, refactor, engine, messaging]
owner: felix
verified:
  by: cargo test --all-targets (193 passed)
  at: 2026-09-23T20:51:50Z
---

# Problem

The shell talks to the analysis backend through a trait — `SemanticEngine`
(`src/engine/mod.rs:16`, 17 methods) held as `Arc<RwLock<Box<dyn
SemanticEngine>>>` (`src/server.rs:37`) and read-locked in all ~16 handler
sites. The boundary is **one-directional**: `&self` queries can return a value,
but the engine can never push anything to the shell. That is why diagnostics are
*pulled* by the shell after every edit rather than pushed, and why an entire
seam had to be invented to report warm-up progress — there is no reverse
channel. The abstraction also carries dead weight: `SyntaxOnlyEngine`
(`src/engine/stub.rs`) is no longer selected by the shell (it is only
re-exported, and the two tests/comments calling it "the stub" actually exercise
the real engine), so new backends must implement 17 methods for no isolation the
shell uses today.

# Proposal

Replace the trait with a message boundary between the shell and a single
analysis core:

- **`src/engine.rs`** (the message layer) — a `Command` enum (shell → engine:
  one variant per handler plus `SetWorkspaceRoot`), an `EngineEvent` enum
  (engine → shell: today `Diagnostics { uri, version, diagnostics }`; this is
  where progress will ride later), an `EngineHandle` the shell clones and calls
  (`handle.hover(..).await`, each command carrying a `oneshot` reply), and the
  dispatcher task that owns the core.
- **`src/analysis.rs`** (the core) — today's `src/engine/syntax.rs` moved and
  flattened, `TreeSitterEngine` kept as a concrete `Send + Sync` type whose
  methods become **inherent** instead of a trait impl. It keeps the internal
  locks it already has.
- **Dispatcher semantics**: mutations and orchestration (`SetWorkspaceRoot`,
  `Open`, `Change`, `Close`) are applied **inline, in arrival order** — the one
  place ordering matters — while read-only queries are **spawned**, so a slow
  `references` never delays typing. This preserves today's concurrency and R6.
- **Diagnostics become an event**: after an applied open/change the dispatcher
  emits `EngineEvent::Diagnostics`; the shell's drain task publishes it. The
  observable protocol behaviour is unchanged.
- **Delete** the `SemanticEngine` trait and `SyntaxOnlyEngine`.

# Decisions

- **Option 2 (dispatcher + concurrent reads), not a strict actor.** A
  single-task actor would serialize every request behind the slowest one and
  regress R6 (`references` over a big workspace would block `didChange`); it
  also cannot host the multi-second warm-up on its task, so the simplicity it
  promises is partly illusory. The dispatcher keeps today's latency profile and
  still gives the channel boundary.
- **`EngineHandle` is the new seam; the trait and the stub are removed.** The
  isolation the trait was standing in for is now expressed by the boundary
  itself, which is also the shape a future out-of-process engine would ship
  across — matching the "analysis daemon, later" note in `requirements.md`. The
  two stale "stub engine" test names/comments are corrected to say what they
  actually exercise.
- **The core stays a plain synchronous type, unit-tested directly.** The
  analysis logic and its large synchronous test surface do not move behind the
  queue; only `tests/harness.rs` changes (its index polling goes through the
  handle). This keeps ~160 unit tests meaningful and the actor thin.
- **Events use an unbounded channel; commands a bounded one.** Emitting an event
  must never stall the dispatcher (tower-lsp's client socket has capacity 1, so
  a blocked `publishDiagnostics` would otherwise back up into request handling).
  Command senders are bounded for normal backpressure. Event volume is one
  message per edit, so unbounded is safe here.
- **No behaviour change.** Same results, same notifications at the same points,
  same non-blocking guarantees. Query failures are impossible by construction
  today (a lock cannot fail); with a queue the engine may be gone, so a dropped
  reply yields the empty result the trait used to default to (`None`, empty
  list, or `true` for `index_ready`).
- **Out of scope, deliberately**: a separate analysis process/daemon, request
  cancellation, multi-root workspaces, and any change to query results or the
  warm-up/source-download behaviour.
- **Flattened layout**: `src/engine.rs` and `src/analysis.rs`; the `src/engine/`
  directory and `src/engine/stub.rs` are removed. No new dependencies (`tokio`'s
  `mpsc`/`oneshot` are already available).
- **This supersedes the trait-seam decision** recorded in `docs/architecture.md`
  (the "`SemanticEngine` trait seam gives the same isolation" note and the
  `SyntaxOnlyEngine` conformance-reference bullet); the docs are reworked as
  part of this change.

# Acceptance criteria

- `SemanticEngine`, `SyntaxOnlyEngine`, and `src/engine/` are gone;
  `src/engine.rs` (Command/EngineEvent/EngineHandle/dispatcher) and
  `src/analysis.rs` (the concrete core) are in place, and the shell holds an
  `EngineHandle`, not `Arc<RwLock<Box<dyn …>>>`.
- Every existing test still passes (`cargo test --all-targets`), and the harness
  drives queries and index polling through the handle.
- Diagnostics are published in response to `didOpen`/`didChange` (and cleared on
  `didClose`) exactly as before, now driven by `EngineEvent::Diagnostics`.
- A read-only query cannot delay a mutation: dispatching a slow query does not
  block `didChange`/`didOpen` handling (verified by the design and the existing
  non-blocking bench).
- The core remains synchronous: its unit tests call the concrete type directly
  with no queue involved.
- `docs/architecture.md` describes the message boundary instead of the trait
  seam — title/description, the layout tree, the component bullets, the
  data-flow diagram, and the decisions section all updated; no dangling
  reference to `SemanticEngine`.

# Docs touched

- `docs/architecture.md` — **rework**: front-matter description, the "single
  crate / trait seam" opening, the layout tree (`engine.rs`, `analysis.rs`,
  no `engine/`), the `SemanticEngine` component bullet (replaced by a
  command/event-boundary bullet), the shell bullet (handlers dispatch through
  the handle; diagnostics arrive as events), the `TreeSitterEngine` bullet
  (concrete core in `analysis.rs`), the data-flow diagram (shell ⇄ queue ⇄
  core), and the decisions section (the trait-seam note replaced).
- `docs/requirements.md` — a one-line note that the shell/engine isolation is
  provided by a message boundary (only if it reads as drifting after the
  rework; its technology choices do not name the trait).

# Implementation plan

## Approach

Two new flattened modules, a mechanical move of the core, and a shell rewrite;
no new dependencies and no behaviour change. `tokio::sync::mpsc` + `oneshot`
carry the boundary; `tokio::spawn` runs the dispatcher and the read tasks.

- **`src/analysis.rs`** — `git mv src/engine/syntax.rs`; then update the module
  doc, drop `use super::SemanticEngine;`, and turn `impl SemanticEngine for
  TreeSitterEngine` into `impl TreeSitterEngine`. The type, its fields, its
  internal locks, and its ~160 unit tests are untouched (the tests' `use
  super::*` keeps working). Delete `src/engine/stub.rs` and `src/engine/mod.rs`.
- **`src/engine.rs`** — `Command` (SetWorkspaceRoot, Open, Change, Close, and one
  variant per query, each carrying `oneshot::Sender<T>`), `EngineEvent`
  (Diagnostics), `EngineHandle` (`Clone`; async methods per query, a private
  `request<T>` helper that sends the command and awaits the reply, defaulting to
  the empty value when the engine is gone), and `spawn(events) -> EngineHandle`
  which creates the core and the dispatcher task. `dispatch` applies mutations
  inline and emits `Diagnostics` after open/change (and an empty one after
  close); it hands read commands to a `spawn_read` helper that runs the query on
  a spawned task and replies.
- **`src/lib.rs`** — `pub mod analysis;` and `pub mod engine;` replace
  `pub mod engine` (directory).
- **`src/server.rs`** — hold `engine: EngineHandle`; `new` creates the unbounded
  event channel, spawns the drain task (publish diagnostics), and calls
  `engine::spawn`; `initialized` awaits `set_workspace_root`; `did_open`/
  `did_change`/`did_close` update the store then send the command (no publish
  call — the event does it); every query handler awaits the handle; drop
  `publish_engine_diagnostics` and the `SemanticEngine`/`TreeSitterEngine`
  imports; `engine()` returns `EngineHandle`.
- **`tests/harness.rs`** — drop the `SemanticEngine` import; `wait_for_index`
  takes `&EngineHandle` and awaits `indexed_symbols()`/`index_ready()`; the few
  direct `engine.read().await.indexed_symbols()` sites become
  `engine.indexed_symbols().await`; rename the "stub engine" test/comment to
  reflect the real engine.
- **`docs/architecture.md`** — the rework described above.

## Steps

- [x] Move the core: `src/engine/syntax.rs` → `src/analysis.rs`, update its
      module doc, drop the `super::SemanticEngine` import, and convert the trait
      impl to an inherent `impl TreeSitterEngine`; delete `src/engine/mod.rs`
      and `src/engine/stub.rs`; update `src/lib.rs`. (AC: trait/stub gone, core
      flattened, unit tests untouched)
- [x] Add `src/engine.rs`: `Command`, `EngineEvent`, `EngineHandle` + `request`,
      `spawn`, and the dispatcher (mutations inline in order; reads spawned;
      diagnostics emitted as events). (AC: command/event boundary exists)
- [x] Rewrite the shell (`src/server.rs`) onto `EngineHandle` and the event
      drain task. (AC: shell holds a handle; diagnostics still published on
      open/change and cleared on close)
- [x] Update `tests/harness.rs` (handle-based polling, drop the trait import,
      fix the stale "stub" naming) and run `cargo test --all-targets`.
      (AC: all tests pass; a slow query does not block mutations)
- [x] Rework `docs/architecture.md` per the doc note above. (Doc step — the
      architecture must describe the boundary, not the removed trait)
