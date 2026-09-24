---
type: ChangeRequest
kind: bug
title: Make byte-offset to LSP position conversion linear, not quadratic, per file
description: lsp_position rescans the whole text from byte 0 on every call, so indexing a file is O(declarations x file size) — ~110 ms/file on large sources; build a line-start index once and binary-search it.
state: done
priority: high
tags: [dev, bug, performance, indexing]
owner: felix
verified:
  by: "measured on 30 real `*-sources.jar` (3274 files): the dependency-source pass fell from 373.6 s to 18.7 s, with `extract_entries` falling from 358.5 s to 4.0 s (the 96%-of-time phase). cargo build and cargo fmt --check clean; the server-free `sources` test and the `bus` tests pass. The `java-lsp-bench` fixture (small, uniform ~5 KB files) is unchanged, as expected — it is parse/type-infos bound, not offset-conversion bound."
  at: 2026-09-30T00:00:00Z
---

# Problem

`analysis.rs::lsp_position(text, byte_offset)` converts a byte offset to an LSP
position by **rescanning the text from byte 0 on every call**:

```rust
let line = text.as_bytes()[..offset].iter().filter(|&&b| b == b'\n').count() as u32;
let line_start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
```

`lsp_range` calls it twice (start and end), and `index.rs::entry` calls
`lsp_range` twice per declaration (full + selection) — so indexing is
**O(declarations × file size)**, quadratic per file.

Measured on 30 real `*-sources.jar` (3,274 files), the whole dependency-source
pass took **373.6 s**, of which `extract_entries` (the phase that calls
`lsp_range` per declaration) was **358.5 s** — **96 %**, ~**110 ms per file**.
Neither the inflate (1.3 s), the cache writes (1.1 s), the tree-sitter parse
(7.7 s) nor `collect_type_infos` (4.4 s) came close. The same conversion runs in
the workspace source scan, the JDK `src.zip` indexer, and the on-demand features
(`document_symbols`, `references`, `semantic_tokens`, inlay hints), so large
files pay it everywhere.

This also explains why the `references`/`rename` unit tests were slow enough to
make the suite impractical to run.

# Proposal

Introduce `analysis.rs::LineIndex` — the file's line-start byte offsets, built
**once** per text (`O(n)`) — and answer each position with
`partition_point` (binary search, `O(log n)`). Use it in the entry-collection
path (`index.rs::extract_entries` → `collect_entries`/`entry`/
`collect_record_components`/`collect_lombok_entries`), which is the measured hot
spot.

# Decisions

- **D1 — A `LineIndex` built once per file, binary-searched per position.**
  `LineIndex::new(text)`, `LineIndex::position(text, offset)`,
  `LineIndex::range(text, node)`. Reason: `O(n + k log n)` instead of
  `O(n · sum(offsets))`; the fix is at the call site that has one text and many
  offsets.
- **D2 — Thread the index through the entry collectors rather than change
  `lsp_position`.** The targeted path is six `lsp_range` calls in `index.rs`.
  Reason: fixes the measured 96 % with a small, low-risk diff; the global
  refactor (134 `lsp_range`/`lsp_position` call sites across `analysis.rs`) is
  deliberately left out.
- **D3 — Out of scope (follow-up).** Change `lsp_position`/`lsp_range` themselves
  to take a `&LineIndex` and update all 134 call sites, so the on-demand features
  on large buffers also become linear. Kept separate because it is a broad
  mechanical change with no behaviour change.
- **D4 — Behaviour is identical.** The positions are byte-for-byte the same; only
  the computation changes.

# Acceptance criteria

1. `extract_entries` no longer rescans the text per declaration: the
   dependency-source pass on the 30-jar sample drops from 373.6 s to <20 s.
2. Extracted `SymbolEntry` ranges are unchanged (same lines/characters), the
   server-free `sources` test passes, and `cargo build`/`cargo fmt --check` are
   clean.
3. `java-lsp-bench` shows no regression.

# Docs to update

- `docs/dev/backlog/index.md` — this request's row.
- `docs/dev/changelog.md` — an entry at implementation.
