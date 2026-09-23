---
type: ChangeRequest
kind: improvement
title: Warm-up and source-fetch progress reporting
description: Report the background warm-up and dependency-source fetch to the client through LSP work-done progress and a single notice message.
state: done
priority: medium
tags: [dev, improvement, observability, lsp]
owner: felix
verified:
  by: cargo test --all-targets (198 passed)
  at: 2026-09-23T21:02:59Z
---

# Problem

The background warm-up and the dependency-source fetch are invisible to the
user. Everything they do is reported only to stderr (`RUST_LOG`), so an editor
shows nothing while the index is built and nothing while sources are downloaded
— a Java developer opening a large project sees a server that is silently busy.
Two consequences: the sources feature looks like it does nothing on first open
(go-to-definition into a library simply returns nothing until the fetch lands),
and no one can tell whether a long warm-up is progressing or stuck. The
`message-based-engine` refactor gave the engine a reverse channel
(`EngineEvent`), but its only variant is `Diagnostics`, so nothing else is
reported.

# Proposal

Report the background job through the standard client-messaging mechanisms, so
any LSP client can show it:

1. **Work-done progress** — `window/workDoneProgress/create` followed by
   `$/progress` (Begin/Report/End) drives a single status-bar item titled
   `java-lsp`, whose message names the current phase with counts: indexing
   source files, dependency jars, JDK classes, then fetching/parsing dependency
   sources. This is the mechanism that puts a progress entry in Zed's status
   bar, the same way rust-analyzer does.
2. **One notice message** — a single `window/showMessage` (Info) when
   dependency sources are disabled by `JAVA_LSP_OFFLINE` *and* the workspace
   actually resolved dependencies, so the user learns why library
   go-to-definition is unavailable instead of silently getting nothing.
3. **stderr logging is unchanged** — the existing `tracing` lines stay as the
   diagnostic channel; protocol messages are best-effort and some clients
   ignore them.

Progress is emitted through the existing engine boundary: the warm-up reports
into the event channel, and the shell's drain task turns each report into the
corresponding client message.

# Decisions

- **Work-done progress, not `showMessage`, for the background job** — that is
  the mechanism purpose-built for a status-bar item and what rust-analyzer uses;
  `showMessage` is reserved for the one discrete notice. (Earlier discussion:
  `showMessage` surfaces as a notification, work-done progress as the status-bar
  entry.)
- **One progress item for the whole job**, with the `message` changing per phase
  and a `percentage` only during the download pass (completed artifacts out of
  the total). No byte-level progress, no separate item per phase — the job is
  short-lived and one item is what a user reads.
- **Emitted through the message boundary** — the core reports with a small
  `Reporter` handle (wrapping the same event sender), so no new trait or sink
  abstraction is introduced; the shell maps `EngineEvent::Progress` to
  `$/progress` and `EngineEvent::Message` to `window/showMessage`.
- **Gated on the client capability** — `initialize` reads
  `capabilities.window.workDoneProgress`; when it is absent or false, no
  progress or notice is sent at all (the events are consumed and dropped).
- **The `create` request is not awaited** — `window/workDoneProgress/create` is
  sent once (fire-and-forget, on the same ordered transport, so it precedes the
  first `$/progress`) and its response is ignored. Awaiting it would deadlock a
  client that never replies (including the test harness), and the capability
  flag is the real gate.
- **Not cancellable in v1** — the job is not abortable, so no cancel button is
  advertised (`cancellable: false`) and a `window/workDoneProgress/cancel` is
  ignored.
- **Failures stay in logs** — a dependency whose sources cannot be fetched
  already logs a warning; it does not become a user-visible message, so a
  project with many source-less artifacts does not nag on every open.
- **The offline notice is narrow** — only when `JAVA_LSP_OFFLINE` is set *and*
  at least one dependency with a jar was resolved; otherwise nothing is shown
  (no workspace, no dependencies, or online all stay silent).
- **No behaviour change beyond messaging**: the same warm-up, the same results,
  the same non-blocking guarantees (R6); this only adds client notifications.

# Acceptance criteria

- With a client that advertises `window.workDoneProgress`, opening a workspace
  produces exactly one progress item: `window/workDoneProgress/create`, then
  `$/progress` with a `begin` (a title and a message), intermediate `report`
  updates naming the phase and counts, and a final `end`.
- The download phase reports a rising `percentage` over the number of artifacts
  being fetched.
- With a client that does not advertise `window.workDoneProgress`, no
  `$/progress` and no `window/workDoneProgress/create` are sent; every other
  behaviour is unchanged.
- A workspace with dependencies opened with `JAVA_LSP_OFFLINE` set yields a
  single `window/showMessage` (Info) explaining that dependency sources are
  disabled; a workspace with no dependencies, or without the flag, yields none.
- stderr logging is unchanged.
- The full suite still passes (`cargo test --all-targets`).

# Docs touched

- `docs/architecture.md` — the engine-boundary bullet gains the progress/message
  events and the `Reporter`; the LSP-shell bullet gains the drain task's
  `$/progress` and `window/showMessage` behaviour and the capability gate.

# Implementation plan

## Approach

Add one reporting type and two event variants, thread a reporter through the
warm-up, and teach the shell's drain task to speak the client protocol. No new
dependencies (the LSP progress types are already in `lsp-types`).

- **`src/engine.rs`** — `ProgressUpdate` (`Begin { title, message }` /
  `Update { message, percentage }` / `End { message }`), `MessageLevel`
  (`Info`/`Warning`), a cloneable `Reporter` (holds the events sender; a
  detached `Reporter` is a no-op), and two `EngineEvent` variants
  (`Progress(ProgressUpdate)`, `Message { level, text }`). `spawn` attaches a
  `Reporter` to the core.
- **`src/analysis.rs`** — `TreeSitterEngine` holds a `Mutex<Reporter>` with a
  `set_reporter` setter and passes a clone into the warm-up from
  `set_workspace_root`. The reporter defaults to detached, so the ~160 unit
  tests are unaffected.
- **`src/index.rs`** — `scan_workspace_core` takes a `&Reporter` and reports
  the phase counts (`ScanOutcome` gains the counts it already logs);
  `scan_workspace_async` takes a `Reporter`, emits the offline notice (via a
  small pure `offline_notice` helper) and the final `End`.
- **`src/sources.rs`** — `index_sources`/`fetch_sources`/`index_extracted` take
  a `&Reporter` and report the fetch phase with a rising percentage and the
  parse phase with a count.
- **`src/server.rs`** — `initialize` records the `window.workDoneProgress`
  capability into an `Arc<AtomicBool>`; the drain task handles `Progress`
  (`window/workDoneProgress/create` once, fire-and-forget, then `$/progress`)
  and `Message` (`window/showMessage`); a `window/workDoneProgress/cancel` is
  ignored (no handler).
- **Tests** — an `engine.rs` unit test for `Reporter`'s event stream; a
  `index.rs` unit test for `offline_notice`; two harness tests (progress is
  reported when the capability is advertised, and is not sent without it).
- **`docs/architecture.md`** — as noted above.

## Steps

- [x] Add `ProgressUpdate`, `MessageLevel`, `Reporter`, and the two `EngineEvent`
      variants to `src/engine.rs`; attach the reporter in `spawn`; unit-test
      the reporter's event stream. (AC: reports reach the event channel)
- [x] Thread the reporter through `src/analysis.rs` (`set_reporter`,
      `set_workspace_root`) and `src/index.rs` (`ScanOutcome` counts, phase
      reports, `offline_notice`, final `End`); unit-test `offline_notice`.
      (AC: phases with counts)
- [x] Report the source fetch in `src/sources.rs` (phase messages and a rising
      percentage over the artifacts). (AC: download percentage)
- [x] Teach the shell (`src/server.rs`) to gate on the capability and to send
      `window/workDoneProgress/create` + `$/progress` and `window/showMessage`;
      add the two harness tests. (AC: progress present with the capability,
      absent without it; the offline notice is sent only when warranted)
- [x] Run `cargo test --all-targets`. (AC: suite green, stderr logging intact)
- [x] Update `docs/architecture.md` per the doc note above. (Doc step)
