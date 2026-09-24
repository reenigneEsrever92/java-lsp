---
type: ChangeRequest
kind: improvement
title: Report progress while dependency source archives are extracted and indexed
description: The download driver reports per-artifact progress while fetching sources, then goes silent through the extraction/indexing phase, so the hub log's per-artifact lines look like endless downloading; emit a progress update per archive.
state: done
priority: low
tags: [dev, improvement, observability, sources]
owner: felix
verified:
  by: cargo build clean; cargo fmt --check clean; the server-free `sources::tests::a_sources_jar_is_published_as_one_base_artifact` passes and now also asserts the per-archive progress update (`Parsed 1/1 dependency source archives`); the `bus::tests` still pass. The other `sources` tests cannot run here — they bind a loopback `TestServer`, which the sandbox forbids (the known environmental `PermissionDenied` failures), unchanged by this work.
  at: 2026-09-30T00:00:00Z
---

# Problem

`src/sources.rs` runs the dependency-sources pass in two sequential phases:

1. **Fetch** (`fetch_sources`) — reports `Fetching N dependency sources` (0%) and
   `Fetched x/N dependency sources` as each download completes.
2. **Extract + index** (`index_extracted`) — reports `Parsing N dependency source
archives` once, then emits one `RemoveBase` + `BaseArtifact` per artifact (the
   `sender=download notify …` lines in the hub log), then `Indexed M dependency
source files`.

Phase 2 can be the slow one — tree-sitter parsing every downloaded
`<a>-<v>-sources.jar` — but it emits **no per-artifact progress**: the editor's
work-done bar sits still between "Parsing N" and "Indexed M", and the only sign of
movement is the `debug`-level `BaseArtifact` line per artifact. On a large
dependency set that reads exactly like a downloader that never finishes: the hub
log "has been logging that kind of message for a long time since startup", with no
way to tell "still extracting" from "stuck", or to see that it has finished.

# Proposal

Have `index_extracted` emit a `Progress(Update)` per archive as it moves through
the loop — `Parsed x/N dependency source archives` with a percentage — so the
work-done item keeps advancing through extraction and the sequence
`Parsing N …` → `Parsed N/N …` → `Indexed M dependency source files` →
`library sources indexed …` → `workspace index warm-up complete` makes the finish
unmistakable.

# Decisions

- **D1 — Per-archive progress in the extraction loop.** One `Progress(Update)` per
  artifact, in the same `x/N` + percentage shape as `fetch_sources`'s `Fetched
x/N`. Reason: the two phases should look alike, and the count reaches N/N to
  show completion.
- **D2 — Emitted after each archive, not before.** The message is `Parsed x/N …`
  and the percentage is `(x * 100 / N)`, so the bar is honest (it does not reach
  100% before the last archive is handled). The loop's skip paths (a missing jar,
  no `.java` files, an unreadable path) move from `continue` into a labeled block
  so the progress line still fires for every archive.
- **D3 — No new message types or bus changes.** Progress rides the existing
  `DriverMessage::Progress(ProgressUpdate::Update)`, already rendered by the shell
  and already named `sender=download` in the hub log.
- **D4 — Out of scope.** The fetch phase's reporting is already adequate; no
  change there. The per-artifact `RemoveBase`/`BaseArtifact` `debug` lines stay.

# Acceptance criteria

1. `index_extracted` emits one `Progress(Update)` per artifact, `Parsed x/N
dependency source archives`, with `x` running `1..=N` (the last is `Parsed N/N
…`) and a percentage reaching 100 on the last.
2. The existing messages are unchanged: `Parsing N dependency source archives` at
   the start, `Indexed M dependency source files` and the `library sources indexed
…` log at the end.
3. A test drives `index_extracted` over a two-file sources jar and asserts the
   per-archive progress update is published. (The test needs no server: it writes
   the jar straight into the repository path.)
4. `cargo build` and `cargo fmt --check` are clean and the `sources` tests pass.

# Docs to update

- `docs/dev/backlog/index.md` — this request's row.
- `docs/dev/changelog.md` — an entry at implementation.
- (`docs/architecture.md` needs no change — it already describes the pass's
  progress/log lines generally; only an existing progress line's granularity
  changes.)

# Implementation plan

## Approach

`src/sources.rs::index_extracted` only. Wrap each artifact's body in a labeled
block so its `continue` guards become `break`, then emit a progress update after
the block:

```rust
let total = artifacts.len();
for (index, artifact) in artifacts.iter().enumerate() {
    'artifact: {
        let (group, id, version) = artifact;
        let Ok(data) = std::fs::read(sources_jar_path(repo, group, id, version)) else {
            break 'artifact;
        };
        // … parse every `.java`; `break 'artifact` on an empty file set or a bad URI …
        artifacts_indexed += 1;
    }
    sink(DriverMessage::Progress(ProgressUpdate::Update {
        message: format!("Parsed {}/{} dependency source archives", index + 1, total),
        percentage: Some(((index + 1) * 100 / total.max(1)) as u32),
    }));
}
```

## Steps

- [x] `src/sources.rs`: wrap the per-artifact body in a labeled block (`break`
      instead of `continue`) and emit the per-archive `Progress(Update)`.
- [x] `src/sources.rs`: extend `a_sources_jar_is_published_as_one_base_artifact`
      to assert the per-archive progress update is published.
- [x] Docs: `docs/dev/backlog/index.md` and `docs/dev/changelog.md`.
- [x] Build and test: `cargo build`, `cargo fmt --check`, the `sources` tests.
