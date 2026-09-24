---
type: ChangeRequest
kind: bug
title: Forget a deleted source file in the type model
description: Deleting a source file on disk drops its index entries but leaves its types in the declared-type model, so member completion, hover, signature help, and inlay hints still resolve the deleted type.
state: done
priority: high
tags: [dev, bug, lsp, index]
owner: felix
verified:
  by: cargo test --all-targets (239 passed - 206 lib, 6 bench, 26 harness,
    1 stdio) and cargo clippy --lib (15 warnings, none in the changed code) and
    java-lsp-bench --files 200 --methods-per-class 5 (first responses <= 1.1 ms,
    hover RTT during warm-up max 0.9 ms, post-warm-up 0.1 ms)
  at: 2026-09-24T21:15:00Z
---

# Problem

`external-change-detection` added a file watcher and a "dirty" type overlay, but
a file that existed at warm-up and is then **deleted** is only partly forgotten.
The index drops it (`WorkspaceIndex::remove_file`) and the overlay drops it
(`drop_types`), yet the file's types were merged into the warm-up
`TypeModel` — a single, immutable, name-keyed model built by `scan_workspace_core`
and stored via `index.set_types`. That merged model cannot forget a file, so the
deleted type's members are still resolved by every **model-based** feature:
`.`-member completion, hover, signature help, and inlay hints. Index-based
features (definition, references, workspace symbols, unqualified completion) drop
it correctly.

Impact: delete a class and it keeps completing and hovering — the type appears
to "stay in the index" even though its index entries are gone. This is the
index-vs-model asymmetry again, now on deletion (the previous request's D5 called
this a documented consequence and deferred it; it is not acceptable).

# Reproduction

Isolated with a unit test (`a_deleted_warmup_source_is_forgotten` in
`src/analysis.rs`, added with this request):

1. A scanned workspace with `a/Widget.java` (`public void run() {}`) and
   `a/Use.java` (`Widget w = null; w.run();`); open `Use.java`.
2. `completions` after `w.` offers `run` (correct).
3. Delete `a/Widget.java` on disk and deliver `watched_files([(Widget.java,
   Deleted)])`.

Observed: `engine.index.query_name("Widget")` is empty, but `completions` after
`w.` still offers `run` — "deleted type still resolves: [\"run\"]".

Expected: after the delete, no model-based feature resolves `Widget` or its
members.

# Proposal

Split the declared-type model so a single source file's contribution can be
removed. Today `scan_workspace_core` merges workspace sources, dependency jars,
and the JDK into one `TypeModel`. Instead, keep the **non-source base** (jars and
JDK) merged as today, and keep each **workspace source file's** types in its own
model keyed by URI. The analysis overlay (the `dirty` overlay the previous
request introduced) becomes the union of the current per-source models, with the
dirty entry for a URI — an open buffer, a watched change, or a close re-read —
overriding its warm-up model. A delete then removes the URI from both maps, so
the type disappears from the index **and** the model.

# Decisions

- **D1 — Split the model into a non-source base and per-source-file models.**
  `scan_workspace_core` keeps merging dependency jars and the JDK into the base
  (unchanged cost and memory) and stores each workspace source file's
  `TypeModel` under its URI. Reason: only a per-file split can forget one file;
  a merged, name-keyed model cannot.

- **D2 — The overlay is the union of current sources, dirty overriding per URI.**
  The engine's overlay becomes `union(per-source models)`, with the `dirty` entry
  for a URI replacing its warm-up entry. Reason: create/change/delete all reduce
  to one map operation, and the previous request's D4/D5 behaviour (open buffers
  and watched edits visible cross-file) is preserved.

- **D3 — A delete removes the URI from both the source models and `dirty`.** The
  index removal already happens; the model must agree. Reason: index and model
  drifting is exactly this bug.

- **D4 — No re-scan on delete (R6).** Removal is a map delete, not a source-root
  re-scan. Reason: a re-scan is O(workspace) per deletion and would block.

- **D5 — The base model's meaning narrows to "jars and JDK".** `index.type_model()`
  (or its successor) no longer contains workspace source types; every consumer
  already pairs it with the overlay (after the previous request), so this is a
  rename/re-shaping, not a new query path. Reason: keeps one obvious place for a
  file's types.

- **D6 — The watcher and republish rules are unchanged.** This request changes
  only what the model holds and how a deletion is recorded; the watcher, the
  open-buffer skip, and the republish-all-on-change behaviour stay as
  `external-change-detection` decided. Reason: they are orthogonal and already
  tested.

# Acceptance criteria

- `a_deleted_warmup_source_is_forgotten` passes: after deleting a warm-up source
  file and delivering the delete event, `index.query_name` is empty **and**
  completion after the receiver's `.` no longer offers the deleted member; hover,
  signature help, and inlay hints likewise no longer resolve the deleted type.
- A file that existed only in the dirty overlay (created/edited after warm-up) and
  is then deleted still leaves nothing behind.
- The previous request's behaviour is preserved: a created/edited file (open
  buffer or watched change) is visible cross-file; a watched event for an open
  buffer is still ignored; diagnostics for referring documents still republish.
- `cargo test --all-targets` green (the current 236 tests plus the regression
  test), and `cargo clippy --lib` gains no warnings over the baseline of 16.
- `java-lsp-bench` shows no regression in first-response or warm-up hover RTT.

# Docs to update

- `docs/architecture.md` — the type-layer and workspace-index bullets: the model
  is a non-source base plus per-source-file models, and a deleted file is
  forgotten; correct the previous request's "keeps its types until restart"
  consequence.
- `docs/dev/backlog/external-change-detection.md` — its D5 note says the deletion
  limitation is deferred to "until restart"; point it at this request instead.
- `docs/dev/backlog/index.md` — this request's row.
- `docs/dev/changelog.md` — an entry at implementation.

# Implementation plan

## Approach

Only the *storage shape* of the declared-type model changes; no query path, no
watcher rule, and no feature call site changes its behaviour (D6). The engine
already builds `TypeQuery::new(base, overlay)` at nine model-based call sites;
the base becomes "jars + JDK + library sources" and the overlay becomes the
union of the current per-source models with the dirty entries overriding them.

**Index (`src/index.rs`) — split the model (D1, D3, D5).** `WorkspaceIndex`
gains `source_types: Arc<RwLock<HashMap<Url, Arc<TypeModel>>>>`: the declared-type
model of each *workspace source file* from the warm-up scan, keyed by URI. The
existing `types` slot (`set_types`/`type_model`) keeps its shape but narrows to
the non-source **base** — dependency jars, the JDK, and (from the library-source
pass) extracted dependency sources, none of which is a workspace source file.
`remove_file` also drops the URI from `source_types`, so a single call forgets a
file's index entries *and* its model — the map delete D4 requires, never a
source-root re-scan. Two new accessors: `set_source_types(&Url, Arc<TypeModel>)`
used by the scan, and `source_models() -> Vec<(Url, Arc<TypeModel>)>` (ordered by
URI) for the engine to union. `scan_workspace_core` builds each source file's
`TypeModel` as it parses it (leaving the base untouched by source types) and
stores it under the file's URI; jars and the JDK extend the base as before.
`sources.rs::index_extracted` is already correct: it clones `type_model()` (the
base), extends it with library-source types, and writes it back.

**Analysis (`src/analysis.rs`) — the overlay is the source union (D2).**
`overlay_model()` currently unions only the `dirty` map. It becomes the union of
`index.source_models()` with, per URI, the `dirty` entry replacing the warm-up
entry, plus any `dirty` URI that has no source model (a file created after
warm-up). `store_tree`, `record_types`, `drop_types`, `is_workspace_source`,
`watched_files`, `close`, `open_documents`, and `is_open` are unchanged; the
delete path already calls `index.remove_file` and `drop_types`, and with
`remove_file` dropping the source model both maps now agree (D3). No model-based
call site changes: base is still `index.type_model()` and the overlay is still
`overlay_model()`.

**`src/types.rs` — unchanged.** `TypeModel::merge` (from the previous request)
is all that is needed to union the per-source models.

**Tests.** The regression test `a_deleted_warmup_source_is_forgotten` (already
present, currently failing) must pass. Add: a hover test that the deleted type's
member no longer resolves, and a dirty-only test (a file created through a
watched event and then deleted, never open) that leaves nothing behind. Update
`src/index.rs`'s `scan_workspace_indexes_a_directory_tree_and_sets_ready`, whose
`type_model()` assertions on workspace source types move to `source_models()`
(the base no longer carries them).

## Steps

- [x] `src/index.rs`: add the per-source `source_types` map to `WorkspaceIndex`
      with `set_source_types`/`source_models`; have `remove_file` drop the URI's
      source model too; split `scan_workspace_core` so source files go to
      `source_types` and only jars/JDK extend the base `set_types`. (D1, D3, D5.)
- [x] `src/analysis.rs`: make `overlay_model()` the union of
      `index.source_models()` with `dirty` overriding per URI (plus dirty-only
      URIs). Leave the watcher and the republish rules untouched; every
      model-based feature keeps its behaviour, though the two occurrence
      collectors that still paired the base with a per-file model also move to
      `overlay_model()` (see notes). (D2, D6.)
- [x] Update `src/index.rs`'s `scan_workspace_indexes_a_directory_tree_and_sets_ready`
      to assert the per-source models via `source_models()`. (AC — suite green.)
- [x] Make `a_deleted_warmup_source_is_forgotten` pass, and add a hover
      regression (the deleted member no longer hovers) and a dirty-only delete
      test. (AC1, AC2.)
- [x] `cargo test --all-targets` green and `cargo clippy --lib` no new warnings
      over the baseline of 16; run `java-lsp-bench` to confirm no first-response
      or warm-up hover RTT regression. (AC4, AC5.)
- [x] `docs/architecture.md`: the type-layer bullet records the non-source base
      plus per-source-file models and that a deleted file is forgotten; correct
      the workspace-index bullet's "keeps its warm-up types in the model until
      restart" limitation. (Docs.)
- [x] `docs/dev/backlog/external-change-detection.md`: its D5 "until restart"
      consequence points at this request instead. (Docs.)
- [x] `docs/dev/backlog/index.md` row to `done` and a `docs/dev/changelog.md`
      entry. (Docs.)

## Implementation notes

- `overlay_model()` builds the union on each call: it iterates
  `index.source_models()` and, per URI, takes the dirty entry if present else the
  warm-up model, then merges dirty-only URIs (files created after warm-up). The
  bench shows this is invisible (post-warm-up hover RTT 0.1 ms), so no cache was
  added.
- Two call sites the previous request did not convert — `collect_occurrences`
  (`TargetKind::Local`) and `collect_member_occurrences` — still paired
  `index.type_model()` with a per-file `local` model. With the base narrowed to
  non-source types they lost the ability to resolve a receiver's type declared in
  another workspace file, so both now build their query from `overlay_model()`.
- `collect_member_occurrences` keeps its 7-argument signature (it builds the
  overlay itself) rather than taking the base and overlay as parameters, which
  would have tripped `clippy::too_many_arguments`.
- `cargo clippy --lib` reports 15 warnings, none in the changed files; the
  stated baseline was 16, so no warning was added.
- Assumption: the bench opens one document and its hover samples run after
  warm-up, so the per-call union cost is not exercised at scale; it is measured
  and holds (R6).
- Implementation is **uncommitted**, left for review; no commit or branch was
  created, so no commit is linked in the body.
