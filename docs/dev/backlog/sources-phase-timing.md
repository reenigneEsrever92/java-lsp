---
type: ChangeRequest
kind: improvement
title: Log per-phase timings for the dependency-source pass
description: The pass reports nothing about where its time goes; log the fetch and per-phase extract totals (read, inflate, write, parse, entries, types) so the bottleneck is measured on the real corpus.
state: done
priority: low
tags: [dev, improvement, observability, sources]
owner: felix
verified:
  by: "`cargo build`/`cargo fmt --check` clean; the `sources`/`bus` tests pass; a throwaway run over 40 real `*-sources.jar` (3601 files) printed `library sources indexed: 40 artifacts, 3601 files` and `dependency source extract: read 1318ms, inflate 1546ms, write 6416ms, parse 9014ms, entries 4826ms, types 5141ms`."
  at: 2026-10-01T00:00:00Z
---

# Problem

The dependency-source pass gave no clue where its time went, so its cost was
guessed at rather than measured — first assumed to be the parse, then the
download. On a large closure (thousands of artifacts, hundreds of thousands of
files) that matters: the fix depends entirely on which phase dominates.

# Proposal

Time the phases and log them: `index_sources` logs the fetch (`dependency source
fetch: N of M archives available in Xms`), and `index_extracted` accumulates
per-phase wall clock across the workers and logs one line (`dependency source
extract: read …ms, inflate …ms, write …ms, parse …ms, entries …ms, types …ms`).
Both are one `Info` line each, once per warm-up.

# Decisions

- **D1 — Phase counters, not per-file logging.** One `AtomicU64` of nanoseconds
  per phase, summed across workers, printed once. Reason: readable at a glance
  without flooding the log.
- **D2 — `inflate` is measured as the `for_each_zip_entry` wall time minus the
  callback time** (the reader inflates before the callback). Reason: the phases
  the callback owns are measured directly; the remainder is the reader.
- **D3 — Always on.** A handful of `Instant::now` per file is negligible against
  the work it measures, and the line is useful every run.
- **D4 — Logged periodically, not only at the end.** During the pass the timings
  are logged every `TIMING_STEP` (200) archives, and once at the end, each line
  named `(attempted/total archives)`. Reason: a large closure takes minutes, and
  a line only at the end gives no interim signal — the pass looks like it never
  finishes.
- **D5 — The lines are `DriverMessage::Log`, so they surface on two targets.**
  The hub logs every notification (including `Log`) at `java_lsp::bus` debug, and
  `translate` re-emits it at `java_lsp::messages` info. Note: `main.rs` uses
  `EnvFilter::try_from_default_env()`, so `RUST_LOG=java_lsp::bus=debug`
  disables every other target — the completion/summary lines are then invisible;
  use `RUST_LOG=java_lsp=debug` to see both.

# Acceptance criteria

1. A warm-up logs the fetch line and the extract phase line — periodically
   (every 200 archives) during the pass and once at the end — with the phases
   summing to roughly the pass's wall clock times the worker count.
2. `cargo build`/`cargo fmt --check` clean and the `sources`/`bus` tests pass.

# Docs to update

- `docs/dev/changelog.md` — an entry at implementation.
- `docs/architecture.md` — the dependency-sources bullet already mentions the
  `library sources indexed` line; a follow-up can note the timing line.

# Measured result (40 real jars, 3601 files)

`read 1318ms, inflate 1546ms, write 6416ms, parse 9014ms, entries 4826ms,
types 5141ms` — i.e. parse 32 %, **write 23 %**, types 18 %, entries 17 %,
inflate 5 %, read 5 %. The cache **writes** (every start rewrites every
extracted file) are the second-largest cost; the parse side is ~67 %.
