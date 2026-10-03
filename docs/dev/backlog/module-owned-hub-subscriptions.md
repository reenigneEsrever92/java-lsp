---
type: ChangeRequest
kind: refactor
title: Every module handles its own hub subscription
description: Move the six warm-up drivers out of engine.rs into one module each, and have every module label, subscribe, and start itself, so engine.rs is pure wiring and the warm-up producers stop leaking as pub(crate).
state: done
verified: { by: felix, at: 2026-10-02T16:05:00Z }
priority: medium
tags: [dev, refactor, hub]
owner: felix
---

# Problem

`engine::start` is meant to be pure wiring — "the one setup path" that starts
the hub and builds every participant. In practice it is also where the six
warm-up drivers live: `project_driver`, `dependency_driver`, `source_driver`,
`jar_driver`, `jdk_driver`, and `download_driver` are defined in `src/engine.rs`
and each calls `client.subscribe()` there. Every other participant — the four
request-serving modules and the shell — already subscribes itself, so the drivers
are the outlier: a module's own subscription sits in the engine, far from the
work it drives.

The cost is twofold. `engine.rs` grows with each participant and knows each
driver's hub identity, receive loop, and label, so the "wiring only" module is
really a sixth module by accident. And because the drivers reach into `index.rs`
for the work they drive (`walk_project`, `resolve_artifacts`, `offline_notice`,
`scan_sources`, `index_jars`, `index_jdk`), those producers are `pub(crate)` only
to be callable from the engine, so the index's internals leak across the crate.

# Proposal

Give every driver its own module and have every module own its whole hub life —
label, subscribe, and start — so `engine::start` becomes the hub plus spawn calls
and nothing else. Each driver's module also owns the producer function it drives,
so the warm-up producers stop being `pub(crate)` and `index.rs` keeps only the
index itself:

| driver (label)            | module          | work moved in                                             |
| ------------------------- | --------------- | --------------------------------------------------------- |
| project (`project`)       | `project.rs`    | `walk_project`, `collect_java_files`                      |
| dependency (`dependency`) | `resolve.rs`    | `resolve_artifacts`, `offline_notice`, `local_repository` |
| source (`source`)         | `scan.rs` (new) | `scan_sources`                                            |
| jar (`jar`)               | `jars.rs` (new) | `index_jars`                                              |
| jdk (`jdk`)               | `jdk.rs`        | `index_jdk`, `jdk_cache_key`                              |
| download (`download`)     | `sources.rs`    | `index_sources` (already there)                           |

Each new module exposes one `spawn(client: &HubClient)` that does
`client.labeled("<label>")`, `subscribe()`, and `tokio::spawn`s its own receive
loop. The four existing modules (`index`, `analysis`, `diagnostics`, `quickfix`)
take the **raw** client instead of a pre-labelled one and do the same with
`serve(Module::X)`, and `JavaLanguageServer::new` labels itself `server`.
`engine::start` keeps only the hub and the ten spawns.

# Decisions

- **D1 — One module per driver, using the file that already owns the concept.**
  Four drivers land in existing files (`project.rs`, `resolve.rs`, `jdk.rs`,
  `sources.rs`); the source scanner and jar indexer get new `scan.rs` and
  `jars.rs`, because `index.rs` cannot host two drivers. Chosen over a single
  `warmup.rs` so each driver's module is exactly its concept.
- **D2 — Each module owns its producer.** `walk_project`/`collect_java_files`
  move to `project.rs`, `resolve_artifacts`/`offline_notice`/`local_repository`
  to `resolve.rs`, `scan_sources` to `scan.rs`, `index_jars` to `jars.rs`,
  `index_jdk`/`jdk_cache_key` to `jdk.rs`. They move out of `index.rs` and stay
  crate-visible (`pub(crate)`) rather than `pub`, because the test-only
  `warm_up_sync` still orchestrates them (D7). This is the point of the refactor:
  the module that subscribes is the module that does the work.
- **D3 — Shared parsing stays in `index.rs`.** `java_parser`, `extract_entries`,
  and `drop_import_entries` are used by the index, analysis, diagnostics,
  quickfix, jdk, and sources, so they remain in `index.rs` — the index's parsing
  home — and the moved producers call them there. Only the warm-up producers
  move.
- **D4 — Every module labels, subscribes, and starts itself.** `spawn` takes the
  raw `HubClient`; the module applies its own label (`labeled("index")`,
  `labeled("project")`, …), then `serve(Module::X)` (the request-serving modules)
  or `subscribe()` (the drivers), then starts its own thread (modules) or task
  (drivers). The shell labels itself `server` inside `JavaLanguageServer::new`.
- **D5 — Labels, participants, and FIFO ordering are preserved.** The six driver
  labels (`project`, `dependency`, `source`, `jar`, `download`, `jdk`) and the
  four module labels are unchanged, and every participant still subscribes
  synchronously before its thread or task starts, so no message is lost.
  Behaviour is unchanged.
- **D6 — `next_notification` becomes a shared `hub.rs` helper.** The `Hub::Notify`
  filter the drivers share moves to `hub.rs` as a `pub(crate)` function rather
  than being duplicated across six modules.
- **D7 — `warm_up_sync` (test-only) stays in `index.rs`** and imports the moved
  producers, so `analysis.rs`'s test-only synchronous warm-up is unchanged.
- **D8 — Tests move with their producer.** The unit tests that exercise a moved
  producer move with it (e.g. `offline_notice_only_when_offline_with_dependencies`
  to `resolve.rs`, `a_second_jdk_warmup_is_served_from_the_cache` to `jdk.rs`).
- **D9 — `engine.rs` becomes pure wiring.** After the change it holds only
  `start()`: the hub, `JavaLanguageServer::new`, and ten `spawn(&hub)` calls — no
  labels, no `subscribe`/`serve`, no driver bodies.

# Acceptance criteria

1. `src/engine.rs` contains no `subscribe`/`serve` call, no `labeled(...)`, and no
   driver body; `start` is the hub plus one `spawn(&hub)` per participant and
   `JavaLanguageServer::new`.
2. Each of the six drivers lives in its own module (`project.rs`, `resolve.rs`,
   `scan.rs`, `jars.rs`, `jdk.rs`, `sources.rs`) and its `spawn` labels,
   subscribes, and starts itself.
3. Every module's `spawn` takes the raw `HubClient`; `JavaLanguageServer::new`
   labels itself `server`; no caller passes a pre-labelled client.
4. `index.rs` no longer exposes `walk_project`, `collect_java_files`,
   `resolve_artifacts`, `offline_notice`, `local_repository`, `scan_sources`,
   `index_jars`, `index_jdk`, or `jdk_cache_key`; `java_parser`,
   `extract_entries`, and `drop_import_entries` remain the shared parsing API.
5. Participant labels and message ordering are unchanged; `warm_up_sync` still
   runs the same producers.
6. `cargo build --all-targets` is warning-free and `cargo test` passes (library,
   harness, and the `example/` feature tests) with no behavioural change.

# Docs

The crate layout and the "who subscribes" story change, so the implementation
plan must update:

- `docs/architecture.md` — the **Layout** module list (add `scan.rs` and
  `jars.rs`; correct `index.rs` and `engine.rs` one-liners), the **components and
  data flow** diagram and the **"The shell on the hub"** prose (each participant,
  drivers included, now labels/subscribes itself in its own module), and the
  **"Every subsystem is a module or driver on one hub"** decision (the drivers
  are no longer spawned by the engine but by their own modules).
- `src/index.rs` module doc — "the drivers the engine spawns" becomes the
  warm-up drivers in their own modules.
- `src/index.rs`, `src/engine.rs`, and `src/hub.rs` module docs — describe
  `engine::start` as pure wiring and note the shared `next_notification` helper.

# Implementation plan

## Approach

Each participant becomes a module that owns its whole hub life. A module's
`spawn(hub: &HubClient)` (analysis also takes the runtime `Handle`) applies its
own label, subscribes (`serve(Module::X)` for the request-serving modules,
`subscribe()` for the drivers), and starts its own thread or task — so
`engine::start` holds only the hub, `JavaLanguageServer::new`, and the ten
spawns, in the current order (shell, the four modules, then the six drivers).

Each driver moves into the module that already owns its concept, together with
the producer it drives and that producer's private helpers: the project driver +
`walk_project`/`collect_java_files` in `project.rs`, the dependency driver +
`resolve_artifacts`/`offline_notice`/`local_repository` in `resolve.rs`, and the
JDK driver + `index_jdk`/`jdk_cache_key` in `jdk.rs`. The source scanner and jar
indexer get new `scan.rs` and `jars.rs` (one driver each). The moved producers
stay `pub(crate)` so `index.rs`'s test-only `warm_up_sync` can still run the same
pipeline synchronously. Shared parsing (`java_parser`, `extract_entries`,
`drop_import_entries`) and the index itself stay in `index.rs`.

The driver receive loop's shared helper `next_notification` moves to `hub.rs` as
a `pub(crate)` function. Labels (`project`, `dependency`, `source`, `jar`,
`download`, `jdk`, `index`, `analysis`, `diagnostics`, `quickfix`, `server`) and
the subscription order are unchanged, so behaviour and the hub log are preserved.

## Steps

- [x] `src/hub.rs` — add a shared `pub(crate) async fn next_notification` (the
      `Hub::Notify` filter `engine.rs`'s drivers share), and change
      `standalone_client` to call `index::spawn(&client)`.
- [x] `src/project.rs` — move `walk_project` and `collect_java_files` from
      `index.rs`; add `pub fn spawn(hub: &HubClient)` that labels `project`,
      subscribes, and `tokio::spawn`s the project driver (the walk plus the
      `Ready`/`Summary`/`Progress End` coordination).
- [x] `src/resolve.rs` — move `local_repository`, `resolve_artifacts`, and
      `offline_notice` from `index.rs`; add `pub fn spawn(hub: &HubClient)` (the
      dependency driver); move the `offline_notice` unit test here.
- [x] `src/scan.rs` (new) + `src/lib.rs` — move `scan_sources` in; add
      `pub fn spawn(hub: &HubClient)` (the source driver); declare `pub mod scan`.
- [x] `src/jars.rs` (new) + `src/lib.rs` — move `index_jars` in; add
      `pub fn spawn(hub: &HubClient)` (the jar driver); declare `pub mod jars`.
- [x] `src/jdk.rs` — move `index_jdk` and `jdk_cache_key` from `index.rs`; add
      `pub fn spawn(hub: &HubClient)` (the JDK driver); move the JDK-cache unit
      test here.
- [x] `src/sources.rs` — add `pub fn spawn(hub: &HubClient)` (the download
      driver wrapping `index_sources`); repoint `local_repository` to
      `crate::resolve`.
- [x] `src/index.rs` — drop the moved producers and helpers; rename
      `spawn_module` to `spawn(hub: &HubClient)` (labels `index`, serves
      `Module::Index`); keep `warm_up_sync` calling the moved crate-visible
      producers; prune now-unused imports.
- [x] `src/analysis.rs`, `src/diagnostics.rs`, `src/quickfix.rs` — rename
      `spawn_module` to `spawn`, take `&HubClient`, and label themselves
      (`analysis`/`diagnostics`/`quickfix`).
- [x] `src/server.rs` — `JavaLanguageServer::new(client, hub: &HubClient)` labels
      itself `server` and stores the labeled client.
- [x] `src/engine.rs` — reduce `start` to the hub, `JavaLanguageServer::new`, and
      the ten `spawn(&hub)` calls; delete the driver functions and the local
      `next_notification`; correct the module doc.
- [x] `docs/architecture.md` — update the **Layout** module list (add `scan.rs`
      and `jars.rs`, correct `index.rs`/`engine.rs`), the components/data-flow
      diagram, the **"The shell on the hub"** paragraph, and the **"Every
      subsystem is a module or driver on one hub"** decision.
- [x] `src/index.rs`, `src/engine.rs`, `src/hub.rs` module docs — describe
      `engine::start` as pure wiring and note the shared `next_notification`.
- [x] Verify with `cargo build --all-targets`, `cargo fmt --check`, and
      `cargo test` (library, harness, `example/` features).

## Progress

- Done. The six drivers now live in `project.rs`, `resolve.rs`, `scan.rs`,
  `jars.rs`, `jdk.rs`, and `sources.rs`, each with a `spawn` that labels,
  subscribes, and starts itself; `walk_project`/`collect_java_files`,
  `resolve_artifacts`/`offline_notice`/`local_repository`, `scan_sources`,
  `index_jars`, and `index_jdk`/`jdk_cache_key` moved with their driver. The
  index, analysis, diagnostics, and quick-fix modules and the shell now label
  themselves too, and `engine::start` is the hub plus ten `spawn(&hub)` calls.
  `next_notification` is shared from `hub.rs`. Verified: `cargo build
--all-targets` warning-free, `cargo fmt --all -- --check` clean, and `cargo
test` green (299 library, 7 bench, 1 `example/` feature, 29 harness, 1 stdio
  smoke).
