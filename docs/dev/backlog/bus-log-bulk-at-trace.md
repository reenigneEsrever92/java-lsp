---
type: ChangeRequest
kind: improvement
title: Keep the hub debug log readable and emit progress in steps
description: The hub logs every per-item notification at debug, so a large workspace is thousands of lines; log the bulk ones (a source file, an artifact, a progress tick) at trace, and report extraction progress ~5% of the way instead of per archive.
state: done
priority: medium
tags: [dev, improvement, observability, messaging]
owner: felix
verified:
  by: "cargo build and cargo fmt --check clean; the `bus` and `sources` tests pass. `is_bulk` classifies the per-item notifications; `Progress` from the sources pass is now emitted ~5 % of the way (20 updates) rather than per archive (thousands)."
  at: 2026-10-01T00:00:00Z
---

# Problem

Two sources of log/message volume:

1. **The hub logs every notification at `debug`.** On a large workspace and a
   thousands-of-artifacts closure, the per-item notifications — one `SourceFile`
   per workspace file, one `BaseArtifact` per jar/jmod/artifact, a `Progress` tick
   per artifact, plus `SourceEntries`/`SourceTypes`/`DirtyTypes` per file — are
   thousands of lines, drowning the message *flow* the log exists to show.
2. **The dependency-source pass emitted a `Progress` update per archive.** At
   2,429 artifacts that is 2,429 bus messages and 2,429 work-done updates, for a
   bar that only needs to move.

# Proposal

- The hub logs the high-cardinality, per-item notifications at **`trace`** and
  everything else at **`debug`**, so `debug` shows the flow (`FolderAdded`,
  `ProjectModel`, `SourceInventory`, `Artifacts`, `StageDone`, `Ready`, documents,
  diagnostics, notices, logs, summaries, requests, replies) without the per-item
  flood. `RUST_LOG=java_lsp::bus=trace` still shows everything.
- The extraction progress is emitted **~5 % of the way** (about 20 updates) plus
  the final one, instead of per archive.

# Decisions

- **D1 — Bulk = the per-item data notifications.** `SourceFile`, `BaseArtifact`,
  `RemoveBase`, `SourceEntries`, `SourceRemoved`, `SourceTypes`, `DirtyTypes`,
  `DirtyTypesDropped`, and `Progress`. Reason: these are the ones with cardinality
  proportional to the corpus; the structural messages carry the flow.
- **D2 — Keep them, just at `trace`.** They are real messages feeding the index;
  only their *logging* moves down a level.
- **D3 — Progress in ~5 % steps.** `progress_step = (total / 20).max(1)`, plus the
  final `total`. Reason: enough to see it move, ~20 lines not thousands.
- **D4 — The timing `Log` lines stay at `debug`** (they are `Log`, not `Progress`),
  so the per-phase split remains visible under `RUST_LOG=java_lsp=bus=debug`.

# Acceptance criteria

1. `RUST_LOG=java_lsp::bus=debug` no longer prints a line per source file / artifact
   / progress tick; the flow and the timing lines remain.
2. `RUST_LOG=java_lsp::bus=trace` still prints every message.
3. The extraction pass emits ~20 progress updates, not one per archive.
4. `cargo build`/`cargo fmt --check` clean and the `bus`/`sources` tests pass.

# Docs to update

- `docs/dev/changelog.md` — an entry at implementation.
- `docs/architecture.md` — the hub-log note (the bus decision bullet) now says the
  per-item notifications are at `trace`.
