---
type: ChangeRequest
kind: bug
title: Keep the request path responsive while diagnostics and references run on a large workspace
description: On a ~10k-file workspace with many open buffers, typing stalls completion and navigation, diagnostics take seconds to update, and references is slow or never returns, because analysis runs inline on the single dispatcher, holds the global documents lock across the work, holds the parser lock across a whole-workspace search, and rebuilds an O(workspace) overlay per request.
state: done
priority: high
tags: [dev, bug, performance, diagnostics, references, warmup]
owner: felix
verified:
  by: cargo test --all-targets — lib 255 passed, harness 28 passed, stdio 1 passed; the only failures (4 `sources` lib tests, 1 harness test) are loopback-socket binds this sandbox forbids; java-lsp-bench --files 10000 --methods-per-class 10 --open-docs 20 --edits 10 --references — edit→publish 6.2 ms, edit→hover 6.2 ms, edit→definition 6.2 ms, references 5.4 s for 10001 locations
  at: 2026-09-28T00:00:00Z
---

# Problem

On a large workspace (~10,000 Java source files, ~20 files open) the editor is
largely unresponsive while the index warms up and afterwards. Completion and
go-to-definition stall for long stretches, diagnostics take many seconds to
update, and find-references is "somewhat working but often very slow or taking
forever". This contradicts R6 ("the initial scan never blocks text sync or
request handling; individual features may be briefly unavailable while warming
up") — the features are not briefly unavailable, they are blocked, and the block
is not the scan but the analysis the requests run once the index is populated.

Four defects compound, and they multiply with the two dimensions the bench does
not exercise (many open documents, a large index):

- **Diagnostics are recomputed inline on the single dispatcher, for every open
  document, on every edit.** `dispatch` (`src/engine.rs`) handles
  `Open`/`Change`/`Close`/`WatchedFiles` inline (L438–464) and calls
  `publish_all_diagnostics` (L575–597), which loops over **all** open documents
  and calls `engine.diagnostics` synchronously. Read-only queries are spawned
  onto tasks, but they are queued behind this inline work in the same task, so a
  hover or definition sent during a sweep waits for it. With `K ≈ 20` open
  documents, a single keystroke costs `K` full semantic passes, and a diagnostics
  republish is not merely delayed — it is serialized behind those passes.

- **`diagnostics` holds the global `documents` mutex across the entire semantic
  pass.** In `TreeSitterEngine::diagnostics` (`src/analysis.rs:948-967`) the
  `documents` `MutexGuard` is taken and the borrowed `document` is used by
  `semantic_diagnostics(document, …)` before the guard drops at end of scope, so
  the lock is held for the whole analysis. Every other handler that touches
  documents — `store_tree` (didOpen/didChange), `hover`, `completions`,
  `definition`, `implementation`, `inlay_hints`, `semantic_tokens`,
  `document_symbols`, `code_actions` — blocks on that mutex for the duration.
  (`implementation`, `src/analysis.rs:1606-1660`, likewise holds `documents`
  across its whole type loop, which reads and parses other files.) This is the
  direct mechanical cause of "completion and go-to-* are unresponsive while
  diagnostics churn".

- **References and rename hold the shared `parser` mutex for an entire
  whole-workspace search, reading and re-parsing up to `N` files from disk.**
  `collect_occurrences` (`src/analysis.rs:706-807`) takes `self.parser.lock()` at
  L714 and keeps it for the whole loop, `std::fs::read_to_string`-ing every
  candidate source file (L741) and parsing each that mentions the name. The same
  `parser` mutex is what `store_tree` (L128-140) takes to parse a document on
  every didOpen/didChange. So one references or rename request holds the parser
  for the entire search — blocking all typing and all other parsing — and pays
  `O(N)` disk reads plus parses for a single answer. The `text.contains(name)`
  prefilter only helps for rare names; for a common identifier it degrades to
  parsing most of the workspace. This is why references is "slow or taking
  forever" and why everything else freezes with it.

- **Each analysis pass is O(workspace files), and the cost peaks exactly when
  warm-up ends.** `type_layers()` (`src/analysis.rs:221-251`) calls
  `WorkspaceIndex::source_models()` (`src/index.rs:254-264`), which clones and
  sorts the whole per-file model map into a fresh `Vec` plus a `HashSet`, on
  **every request**; `ModelLayers::find_unique`/`find_in_package`/`contains`
  (`src/types.rs:743-797`) then linearly scan all `N` layers **per lookup**, and
  `SemanticCheck` performs many lookups per file. So one semantic pass is
  `O(N · lookups)` and a keystroke is `O(K · N · lookups)`, all serialized on the
  dispatcher behind one global lock. During the scan `index.type_model()` is
  `None` so `semantic_diagnostics` returns early; the moment the core scan ends
  (before the long dependency-source phase, and with the full index now present)
  the cost jumps to its maximum — the problem appears "during warm-up" but is
  worst once the index is populated.

# Reproduction

1. Open a workspace of ~10,000 `.java` sources (a real Maven project with
   dependencies, so the resolved jars and the JDK are in the index).
2. Open ~20 files.
3. Type in one of them; invoke completion and go-to-definition; then invoke
   find-references on a commonly-named symbol (a method or type referenced
   across many files).

**Observed:** completion and navigation stall for long stretches; diagnostics for
the open files take seconds to update; find-references is slow and sometimes
never returns; while any of these run, typing hangs and other features queue
behind them.

**Expected:** per R6, typing and navigation stay responsive throughout (and after)
the initial scan; diagnostics refresh promptly without blocking the other
features, and without recomputing every open document for a single edit;
references returns in bounded time without freezing editing.

**Why the existing harness misses it:** `java-lsp-bench` opens **one** document,
never types, and never issues references; its synthetic fixture has no cross-file
semantic load (and, in this environment, no JDK/dependency passes). So hover
during warm-up stays sub-millisecond and warm-up is linear in file count
(`--files 2000` → 2.3 s, max warm-up hover RTT 0.78 ms). The defect needs `K`
open documents, a large `N`, and a references request to appear.

# Proposal

Get analysis work off the serialized, globally-locked path, stop holding shared
locks across whole-workspace work, and stop rebuilding the per-request overlay:

- **Run diagnostics off the dispatcher.** Do not compute `publish_all_diagnostics`
  inline in the dispatch loop for `Open`/`Change`/`Close`/`WatchedFiles`. Schedule
  it on spawned / `spawn_blocking` tasks (as read-only queries already are), and
  coalesce pending work per URI so a burst of edits publishes once, for the latest
  version, rather than once per keystroke across all open documents. The
  cross-file republish required by `external-change-detection` D3 stays, but it no
  longer blocks the dispatcher or the other handlers.
- **Stop holding `documents` across analysis.** In `diagnostics` (and
  `implementation`) take the parsed document under a short lock and drop the guard
  before the semantic pass — the document is already stored as an `Arc`-able
  parsed value, so the pass can run on a snapshot without holding the store.
- **Stop holding the `parser` across a whole-workspace search.** Give
  `collect_occurrences` its own parser (or a per-task parser pool) instead of the
  shared mutex, and release it per file, so a references/rename search never
  blocks `store_tree`. Reuse index data for the candidate prefilter (e.g. a
  per-name token/occurrence index built during warm-up) so a common name does not
  degrade to reading and parsing most of the workspace.
- **Make the per-request overlay cheap.** Avoid the per-request `Vec` + `HashSet`
  rebuild in `source_models()`/`type_layers()` and the `O(N)` layer scan in
  `ModelLayers` lookups — cache the assembled layer view behind an
  index/dirty generation, or index names across layers — so a diagnostics pass or
  a references search is not `O(N)` per lookup.

# Decisions

- **D1 — Split responsiveness from warm-up throughput.** This request fixes the
  request-path blocking (the four defects above). The scan duration itself (the
  progress stalling on the jar/JDK class-file phase after "Indexed N source
  files") is a separate `kind: improvement` handled on its own. The two are
  independent and touch different code.
- **D2 — Fix the causes, not the symptom.** Moving diagnostics off the dispatcher
  alone would hide the freeze but leave the `documents`/`parser` lock holds and
  the `O(N)` per-request cost; all four are addressed.
- **D3 — Preserve behaviour exactly.** The set of diagnostics published and the
  set of references returned must be unchanged; this is a concurrency/cost fix,
  not a semantic one. In particular the cross-file republish on a watcher event
  (`external-change-detection` D3) and rename's "refuse when incomplete" rule
  stay.
- **D4 — Bound references work.** A references/rename search must not run
  unbounded work while holding any shared lock; candidate prefiltering and
  per-file lock release are in scope.
- **D5 — No LSP-surface change.** Capabilities and the protocol are untouched.

# Acceptance criteria

- A responsiveness test (or an extended `java-lsp-bench`) with `K` open documents
  and a large index: `didChange` for one document, and a `hover` / `definition`
  issued during a diagnostics sweep, each respond within a bounded time; no
  handler is blocked for the duration of a sweep. Concretely, the dispatcher
  never computes diagnostics inline, and `documents` is not held across
  `semantic_diagnostics`.
- A references test on a name shared across many files completes in bounded time
  and does **not** block a concurrent `didChange` (asserted structurally: the
  shared `parser` mutex is not held across `collect_occurrences`, and no
  shared lock is held across the search).
- Answers are unchanged: the diagnostics set for a given workspace/open set, and
  the references set for a given cursor, are identical to today.
- `cargo test --all-targets` is green, and the existing bench numbers show no
  regression.
- The bench/harness can express the multi-document, typing, and references
  scenarios (today it cannot), so the fix is measured, not just asserted.

# Docs to update

- `docs/architecture.md` — the data-flow section: diagnostics are not on the
  dispatcher's inline path, the `documents` lock is not held across analysis, the
  `parser` lock is not held across a workspace search, and the per-request type
  view is cached rather than rebuilt.
- `docs/requirements.md` — the R6 note (the scan never blocks the request path)
  should reflect that request handling stays responsive too, with the guarantee
  the fix restores.
- `README.md` — the "answers … immediately while the workspace index warms up"
  claim is only true once this is fixed on large workspaces; qualify or leave as
  the target.
- `docs/dev/backlog/index.md` — this request's row.
- `docs/dev/changelog.md` — an entry at implementation.

# Implementation plan

## Approach

The measurement vehicle lands first, so every later change has a before/after;
then the four defects are fixed in sequence; then the docs and close-out. The
work builds on the uncommitted `large-project-memory` refactor.

- **Measurement vehicle (AC5)** — extend `src/bin/java-lsp-bench.rs`: a shared
  `bench/BenchShared.ping(int)` called by every fixture class (so a references
  search must consider every file), `--open-docs`/`--edits`/`--references`
  flags (defaults reproduce today's report), and the multi-document + typing +
  references scenario in `run_bench`, surfaced in the report; plus acceptance
  tests in `tests/harness.rs`.
- **Defect 2 (AC1)** — store `Arc<ParsedDocument>`, add `version: i32`, thread
  the client version through `open`/`change` → `store_tree`, and snapshot the
  `Arc` under a short lock and drop the guard before analysis in `diagnostics`,
  `implementation`, and the other document-reading handlers.
- **Defect 1 (AC1)** — add a `DiagnosticsPublisher` in `src/engine.rs`
  (generation + `Notify`, sweep on `spawn_blocking`, per-URI coalescing,
  latest-version publication, preferred-first, cross-file republish preserved);
  rewire the four mutation arms of `dispatch` to `schedule(...)`; drop the
  `versions` map.
- **Defect 3 (AC2)** — give `collect_occurrences` a private parser from a small
  pool (never the shared `parser` mutex), threaded into `entry_method_params`
  and its callers, so a search never blocks `store_tree`.
- **Defect 4 (AC1)** — a name-indexed `SourceLayerIndex` cached by
  `WorkspaceIndex` behind a source-model generation, and a `ModelLayers` of a
  cached base + small copy-on-write dirty overlay, so `type_layers()` is two
  `Arc` clones and a lookup consults only the layers that declare the name.
- **Concurrency/ordering/answer-equivalence (D3)** — the dispatcher stays the
  single serialization point for mutations; a sweep reads a coherent snapshot;
  precedence and slot-dedup are pinned by equivalence tests.

## Steps

- [x] Extend `src/bin/java-lsp-bench.rs` with `bench/BenchShared.ping` and the
      `--open-docs`/`--edits`/`--references` scenario. (AC5)
- [x] Document the new bench flags and scenario in the `README.md` Benchmarks
      section. (AC5)
- [x] Add the multi-document + typing + references acceptance tests and the
      bounded references test to `tests/harness.rs`. (AC1, AC2, AC3, AC5)
- [x] Store `Arc<ParsedDocument>`, add `version: i32`, and snapshot-and-drop the
      guard before analysis in every document-reading handler (`src/analysis.rs`,
      `src/engine.rs`). (AC1)
- [x] Add the `DiagnosticsPublisher` and rewire `dispatch` to schedule; add the
      engine unit tests (dispatch schedules and never computes; a manual sweep
      emits exactly today's events; `Close` clears inline). (AC1, D3)
- [x] Give `collect_occurrences` a private pooled parser (never the shared
      `parser`); add the lock-not-held structural test and the cross-file
      references equivalence test. (AC2, AC3)
- [x] Cache a name-indexed layer view behind a source-model generation and
      restructure `ModelLayers`; add the name-indexed equivalence tests and the
      feature-equivalence tests. (AC1, AC3)
- [x] Update `docs/architecture.md` (data flow, engine boundary, references,
      type layer, `WorkspaceIndex`), `docs/requirements.md` (R6), and `README.md`
      (the responsiveness claim and bench flags). (AC1)
- [x] Run `cargo test --all-targets`; set this request `done` with `verified`,
      update the `docs/dev/backlog/index.md` row, and add the
      `docs/dev/changelog.md` entry. (AC4)
