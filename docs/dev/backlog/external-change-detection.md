---
type: ChangeRequest
kind: bug
title: Detect external file changes and keep the analysis model fresh
description: A file the editor never told the server about leaves stale diagnostics, and model-based features read a warm-up-only type model, so cross-file edits are invisible to completion but not to definition.
state: done
priority: high
tags: [dev, bug, lsp, index]
owner: felix
verified:
  by: cargo test --all-targets (236 passed - 203 lib, 6 bench, 26 harness,
    1 stdio) and cargo clippy --lib (16 warnings, unchanged) and
    java-lsp-bench --files 200 --methods-per-class 5 (first responses <= 1.0 ms,
    hover RTT during warm-up max 0.6 ms, post-warm-up 0.2 ms)
  at: 2026-09-24T20:49:11Z
---

# Problem

Two related defects surfaced after the create-symbol quick fixes shipped.

**1. The server never learns about files the editor did not tell it about.** The
workspace index is updated only from `didOpen`/`didChange` (open buffers) and on
close re-reads; there is no file watcher. A `.java` file created on disk, or
edited in a module that is not open, is therefore invisible: a class the user
creates themselves leaves its use flagged forever. Worse, even once such a file
*were* known, diagnostics are recomputed only for the document that changed, so
the *referring* document (`Main.java`) is never re-analysed and its squiggles
never clear.

**2. Model-based features read a warm-up-only type model.** `.`-member
completion (`member_items`), hover, signature help, and inlay hints build
`TypeQuery::new(index.type_model(), local_model(document))`, where
`index.type_model()` is built once during warm-up and `local_model` covers only
the *requested* document. Definition does not have this problem: when the model
cannot pin a member, `call_definition` returns `None` and `definition` falls
back to the always-current index (`unique_location(self.index.query_name(word))`).
So after a method is added to another **open** file, go-to-definition reaches it
while `.`-completion returns nothing — an inconsistency the user hit directly.

Impact: stale red squiggles on valid code and missing completions whenever code
changes outside the current buffer or across files — the everyday
"create a class/method and expect the tool to catch up" workflow.

# Reproduction

1. The commit's `example/` workspace: `Main.java` in `com.example.app`, and
   `Data.java` in the sibling `greeting-lib` module.
2. Create `XY.java` on disk (never opened in the editor), then add
   `var xyz = new XY();` to `Main.java`.
3. Add a `shout()` method to `Data.java` (not open), then add `data.shout();`
   to `Main.java`.

Observed:

- `Main.java`'s diagnostics never react: no diagnostic appears for the
  unresolved `XY`, and none clears for the now-valid `shout()`.
- If instead the file is **open** when a method is added (`didChange` updates the
  index), go-to-definition on `widget.newMethod()` resolves but `.`-completion
  after `widget.` returns nothing.

Isolated in a unit test: with the workspace type model built before the edit,
`engine.definition(...)` returns `Some(Location { Widget.java })` while
`engine.completions(...)` after `w.` returns an empty array.

Expected: creating or changing a `.java` file — on disk or in another open
buffer — updates the index **and** the analysis model, and refreshes published
diagnostics for every open document that can see it.

# Proposal

Three coordinated changes in the engine, shell, and index:

1. **Watch `.java` files.** Register `workspace/didChangeWatchedFiles` dynamically
   in `initialized` (`client/registerCapability`, glob `**/*.java`) and handle the
   notification: Created/Changed → re-read and re-index from disk (never over an
   open buffer); Deleted → drop the file's entries.
2. **Republish diagnostics for every open document** whenever the index or model
   changes — from a watched-file event or an open/change/close — not just for the
   document that changed, so a referring file's squiggles refresh.
3. **Keep the analysis model fresh.** Build the overlay from **all open buffers**
   (not just the requested one) and track "dirty" source files per URI so a
   changed file's types replace their warm-up contribution, letting model-based
   features (completion, hover, signature help, hints) see edits to other files —
   the same edits definition already reaches through the index.

# Decisions

- **D1 — A file watcher is the mechanism for on-disk changes.** Register
  `workspace/didChangeWatchedFiles` for `**/*.java` dynamically in `initialized`
  and handle it in the shell, forwarding to a new engine command that re-indexes
  created/changed files and drops deleted ones. Reason: LSP gives no other signal
  for files the editor never opened, and the architecture already records the
  absence of a watcher as a v1 limitation — this bug is a consequence of it.

- **D2 — Only source files, and never over an open buffer.** A watched event is
  honoured only for a path inside a known source root (or the workspace root when
  there is no project model), and is ignored for a file the editor currently has
  open, whose `didChange` is authoritative. Reason: avoid indexing stray files and
  fighting the editor over the same buffer.

- **D3 — Republish diagnostics for all open documents on an index/model change.**
  Reason: the referring document does not change when another file gains the
  missing symbol, so per-document recomputation cannot clear it. Cost is bounded
  by the number of open documents; it is measured with the bench and, if the
  inline dispatcher path regresses (R6), moved to a spawned task — the same
  guard the diagnostics change already uses.

- **D4 — The analysis overlay covers all open documents.** Replace
  `local_model(document)` (the requested buffer only) with an overlay built from
  every open buffer, used by `member_items`, `hover`, `signature_help`,
  `inlay_hints`, `resolve_target`/`call_definition`, and `semantic_diagnostics`.
  Reason: these read the warm-up model, which never sees later edits; definition
  already falls back to the index, and the two must agree.

- **D5 — Track dirty source types per URI; the model is warm-up base plus the
  union of dirty sources.** On open/change take the buffer's types; on a watched
  change/create of a non-open file take the disk file's types; on delete drop the
  URI. Reason: this replaces one file's contribution without a full re-scan (R6),
  and shadows the stale warm-up entry for an edited file because `TypeQuery`
  prefers the overlay. Known consequence: a *deleted* file that existed only in
  the warm-up base keeps its types until restart — fixed by
  [Forget a deleted source file in the type model](deleted-file-stays-in-type-model.md).

- **D6 — Capability fallback.** If the client does not support watched-file
  dynamic registration, skip the watcher; the server still republishes on
  open/change/close and still covers open buffers via D4. Reason: not every
  client supports the capability, and the rest of the fix is client-agnostic.

- **D7 — No new dependency, no protocol beyond LSP.** The watcher is one
  `client/registerCapability` request plus the `workspace/didChangeWatchedFiles`
  handler; `tower-lsp` supports both. Reason: the fix is protocol plumbing, not a
  new mechanism.

# Acceptance criteria

- With `Main.java` (`com.example.app`) using `new XY()` and `XY.java` created on
  disk (or in `com.example.app`), saving `XY.java` makes the referring file's
  diagnostic clear within a watcher round-trip, **without any edit to
  `Main.java`**; when `XY`'s package differs, the correct "needs an import"
  diagnostic appears instead.
- Adding `shout()` to `Data.java` (a different module, not open) and using
  `data.shout()`: once `Data.java` is saved, the unresolved-member diagnostic
  clears and `.`-completion on `data.` offers `shout`.
- Adding a method to another **open** file: `.`-completion, hover, signature
  help, and inlay hints all see it — the completion/definition asymmetry is gone.
- A watched event for a file the editor currently owns does not override the
  buffer.
- With no watched-file support advertised, the server behaves as today plus
  open/close republishing (no error, no watcher registration).
- `cargo test --all-targets` is green; the regression tests added during
  isolation (add-import clears; same-file member completion) remain, and a
  cross-file test proves completion and definition now agree.
- `cargo clippy --lib` introduces no new warnings over the current 16.
- `java-lsp-bench` shows no regression in first-response or warm-up hover RTT; if
  the republish or model-union cost regresses the benchmark, the work moves off
  the dispatcher's inline path.

# Docs to update

- `docs/architecture.md` — the workspace-index bullet's "there is no file
  watcher" limitation is now wrong (record the watcher and D2's rules); the
  diagnostics paragraph gains the republish-all behaviour; the completion
  paragraph's "layered with the open buffer" becomes "all open buffers".
- `docs/requirements.md` — note the watcher and refresh under the appropriate
  requirement (R5/R6 wording), if it changes what is claimed.
- `README.md` — if it claims out-of-editor changes are picked up on close
  re-reads only.
- `docs/dev/backlog/index.md` — this request's row.
- `docs/dev/changelog.md` — an entry at implementation.

# Implementation plan

## Approach

The fix has three parts, matching Proposal 1–3 and D1–D7. No new dependency and
no protocol beyond stock LSP (D7).

**Watcher (D1, D2, D6).** `server.rs` reads
`capabilities.workspace.didChangeWatchedFiles.dynamicRegistration` in
`initialize` (alongside the existing `resourceOperations` read) and stores it in
an `AtomicBool`. In `initialized`, when it is set, the shell sends a
`client/registerCapability` request for `workspace/didChangeWatchedFiles` with one
watcher, glob `**/*.java`. The registration is spawned fire-and-forget (like
`create_progress`) so the handshake can never block on a client that does not
answer. A new `did_change_watched_files` handler maps each `FileEvent` to a
`(Url, WatchedChange)` pair and forwards them to the engine; a client that did not
advertise the capability simply never gets one, and the rest of the fix stands
(D6).

**Engine command (D1, D2).** `engine.rs` gains
`Command::WatchedFiles { changes: Vec<(Url, WatchedChange)> }`, a `WatchedChange`
enum (`Created`/`Changed`/`Deleted`), and `EngineHandle::watched_files`. The
dispatcher applies it inline (it mutates) and, when the engine reports a change,
republishes diagnostics for all open documents. `analysis.rs` implements
`TreeSitterEngine::watched_files`: for each event it skips a URI the editor
currently has open (D2, `didChange` is authoritative), honours only a path inside
a known source root (or the workspace root when no project model exists), then
either re-reads/parses/re-indexes the file (Created/Changed) or drops its index
entries (Deleted). The source-root test is factored out of `reindex_from_disk`
into `is_workspace_source` and shared.

**Fresh model (D4, D5).** `TreeSitterEngine` gains
`dirty: Mutex<HashMap<Url, TypeModel>>` — one declared-type model per URI whose
types currently differ from the warm-up base. `store_tree` (open/change) records
the buffer's model; a watched create/change records the disk file's model; a
watched delete or a close outside every source root drops the entry. A new
`overlay_model()` unions the dirty models, and the model-based features
(`member_items`, `hover`, `signature_help`, `inlay_hints`, `resolve_target` /
`call_definition`, `code_actions`, and `semantic_diagnostics`) build
`TypeQuery::new(index.type_model(), &overlay_model())` instead of
`local_model(document)`. `TypeQuery` already prefers the overlay, so an edited
file's types shadow their stale warm-up contribution and every open file's edits
are visible — the same world definition already reaches through the index. The
documented consequence — a deleted file that existed only in the warm-up base
keeps its types until restart — is fixed by
[Forget a deleted source file in the type model](deleted-file-stays-in-type-model.md).

**Republish all (D3).** The dispatcher tracks the last version per open URI (from
`Open`/`Change`) and, after an open, change, close, or a watcher event that
changed the index/model, publishes diagnostics for **every** open document
(the changed one first), not just the one edited. `publish_diagnostics` becomes
`publish_all_diagnostics`, fed by a new `TreeSitterEngine::open_documents()`. It
runs inline on the dispatcher as today; the bench decides whether it must move to
a spawned task (R6).

## Steps

- [x] Add `dirty: Mutex<HashMap<Url, TypeModel>>` to `TreeSitterEngine`
      (`analysis.rs`), with `record_types`/`drop_types`/`overlay_model` helpers,
      and record the model in `store_tree`; extract `is_workspace_source` from
      `reindex_from_disk` and have the latter refresh the disk file's types too.
- [x] Replace `local_model(document)` with `overlay_model()` in `member_items`,
      `resolve_target`, `hover`, `call_definition`, `completions`,
      `signature_help`, `inlay_hints`, and `code_actions`; change
      `semantic_diagnostics` to take the overlay model.
- [x] Add `WatchedChange`, `Command::WatchedFiles`, `EngineHandle::watched_files`,
      and the `watched_files` engine method (skip open buffers, source-root only,
      re-index or drop, update `dirty`).
- [x] Add `open_documents()` to `TreeSitterEngine`; track per-URI versions in the
      dispatcher and turn `publish_diagnostics` into `publish_all_diagnostics`
      invoked on open/change/close/watched change (`engine.rs`).
- [x] Register `workspace/didChangeWatchedFiles` for `**/*.java` in `initialized`
      when the client capability allows, and add the `did_change_watched_files`
      handler (`server.rs`); keep the capability fallback silent (D6).
- [x] Keep the two isolation regression tests in `analysis.rs`.
- [x] Add a cross-file `analysis.rs` test proving completion and definition agree
      after a method is added to another open file, plus watched create/delete
      unit tests.
- [x] Add a `tests/harness.rs` test: with the capability advertised,
      `client/registerCapability` arrives, and a `workspace/didChangeWatchedFiles`
      create of a file on disk clears the referring document's diagnostic.
- [x] Run `cargo test --all-targets` green and `cargo clippy --lib` with no new
      warnings over the current 16; run the bench and, only if it regresses,
      move the republish off the dispatcher's inline path.
- [x] Update `docs/architecture.md`: the workspace-index bullet's "there is no
      file watcher" limitation (record the watcher and D2's rules), the
      diagnostics paragraph (republish-all), and the completion paragraph (all
      open buffers).
- [x] Update `docs/requirements.md` (R5/R6 wording if it changes what is
      claimed) and `README.md` (out-of-editor changes claim, if present).
- [x] Update `docs/dev/backlog/index.md` (row to `done`) and append a
      `docs/dev/changelog.md` entry.

## Implementation notes

- Tests added in `analysis.rs`: one cross-file end-to-end test
  (`a_method_added_to_another_open_file_completes_and_defines_alike`) plus two
  watcher unit tests (`a_watched_create_is_indexed_and_a_delete_drops_it_again`,
  `a_watched_event_for_an_open_buffer_is_ignored`), and two harness tests
  (`watched_file_changes_refresh_a_referring_document`,
  `watched_files_are_not_registered_without_the_client_capability`).
- One existing test changed expectation: `member_completions_import_their_enclosing_type`
  now sees the helper buffer in the overlay (D4), so the member is offered with
  its resolved signature rather than the `Container.name` fallback; the import
  edit assertion is unchanged.
- Assumption: the bench opens one document, so it does not exercise the
  republish-all cost with many open buffers. With a single open document the
  numbers are unchanged (R6 holds); no move off the dispatcher's inline path was
  needed. The bench should be revisited if profiles with many open documents
  appear.
- Implementation is **uncommitted**, left for review; no commit or branch was
  created, so no commit is linked in the body.
