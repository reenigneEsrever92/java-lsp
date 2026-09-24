---
type: ChangeRequest
kind: improvement
title: Log what the index holds, so its size can be targeted
description: Nothing reports the index's composition, so reducing its size would be guesswork; log entries by kind, names, base types/members, and approximate bytes at the warm-up milestones.
state: done
priority: low
tags: [dev, improvement, observability, memory, indexing]
owner: felix
verified:
  by: "cargo build/cargo fmt --check clean; `index::tests::composition_summarises_the_index` asserts the line's shape and counts, and all 22 `index` tests pass."
  at: 2026-10-01T00:00:00Z
---

# Problem

The index is large — the class-file base cache alone reached 3.7 GB before it was
split per archive ([base-cache-per-archive](base-cache-per-archive.md)) — but
nothing reports *what* it holds, so any size reduction
([index-entry-size](index-entry-size.md)) would be guesswork: is the cost the
flat entries, the base type model, the per-file source models, or the duplication
between the flat entries and the model?

# Proposal

`WorkspaceIndex::composition()` returns a one-line summary, logged on the two
warm-up milestones (the base `Ready` and the closing `StageDone(Downloads)`, so
the library-sources contribution is visible as a delta):

```
index composition: entries=N (types=… enums=… methods=… fields=… imports=…) names=… files=… ~XKB; base layers=… types=… members=… ~YKB; source models=… ~ZKB
```

The byte figures are approximate: the struct sizes (`size_of`) plus the owned
strings, types, params, and members (`Ty` recurses into its arguments). Good
enough to see which part dominates.

# Decisions

- **D1 — Counts by kind + approximate bytes, not exact.** An exact heap audit is
  not worth it; the split between entries and the type model is what matters.
- **D2 — Logged by the index module at `target: "java_lsp::bus"`.** The index
  module has no bus client (it only receives), and the hub is the router (it must
  not block on a request from its own thread), so the module logs directly; the
  `java_lsp::bus` target keeps it visible under the existing
  `RUST_LOG=java_lsp::bus=debug` filter (it is `INFO`, shown at debug and above).
- **D3 — At `Ready` and at `StageDone(Downloads)`.** The delta shows how much the
  extracted library sources add on top of the jars/JDK.
- **D4 — Measurement only; no behaviour change.** It reads the index under its
  existing locks once per milestone.

# Acceptance criteria

1. A warm-up logs the composition line at `Ready` and again after the downloads
   stage, each naming the entry counts by kind, the base type/member counts, and
   approximate bytes.
2. `cargo build`/`cargo fmt --check` clean and the `index` tests pass.

# Docs to update

- `docs/dev/changelog.md` — an entry at implementation.

# Next

Run a real warm-up and read the two lines; then `index-entry-size` (step 3) cuts
the part that dominates.
