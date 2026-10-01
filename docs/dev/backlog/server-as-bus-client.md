---
type: ChangeRequest
kind: refactor
title: Make the LSP shell just another bus client
description: Replace the shell's bespoke Command/EngineHandle/EngineEvent channels and the engine dispatcher with a BusClient — the shell sends notifications and requests, a new Analysis module owns the core's queries, and the shell consumes the editor-facing notifications itself.
state: done
priority: medium
tags: [dev, refactor, messaging, lsp]
owner: felix
verified:
  by: cargo build --all-targets is clean with no warnings; cargo test passes (297 library tests, 29 harness tests, the stdio smoke test), run outside the sandbox because the sources tests bind a local test server
  at: 2026-10-01T22:00:00Z
---

# Problem

The engine bus (`unified-bus.md`) made every module a peer — except the shell.
`JavaLanguageServer` still reaches the engine over two bespoke channels:

- **In:** `EngineHandle` sends a `Command` (an mpsc with a `oneshot` per query)
  to the `engine.rs::dispatch` task, which runs mutations on `TreeSitterEngine`
  inline, spawns queries, and re-emits the document events onto the bus.
  `Command` is a second vocabulary that mostly duplicates `DriverMessage`
  (`Open`/`DocumentOpened`, `SetWorkspaceRoot`/`FolderAdded`,
  `WatchedFiles`/`FileEvent`, `SetClientCapabilities`/`ClientCapabilities`) and
  `Request` (`CodeActions`/`QuickFixForDocument`,
  `IndexedSymbols`/`IndexAllSymbols`, `IndexReady`/`IndexReady`).
- **Out:** `EngineEvent` over its own channel. Because of it the hub is not
  neutral: `bus::spawn_router` takes the `events` sender and runs
  `messages::translate` on every notification. The dispatcher also skips the bus
  on `Close`: it writes the diagnostics clear straight to `events`. And
  `TreeSitterEngine.events`/`set_events` is dead (set, never read).

As a result, every new editor-facing feature has to be wired in three places
(`Command`, `EngineHandle`, `dispatch`), the hub carries editor knowledge, and the
core is the one component that still is not a bus module (`unified-bus.md` D6).

# Proposal

The shell holds a `BusClient` (labeled `server`) and registers with the hub as
a module, like the index, diagnostics, and quick-fix modules. It sends the
document lifecycle, root, watched files, and client capabilities as
notifications (`DocumentOpened/Changed/Closed`, `FolderAdded`, `FileEvent`,
`ClientCapabilities`). It sends every query as a `Request`. It consumes the
`Diagnostics`, `Progress`, and `Notice` notifications from its own channel and
renders them to the client, the way its drain task does today.

A new **Analysis module** wraps `TreeSitterEngine` and owns the query requests
(hover, definition, implementation, completions, document symbols, folding
ranges, semantic tokens, inlay hints, signature help, references, rename,
workspace symbols). Code actions stay with the quick-fix module. With this,
`Command`, `EngineHandle`, `EngineEvent`, `dispatch`, and `translate` are gone,
and the hub only broadcasts, routes, and logs.

# Decisions

- **D1 — Scope: the shell is a peer module, and the core is a request-owning
  module.** A new `Module::Analysis` owns the query `Request`s. The shell sends
  the notifications and requests listed above and subscribes to the bus.
  `DocumentStore` stays in the shell, because turning incremental LSP edits into
  full text is LSP-specific. Reason: one vocabulary and one mechanism for every
  component.
- **D2 — Async requests on `BusClient`.** `ReplyHandle` switches from
  `std::sync::mpsc` to `tokio::sync::oneshot`. `BusClient` keeps its blocking
  `request` (via `blocking_recv`) and gains an async variant that the shell
  awaits. Reason: the shell's handlers are async, and the harness runs on a
  current-thread runtime, so blocking there, or wrapping every handler in
  `spawn_blocking`, is wrong or wasteful.
- **D3 — The Analysis module runs on its own thread, and its queries run on the
  blocking pool.** The module thread applies notifications to the core inline,
  in arrival order. It hands each query to the blocking pool, which runs on the
  runtime's large-stack threads (see `main.rs` `RUNTIME_STACK_SIZE`), and the
  query answers through its `ReplyHandle`. Ordering is preserved: one sender, a
  FIFO hub, and a FIFO module channel mean an edit still lands before the query
  the client sends at the new cursor. A slow query still never delays a later
  edit. Reason: this matches the index and diagnostics modules, and the queries
  block on index requests anyway.
- **D4 — The hub becomes neutral.** `spawn_router` no longer takes `events`, and
  `messages::translate` is removed. The hub keeps rendering `Log` and `Summary`
  to `tracing` as part of its existing logging role (so the
  `workspace index warm-up complete` line the bench parses is unchanged). The
  shell renders `Diagnostics`, `Progress`, and `Notice`. The progress and notice
  gating on `window.workDoneProgress` stays in the shell.
- **D5 — The diagnostics module clears a closed document.** On
  `DocumentClosed` it emits `Diagnostics { version: None, diagnostics: [] }`
  before republishing the remaining open documents. The shell publishes no
  clear of its own.
- **D6 — Naming and test hooks.** `engine.rs` keeps the name and becomes only
  the wiring (spawning the hub, the modules, and the drivers) plus the drivers.
  `JavaLanguageServer::engine()` becomes `bus()`, returning the shell's
  `BusClient`. The harness's `wait_for_index` uses `all_symbols()` and `ready()`
  on it (async variants). `Command::IndexedSymbols` and `Command::IndexReady` go
  away. The dead `TreeSitterEngine.events`/`set_events` is deleted.
- **D7 — Behaviour preserved.** The LSP surface, the published diagnostics
  (including the clear on close and the republish of the other documents),
  progress, notices, the watcher registration, and the warm-up are unchanged.
- **D8 — The diagnostics sweep follows `AnalysisUpdated`** (found while
  planning). Before, the core applied an edit and sent its index updates before
  the document event reached the diagnostics module. With the shell
  broadcasting directly, the two modules would race. The analysis module
  therefore notifies `AnalysisUpdated` after applying a document or file event,
  and the diagnostics module sweeps on that. Ordering through the FIFO hub keeps
  the old guarantee: the sweep reads an index that holds the edit.
- **D9 — The analysis thread gets the runtime's stack size** (found while
  implementing). `store_tree` used to run on a runtime worker with the 256 MiB
  stack from `runtime-stack-overflow`. On a default 2 MiB thread it would bring
  back that abort. `RUNTIME_STACK_SIZE` moves to `lib.rs` and sizes both the
  runtime and the analysis module's thread.
- **D2 detail.** `ReplyHandle` carries a boxed deliverer rather than a channel.
  The blocking request delivers into a `std` channel, because tokio's
  `blocking_recv` panics inside a runtime context. The async request delivers
  into a `tokio` oneshot.

# Acceptance criteria

- `JavaLanguageServer` holds a `BusClient` and a bus channel. It has no
  `EngineHandle`, and nothing outside the shell renders LSP notifications.
- `Command`, `EngineHandle`, `EngineEvent`, `engine::dispatch`, and
  `messages::translate` no longer exist. `spawn_router` takes no editor event
  sender.
- `Module::Analysis` owns the query requests. `owner_of` routes them there, and
  the hub log names `analysis` as the responder.
- `BusClient` offers both blocking and async requests; the async ones await a
  `tokio` oneshot reply.
- Closing a document clears its diagnostics via a `Diagnostics` notification
  from the diagnostics module.
- `TreeSitterEngine.events`/`set_events` is gone.
- `cargo build` is clean with no new warnings. The library tests, the harness
  (`tests/harness.rs`, switched to `server.bus()`), and `tests/stdio_smoke.rs`
  pass.
- An edit followed immediately by a query at the new position sees the edit:
  the existing harness tests that do this pass. Add a targeted test if none
  exercises it through the bus.

# Docs to update

- `docs/architecture.md`:
  - Layout: `engine.rs` becomes the wiring and drivers; `bus.rs` gains
    `Module::Analysis` and async requests; `messages.rs` loses `Command`,
    `EngineEvent`, and `translate`; `server.rs` is a bus module.
  - Mermaid diagram: the shell sits on the hub. Drop `EngineHandle` and the
    dispatcher, and add the Analysis module.
  - Rewrite the **LSP shell** and **Engine boundary** bullets: notifications
    and requests replace commands, the shell consumes the editor-facing
    notifications, and the ordering argument now rests on the FIFO hub and the
    Analysis module's inline mutations.
  - Remove the "hub is the sole translator" wording. Note in the **Diagnostics
    subsystem** bullet that it emits the close clear.
- `docs/dev/backlog/unified-bus.md`: this request supersedes its D6 ("the core
  is not itself a bus consumer"). Leave that request as is; reference it from
  the changelog entry.

# Implementation plan

## Approach

**Ordering finding (D8).** Today the dispatcher applies an edit to the core
first. The core then notifies `SourceEntries`/`DirtyTypes` to the index, and
only after that is `DocumentChanged` broadcast. So the diagnostics sweep reads an
overlay that already holds the edit. If the shell broadcast the edit straight to
both modules, the diagnostics module would race the analysis module and could
sweep a stale overlay. That would break the cross-file republish, where a
referring file's squiggles clear without an edit of its own.

The fix: after the Analysis module applies `DocumentOpened`, `DocumentChanged`,
`DocumentClosed`, or `FileEvent`, it notifies `DriverMessage::AnalysisUpdated`.
The diagnostics module records text on open and change and drops it on close,
emitting the clear. It **sweeps on `AnalysisUpdated`** instead of on the
lifecycle messages and `FileEvent`. The analysis thread sends its index
notifications before `AnalysisUpdated`, the hub is FIFO, and the index channel
is FIFO. So the sweep's `type_layers` request is answered after the index has
applied the edit, which is the same guarantee as today.

**Reply delivery (D2 detail).** `ReplyHandle<R>` stops holding an
`std_mpsc::Sender` and holds a boxed `FnOnce(R) + Send` deliverer instead. The
blocking `request` delivers into a `std_mpsc` channel. It does not use tokio's
`blocking_recv`, which panics inside a runtime context, and the drivers and
tests do call blocking requests there. The new `request_async` delivers into a
`tokio::sync::oneshot` and awaits it. Both fall back to `R::default()` when the
bus is gone.

**Bus wiring.** `spawn_router(modules, notifications, owners)`: the shell's
channel is just one more entry in `modules`. The hub keeps its logging and adds
the `Log`/`Summary` rendering that `translate` did. `engine::spawn(shell)`
takes the shell's bus sender, starts the index, diagnostics, quick-fix, and
Analysis modules and the six drivers, and returns the shell's `BusClient`
(labeled `server`).

**Analysis module** (`analysis::spawn_module`). It runs on a thread named
`java-lsp-analysis` and owns an `Arc<TreeSitterEngine>`, built over the module's
own client, so the engine's index requests log as `analysis` too. It consumes
notifications inline:

- `FolderAdded` sets the workspace root.
- `ClientCapabilities` sets resource operations.
- `DocumentOpened` and `DocumentChanged` call `open`/`change`.
- `DocumentClosed` calls `close`.
- `FileEvent` calls `watched_files`.

Each of the last three is followed by `AnalysisUpdated`. Each `Request::Analysis*`
is handed to the runtime's blocking pool through a captured
`tokio::runtime::Handle`.

**Shell.** `JavaLanguageServer` holds the `BusClient`. Its drain task reads
its bus channel and renders `Diagnostics`, `Progress`, and `Notice`, gated as
today. Handlers notify, or `await` the async request methods. `bus()`
replaces `engine()`.

## Steps

- [x] `messages.rs`: remove `Command`, `EngineEvent`, and `translate`. Add the
      `Request::Analysis*` variants (hover, definition, implementation,
      completions, document symbols, folding ranges, semantic tokens, inlay
      hints, signature help, references, rename, workspace symbols) and
      `DriverMessage::AnalysisUpdated`. Make `ReplyHandle` carry a boxed
      deliverer.
- [x] `bus.rs`: add `Module::Analysis` and its `owner_of`, `module_label`, and
      `describe_request` arms, and describe `AnalysisUpdated`. Add
      `request_async` and async methods for the shell's queries,
      `code_actions`, `all_symbols`, and `ready`. Drop `events` from
      `spawn_router`, and have the hub render `Log`/`Summary` to `tracing`.
- [x] `analysis.rs`: add `spawn_module` (the Analysis module) and remove the
      dead `events`/`set_events`.
- [x] `diagnostics.rs`: sweep on `AnalysisUpdated`, and on `DocumentClosed`
      drop the document and emit `Diagnostics { version: None }`.
- [x] `engine.rs`: `spawn(shell)` returns the shell's `BusClient`. Remove
      `EngineHandle`, `dispatch`, and `read`, and rewrite the module test
      without `translate`.
- [x] `server.rs`: hold the `BusClient`, drain the bus channel, and send
      notifications and async requests. `bus()` replaces `engine()`.
- [x] Tests: the quick-fix module test uses the new `spawn_router`. The
      harness uses `server.bus()` with `all_symbols_async`/`ready_async`.
- [x] `docs/architecture.md`: rewrite the layout entries for `engine.rs`,
      `bus.rs`, `messages.rs`, and `server.rs`, and redraw the diagram with the
      shell and the Analysis module on the hub. Rewrite the **LSP shell** and
      **Engine boundary** bullets: notifications and requests replace commands,
      and ordering rests on the FIFO hub, inline mutations, and
      `AnalysisUpdated`. In the **Diagnostics subsystem** bullet, the close
      clear and the sweep trigger.
- [x] `cargo build`, the library tests, the harness, and `stdio_smoke` pass.
- [x] Size the analysis module's thread with `RUNTIME_STACK_SIZE` (D9), and
      update the stack-size decision in `docs/architecture.md`.
- [x] `docs/dev/changelog.md` entry, and mark this request done.

Implemented on top of `f4b2884`; not yet committed.
