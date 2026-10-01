---
type: ChangeRequest
kind: refactor
title: Bus requests return an awaitable reply receiver
description: Replace the BusClient's closure-built requests, the boxed reply deliverer, and the blocking/async method pairs with one request form that returns a Reply — a thin wrapper over a tokio oneshot receiver that callers await (or receive blocking from synchronous module code).
state: done
priority: low
tags: [dev, refactor, messaging]
owner: felix
verified:
  by: cargo build --all-targets is clean with no warnings; cargo test passes (297 library tests, 29 harness tests, the stdio smoke test), run outside the sandbox because the sources tests bind a local test server
  at: 2026-10-01T23:00:00Z
---

# Problem

`BusClient` had two request paths built around callbacks. Each query method
passed a closure that built the `Request` around a `ReplyHandle`, and the handle
carried a boxed `FnOnce(R)` deliverer. The blocking `request` delivered over a
`std` mpsc channel; `request_async` delivered over a tokio oneshot. That meant a
second, `_async` copy of every method a runtime caller needed
(`code_actions_async`, `all_symbols_async`, `ready_async`), and the indirection
made the request flow harder to read.

# Proposal

Every `BusClient` request returns a `Reply<R>`: a newtype over the request's
`tokio::sync::oneshot::Receiver<R>`. It implements `Future<Output = R>`, so
runtime callers `.await` it, and it offers `blocking_recv()` for synchronous
module code. `ReplyHandle` carries the oneshot sender instead of a boxed
deliverer, and the client builds the request directly — no builder closure, one
method per request.

# Decisions

- **D1 — Synchronous callers stay synchronous (option A).** The analysis core,
  the diagnostics and the quick-fix code are deep, recursive tree-sitter walks
  that look names up mid-walk, and they already run off the runtime's workers
  (their module threads or the blocking pool). They call `Reply::blocking_recv`
  at the call site. Making them `async` (option B) was rejected: a large
  rewrite with boxed async recursion and hundreds of sync tests, for no gain.
- **D2 — A thin `Reply<R>` wrapper, not a bare receiver.** A bare
  `oneshot::Receiver` yields `Result<R, RecvError>`, so every call site would
  repeat `.unwrap_or_default()`. `Reply` keeps the bus's existing contract: an
  unanswered request or a gone bus resolves to `R::default()`.
- **D3 — The reply still rides the hub.** `ReplyHandle::send` posts the value
  back through the hub (which logs and times it) before it reaches the oneshot;
  the hub's `Inbound::Reply` keeps its internal type-erased delivery.
- **D4 — The `_async` methods are gone.** `code_actions`, `all_symbols` and
  `ready` serve both kinds of caller; the analysis queries return `Reply`
  rather than being `async fn`. Behaviour is unchanged.

# Acceptance criteria

- `BusClient` has no blocking request method, no `request_async`, and no
  `_async` methods; every request returns `Reply<R>`.
- `ReplyHandle` holds a `oneshot::Sender<R>`; the `std` mpsc delivery is gone.
- Calls from async code (the shell, tokio tests) await their `Reply`; the
  synchronous modules use `blocking_recv`.
- Existing tests pass unchanged in behaviour.

Docs touched: `docs/architecture.md` (the "shell on the bus" bullet and the
shell's query handlers) and the `bus.rs` module comment.

# Implementation plan

## Steps

- [x] `messages.rs`: `ReplyHandle` carries a `oneshot::Sender`, exposes `id()`;
      `AnalysisRequest::id()`.
- [x] `bus.rs`: `Reply<R>` (`Future` + `blocking_recv`); `BusClient::reply` and
      `post` replace `request`/`request_async`; every query returns `Reply`.
- [x] Sync callers in `analysis.rs`, `diagnostics.rs`, `quickfix.rs`, and
      `NameLookup for IndexHandle` call `blocking_recv`.
- [x] `server.rs` and `tests/harness.rs` drop the `_async` names; tokio tests
      await, sync tests receive blocking.
- [x] Update `architecture.md` and the bus module comment; build and test.

Implemented on top of `f4b2884` alongside `server-as-bus-client`; not yet
committed.
