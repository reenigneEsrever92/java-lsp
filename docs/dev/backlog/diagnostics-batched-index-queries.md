---
type: ChangeRequest
kind: improvement
title: Batch and cache the diagnostics pass's index queries
description: The semantic diagnostics pass sends one IndexQueryName bus round trip per lookup — several per type reference, repeated for every open document on every sweep; prefetch the names in one batched request per sweep, cache the results, and fetch the per-sweep index state once.
state: done
priority: medium
tags: [dev, improvement, performance, diagnostics, messaging]
owner: felix
verified:
  by: "cargo build and cargo fmt --check clean; cargo test passes (the 5 socket-binding tests in `sources` and `harness` re-run outside the sandbox). New `diagnostics::tests` capture the hub log: a two-document sweep sends one `IndexQueryNames`, at most one `IndexHasPackages`, no repeated `IndexQueryName`, and one `IndexReady`/`IndexTypeModel`/`IndexTypeLayers`, with the usual findings; a sweep with semantic diagnostics off sends no name, ready, or type-model requests."
  at: 2026-10-01T00:00:00Z
---

# Problem

The diagnostics subsystem reads the index only through `IndexHandle`
(= `BusClient`, `src/index.rs:1150`), so every `query_name` is a blocking round
trip through the hub, logged as a `request` and a `reply` line at `debug`
(`src/bus.rs:310`, `src/bus.rs:335`). The semantic pass (`src/diagnostics.rs`)
issues these lookups per reference and without memoisation:

- `type_visible` (`L253`) calls `query_name(name)` once for the file's package,
  once per non-static wildcard import, and once for `java.lang` — a `String`
  reference alone costs 2+ identical round trips, in both `check_type` and
  `check_identifier`.
- `type_candidates` (`L275`), `check_identifier`'s non-type fallback (`L460`),
  `workspace_owner` (`L391`), `type_import_resolves` (`L565`), and
  `static_import_resolves` (`L578`) query again; `importable` → `import_edit`
  (`src/analysis.rs:4382`) adds one more per candidate.
- Wildcard imports cost one `IndexHasPackage` each, and every document's pass
  re-fetches `IndexTypeModel` and `IndexReady` (`L95`, `L103`).

`DiagnosticsModule::sweep` (`L849`) re-checks **every** open document on every
open/change/close and every `FileEvent` (`L797`). The result is hundreds of
identical bus round trips per keystroke on a few open files, a `debug` log
flooded with `IndexQueryName` lines, and avoidable latency on the diagnostics
thread.

# Proposal

Make one sweep cost a handful of bus messages regardless of document size:

1. A **per-sweep name cache** in the diagnostics module: every simple-name
   lookup in the pass goes through it, so each distinct name crosses the bus at
   most once per sweep, shared across all open documents.
2. **Batched prefetch:** before checking, a cheap walk over each open
   document's tree collects the names the pass will look up, and one new
   `Request::IndexQueryNames` fills the cache. Wildcard-import packages are
   batched the same way with `Request::IndexHasPackages`. A name the prefetch
   did not anticipate falls back to a single (cached) `query_name`.
3. **Per-sweep index state:** `ready`, `type_model`, and `type_layers` are
   fetched once per sweep, not once per document.

Diagnostics output is unchanged; only how the pass reads the index changes.

# Decisions

- **D1 — The cache lives for one sweep, shared across all open documents.**
  Reason: the index is already read non-atomically across a sweep, so sharing
  adds no new staleness, and common names (`String`, `List`) are then fetched
  once per sweep instead of once per file. A new sweep starts with an empty
  cache, so index changes are seen on the next sweep as today.
- **D2 — Batch with new requests `IndexQueryNames { names: Vec<String> }` →
  `HashMap<String, Vec<Arc<SymbolEntry>>>` and `IndexHasPackages { packages:
Vec<String> }` → `HashSet<String>` (the packages that exist).** Both are owned
  by `Module::Index` (`owner_of`) and answered in `index::answer` from the
  existing `query_name`/`has_package`. The hub logs them as
  `IndexQueryNames count=N` / `IndexHasPackages count=N` — counts, never the
  names list (the bus log never carries payloads). Reason: one round trip and
  two log lines per sweep instead of one per lookup.
- **D3 — The prefetch walk collects: base type names of the positions
  `check_type` visits, symbolic identifiers, import simple names, static-import
  owner simple names, and wildcard-import packages.** It may over-collect;
  missing a name is only a fallback, never a wrong answer. Reason: correctness
  stays with the existing checks; the prefetch is a pure optimisation.
- **D4 — `ready`, `type_model`, and `type_layers` are fetched once per sweep**
  and passed into each document's pass. Reason: they are per-index, not
  per-document, state.
- **D5 — `import_edit` (`src/analysis.rs`) takes a small name-lookup
  abstraction** (a trait with `query_name`, implemented by `IndexHandle` and by
  the sweep cache) instead of `&IndexHandle`, so the diagnostics pass reads
  through the cache while the completion pipeline's call sites keep passing the
  handle unchanged.
- **D6 — Out of scope:** re-checking only the changed document (conflicts with
  the architecture's deliberate every-open-document sweep, D3 there); filtering
  non-Java `FileEvent`s; moving these requests to `trace`. Reason: batching
  already reduces the log to a few lines per sweep; the rest are separate
  decisions.
- **D7 — Noted, not fixed here:** `docs/architecture.md` says "a burst of edits
  collapses to one sweep of the latest state", but `diagnostics::spawn_module`
  handles each message in turn without draining the queue, so a burst triggers
  one sweep per message. Worth its own request.
- **D8 — Measured by a bus-log test**, using the `tracing`-capture approach of
  the hub-log tests (`message-hub-log-sender-followups`, D5): one sweep over a
  fixture document must produce at most one `IndexQueryNames`, no repeated
  `IndexQueryName` for the same name, and the same diagnostics as before. No
  new benchmark.

# Acceptance criteria

1. All existing diagnostics and quick-fix tests pass unchanged — the pass
   produces the same diagnostics (codes, ranges, messages, fix data).
2. A new test sweeps a fixture document that references `String` several
   times, uses a wildcard import, and has an unresolved type; the captured
   `java_lsp::bus` output shows exactly one `IndexQueryNames` request, at most
   one `IndexHasPackages` request, no `IndexQueryName` repeated for the same
   name, and one `IndexReady` / `IndexTypeModel` / `IndexTypeLayers` per sweep
   regardless of the number of open documents.
3. Sweeping two open documents that share type names issues one
   `IndexQueryNames` for the sweep, not one per document.
4. `IndexQueryNames` and `IndexHasPackages` log as `count=N`, and their replies
   log with responder, requester, and elapsed time like every other request.
5. Completion's auto-import edits (`import_edit` callers in `analysis.rs`) are
   unaffected — their tests pass.
6. `cargo build`, `cargo fmt --check`, and `cargo test` are clean.

# Docs to update

- `docs/architecture.md` — the diagnostics-subsystem bullet: the pass reads the
  index through a per-sweep, batch-prefetched name cache (`IndexQueryNames`,
  `IndexHasPackages`) and fetches the index state once per sweep.
- `src/bus.rs` module docs / `describe_request` — the two new requests and how
  they are logged.
- `docs/dev/changelog.md` — an entry at implementation.

# Implementation plan

## Approach

**Index side (`messages.rs`, `bus.rs`, `index.rs`).** Two new `Request`
variants, `IndexQueryNames { names: Vec<String>, reply:
ReplyHandle<HashMap<String, Vec<Arc<SymbolEntry>>>> }` and `IndexHasPackages {
packages: Vec<String>, reply: ReplyHandle<HashSet<String>> }`. `owner_of` maps
both to `Module::Index`; `describe_request` renders them as `IndexQueryNames
count=N` / `IndexHasPackages count=N`. `BusClient` gains `query_names` and
`has_packages`; `index::answer` serves them from the existing
`WorkspaceIndex::query_name` / `has_package`, one map entry per requested name
(empty results included, so a miss is cached too).

**Lookup abstraction (`index.rs`).** `pub(crate) trait NameLookup { fn
query_name(&self, name: &str) -> Vec<Arc<SymbolEntry>>; }`, implemented for
`IndexHandle`. `analysis::import_edit` takes `&dyn NameLookup`; the completion
and quick-fix callers pass `&IndexHandle` unchanged (unsized coercion) (D5).

**Sweep state (`diagnostics.rs`).** A `SweepIndex` holds the `IndexHandle`, the
workspace layer fetched once (`type_model`, then `ready`, then the `java.lang`
gate — `None` when the semantic pass must not run), and two `RefCell` caches:
names → entries and package → exists. `prefetch(names, packages)` sends one
`IndexQueryNames` and one `IndexHasPackages` for the not-yet-cached keys (none
when empty); `query_name`/`has_package` read the cache and fall back to a single
request on a miss, caching the answer (D1, D2). `SweepIndex` implements
`NameLookup`, so `importable` → `import_edit` reads through it. `SemanticCheck`
holds `&SweepIndex` instead of `&IndexHandle`; every `self.index.query_name` /
`has_package` call goes through the cache, so `type_visible`'s repeated lookups
of one name cost one entry.

**Prefetch walk (`diagnostics.rs`).** `collect_lookups(root, text, names,
packages)` gathers: every `type_identifier`; every symbolic identifier (the
existing `is_symbolic_identifier`, made a free function) not in the file's
declared names; per import, the simple name (single-type), the owner's simple
name (static), or the package (non-static wildcard). Import-path parsing is
factored out of `check_import` into `parse_import` so both agree (D3).

**Sweep (`DiagnosticsModule::sweep`).** Parse every open document first; when
semantic diagnostics are on, build one `SweepIndex`, prefetch the union of all
clean-parsing documents' lookups, then run each document's pass against it and
the once-fetched `type_layers` overlay (D4). `diagnostics_for` (the core's
single-document entry point) builds its own `SweepIndex` and prefetches the one
document, so the core tests exercise the same path.

**Test.** A `#[cfg(test)]` module in `diagnostics.rs` installs a global
`tracing_subscriber::fmt` subscriber (once, `OnceLock`) at `debug` writing into
an in-memory buffer, and drives `DiagnosticsModule::sweep` over a standalone bus
whose client is labeled uniquely per test, so its `sender=<label> request …`
lines can be filtered from other tests' output (D8).

## Steps

- [x] `messages.rs`: add `IndexQueryNames` and `IndexHasPackages`. (D2)
- [x] `bus.rs`: `query_names`/`has_packages` on `BusClient`; route both in
      `owner_of`; log them as `count=N` in `describe_request`; mention them in the
      module docs. (D2, AC4)
- [x] `index.rs`: answer both requests; add the `NameLookup` trait and its
      `IndexHandle` impl. (D2, D5)
- [x] `analysis.rs`: `import_edit` takes `&dyn NameLookup`. (D5, AC5)
- [x] `diagnostics.rs`: `SweepIndex` (once-per-sweep workspace/ready gate, name
      and package caches, batched `prefetch`, single-request fallback);
      `SemanticCheck` reads through it; `parse_import` and free
      `is_symbolic_identifier`; `collect_lookups`. (D1–D4)
- [x] `diagnostics.rs`: `sweep` parses all documents, prefetches once, and runs
      every pass against one `SweepIndex`; `diagnostics_for` uses a per-call
      `SweepIndex`. (D1, D4, AC2, AC3)
- [x] `diagnostics.rs` tests: bus-log capture asserting one `IndexQueryNames`,
      at most one `IndexHasPackages`, no repeated `IndexQueryName`, and one
      `IndexReady`/`IndexTypeModel`/`IndexTypeLayers` for a two-document sweep,
      plus the expected unresolved-type diagnostic. (D8, AC2, AC3)
- [x] `docs/architecture.md`: the diagnostics-subsystem bullet describes the
      per-sweep, batch-prefetched index reads.
- [x] `docs/dev/changelog.md`: an entry.
- [x] `cargo build`, `cargo fmt --check`, `cargo test` clean. (AC1, AC5, AC6)

## Implementation notes

- Not yet committed; link the commit here once it lands.
- `diagnostics_for` (the core's single-document entry point) builds its own
  `SweepIndex`, so the core's existing diagnostics tests run the batched path.
- A sweep whose open documents all fail to parse builds no `SweepIndex`, so it
  sends no index requests beyond `IndexTypeLayers`, as before.
- The test subscriber is installed globally (the hub logs on its own thread) at
  `debug`; each test filters on a unique sender label.
