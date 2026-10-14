---
type: ChangeRequest
kind: feature
title: Report indexing progress to the editor
description: Add a generic progress notification any module can publish, driven by the index and the maven module during warm-up, and rendered by the shell as $/progress gated on the client's window.workDoneProgress capability.
state: done
priority: medium
tags: [dev, lsp, hub]
owner: felix
verified: { by: felix, at: 2026-10-14T00:00:00Z }
---

# Problem

The server builds its index in the background but reports nothing while it does.
On `initialized` the shell notifies `ServerStarted`; the `maven` module resolves
the workspace's Maven project and fetches the dependency sources
(`src/maven/mod.rs`, `spawn`), publishing `FoldersDiscoverred` batches; and the
`index` module scans each batch into its symbol table and declared-type model
(`src/index/mod.rs`, `serve`). None of it reports progress, so a user opening a
large project watches completions and navigation fill in gradually with no
indication of what is running or how long it will take.

The vocabulary for this was lost in the hub rework. `src/messages.rs`'s
`Notification` enum holds only `ServerStarted`, `FoldersDiscoverred`, and the
three `Document*` variants, and the shell's drain task (`src/server.rs`) renders
nothing. The intended shape survives in the deprecated code —
`DriverMessage::Progress(ProgressUpdate::{Begin, Update, End})`, published by the
warm-up drivers (`src/deprecated/jars.rs`, `jdk.rs`, `startup.rs`) and rendered
by the old shell — and `docs/architecture.md` still documents the shell rendering
`Progress` as `$/progress`, gated on the client's `window.workDoneProgress`. The
disabled integration harness (`tests/harness.rs`, guarded by `#![cfg(any())]`)
already pins a wire shape (a `$/progress` begin titled `java-lsp`, at least one
report, and an end, all gated on the capability); its single `java-lsp/warm-up`
token predates the per-publisher split chosen here.

# Proposal

Add a generic progress notification to the hub vocabulary, have the `index` and
`maven` modules drive it during warm-up, and have the shell render it to the
editor.

- `src/messages.rs` gains `Notification::Progress { token: String, update:
  ProgressUpdate }` with `ProgressUpdate::Begin { title, message }`,
  `Update { message, percentage }`, and `End { message }`. The `token` names the
  progress stream — LSP's own grouping key, which `window/workDoneProgress/create`
  registers and every `$/progress` echoes — so the shell maps each update onto the
  right item without guessing, and any module may publish its own stream.
- The `index` module publishes Begin/Update/End on the token `java-lsp/index`
  around each `FoldersDiscoverred` batch it scans: Begin names the phase from the
  batch origin (`"Indexing workspace"` / `"Indexing library sources"`), Update
  reports the files indexed so far (with a percentage once the batch total is
  known), and End reports the batch total.
- The `maven` module publishes the same notifications on the token
  `java-lsp/maven` around its own phases — resolving the project (`resolve_pass`)
  and fetching the dependency sources (`sources::fetch_and_extract`).
- The shell renders each `Notification::Progress` against its token: the first
  update for a token sends `window/workDoneProgress/create` for it (once), and
  every update becomes a `$/progress` notification on that token (Begin → begin,
  Update → report, End → end), so each token is one status-bar item titled
  `java-lsp`. Progress is dropped silently unless the client advertised
  `window.workDoneProgress` in `initialize`.

# Decisions

- **D1 — No new module: the shell renders the progress.** The warm-up publishers
  (`index`, `maven`) only publish the notification; the shell keeps every
  editor-facing message and the `tower_lsp::Client`. Agreed with the requester
  over an earlier idea of a dedicated notification module.
- **D2 — Generic vocabulary, keyed by a token.** `Notification::Progress { token,
  update }` with Begin/Update/End, mirroring the deprecated
  `DriverMessage::Progress` but carrying a `token` that names the progress stream
  — LSP's own grouping key — so `index` and `maven` report with the same shape,
  the shell never guesses which item a message belongs to, and any future module
  can publish its own stream.
- **D3 — Wire mechanism: `$/progress`.** `window/workDoneProgress/create` plus
  `$/progress`, one status-bar item per token titled `java-lsp`, gated on the
  client's `window.workDoneProgress` capability. Follows the intended design in
  `docs/architecture.md`; with no capability the notifications are dropped
  silently.
- **D4 — Publishers: `index` and `maven`.** The `index` reports its scan of each
  batch; the `maven` module reports its resolve and source-fetch phases. The scan
  emits a report per chunk of files, so the item advances without a sink threaded
  through `scan_sources`.
- **D5 — A token per publisher.** `maven` publishes on `java-lsp/maven` and the
  `index` on `java-lsp/index`, so the editor shows one item each: maven reports
  its resolve (and, when it runs, its source fetch) and publishes
  `FoldersDiscoverred` when done; the index then reports its scan of that batch.
  Chosen by the requester over one shared token spanning both modules.
- **D6 — The shell learns the client.** `JavaLanguageServer` stores the
  `tower_lsp::Client` and records the `window.workDoneProgress` capability in
  `initialize`; `main.rs` passes the client `LspService::build` hands the
  constructor into `JavaLanguageServer::new`. The drain task already receives the
  notifications.
- **D7 — Scope: progress only.** Reintroducing `Notice`/`Summary`/`Log` (and the
  "dependency sources disabled" notice) is out of scope.

# Acceptance criteria

1. `src/messages.rs` defines `Notification::Progress { token, update }` with a
   `ProgressUpdate` of `Begin`/`Update`/`End`, and `Notification::summary()`
   renders it compactly (token, phase, counts, no bodies).
2. The `index` publishes Begin/Update/End on `java-lsp/index` around each scan; a
   single-batch workspace yields one begin, at least one report, and one end.
3. The `maven` module publishes progress on `java-lsp/maven` for its resolve phase
   and, when it runs, its source-fetch phase.
4. With `window.workDoneProgress: true`, the shell sends
   `window/workDoneProgress/create` for a token (once) and then `$/progress` begin
   (title `java-lsp`, non-empty message), at least one report, and end; each
   distinct token is a distinct item.
5. With no `window.workDoneProgress`, neither `window/workDoneProgress/create`
   nor `$/progress` is sent.
6. `cargo build --all-targets` is warning-free and `cargo test` passes, with
   tests covering both capability branches.
7. No new module; the request-serving behaviour is otherwise unchanged.

# Docs

- `docs/architecture.md` — the LSP shell row and the shell implementation note
  already describe rendering `Progress` as `$/progress`; correct them to the
  shipped shape (the `Notification::Progress { token, update }` vocabulary, one
  item per token, `window.workDoneProgress` gating), narrow the data-flow arrow
  `HUB --> S "Diagnostics, Progress, Notice"` to what is live, add the publish
  side to the index and maven rows, and record the landed pass in the rework
  note.
- `src/messages.rs` module and hub doc comments — describe `Notification::Progress`
  as the generic progress vocabulary every warm-up module may publish.
- `docs/dev/changelog.md` — a new entry when the change is implemented.

# Implementation plan

## Approach

`messages.rs` gains a `ProgressUpdate` enum (`Begin { title, message }`,
`Update { message, percentage }`, `End { message }`) and a
`Notification::Progress { token: String, update: ProgressUpdate }` variant, with
a `summary()` arm rendering the token and phase.

The `index`'s `serve` loop (`src/index/mod.rs`) sends `Begin` before scanning a
`FoldersDiscoverred` batch and `End` after, and — because the scan runs on the
blocking pool — clones the `HubClient` into the `spawn_blocking` closure and
sends an `Update` per chunk of the batch's file list (percentage from the running
count). Token `java-lsp/index`; phase from the batch's `FileOrigin`.

The `maven` spawn (`src/maven/mod.rs`, `spawn`) wraps `resolve_pass` and
`fetch_and_extract` with `Begin`/`Update`/`End` on token `java-lsp/maven`. Its
`Update`s report counts (dependencies resolved, sources fetched); granular
per-artifact progress inside the producers is not threaded (coarse begin/end is
acceptable there, D4).

The shell (`src/server.rs`) takes the `tower_lsp::Client` in
`JavaLanguageServer::new` and moves it into the existing drain task, alongside a
shared `Arc<AtomicBool>` holding the client's `window.workDoneProgress`
capability (recorded in `initialize`). The drain task keeps the set of tokens it
has created; for each `Notification::Progress` it creates the token once
(`window/workDoneProgress/create`), then sends `$/progress` (`Begin`/`Report`/
`End`) through `lsp_types::notification::Progress`. With the capability unset it
drops the notification. `main.rs` passes the client through.

Tests: a new enabled `tests/progress.rs` drives the live server through
`LspService` (maven + index + shell) and asserts both capability branches; a
`messages.rs` unit test covers `Notification::Progress` summary; `maven` and
`index` unit tests assert the publishers emit Begin/Update/End on their tokens.

## Steps

- [x] `src/messages.rs` — add `ProgressUpdate` and `Notification::Progress { token,
      update }`; extend `summary()`; add the summary unit test; update the module
      and `Notification` doc comments.
- [x] `src/index/mod.rs` — publish Begin/Update/End (token `java-lsp/index`)
      around each batch scan, chunked reports from inside the scan; add a test
      that a subscriber receives the three updates.
- [x] `src/maven/mod.rs` — publish Begin/Update/End (token `java-lsp/maven`)
      around `resolve_pass` and `fetch_and_extract`; add a test asserting the
      published progress.
- [x] `src/server.rs` — take and store the `tower_lsp::Client`, record
      `window.workDoneProgress` in `initialize`, and render
      `Notification::Progress` per token as `window/workDoneProgress/create` +
      `$/progress`.
- [x] `src/main.rs` — pass the client into `JavaLanguageServer::new`.
- [x] `tests/progress.rs` (new) — an enabled integration test: with the capability
      a create + begin + report + end arrive; without it, none do.
- [x] `docs/architecture.md` — correct the LSP shell row and the shell
      implementation note to the shipped shape (one item per token, capability
      gating, the `Notification::Progress { token, update }` vocabulary), narrow
      the `HUB --> S` data-flow arrow, add the publish side to the index and maven
      rows, and record the landed pass in the rework note.
- [x] `docs/dev/changelog.md` — a new bullet under today's date, linking this
      request.
- [x] Verify with `cargo build --all-targets`, `cargo fmt --check`, and
      `cargo test`.
