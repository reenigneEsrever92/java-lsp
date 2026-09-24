---
type: ChangeRequest
kind: improvement
title: Extract and parse dependency sources across worker threads
description: index_extracted walks the artifact list on one blocking-pool thread; extract, parse, and cache-write the artifacts concurrently on available_parallelism workers, emitting from the calling thread.
state: done
priority: medium
tags: [dev, improvement, performance, sources]
owner: felix
verified:
  by: "measured end-to-end on the same 30 real `*-sources.jar`: `index_extracted` runs in 4.77 s versus 18.7 s sequential (same work, after the line-index fix) — the win is bounded by the largest artifacts and this loaded machine. cargo build and cargo fmt --check clean; the server-free `sources` test (one BaseArtifact per artifact, per-archive progress) and the `bus` tests pass. `java-lsp-bench` (small uniform files) is unchanged, as expected."
  at: 2026-09-30T00:00:00Z
---

# Problem

`src/sources.rs::index_extracted` processes every artifact on **one** thread —
read the jar, inflate each `.java`, write it to the cache, parse it, and collect
its entries/types — while the other cores idle. With a large resolved closure
(hundreds of artifacts, tens of thousands of files) this is the bulk of the
download driver's time, on top of the now-fixed quadratic in offset conversion
([line-index-quadratic](line-index-quadratic.md)).

# Proposal

Fan the artifacts across `std::thread::available_parallelism()` worker threads
(`std::thread::scope`), each with its **own** `java_parser()` (`Parser` is `Send`
but not `Sync`). A worker claims the next artifact from an `AtomicUsize` and
returns a finished layer over an `mpsc` channel; the **calling thread** is the
only one that touches the `sink`, so the bus, the per-artifact `BaseArtifact`,
`RemoveBase`, and the `Parsed x/N … (F source files)` progress line are
unchanged.

# Decisions

- **D1 — Parallelize across artifacts (option A).** Each artifact is one work
  item. Reason: matches the many-dependencies workload and bounds memory (one jar
  per worker in flight); file-level fan-out within an artifact (B) is deferred
  until a single dominant dependency is the bottleneck.
- **D2 — Scoped OS threads, one parser each, no new dependency.** `thread::scope`
  + `available_parallelism`; `Parser` is per-worker (it is not `Sync`). No
  `rayon`.
- **D3 — Workers return their layer; the calling thread emits.** The `sink` is
  `&mut dyn FnMut(DriverMessage)` and not `Send`, and keeping emission on one
  thread preserves the message order the index expects for each artifact
  (`RemoveBase` then `BaseArtifact`). Emission order across artifacts becomes
  completion order, which is already arbitrary (each layer is keyed by its own
  sources-jar URI).
- **D4 — Extract the per-artifact body into `index_one`.** It returns an
  `Extracted` enum (`Skipped` or the merged layer) and never touches the `sink`,
  so it is safe to run off the calling thread.
- **D5 — Worker count is `min(available_parallelism(), artifacts.len())`,** so a
  one-artifact run spawns no thread.

# Acceptance criteria

1. `index_extracted` extracts and parses artifacts concurrently, one parser per
   worker, and emits only from the calling thread.
2. The published messages are unchanged: one `RemoveBase` + one `BaseArtifact`
   per artifact, and the `Parsing N …` → `Parsed x/N … (F source files)` →
   `Indexed M dependency source files` → `library sources indexed …` sequence.
3. The 30-jar `index_extracted` measurement drops from ~18.7 s (sequential) to a
   few seconds.
4. `cargo build` and `cargo fmt --check` are clean and the `sources`/`bus` tests
   pass.

# Docs to update

- `docs/dev/backlog/index.md` — this request's row.
- `docs/dev/changelog.md` — an entry at implementation.
- (`docs/architecture.md` already says the extraction "runs on the blocking
  pool"; a follow-up can note the workers, but the behaviour it documents is
  unchanged.)
