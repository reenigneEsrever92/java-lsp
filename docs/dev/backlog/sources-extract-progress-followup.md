---
type: ChangeRequest
kind: improvement
title: Drive the dependency-source extraction progress by counts, with the parsed source-file count
description: The per-archive extraction progress reports the archive count plus a percentage; report the running parsed source-file count instead and drop the percentage, which collides with the fetch phase's.
state: done
priority: low
tags: [dev, improvement, observability, sources]
owner: felix
verified:
  by: cargo build clean; cargo fmt --check clean; `sources::tests::a_sources_jar_is_published_as_one_base_artifact` passes and asserts the new per-archive message (`Parsed 1/1 dependency source archives (2 source files)`). The other `sources` tests cannot run here — their loopback `TestServer` is forbidden by the sandbox, as before.
  at: 2026-09-30T00:00:00Z
---

# Problem

The per-archive progress added in
[sources-extract-progress](sources-extract-progress.md) is
`Parsed x/N dependency source archives` plus a percentage. Two things are wrong
with that shape:

- The percentage is a second, redundant signal for the same count, and it collides
  with the **fetch** phase's percentage, which has already reached 100% by the time
  extraction starts — so during extraction the bar reads "done" while the text is
  still climbing. Count-driven text is the honest signal.
- Nothing surfaces the pass's real unit of work: the number of source files parsed
  so far. That number is already tracked (`files_indexed`) and is far more telling
  than an archive count when one jar holds hundreds of files.

# Proposal

Report the running parsed source-file count in the per-archive progress and drop
the percentage:

```
Parsed 7/312 dependency source archives (1840 source files)
```

The start message (`Parsing N dependency source archives`, no percentage) and the
end messages (`Indexed M dependency source files`, `library sources indexed …`)
are unchanged.

# Decisions

- **D1 — Count-driven; no percentage.** The per-archive updates carry no
  percentage, so the extraction phase is not conflated with the fetch phase's bar,
  which hits 100% before extraction begins. (Agreed — "driven purely by the archive
  count with no percentage".)
- **D2 — Include the running parsed source-file count.** The message also reports
  `files_indexed` at that point — the pass's real unit of work. (Agreed — "also
  show the parsed-file count".)
- **D3 — Only the per-archive update changes.** The fetch phase's
  `Fetched x/N` percentages, the `Parsing N …` start, and the `Indexed M …` /
  `library sources indexed …` end are unchanged.

# Acceptance criteria

1. Each per-archive progress update reads `Parsed x/N dependency source archives
(F source files)`, where `x` runs `1..=N` and `F` is the running count of parsed
   files; none carries a percentage.
2. The start and end messages are unchanged: `Parsing N dependency source
archives`, `Indexed M dependency source files`, and the `library sources indexed
…` log.
3. The existing `sources` test asserts the new message text.
4. `cargo build` and `cargo fmt --check` are clean and the `sources` tests pass.

# Docs to update

- `docs/dev/backlog/index.md` — this request's row.
- `docs/dev/changelog.md` — an entry at implementation.
- (`docs/architecture.md` needs no change.)

# Implementation plan

## Approach

`src/sources.rs::index_extracted` only — the per-archive `sink(...)` after the
labeled block becomes:

```rust
sink(DriverMessage::Progress(ProgressUpdate::Update {
    message: format!(
        "Parsed {}/{} dependency source archives ({} source files)",
        index + 1,
        total,
        files_indexed
    ),
    percentage: None,
}));
```

## Steps

- [x] `src/sources.rs`: change the per-archive progress to the count + running
      source-file count with no percentage.
- [x] `src/sources.rs`: update the `a_sources_jar_is_published_as_one_base_artifact`
      assertion to the new message text.
- [x] Docs: `docs/dev/backlog/index.md` and `docs/dev/changelog.md`.
- [x] Build and test: `cargo build`, `cargo fmt --check`, the `sources` tests.
