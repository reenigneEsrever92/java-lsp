---
type: ChangeRequest
kind: bug
title: Stop the hub logging a reply for an unanswered request
description: The hub logs a reply line for a request that was never answered, because a dropped reply handle is indistinguishable in the log from a real one; log only real replies, and fix the stale "no round-trip through the hub" claim in the architecture.
state: done
priority: low
tags: [dev, review, bug, observability, messaging]
owner: felix
verified:
  by: cargo build clean; cargo fmt --check clean; the three new `bus::tests` pass (`an_answered_request_logs_a_reply_with_its_responder_and_requester`, `an_unanswered_request_is_not_logged_as_a_reply`, `a_reply_is_not_logged_when_no_description_was_captured`); and an end-to-end `RUST_LOG=java_lsp::bus=debug` run of the stdio smoke test still shows `sender=index reply to=core … elapsed=0ms` lines.
  at: 2026-09-30T00:00:00Z
---

# Problem

A review of the uncommitted `message-hub-log-sender` work (the hub now names
every sender and routes request replies back through the hub for timing) found
three items.

1. **The hub logs a `reply` line for a request that was never answered.**
   `src/bus.rs` (the `Inbound::Reply` arm of `spawn_router`) logs whenever the
   request id is still in `pending` and its `desc` is `Some`, without
   distinguishing a real reply from a cancellation:

   ```rust
   Inbound::Reply { id, deliver } => {
       if let Some(done) = pending.remove(&id) {
           if let Some(desc) = &done.desc {
               tracing::debug!(… "sender={} reply to={} {} elapsed={}ms" …);
           }
       }
       if let Some(deliver) = deliver {
           deliver();
       }
   }
   ```

   A `ReplyHandle` dropped unanswered — a request the owner does not handle, or
   any request on the standalone bus with no registered owner — posts
   `Inbound::Reply { id, deliver: None }` (`src/messages.rs`). That is a
   cancellation, but the arm above still emits a
   `sender=<owner> reply to=<requester> … elapsed=<ms>` line for it. This
   contradicts the request's own acceptance criterion ("a request the owner does
   not answer produces no reply line") and the `Inbound::Reply` doc, which say
   the hub "only forgets it". Impact: the hub log can claim a reply that never
   happened, with a fabricated latency, defeating the log's purpose — attributing
   the request/response flow truthfully.

2. **A stale architecture claim is now doubly wrong.** `docs/architecture.md`
   (the "The index is its own subsystem" bullet) still says "Queries are messages
   to the subsystem rather than round-trips through the hub task, which itself
   calls the index synchronously while dispatching (routing them through it would
   deadlock)." The code contradicts this: bus requests are routed _through_ the
   hub (`BusClient::request` posts `Inbound::Request` and the hub dispatches it),
   and with this work a reply also round-trips through the hub. The sentence
   predates the change (it is already stale from the bus rework) and this work
   makes it wrong in the reply direction as well.

3. **The new log contract is untested.** Nothing asserts the `sender=` prefix,
   the `reply … elapsed=` line, or the rule that an unanswered request produces
   no reply line. The reply _routing_ is covered implicitly — every
   request-based test would fail if replies stopped being delivered — but the log
   shape and finding 1's rule are not.

# Proposal

A small, focused fix to the hub log, its one wrong doc, and a test for the new
contract:

- In the `Inbound::Reply` arm, log the reply line only when the message is a real
  reply (`deliver.is_some()`); a cancellation (`deliver: None`) removes the
  pending entry and is not logged.
- Correct the `docs/architecture.md` sentence so it matches the code: a query is
  a bus request the hub routes to the owning subsystem, and the reply is routed
  back through the hub for logging/timing (the hub never blocks on the subsystem,
  so there is no deadlock).
- Add a test that pins the contract: a real request/reply logs a `sender=… reply
to=… elapsed=` line, and a request dropped unanswered logs no reply line.

# Decisions

- **D1 — Scope is the `message-hub-log-sender` work only.** The wider uncommitted
  `unified-bus`/`quickfix-subsystem` refactor is already captured in two `done`
  requests and is out of scope for this follow-up. (Agreed.)
- **D2 — Kind is `bug`.** The dominant finding (1) is a defect: the log asserts
  something false. (Agreed.)
- **D3 — One request covers findings 1–3.** The doc correction (2) is directly
  caused by the same behaviour, and the test (3) pins it; splitting them would
  leave the doc and the test orphaned from the fix. (Agreed.)
- **D4 — A cancellation is detected by `deliver`, not by a new flag.** An
  unanswered request already carries `deliver: None` (versus `Some` for a real
  reply), so the log guard is `deliver.is_some()`; no extra field or message
  variant is added. Reason: the distinction already exists in the protocol.
- **D5 — The test installs a `tracing` subscriber and captures the hub's
  `java_lsp::bus` lines.** It drives the standalone bus (a real `IndexQueryName`
  for the logged reply) and a request with no registered owner (for the
  no-log rule), asserting on the captured output. Reason: the contract is about
  emitted log lines, so the test must observe them, not just the values.

# Acceptance criteria

1. A request whose owner answers produces exactly one
   `sender=<owner> reply to=<requester> <desc> elapsed=<ms>ms` line; a request
   dropped unanswered produces no reply line and leaves no `pending` entry.
   (Resolves finding 1.)
2. `docs/architecture.md` no longer claims queries avoid the hub: the index
   bullet states that a query is a bus request the hub routes to the owning
   subsystem and that the reply is routed back through the hub, with no
   synchronous hub-side index call (so no deadlock). (Resolves finding 2.)
3. A test asserts both the logged reply line and the absence of a line for an
   unanswered request, and fails before the fix in finding 1. (Resolves
   finding 3.)
4. `cargo build` and `cargo fmt --check` are clean and the touched tests pass.

# Docs to update

- `docs/architecture.md` — the "The index is its own subsystem" bullet: replace
  the "rather than round-trips through the hub task … would deadlock" sentence
  with the actual flow (a routed request, a reply routed back through the hub for
  logging/timing).
- `docs/dev/backlog/index.md` — this request's row.
- `docs/dev/changelog.md` — an entry at implementation.

# Implementation plan

## Approach

The fix is three small, contained changes.

**The log guard (`bus.rs`).** In `spawn_router`'s `Inbound::Reply` arm, a reply is
real exactly when its `deliver` is `Some` (a `ReplyHandle` dropped unanswered
posts `deliver: None`), so the line is logged only when `deliver.is_some()`.
Extract the line into a pure helper so the rule is unit-testable without a
global tracing subscriber:

```rust
/// The hub-log line for a reply, or `None` when there is nothing to log — an
/// unanswered request (`answered` false), or logging disabled at request time
/// (no description).
fn reply_log(done: &Pending, answered: bool) -> Option<String> {
    if !answered {
        return None;
    }
    let desc = done.desc.as_deref()?;
    Some(format!(
        "sender={} reply to={} {} elapsed={}ms",
        module_label(done.owner),
        done.requester,
        desc,
        done.started.elapsed().as_millis()
    ))
}
```

The arm becomes: compute `let answered = deliver.is_some();` before consuming
`deliver`, remove the pending entry, log `reply_log(&done, answered)` when it is
`Some`, then run `deliver` if present. (Refines D5: the test targets the extracted
decision rather than installing a process-global subscriber, which would leak
across the parallel test binary.)

**The test (`bus.rs`).** A new `#[cfg(test)] mod tests` builds a `Pending` and
asserts `reply_log` returns a line naming `sender=index reply to=core <desc>
elapsed=` when answered and `None` when not — the exact regression (finding 1);
it fails if the `answered` guard is dropped.

**The doc (`architecture.md`).** Replace the "rather than round-trips through
the hub task … would deadlock" sentence with what the code does: a query is a bus
request the hub routes to the index subsystem, and the subscription's reply is
routed back through the hub for logging/timing, so the hub never calls the index
synchronously.

## Steps

- [x] `src/bus.rs`: add `reply_log(done, answered)`; in the `Inbound::Reply` arm
      compute `answered = deliver.is_some()` and log only `reply_log`'s `Some`.
      (AC1, D4.)
- [x] `src/bus.rs`: add a `#[cfg(test)] mod tests` covering `reply_log` — answered
      logs, unanswered does not. (AC3.)
- [x] `docs/architecture.md`: correct the index-subsystem bullet. (AC2.)
- [x] Build and test: `cargo build`, `cargo fmt --check`, and the new test.
      (AC4.)
- [x] `docs/dev/backlog/index.md`: move this request's row to `done`.
- [x] `docs/dev/changelog.md`: append the entry.
