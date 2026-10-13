---
type: ChangeRequest
kind: improvement
title: Log a compact summary of each hub message
description: The hub's per-message `info` line Debug-prints the whole envelope, so a buffer-text response, a document-open notification, or a full-text change dumps the entire file body into the log; render each message as a compact summary (kind, sender, id, uri, payload lengths) instead.
state: done
verified: { by: felix, at: 2026-10-13T16:15:00Z }
priority: medium
tags: [dev, improvement, hub, logging]
owner: felix
---

# Problem

`hub.rs::forward` logs every message with `info!(?msg, "Hub message")` — full
`Debug` of the `Envelope`. `Debug` of a text-carrying payload embeds the whole
body: `BuffersResponse::Text(Some(Arc<String>))` — the answer to every
buffer-text read a completion or navigation makes — `Notification::DocumentOpened
{ text }` on every file open, and full-text `DocumentChanged` change events. The
default filter is `java_lsp=info` (`main.rs`), so these lines are on by default,
and reading a large file emits a log line of tens of thousands of characters —
per keystroke, since completion reads the buffer on every edit. The noise drowns
the flow trace the hub exists to provide.

# Proposal

Keep the one `info!` line per message, but render a compact summary instead of
the full `Debug`: `id`, `sender`, the message kind and variant, the `uri` where
the message carries one, and each text payload as its byte length (`Some(<len>
bytes)` / `None`, `text: <len> bytes`) rather than the body. A `summary()`
renderer on the message vocabulary in `messages.rs` (the module that owns the
shapes), called from `hub.rs::forward`. The `Debug` derives are untouched, so
test-failure output and any other `Debug` use keep full fidelity. Every message
is still logged at `info` — the documented "logs every message it forwards"
property and one-place flow attribution hold; only payload bodies are elided.

# Decisions

- **D1 — Scope: every text-carrying message, not just buffer responses.** All of
  it comes from the one `?msg` log, and leaving `DocumentOpened`'s full text
  would keep the worst line — the whole file on open — in the log.
- **D2 — Approach: a compact summary, not truncation, not omission.**
  Truncation leaks a body prefix and would change `Debug` semantics globally
  (test-failure output included); omission would lose the message from the
  trace. The summary keeps one line per message with everything but payload
  bodies.
- **D3 — The renderer lives in `messages.rs`.** It owns the shapes, so the
  summary matches the vocabulary; `hub.rs::forward` just calls it. The `Debug`
  derives remain unchanged.
- **D4 — Text payloads render as byte length.** `String::len()` is O(1), so the
  summary never iterates the body: `Some(<len> bytes)` / `None` for a buffer
  text, `text: <len> bytes` for a document-open/change.

# Acceptance criteria

1. No "Hub message" line embeds a text payload's body: a `BuffersResponse::Text`
   logs its variant and `None` or the byte length; a `DocumentOpened` logs
   `uri`, `version`, and the byte length; a full-text `DocumentChanged` change
   logs the byte length — never the text itself.
2. Every forwarded message still logs one `info` line — requests, responses, and
   notifications all remain in the trace.
3. The `Debug` derives on `Envelope`/`Message`/`Request`/`Response`/
   `Notification` are unchanged.
4. A hub test asserts the summary of a text-carrying message carries the length
   and not the body (e.g. a `BuffersResponse::Text(Some(…))` renders its byte
   length and omits the source).
5. `cargo build --all-targets`, `cargo fmt --all -- --check`, and `cargo test`
   are clean, with no new clippy warnings.

# Docs

- `docs/architecture.md` — the hub row ("logs every message it forwards" →
  "logs a compact summary of every message it forwards") and the hub decision
  bullet that describes the `info` logging.
- `src/hub.rs` module doc — the "logs every message passing through" sentence,
  to match (a code comment updated with the change).
- `docs/dev/changelog.md` — the shipped entry, at implementation time.

# Implementation plan

## Approach

The hub logs `info!(?msg, "Hub message")` — the full `Debug` of the `Envelope`,
which embeds text payloads as file bodies. Add a `summary()` renderer to the
message vocabulary in `messages.rs`: `Envelope::summary()` composes `summary()`
implementations on `Message`, `Request`, `Response`, and `Notification` that
render each variant's shape — kind, variant, `uri`/position/counts — with text
payloads (`BuffersResponse::Text`, `DocumentOpened.text`, `DocumentChanged`
texts) elided to their byte length. `hub.rs::forward` logs that summary instead
of `?msg`; the `Debug` derives stay untouched, so test-failure output keeps
full fidelity.

## Steps

- [x] `messages.rs` — `Envelope::summary()` plus `Message`/`Request`/`Response`/
      `Notification` summaries: variant + uri/position/counts, text as `<n>
      bytes`.
- [x] `hub.rs` — log `info!(msg = %msg.summary(), "Hub message")`; update the
      module doc's "logs every message passing through" sentence.
- [x] Hub tests — a `BuffersResponse::Text(Some(…))` and a `DocumentOpened`
      summary carry the byte length and omit the body.
- [x] Docs — `docs/architecture.md` hub wording (Layout, module table,
      decisions: "logs a compact summary of every message it forwards");
      `docs/log.md` entry.
- [x] `cargo build --all-targets`, `cargo fmt --all -- --check`, `cargo test`
      clean, no new clippy warnings.

## Progress

Landed as above. Verified: `cargo fmt --all -- --check` clean; `cargo test`
green — 110 library tests and 7 bench tests; the new `a_text_payload_logs_its_length_not_its_body`
and `a_document_open_logs_the_text_length_not_the_body` hub tests pass. Clippy
shows only pre-existing findings (the `never_loop` in `src/bin/java-lsp-bench.rs`,
`let_and_return` in `src/model.rs`, and dead-code warnings). Not committed.
