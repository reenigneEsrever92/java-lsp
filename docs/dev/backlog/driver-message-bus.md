---
type: ChangeRequest
kind: refactor
title: Make every subsystem a message-driven driver on one engine-owned bus
description: Discovery is a synchronous pre-step that mutates the index outside the boundary and the Reporter is a second reporting vocabulary threaded through every producer, while the filesystem side has no driver of its own; make every subsystem a driver spawned at start that communicates only through one engine-owned message module, broadcast by the engine, which alone turns messages into editor reporting.
state: done
priority: high
tags: [dev, refactor, messaging, indexing]
owner: felix
verified:
  by: cargo test --all-targets — lib 275 passed / 4 failed, bin 7 passed, harness 28 passed / 1 failed, stdio 1 passed; the 5 failures are loopback-socket binds this sandbox forbids (the `sources` `TestServer` and the harness `SourceServer`), not regressions
  at: 2026-09-28T00:00:00Z
---

# Problem

The engine documents one message boundary (`message-based-engine`), but three things sit outside it.

- **Discovery is a synchronous pre-step that bypasses the boundary.**
  `discover_workspace` (`src/index.rs`) is one blocking call that walks for
  `pom.xml`, assembles the Maven effective poms, walks each source root for
  `.java`, resolves the dependency closure and stats the jars, and returns
  `Discovered { model, files, artifacts }`. The warm-up driver then applies the
  model with a direct `WorkspaceIndex::set_model(...)`, for which there is no
  `IndexMessage` variant, before spawning the four producers with the lists it
  produced. So the first and heaviest stage is not a message, and it is the one
  mutation the "the indexing task is the single point the warm-up mutates the
  index" invariant does not cover.

- **`Reporter` is a second vocabulary passed everywhere.** `Reporter`
  (`src/engine.rs`) wraps the `IndexMessage` sender with
  `begin`/`update`/`end`/`message`/`log`/`base_artifact`/`source_file`/`remove_base`/`ready`/`summary`
  and is threaded into `index_jars`, `index_jdk`, `index_source_files`,
  `run_core_producers`, `scan_workspace`/`scan_workspace_async`, and
  `sources::index_sources`. Producers therefore decide _when_ and _how_ to
  report, and every new producer must be handed the reporter to do anything at
  all.

- **The translator and the filesystem have no home.** `apply_message`
  (`src/index.rs`) both mutates the index and emits `EngineEvent`/`tracing`, so
  the "engine" module only defines the enums. The filesystem side is ad hoc: the
  client's `workspace/didChangeWatchedFiles` (registered for `**/*.java`,
  `src/server.rs::register_watcher`) is mapped to `Command::WatchedFiles` and
  handled inline by the core — there is no filesystem driver, no folder-level
  event, and adding one means new bespoke wiring.

Cost: the boundary is claimed to be universal, but discovery, the filesystem, and
reporting each keep their own path; a new subsystem means new plumbing; and the
"model before sources" ordering is a hand-maintained invariant rather than a
consequence of the topology.

# Proposal

Make every subsystem a **driver** that is spawned at start and speaks only the
engine's messages, over one engine-owned module:

- **One engine-owned message module.** All viable messages, incoming and
  outgoing, in one place: `Command` (shell → engine), the subsystem bus messages
  (folder added, file added/changed/deleted, project model, source file, base
  artifact, remove base, progress, notice, log, ready, summary), and
  `EngineEvent` (engine → shell). `Reporter` is deleted; drivers construct and
  send messages directly.
- **The engine is a broadcast hub and the sole translator.** It applies the
  index-affecting messages and turns the reporting ones into
  `EngineEvent`/`tracing`. Each bus message is forwarded to every driver; a
  driver selects the variants it cares about and decides its response. Nothing
  but the engine touches the editor.
- **The filesystem driver** surfaces the workspace root as an _added folder_
  message and relays the client's watched-file notifications as file
  added/changed/deleted messages. It does not enumerate and does not watch the
  OS; the **project driver walks**.
- **The project driver** reacts to the added root by walking for poms, assembling
  the Maven model and source roots, and emitting the model and the source-file
  inventory; a **dependency driver** derives the resolved jar set; the **source
  scanner**, **jar indexer**, **JDK indexer**, and **sources downloader** react to
  those messages. All drivers are spawned at start — there is no pre-step
  scheduling them.
- **Discovery dissolves.** There is no `discover_workspace` call and no direct
  `set_model`; the model, the file list, and the artifacts are ordinary bus
  messages applied by the engine, so the model-before-sources ordering follows
  from the engine's broadcast order rather than from a driver's sequencing.

# Decisions

- **D1 — One engine-owned module defines the whole vocabulary.** `Command`, the
  subsystem bus messages, `EngineEvent`, and the reporting value types
  (progress, notice level, log level) live together; `Reporter` is removed and
  drivers send the message values. Reason: the boundary is one thing and should
  be defined — and changed — in one place.
- **D2 — The engine is a broadcast hub, not a router.** Every bus message goes to
  every driver; each self-selects by variant. Reason: agreed; it keeps the hub
  dumb and the drivers autonomous, and adding a driver changes neither the
  engine nor the other drivers.
- **D3 — The engine is the sole translator to the editor.** Index-affecting
  messages are applied by the engine; reporting messages become
  `EngineEvent`/`tracing` there. `apply_message` moves from `src/index.rs` into
  the engine module. Reason: only the engine should send reporting to the editor
  based on the messages it sees.
- **D4 — Discovery dissolves into drivers; the model is a message.** The
  synchronous pre-step and the direct `set_model` are gone; the project driver
  walks and derives, the dependency driver resolves, and the resulting
  model/files/artifacts ride the bus. Reason: the pre-step is the last
  non-message path and the one index mutation outside the writer.
- **D5 — Filesystem driver shape.** It emits the root as an added-folder message
  and relays client `didChangeWatchedFiles` events as file messages; it neither
  enumerates nor OS-watches, so no new dependency is added and the project driver
  owns the walk. (Agreed.)
- **D6 — The model is dynamic.** A folder added after start recomputes the model
  and updates the drivers that depend on it. In practice this never happens, so
  it is a correctness property to preserve, not an optimization to chase.
  (Agreed.)
- **D7 — Behaviour is preserved.** The final index and declared-type model are
  identical for the same project; `ready` still means the core producers
  finished; the summary line (`workspace index warm-up complete` with
  `files`/`jars`/`jdk_classes`/`maven`/`elapsed_ms`), the progress/notice
  surface, and the diagnostics gating (semantic diagnostics wait for a completed
  source scan and an indexed `java.lang`) are unchanged. Only the internal
  scheduling and the code's shape may differ.
- **D8 — Drivers run on the runtime.** The no-runtime inline path
  (`scan_workspace`, used by unit tests) is replaced by running the drivers on a
  runtime, or a small synchronous driver harness; the ~160 core unit tests, which
  call `TreeSitterEngine` directly, are untouched.
- **D9 — Fold in `warmup-throughput` where it is subsumed.** Its scheduling and
  progress goals (parallelize the independent phases; keep the progress item
  moving) are largely dissolved by drivers that run concurrently and each report
  their own phase; its class-file base **cache** is orthogonal to the message
  topology and remains its own item. `warmup-throughput` is trimmed to the cache
  rather than dropped.
- **D10 — Out of scope.** Query results and the diagnostics algorithm, the Maven
  resolver's semantics (`resolve.rs`), request-path latency
  (`warmup-request-responsiveness`), OS-level filesystem watching, and multi-root
  workspaces.

# Acceptance criteria

- One engine-owned module defines every message, incoming and outgoing;
  `Reporter` is gone, and no type outside that module defines a message.
- Only the engine emits `EngineEvent`; `apply_message` lives in the engine module;
  no subsystem talks to the editor.
- There is no synchronous `discover_workspace` pre-step and no direct
  `set_model`: the model, files, and artifacts arrive as bus messages, and the
  index is mutated only by the engine applying a message.
- The filesystem, project, dependency, source-scanner, jar-indexer, JDK-indexer,
  and sources-downloader drivers are all spawned at start; none is scheduled by a
  pre-step.
- The filesystem driver emits the root as an added-folder message and relays
  client watched-file events, and does not enumerate the tree.
- The final index and declared-type model are identical to today for the same
  project (an equivalence check over entries and the merged base), and `ready`,
  the summary line, the progress/notice surface, and diagnostics gating are
  unchanged.
- `cargo test --all-targets` is green apart from the known sandbox loopback
  failures.
- The docs below are updated.

# Docs to update

- `docs/architecture.md` — the boundary bullet and the data-flow diagram (one
  engine-owned bus; the engine as broadcast hub and sole translator), the
  indexing bullet (discovery is a driver; the model is a message), the
  Maven/dependency-sources bullets, and the decisions section.
- `docs/dev/backlog/index.md` — this request's row, and the adjusted
  `warmup-throughput` scope.
- `docs/dev/backlog/warmup-throughput.md` — trimmed to the class-file base cache
  (D9).
- `docs/dev/changelog.md` — an entry at implementation.

# Implementation plan

## Approach

One engine-owned message module (`src/messages.rs`) defines every message:
`Command` (shell → engine), `EngineEvent` (engine → shell), and `DriverMessage`
(the subsystem bus, both directions between drivers and the engine). `Reporter`
and the message types currently in `engine.rs` move there; `Reporter` is deleted
and producers build `DriverMessage`s directly.

The engine (`src/engine.rs`) becomes a hub: `spawn` starts every driver and a
hub task. The hub loops over `DriverMessage`, applies the index-affecting ones
(authoritative mutation, moved out of `index.rs`), translates the reporting ones
to `EngineEvent`/`tracing`, and forwards each message to every driver. Drivers
select the variants they care about.

Drivers (all spawned at `spawn`, none scheduled by a pre-step):

- **filesystem** — receives the root and the client's watched-file events on a
  private channel and emits `FolderAdded` / `FileEvent`. Never enumerates.
- **project** (also the warm-up coordinator) — on `FolderAdded` walks for poms
  and source roots (`project::discover` + `collect_java_files`) and emits
  `ProjectModel` and `SourceInventory`; tracks `StageDone` and emits `Ready`,
  `Summary`, and the closing `Progress`. Emits `Begin` on the folder.
- **dependency** — on `ProjectModel`, resolves each module's closure against the
  local repository and emits `Artifacts` (plus the offline notice).
- **source scanner** — on `SourceInventory`, parses each file and emits
  `SourceFile`, then `StageDone(Sources, n)`.
- **jar indexer** — on `Artifacts`, reads each jar and emits `BaseArtifact`, then
  `StageDone(Jars, n)`.
- **JDK indexer** — at start, emits `BaseArtifact` per archive, then
  `StageDone(Jdk, n)`.
- **sources downloader** — on `Artifacts`, fetches/extracts sources and emits
  `BaseArtifact`/`RemoveBase`, then `StageDone(Downloads, _)`.

The reactive drivers replace `discover_workspace`, `scan_workspace`,
`scan_workspace_async`, and `run_core_producers`. Heavy work runs in
`spawn_blocking`; the producers take a plain message sink. For the ~160 core
unit tests (which call `TreeSitterEngine` directly and have no runtime),
`TreeSitterEngine::set_workspace_root` keeps a `#[cfg(test)]` synchronous
warm-up (`index::warm_up_sync`) that runs the same producers against the index
directly, so those tests are untouched. The integration harness exercises the
real hub.

## Steps

- [x] Add `src/messages.rs` with `Command`, `WatchedChange`, `EngineEvent`,
      `ProgressUpdate`, `MessageLevel`, `LogLevel`, `Stage`, and `DriverMessage`
      (folder/file events, model, inventory, artifacts, source file, base
      artifact, remove base, stage-done, ready, progress, notice, log, summary);
      register the module in `lib.rs`. (AC: one engine-owned module)
- [x] Rework `src/engine.rs`: delete `Reporter` and the message definitions,
      import them from `messages`; add the bus, the hub task (`apply` + relay),
      and the seven drivers; make `spawn` start them; route `SetWorkspaceRoot`
      and `WatchedFiles` to the filesystem driver. (AC: engine is sole
      translator; drivers spawned at start)
- [x] Rework `src/index.rs`: producers (`walk_project`, `resolve_artifacts`,
      `scan_sources`, `index_jars`, `index_jdk`) take a sink and emit
      `DriverMessage`; delete `discover_workspace`, `scan_workspace`,
      `scan_workspace_async`, `run_core_producers`, and `apply_message`; add the
      `#[cfg(test)]` `warm_up_sync`. (AC: no pre-step; no direct `set_model`)
- [x] Rework `src/sources.rs` to emit through the bus sender instead of a
      `Reporter`. (AC: Reporter gone)
- [x] Rework `src/analysis.rs`: `set_workspace_root` stores the root (and runs
      the `#[cfg(test)]` synchronous warm-up); expose the shared index to the
      hub. (AC: no direct `set_model`)
- [x] Update unit tests: replace the `index.rs` pipeline tests with
      `warm_up_sync`/hub-boundary tests and drop the `Reporter` tests. (AC:
      `cargo test --all-targets` green)
- [x] Update `docs/architecture.md` — the boundary bullet and data-flow diagram
      (one engine-owned bus; the engine a hub and sole translator), the indexing
      bullet (discovery is a driver; the model is a message), and the
      Maven/dependency-sources bullets.
- [x] Trim `docs/dev/backlog/warmup-throughput.md` to the class-file base cache
      (D9), and update its row in `docs/dev/backlog/index.md`.
- [x] Append the changelog entry and mark this request `done` with its
      `verified` block.
