---
type: ChangeRequest
kind: refactor
title: Every participant subscribes to the bus through its BusClient
description: Replace the hub's fixed, wired-up routing table with runtime registration — BusClient::subscribe and BusClient::serve(Module) — so every module, driver, and the server listens through its client alone, and one setup function builds the hub and every participant, the server included.
state: done
priority: medium
tags: [dev, refactor, messaging]
owner: felix
verified:
  by: cargo build --all-targets is clean with no warnings; cargo test passes (299 library tests incl. the new bus::tests for first-owner-wins and dropped-owner pruning, 29 harness tests, the stdio smoke test), run outside the sandbox because the sources tests bind a local test server; a stdio run against example/ shows the subscriptions in order (server, the four modules, the five listening drivers) and go-to-definition resolving Greeter
  at: 2026-10-01T23:45:00Z
---

# Problem

The hub's routing table is fixed when `bus::spawn_router(modules,
notifications, owners)` starts it. So `engine::spawn` has to create a channel for
every module and driver up front, pass the senders to the hub, and thread the
receivers into each `spawn_module(rx, client, …)` and `*_driver(rx, client)`.
The server does the same: `JavaLanguageServer::new` creates its own channel and
hands it to `engine::spawn`. A participant therefore needs two things — a
receiver wired by someone else and a `BusClient` — and the wiring is repeated
in every test that assembles a bus (`bus::standalone`, the quick-fix module
test). The drivers also receive a different type (bare `DriverMessage`) from the
modules (`Bus`).

# Proposal

The hub starts empty (`bus::spawn_hub() -> BusClient`) and learns its
participants at runtime. A client registers by posting a subscription through
the same inbound channel as every other message:

- `client.subscribe()` returns an `UnboundedReceiver<Bus>` of every
  notification;
- `client.serve(Module::X)` does the same and makes that channel the owner of
  `X`'s requests.

Every module and driver then takes only its `BusClient` and subscribes itself;
one setup function, `engine::start(lsp_client) -> JavaLanguageServer`, starts
the hub and builds every participant — the server included.

# Decisions

- **D1 — One message type for everyone.** Drivers receive `Bus` like the
  modules and match `Bus::Notify`; a driver never serves a module, so it never
  receives a request.
- **D2 — One setup function builds everything, the server included.**
  `engine::start(client: tower_lsp::Client) -> JavaLanguageServer` starts the
  hub, builds the server (which subscribes on its own client), then starts the
  index, analysis, diagnostics and quick-fix modules and the drivers.
  `main.rs` and the harness pass `engine::start` to `LspService`.
  `JavaLanguageServer::new` takes the LSP client and its `BusClient`.
- **D3 — No message is lost.** Every participant subscribes synchronously in
  its spawn function, before starting its thread or task. The subscription goes
  through the hub's FIFO inbound channel, so it is applied before any message
  sent afterwards. The drivers are therefore plain functions that subscribe and
  then return the task's future; the JDK driver, which listens to nothing, does
  not subscribe.
- **D4 — A second owner is a wiring bug: log, keep the first.** If a module is
  already served by a live channel, the hub logs an error and keeps the first
  owner; the newcomer still receives notifications. A panic would kill the hub
  thread.
- **D5 — Dropped receivers are pruned.** When a send to a subscriber fails, the
  hub removes it; a closed owner is removed too, and its requests then resolve
  to the default reply, as an unowned request does today.
- **D6 — Behaviour is unchanged.** Same participants, same labels, same
  ordering (one FIFO hub, one FIFO channel per participant). The hub logs each
  subscription at `debug` (`sender=X subscribe`, `sender=X serve Analysis`).

# Acceptance criteria

- `spawn_router`, `bus::channel`, and every `rx` parameter of a module or
  driver spawn function are gone; `index::spawn_module`, `analysis::spawn_module`,
  `diagnostics::spawn_module` and `quickfix::spawn_module` take only a client
  (plus the runtime handle for analysis).
- `BusClient::subscribe` and `BusClient::serve` exist; the hub prunes closed
  subscribers and keeps the first owner of a module, logging an error.
- `engine::start` is the single setup path used by `main.rs` and the harness;
  `JavaLanguageServer::new` no longer starts the engine.
- `cargo build --all-targets` has no warnings and `cargo test` passes.

Docs to update: `docs/architecture.md` (layout entries for `bus.rs`/`engine.rs`,
the "shell on the bus" bullet, the hub decisions) and the module comments of
`bus.rs` and `engine.rs`.

# Implementation plan

## Steps

- [x] `messages.rs`: `Inbound::Subscribe { sender, sink, serves }`.
- [x] `bus.rs`: `spawn_hub()` replaces `spawn_router`/`channel`; the hub keeps
      its subscribers and owners, prunes closed ones, keeps the first owner
      (D4, D5); `BusClient::subscribe`/`serve`; `standalone` uses them.
- [x] `index`, `analysis`, `diagnostics`, `quickfix`: `spawn_module` takes only
      its client and calls `serve(Module)` before starting its thread.
- [x] `engine.rs`: `start(Client) -> JavaLanguageServer` is the one setup
      path; the listening drivers subscribe when called and return their task's
      future (D3); `next_notification` unwraps `Bus::Notify` (D1).
- [x] `server.rs`: `new(client, bus)` subscribes itself; `main.rs` and the
      harness pass `engine::start` to `LspService`.
- [x] Tests: the shell-channel test becomes a subscription test; new
      `bus::tests` for first-owner-wins and dropped-owner pruning.
- [x] Docs: `architecture.md` (layout, the shell bullet, the hub decision) and
      the `bus.rs`/`engine.rs` module comments.

Implemented on top of `f4b2884` alongside `server-as-bus-client` and
`bus-reply-receiver`; not yet committed.
