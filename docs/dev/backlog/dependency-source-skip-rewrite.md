---
type: ChangeRequest
kind: improvement
title: Do not rewrite dependency sources already extracted to the cache
description: index_one re-creates every package directory and rewrites every extracted .java on each start; create each directory once per artifact and write only when the file is missing or the wrong length.
state: done
priority: medium
tags: [dev, improvement, performance, sources]
owner: felix
verified:
  by: "measured with the phase-timing line over the same 40 real `*-sources.jar` (3601 files): the `write` phase fell from 14294 ms (cold cache) to 42 ms (warm cache). Wall clock improved from 5.44 s to 4.65 s — only ~15 %, because the writes overlapped with the parse on the other cores. cargo build/cargo fmt --check clean; the `sources`/`bus` tests pass."
  at: 2026-10-01T00:00:00Z
---

# Problem

`index_one` extracts every artifact the same way on every startup:

```rust
let written = std::fs::create_dir_all(parent)
    .and_then(|()| std::fs::write(&target, text.as_bytes()));
```

Both halves repeat work that a previous run already did. `create_dir_all` is
called once per **file** (the package directory is usually shared by many), and
every `.java` is rewritten even though the extracted tree in
`~/.cache/java-lsp/sources` is already complete — hundreds of thousands of writes
and directory ops per start.

# Proposal

Create each package directory once per artifact (a `HashSet<PathBuf>` of the
directories already made) and write a file only when it is missing or its length
differs from the content (`metadata().len() != text.len()`), which still guards a
truncated or partial file. The parse is unchanged — it uses the in-memory text
from the jar, so the on-disk copy is only needed for navigation.

# Decisions

- **D1 — Skip by existence + length, not existence alone.** One `stat` instead of
  a `write`, and a truncated/partial file from an interrupted run is rewritten.
  Reason: same cost as `is_file()`, and safe against a half-written file.
- **D2 — Directory creation deduped per artifact.** Distinct artifacts extract to
  distinct `<group>/<id>/<version>` subtrees, so per-artifact dedupe is complete.
- **D3 — The `write` phase timing now shows the saving.** `Timers::write` covers
  the (now usually skipped) directory + file I/O for extraction.

# Acceptance criteria

1. On a warm cache the `write` phase is ~0 (versus seconds on a cold cache), and
   the extracted files are still present and correct.
2. `cargo build`/`cargo fmt --check` clean and the `sources`/`bus` tests pass.

# Docs to update

- `docs/dev/changelog.md` — an entry at implementation.

# Note

The wall-clock gain was only ~15 % here because the writes overlapped with the
parse on the other cores; the parse side remains the wall-clock driver. This
change mainly removes I/O churn and is a prerequisite for a warm-cache restart.
