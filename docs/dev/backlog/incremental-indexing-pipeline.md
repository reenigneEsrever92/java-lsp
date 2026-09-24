---
type: ChangeRequest
kind: refactor
title: Make indexing an incremental, message-based pipeline of producer modules
description: Indexing is one monolithic blocking function that publishes the whole base type model once at the end, and the source/artifact downloader is a separate after-the-fact phase that re-clones and re-publishes that model; refactor each indexing step (source scan, jar indexer, JDK indexer, dependency-source downloader) into an independent producer that feeds the index incrementally through the engine's existing message boundary.
state: done
priority: high
tags: [dev, refactor, performance, warmup, indexing]
owner: felix
verified:
  by: cargo test --all-targets — lib 261 passed, bin 7 passed, stdio 1 passed, harness 28 passed; the only failures (4 `sources` lib tests, 1 harness test) are loopback-socket binds this sandbox forbids; java-lsp-bench --files 300 unchanged (warm-up hover RTT max 0.3 ms, post-warm-up 0.1 ms)
  at: 2026-09-28T00:00:00Z
---

# Problem

Indexing does not use the message boundary the rest of the engine is built on
(`message-based-engine`). `scan_workspace_async` (`src/index.rs`) runs one
`spawn_blocking(scan_workspace_core)` that discovers the project, resolves
dependencies, and indexes **sources, then jars, then the JDK** in a single
function, accumulating one local `TypeModel` and calling
`index.set_types(Arc::new(types))` **once, at the very end**. Only then does the
dependency-**source** pass run (`sources::index_sources`), and it starts by
deep-cloning the whole base (`types = index.type_model().clone()`,
`src/sources.rs:238-241`), extends it, and calls `set_types` again.

Two consequences:

- **The base is all-or-nothing and last.** For the entire source scan — the long
  pole on a large project — `type_model()` is `None`, so library receivers
  (JDK and dependency jars) resolve to nothing. (An interim reorder now indexes
  jars + JDK _before_ the source scan and publishes the base there, which
  mitigates the worst of this; but the publication is still one shot and the
  ordering is a hand-maintained invariant.)
- **Producers can't contribute incrementally.** The downloader is a special case
  bolted on after the monolith, it re-clones the entire model to add a few
  types, and nothing else can feed the index while the scan is running. Progress
  is also coarse: a phase's message is not updated until the phase ends.

# Proposal

Turn indexing into a set of **independent producers** that each emit updates
through the engine's existing message boundary, with the index/model updated
**append-only per artifact**:

- **Producers** — project/pom discovery, the workspace source scanner, the
  dependency-jar indexer, the JDK indexer, and the dependency-source
  downloader — become separate units driven by the engine, each sending
  `IndexUpdate`/`Progress` messages (reusing `EngineEvent`/`Command`) instead of
  thread-blocking inside one function.
- **Append-only base model** — replace `set_types(whole_model)` with an
  incremental `WorkspaceIndex::add_base_layer(uri, entries, types)` (or an
  equivalent per-artifact extension), so an artifact's entries and types are
  added when it lands and the downloader no longer deep-clones the base.
- **The downloader is just another producer** — it starts immediately
  (concurrently with the source scan), and each fetched/extracted artifact is
  streamed in as it completes; "wait for all jars, then download" disappears.
- **Gating stays explicit** — features serve whatever has arrived (partial, as
  today); semantic diagnostics stay gated until the workspace sources are
  indexed, so partial state never yields false unresolved-symbol errors.

# Decisions

- **D1 — This is `message-based-engine` applied to indexing.** The engine
  already owns an `EngineHandle`/`EngineEvent` boundary and a `DiagnosticsPublisher`
  task; indexing bypasses it with one blocking call. The refactor brings indexing
  onto that boundary rather than inventing a new one.
- **D2 — Producer set.** Discovery, workspace sources, dependency jars, the JDK,
  and dependency-source download are the producers; the interim reorder (jars +
  JDK before sources) becomes an explicit scheduling decision rather than an
  ordering buried in one function.
- **D3 — Append-only, per-artifact base.** `set_types` (replace-the-world) gives
  way to per-artifact extension, which also removes the O(total-types) clone in
  `index_extracted`. The final index and model must be **identical** to today's.
- **D4 — Diagnostics stay gated.** As with the interim fix, semantic diagnostics
  require an indexed `java.lang` **and** a completed source scan; witnesses
  partial indexing must not produce false positives. Other features serve partial
  results (R6).
- **D5 — Preserve the surface and the observable end state.** No LSP capability
  or protocol change; `ready` still means all producers finished; the same
  entries/types are indexed. Progress events may become finer-grained.
- **D6 — Supersedes the interim ordering.** The interim "publish the base before
  the source scan" (`warmup-request-responsiveness` follow-up) is a stopgap this
  refactor makes structural; the refactor's acceptance subsumes it.

# Acceptance criteria

- Indexing is driven by producer messages; no single function blocks through
  sources → jars → JDK → downloads in sequence, and the downloader runs
  concurrently with the source scan.
- The base model grows per artifact (`add_base_layer`/equivalent); the
  whole-model clone in `index_extracted` is gone.
- Library types (JDK and dependency jars) resolve while the workspace source
  scan is still running (pinned by a test/probe: a JDK/jar member completes
  mid-scan on a large fixture).
- The final index and declared-type model are identical to the current
  implementation for the same project (equivalence test over entries and types).
- Semantic diagnostics never fire from partial indexing (a referring file is not
  flagged unresolved before the sources producer finishes).
- `cargo test --all-targets` is green (apart from the known sandbox failures);
  no regression in the bench's warm-up hover RTT or post-warm-up latency.

# Docs to update

- `docs/architecture.md` — the data-flow diagram and the `WorkspaceIndex` /
  dependency-sources bullets: indexing is a set of producers feeding the index
  incrementally over the message boundary; the base grows per artifact.
- `docs/dev/backlog/index.md` — this request's row.
- `docs/dev/changelog.md` — an entry at implementation.

# Implementation plan

## Approach

Apply `message-based-engine`'s command/event pattern to indexing. Today
`scan_workspace_async` (`src/index.rs:628`) runs one
`spawn_blocking(scan_workspace_core)` (`src/index.rs:669`) that indexes sources →
jars → JDK into a local `TypeModel`, publishes it once with `set_types`
(`src/index.rs:748`), and then `sources::index_sources` (`src/sources.rs:72`)
clones that whole base (`src/sources.rs:238-241`) to add library sources and
`set_types` again (`src/sources.rs:312`). The refactor replaces the monolith and
the clone with concurrent **producers** feeding an **append-only, per-artifact
base** over an **`IndexMessage`** boundary, reusing `EngineEvent`/`Reporter` for
progress, notices, and logs.

**1. A layered, per-artifact base (`src/index.rs`, `src/types.rs`).**
`WorkspaceIndex`'s `types: Arc<Mutex<Option<Arc<TypeModel>>>>` (`src/index.rs:87`)
becomes a per-artifact layer set — one `Arc<TypeModel>` per jar / JDK-archive /
extracted-source-file URI, kept in insertion order (increasing precedence) — read
through the existing name-indexed, generation-cached `SourceLayerIndex` view (the
same pattern `source_layer_index` already uses, `src/index.rs:287-314`).

- `add_base_layer(uri, entries, types)` upserts the artifact's index entries and
  appends/replaces its layer; `remove_base_layer(uri)` drops a class-file jar's
  layer and entries when its sources land (today `index_extracted`'s
  `remove_file` + clone, `src/sources.rs:298-312`).
- `SourceLayerIndex` (`src/types.rs:691`) gains an insertion-order constructor
  (the URI sort moves to the source-side caller) and a `TypeLookup` impl that
  reproduces `ModelLayers`' slot-dedup, highest-precedence-wins answers
  (`src/types.rs:904-963`). `TypeQuery` (`src/types.rs:967`) takes its base as
  `&dyn TypeLookup` so the base can be this layered view.
- **No merge and no whole-model clone is ever performed.** A request reaches the
  base through a cached `Arc`, so this does not regress warm-up hover RTT (AC5)
  the way re-merging per artifact would.

**2. Producers and the indexing task (`src/index.rs`, `src/sources.rs`,
`src/engine.rs`).**
Discovery/resolution, workspace sources, dependency jars, the JDK, and
dependency-source downloads each become a producer that emits messages instead of
blocking inside one function. **One** producer→engine channel carries both the
data updates and everything the pipeline reports, so the warm-up's reporting and
logging cross the message boundary rather than side channels:

- `IndexMessage::BaseArtifact { uri, entries, types }` — jars, JDK archives, and
  extracted sources.
- `IndexMessage::SourceFile { uri, entries, types }` — a workspace source file
  (today's `set_source_types` + `upsert_file`, `src/index.rs:769-770`).
- `IndexMessage::RemoveBase { uri }` — a class-file jar superseded by its sources.
- `IndexMessage::Progress(ProgressUpdate)` — client progress, reusing the
  existing `ProgressUpdate` type.
- `IndexMessage::Notice { level: MessageLevel, text }` — a client notice
  (`window/showMessage`), reusing `MessageLevel`.
- `IndexMessage::Log { level, message, fields }` — a server-side `tracing` line
  for the pipeline (phase summaries and warnings), rendered locally by the
  indexing task.

A single indexing task drains the channel: it applies each data update — the
single serialization point that makes the final index deterministic, ordering
base layers by a per-producer rank so precedence does not depend on interleaving —
and it is the **sole emitter** of the warm-up's outputs, forwarding `Progress`
and `Notice` as `EngineEvent::Progress`/`EngineEvent::Message` on the shell's
event channel and rendering `Log` as `tracing` lines. The existing `Reporter`
(`src/engine.rs:179-218`) is reused as the producer-side handle over this channel
instead of holding the `EngineEvent` sender directly (its
`begin`/`update`/`end`/`message` API is unchanged, and the detached
`Reporter::default()` no-op stays for inline scans and tests). So no warm-up
traffic — index data, progress, notices, or logs — bypasses the boundary, and the
outputs share one ordered stream: a progress message or log line is emitted in the
same order as the update it describes. The `Log` rendering preserves the
`workspace index warm-up complete` line and its `files=`/`elapsed_ms=` fields,
which `java-lsp-bench` parses from stderr (`src/bin/java-lsp-bench.rs:384-404`).
Finer-grained phase messages (per artifact / jar / JDK archive) are the only
observable difference.

**3. Concurrent driver (`src/index.rs`).**
`scan_workspace_async` becomes the driver: resolve the project first (cheap; the
downloader needs the closure), then spawn the source, jar, JDK, and download
producers on the runtime. The downloader no longer waits for "all jars"; each
artifact is streamed in as it lands. `ready` flips when the core producers
(discovery, sources, jars, JDK) finish — an indexed `java.lang` plus a completed
source scan, exactly today's gate (`src/index.rs:775`, `src/analysis.rs:3935`) —
while the download producer may still run.

**4. Gating unchanged (`src/analysis.rs`).**
Features keep serving partial results; `semantic_diagnostics` still returns
nothing before `ready` (`src/analysis.rs:3927-3945`), so partial indexing never
yields false unresolved-symbol errors.

**Decision / risk.** The observable end state must be identical (D5), so the
layered base has to answer exactly as today's merged `TypeModel`; the equivalence
tests below pin that. The `IndexMessage` channel is chosen over producers mutating
the (already thread-safe) `WorkspaceIndex` directly because the CR asks for a
message boundary and it gives one deterministic application order — and it is
the same boundary the `Reporter` now reports and logs through, so no warm-up
traffic bypasses the engine. Logging is centralized for the indexing pipeline's
own lines (phase summaries and warnings in `index.rs`/`sources.rs`); the deep
resolver, class-file, and `jdk.rs` helper logs keep their direct `tracing` calls.

## Steps

- [x] `src/types.rs`: add an insertion-order `SourceLayerIndex` constructor (move
      the URI sort to `WorkspaceIndex::source_layer_index`) and
      `impl TypeLookup for SourceLayerIndex` with slot-dedup,
      highest-precedence-first answers; change `TypeQuery::new` to take
      `base: &dyn TypeLookup`. Add unit tests proving the layered base answers
      identically to a merged `TypeModel` (`find_unique`, `find_in_package`,
      `contains`). (AC: identical model)
- [x] `src/index.rs`: replace `types` with the per-URI base layers plus a
      generation-cached `SourceLayerIndex` view behind `type_model()`; add
      `add_base_layer`/`remove_base_layer` and update the
      `set_source_types`/`remove_file` paths. Unit tests for append-only growth,
      removal, and cache invalidation. (AC: base grows per artifact)
- [x] `src/analysis.rs`: point the `type_model()` consumers and
      `semantic_diagnostics` at the layered base (`&dyn TypeLookup`) and update
      the `set_types`-based test helpers; keep every feature's answers and the
      `ready` gate unchanged. (AC: no partial-index diagnostics, same answers)
- [x] `src/index.rs`: split `scan_workspace_core` into producers (discovery +
      resolution, workspace sources, dependency jars, JDK) and add the
      `IndexMessage` enum (data + progress / notice / log), the indexing task, and
      the concurrent driver in `scan_workspace_async`; keep `ready`, the offline
      notice (`src/index.rs:657`) and the summary message. (AC: producers, no
      monolith)
- [x] `src/engine.rs` + the indexing task: rework `Reporter` to enqueue
      `IndexMessage`s (progress / notice / log) into the indexing channel and have
      the indexing task emit them — `EngineEvent::Progress`/`EngineEvent::Message`
      to the shell and `tracing` lines for the `Log` messages — replacing the
      producers' direct `tracing::*` calls in the indexing path while keeping the
      `workspace index warm-up complete` line and its `files=`/`elapsed_ms=`
      fields the bench parses. Keep the detached `Reporter::default()` no-op for
      inline scans and tests, and wire the `EngineEvent` sender into the indexing
      task. (AC: reporting and logging message-based, single emitter)
- [x] `src/sources.rs`: turn `index_sources`/`index_extracted` into the download
      producer — start concurrently with the source scan, `remove_base_layer` the
      class jar, `add_base_layer` per extracted file, and drop the base clone +
      second `set_types`. (AC: downloader concurrent; clone gone)
- [x] `src/index.rs` / `tests/harness.rs`: add the equivalence test (entries +
      declared-type model vs. the pre-refactor assembly), the mid-scan probe (a
      JDK/jar member resolves while the source scan is still running), and the
      diagnostics-gate test (a referring file is not flagged before `ready`).
      (AC: mid-scan resolution, gating)
- [x] Verify: `cargo test --all-targets`, and re-run `java-lsp-bench` to confirm
      warm-up hover RTT, post-warm-up latency, and its stderr readiness detection
      (the `workspace index warm-up complete` line) are unchanged; record the
      numbers. (AC: green suite, no bench regression)
- [x] `docs/architecture.md`: rework the data-flow diagram (producers → index
      writer → `WorkspaceIndex`/type base; downloader concurrent with the source
      scan), the `WorkspaceIndex` bullet (warm-up is producers feeding an
      append-only, per-artifact base), the dependency-sources and
      standard-library bullets (concurrent producers, no whole-model clone), and
      the decisions section. (Doc step)
- [x] `docs/dev/backlog/index.md` row and a `docs/dev/changelog.md` entry; set
      this request `done` with `verified`. (Close-out)
