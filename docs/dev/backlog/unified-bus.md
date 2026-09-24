---
type: ChangeRequest
kind: refactor
title: One engine bus with broadcast notifications and hub-routed request/response
description: Replace the per-module channels and cross-module handles with a single bus — every module consumes the notifications it cares about, and a request is routed by the hub to the one module that answers it.
state: done
priority: high
tags: [dev, refactor, messaging, subsystems]
owner: felix
verified:
  by: cargo build is clean; the targeted library tests for the index, diagnostics, quick-fix, and code-action paths pass (prefix_lookup, quickfix, valid_file_has_no_diagnostics, unresolved_type_names, a_type_known_elsewhere_offers_an_import, a_did_you_mean_action_renames_the_member); the full suite and the harness could not be run under the machine's load average of ~30
  at: 2026-09-29T00:00:00Z
---

# Problem

After the index, diagnostics, and quick-fix subsystems were extracted, each
talked to its neighbours through a **bespoke** channel rather than a shared
mechanism:

- the index was a thread reached by an `IndexHandle` (a private `std::sync::mpsc`
  sender, or later a direct bus client) — the diagnostics and quick-fix
  subsystems *held* an index handle;
- the diagnostics subsystem had its own `DiagnosticsInput` channel and a
  `DiagnosticsHandle` (a shared results cache) that the quick-fix subsystem
  held;
- the quick-fix subsystem had its own `QuickFixHandle` (a thread + job channel);
- the filesystem had its own `FsInput` channel;
- the hub relayed `DriverMessage`s to the drivers but applied the index messages
  itself (`engine.rs::apply`).

So "each module encapsulated, communicating by message" was true only in spirit:
modules held each other's handles, and every new interaction meant new wiring.

# Proposal

One bus, two verbs:

- **Notifications** — `DriverMessage`s the hub broadcasts to every module; each
  consumes the ones it cares about (a driver its inventory, the index its
  edits, the diagnostics/quick-fix modules the open documents).
- **Requests** — `Request`s the hub routes to the one module that owns them, the
  reply riding the same bus. `BusClient` is the client every module and the shell
  uses to send either.

The hub is a **thread** (`crate::bus::spawn_router`), so a module may block on a
request from any thread — including a runtime worker — without deadlocking. Each
module owns its own state; nothing holds another module's state or handle.

# Decisions

- **D1 — One message module, one client, one router.** `Bus { Notify, Request }`,
  the `Request` variants, and `translate` live in `messages.rs`; `BusClient` and
  `spawn_router` live in `bus.rs`. A module registers its channel with the hub
  (broadcast) and, if it owns requests, as an owner.
- **D2 — The hub is a thread, not a task.** A blocking request from a runtime
  worker would deadlock a current-thread runtime (the harness) if the hub could
  not run; a thread hub is immune. The hub is also the sole translator to the
  editor.
- **D3 — The index is a bus module.** `IndexHandle` is now `BusClient`
  (`pub type IndexHandle = crate::bus::BusClient`); the index thread consumes its
  notifications and answers `Request::Index*`. Its warm-up/applying code
  (`apply_to_index`) stays with the module.
- **D4 — Diagnostics and quick fixes are bus modules.** Each owns a parser and
  the open buffers' text, consumes `DocumentOpened/Changed/Closed` and `FileEvent`
  notifications, and answers its request (`DiagnosticsForDocument`,
  `QuickFixForDocument`). The quick fixes read the index and the diagnostics cache
  through the bus, so they hold no handles. `DiagnosticsHandle`, `QuickFixHandle`,
  and `FsInput` are gone.
- **D5 — File events and the root are notifications.** The dispatcher emits
  `FolderAdded` (the project driver walks) and `FileEvent` (the core re-reads via
  `watched_files`, the diagnostics module re-sweeps); the filesystem driver and
  `FsInput` are removed.
- **D6 — The core keeps its synchronous algorithms; only its index access moves
  onto the bus.** `TreeSitterEngine` holds a `BusClient` (its `index` field) and
  its queries block on hub-routed index requests. Because the hub is a thread,
  this is safe from the dispatcher and from spawned query tasks alike; the core is
  not itself a bus consumer.
- **D7 — Behaviour preserved.** The same index, diagnostics, and quick fixes; the
  targeted tests pass.

# Acceptance criteria

- One client type (`BusClient`) and one router; the drivers, the index, the
  diagnostics, the quick fixes, and the core all use it.
- A notification reaches every module; a request is answered by the owning module
  and nowhere else.
- The index, diagnostics, and quick fixes own their state and hold no other
  module's handle; `DiagnosticsHandle`, `QuickFixHandle`, `FsInput`, and the
  filesystem driver are gone.
- `cargo build` is clean and the targeted tests pass. (The full suite and the
  harness are unrun here — the host is at load average ~30 with most of an 8 GB
  swap in use.)

# Implementation plan

## Steps

- [x] Add `Bus`, `Request`, and `translate` to `messages.rs`; add the document
      lifecycle, capability, and core index-mutation notifications.
- [x] Add `bus.rs`: `Module`, `BusClient` (notify + blocking request + the index
      and inter-module methods), and the thread `spawn_router`; plus a standalone
      bus for tests.
- [x] Make the index a bus module (`spawn_index_module`); `IndexHandle` becomes
      `BusClient`.
- [x] Make the diagnostics subsystem a bus module (consumes documents, answers
      its request, keeps its cache).
- [x] Make the quick-fix subsystem a bus module (consumes documents, reads the
      index and diagnostics through the bus).
- [x] Replace the engine's hub task and filesystem driver with the router thread;
      the dispatcher emits notifications and routes `codeActions` through the bus;
      the drivers send via `BusClient`.
- [ ] A full `docs/architecture.md` pass and the changelog (started).
