---
type: ChangeRequest
kind: improvement
title: Identify the sender, replies, and reply latency in the hub log
description: Give every bus client a name, log it on each hub log line, route replies back through the hub, and log each request/reply pair with how long the reply took.
state: done
priority: low
tags: [dev, improvement, observability, messaging]
owner: felix
verified:
  by: cargo build clean; cargo fmt --check clean; cargo clippy --lib reports no warnings in the touched files (`bus.rs`/`messages.rs` clean; the pre-existing `never_loop` denials in `tests/stdio_smoke.rs` and `src/bin/java-lsp-bench.rs` are untouched). Targeted lib tests over the bus request/reply paths pass — index queries behind hover/definition, watched-file indexing, the diagnostics gate, and references/rename. An end-to-end run of the stdio smoke test with `RUST_LOG=java_lsp::bus=debug` shows the lines the request asks for — `sender=core request IndexTypeLayers`, `sender=index reply to=core IndexTypeLayers elapsed=0ms`, `sender=core notify SourceEntries …`, `sender=dispatch notify DocumentChanged …`, `sender=jdk notify BaseArtifact …`. The full `cargo test --all-targets` did not finish in this environment: the `references`/`rename` tests are CPU-bound (~110s each here) and serialize on the JDK `env_lock`; this is pre-existing and independent of the change (verified by toggling reply delivery between hub-routed and direct, with identical timing).
  at: 2026-09-30T00:00:00Z
---

# Problem

The hub (`src/bus.rs`) is the one place every bus message passes through, and it
logs each one at `debug` (`RUST_LOG=java_lsp::bus=debug`) via `describe`
(`src/bus.rs:318`). But a log line names only the message — `notify SourceFile
file:///… entries=12`, `request IndexQueryName sum` — never **who** sent it.

That is structural, not an oversight: `BusClient` (`src/bus.rs:45`) is a bare
`UnboundedSender<Bus>` with no identity, and `spawn_router` returns a _single_
client that `engine.rs::spawn` clones to everyone — the core, the dispatcher, all
six drivers, and the diagnostics and quick-fix modules. They all push onto one
channel, so the hub cannot tell their messages apart. The stated goal — "the hub
logs every message passing through, so the whole message flow is observable from
one place" (`src/bus.rs:14`) — holds for the message _types_ but not for the
_flow_.

Replies are missing for the same reason, from the other side: a request's answer
never reaches the hub. The requester blocks on a private `std_mpsc` channel and
the owning module (`index.rs`, `diagnostics.rs`, `quickfix.rs`) replies to it
**directly** (`reply.send(...)`), so the hub sees a request leave and nothing come
back. There is no way to see which requests were answered, by which module, or how
long each took.

Cost: when debugging the message flow, a log line cannot be attributed to its
origin, and the request/response path — the hottest path in the engine, and the
one most likely to stall — is invisible and unmeasured.

# Proposal

Give every bus client a **name**, and make the hub the one place that sees (and
logs) the whole exchange.

- **Clients carry a label.** `BusClient` gains a `sender: String`; every client
  names itself — `core`, `dispatch`, the six drivers, `diagnostics`, `quickfix` —
  and its `notify`/`request` post the label with the message.
- **The hub's inbound is an envelope.** Its channel carries the label alongside
  the message, so the hub can prefix every line with `sender=<label>` while the
  value the modules receive (`Bus`) is unchanged.
- **Replies go back through the hub.** A reply becomes a hub message too, carrying
  a correlation id and a type-erased delivery closure. The hub records each
  request as it routes it (requester, description, owner, start time) and, when
  the reply returns, logs `sender=<owner> reply to=<requester> <desc>
elapsed=<ms>` before delivering the payload to the waiting caller.

Result: one log line per message with a sender on it, a matching line per reply
with the responder and the round-trip time, and a reply path the hub actually
observes. Still debug-gated and still payload-free.

# Decisions

- **D1 — The label is a free-form string, set by each client.** `BusClient`
  holds a `sender: String`; a client is created with a name (and a clone can be
  relabeled). Chosen over an enum so a client can name itself without the bus
  type enumerating every sender. (Agreed.)
- **D2 — Every sender is distinct.** The labels are `core` (`TreeSitterEngine`),
  `dispatch` (`engine.rs::dispatch`, which emits the folder/document/file events),
  the six drivers (`project`, `dependency`, `source`, `jar`, `jdk`, `download`),
  `diagnostics`, and `quickfix`. The index module never sends (it only answers
  requests); the three request owners are labeled `index`/`diagnostics`/
  `quickfix` for the reply lines. The core and the dispatcher — which share one
  client today — are split into two named clients. (Agreed.)
- **D3 — The hub's inbound channel is an envelope; `Bus` is unchanged.** The
  inbound channel carries a small enum — a named notification, a named request
  (with its correlation id), or a reply (id + delivery closure) — while the value
  the modules and drivers receive (`Bus { Notify, Request }`, `channel()`) stays
  exactly as it is, so the `Bus::Notify`/`Bus::Request` match arms in `index.rs`,
  `diagnostics.rs`, and `quickfix.rs` are untouched. (Agreed.)
- **D4 — Log shape.** Every line is prefixed `sender=<label>`; the message
  portion is today's `describe` output. A reply line is `sender=<owner>
reply to=<requester> <desc> elapsed=<ms>`. Same `debug` level, same
  `java_lsp::bus` target, and still never a payload. (Agreed.)
- **D5 — Replies are routed through the hub, and timing is measured there.**
  A request is built with a `Reply<R>` handle; when the owner answers, the handle
  posts a hub message carrying the request's id and a type-erased closure that
  delivers the value to the caller. The hub keeps a `id → { requester, desc,
owner, start }` map: it inserts on request arrival and, on reply, logs the pair
  with `elapsed = now − start` and then runs the delivery closure. Accepted
  tradeoff: every request/reply gains a second hop through the hub thread even
  when debug logging is off, so the hub sits on the reply path — chosen so the
  hub is the single observer and timer, over the alternative of timing at the
  caller. (Agreed — the user chose hub-routed replies.)
- **D6 — The reply field type changes; the call sites do not.** Each `Request`
  variant's `reply: std_mpsc::Sender<R>` becomes `reply: Reply<R>`, whose
  `send(self, value)` consumes the handle exactly as the sender did, so every
  `reply.send(value)` in the handlers stays textually the same. `BusClient::request`
  builds the `Reply` (allocating the id) and passes it to the request constructor.
- **D7 — A dropped reply is reported.** If a `Reply` is dropped without sending
  (e.g. a request the owner does not answer), it posts a cancellation so the hub
  drops the pending entry, keeping the map from leaking.
- **D8 — Labels are not asserted.** The standalone bus used by unit tests
  (`BusClient::standalone`, `TreeSitterEngine::new`) gets a label too, but no test
  asserts log text; the improvement is verified by the labels being present and
  correct.
- **D9 — Out of scope.** The shell's `Command`s (they are not bus messages) and
  the per-message payload rendering stay as they are; only the message flow the
  hub already sees is annotated.

# Acceptance criteria

- Every hub log line is prefixed with `sender=<label>`: a notification or request
  names the client that sent it, and a reply names the module that answered it and
  shows `to=<requester>` and `elapsed=<ms>`.
- The bus clients that send are labeled distinctly: `core`, `dispatch`,
  `project`, `dependency`, `source`, `jar`, `jdk`, `download`, `diagnostics`,
  `quickfix`; the core and the dispatcher no longer share one identity.
- Each request/reply produces a request line and a matching reply line carrying the
  elapsed time; a request the owner does not answer produces no reply line and
  leaves no pending entry behind.
- The module- and driver-facing `Bus` value, `channel()`, and the match arms in
  `index.rs`, `diagnostics.rs`, and `quickfix.rs` are unchanged.
- Logging is still gated by `tracing::enabled!(tracing::Level::DEBUG)` and never
  emits a payload.
- Existing behaviour is preserved and the test suite passes as before.

# Docs to update

- `src/bus.rs` — the module doc (lines 14–17) now describes the sender prefix,
  the reply line, and the elapsed time, not just "one line per message".
- `docs/architecture.md` — the data-flow note `HUB -- tracing log lines --> E`
  and the bus decision bullet: the hub log now identifies each sender and shows
  request/reply pairs, and replies are routed through the hub.
- `docs/dev/backlog/index.md` — this request's row.
- `docs/dev/changelog.md` — an entry at implementation.

# Implementation plan

## Approach

The change is confined to the bus vocabulary (`src/messages.rs`), the bus itself
(`src/bus.rs`), and the wiring (`src/engine.rs`); the modules and drivers
(`index.rs`, `diagnostics.rs`, `quickfix.rs`) are not touched — they keep
receiving the same `Bus` value and calling `reply.send(value)` unchanged.

**The vocabulary (`messages.rs`).** Add two types. `ReplyHandle<R>` is the reply
handle a request now carries instead of a bare `std_mpsc::Sender<R>` (named
`ReplyHandle` because `messages.rs` already has a private `type Reply<T>` alias
for the shell's `Command` oneshot replies):

```rust
pub struct ReplyHandle<R> {
    inbound: tokio_mpsc::UnboundedSender<Inbound>,
    id: u64,
    tx: Option<std_mpsc::Sender<R>>,
}
```

`ReplyHandle::send(self, value)` takes the `tx` and posts
`Inbound::Reply { id, deliver: Some(Box::new(move || { let _ = tx.send(value); })) }`;
`ReplyHandle` implements `Drop` so that a handle dropped without sending (a request
the owner does not answer, e.g. the `_ => {}` arm in `index.rs::answer`) posts
`Inbound::Reply { id, deliver: None }` so the hub forgets the pending entry.
The reply field of every `Request` variant changes from `std_mpsc::Sender<R>` to
`ReplyHandle<R>`; the handler call sites (`reply.send(...)`) are textually
unchanged.

`Inbound` is the hub's own inbound vocabulary — the envelope plus the reply:

```rust
pub enum Inbound {
    Notify { sender: String, message: DriverMessage },
    Request { sender: String, id: u64, request: Request },
    Reply { id: u64, deliver: Option<Box<dyn FnOnce() + Send>> },
}
```

The module- and driver-facing `Bus { Notify, Request }` and `channel()` are
untouched, so no module match arm changes.

**The bus (`bus.rs`).** `BusClient` becomes
`{ inbound: UnboundedSender<Inbound>, sender: String }`. `notify` posts
`Inbound::Notify { sender: self.sender.clone(), message }`. `request` allocates an
id from a process-wide `AtomicU64`, builds a `ReplyHandle` with it, posts
`Inbound::Request { sender, id, request: build(reply) }`, and blocks on `rx`. A
`labeled(&self, label) -> Self` builds a clone with a new name. `spawn_router`'s
channel becomes `Receiver<Inbound>`; the loop:

- **Notify** — `debug!(target: "java_lsp::bus", "sender={sender} notify {}", describe_notification(&message))`, then `translate` and broadcast to the
  modules (`Bus::Notify`) and drivers, exactly as today.
- **Request** — log `sender={sender} request {desc}`, look up `owner_of`, insert
  `id → Pending { requester: sender, desc, owner, started: Instant::now() }`, and
  route `Bus::Request(request)` to the owner.
- **Reply** — remove the pending entry and, when present, log
  `sender=<owner label> reply to=<requester> {desc} elapsed={ms}`; then run the
  delivery closure (if any).

`Pending` and the `HashMap<u64, Pending>` are local to the hub thread; a
`module_label(Module)` helper names the three owners (`index`, `diagnostics`,
`quickfix`). The `describe(&Bus)` wrapper is dropped in favour of calling
`describe_notification`/`describe_request` directly where the sender is in scope.
`standalone_client` relabels its returned client `core`.

**The wiring (`engine.rs`).** `spawn` keeps the one client `spawn_router`
returns and derives one named client per sender: `core` for
`TreeSitterEngine::with_index`, `dispatch` for the dispatcher task, and
`project`, `dependency`, `source`, `jar`, `jdk`, `download`, `diagnostics`,
`quickfix` for the drivers and modules — the split of core from dispatcher is what
the single shared client hides today.

## Steps

- [x] `src/messages.rs`: add `ReplyHandle<R>` (`send`, `Drop` cancellation) and
      `Inbound { Notify, Request, Reply }`; change every `Request` variant's
      `reply` field to `ReplyHandle<R>`. (AC: call sites unchanged; D5–D7.)
- [x] `src/bus.rs`: rework `BusClient` to `{ inbound, sender }`; post `Inbound`
      from `notify`/`request`; add the id counter and `labeled`. (AC: D1.)
- [x] `src/bus.rs`: rework `spawn_router` over `Inbound` — log `sender=` on
      notify and request, time request→reply through the pending map, and log the
      reply with `to=`/`elapsed=`; add `module_label`; drop `describe(&Bus)`.
      (AC: sender prefix; reply line; D4, D5.)
- [x] `src/bus.rs`: rewrite the module doc (lines 14–17) to describe the sender
      prefix, the reply line, and the elapsed time.
- [x] `src/engine.rs`: derive the ten named clients (`core`, `dispatch`, the six
      drivers, `diagnostics`, `quickfix`) from the router client and pass each to
      its consumer. (AC: distinct labels; D2.)
- [x] Drop the now-unit `let _ =` in the three modules' reply call sites
      (`index.rs`, `diagnostics.rs`, `quickfix.rs`), which `ReplyHandle::send`
      would otherwise make a `let_unit_value` warning. (`engine.rs`'s two are
      `oneshot` command replies and keep theirs.)
- [x] `docs/architecture.md`: update the `HUB -- tracing log lines --> E`
      data-flow note (now `HUB -- tracing logs (sender, latency)`) and the bus
      decision bullet.
- [x] Build and test: `cargo build`, `cargo fmt --check`, `cargo clippy --lib`.
      (AC: behaviour preserved.)
- [x] `docs/dev/backlog/index.md`: move this request's row to `done`.
- [x] `docs/dev/changelog.md`: append the entry.
