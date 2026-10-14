---
type: ChangeRequest
kind: feature
title: "Maven module: hub integration (spawn, FoldersDiscoverred origin, wiring)"
description: Wire the module together — src/maven/mod.rs spawn publishing the two FoldersDiscoverred batches with origins, the FolderOrigin message field, index origin threading, --no-dependencies/--no-downloads flags, main/lib wiring, deleting the superseded project.rs and the deprecated Maven files.
state: done
verified: { by: felix, at: 2026-10-14T12:00:29Z }
priority: medium
tags: [dev, maven, hub]
owner: felix
parent: maven-module
---

# Problem

The four pieces ([settings](maven-module-settings.md),
[resolve](maven-module-resolve.md), [model](maven-module-model.md),
[sources](maven-module-sources.md)) are libraries; nothing drives them on the
hub, the live `src/project.rs` still does the tree walk, `FoldersDiscoverred`
carries no origin so the index cannot tag `LibrarySource`, and the CLI toggles
are gone. The [parent](maven-module.md) decision D5 requires the per-batch
origin; D2 requires superseding `project.rs`.

# Proposal

- `src/maven/mod.rs`: the hub module — `spawn(&Hub, &Cli)` subscribes to
  `ServerStarted`; on it (blocking pool) `model::discover` over
  `settings::local_repository()`, publish the workspace source roots as one
  `FoldersDiscoverred { origin: Workspace }` batch, `resolve::resolve_closure`
  per module (deduped), `sources::fetch_and_extract`, publish the extracted
  roots as a second `FoldersDiscoverred { origin: LibrarySources }` batch.
- `src/messages.rs`: `pub enum FolderOrigin { Workspace, LibrarySources }`;
  `FoldersDiscoverred { uri: Vec<Url>, origin: FolderOrigin }`; the
  `Notification::summary` arm matches with `..` and keeps its format.
- `src/index/mod.rs` + `analyze.rs`: `serve` maps `FolderOrigin` →
  `FileOrigin` and threads it through `scan_sources`/`index_source`/`upsert_file`
  so scanned files are tagged `Workspace` or `LibrarySource`.
- `src/cli.rs` + `src/main.rs`: reintroduce `--no-dependencies` (skip resolution
  and library sourcing) and `--no-downloads` (resolve but never fetch/extract);
  `main.rs::setup` passes the `Cli` to `maven::spawn` and stops spawning
  `project::spawn`.
- `src/lib.rs`: `pub mod maven;`, remove `pub mod project;`.
- Delete `src/project.rs`, `src/deprecated/project.rs`,
  `src/deprecated/resolve.rs`, `src/deprecated/sources.rs`.
- `docs/architecture.md`: the Layout and component rows for `project.rs` become
  `maven/`; the rework note and implementation notes describe the two batches.

# Decisions

- **H1 — `spawn` publishes both batches itself.** No separate driver modules:
  maven owns discovery → workspace batch → closure → extract → library batch, in
  one task, off the request path.
- **H2 — Gating lives in the spawn.** `--no-dependencies` skips closure +
  fetch + extract (workspace discovery still publishes); `--no-downloads` skips
  fetch + extract only (nothing published for dependencies);
  `settings::offline()` behaves like `--no-downloads`.
- **H3 — Env precedent for the summary arm.** The user's uncommitted
  `Notification::summary` in `src/messages.rs` must keep compiling: the
  `FoldersDiscoverred` arm gains `..` and keeps printing the folder count.

# Acceptance criteria

1. On `ServerStarted` over a pom workspace, the module publishes exactly two
  `FoldersDiscoverred` notifications: `Workspace` with the pom-derived source
  roots, then `LibrarySources` with the extracted cache roots; a pom-less
  workspace publishes `Workspace` with the whole root and no library batch.
2. The index tags scanned files per batch origin: `Workspace` →
  `FileOrigin::Workspace` (references/`workspace/symbol`/rename see them),
  `LibrarySources` → `FileOrigin::LibrarySource` (completable and openable —
  `declaration_candidates` admits them — while `WorkspaceFiles` still excludes
  them, per umbrella AC5).
3. `--no-dependencies` publishes the workspace batch only; `--no-downloads`
  publishes the workspace batch and resolves but never fetches; `JAVA_LSP_OFFLINE`
  or settings `offline` behaves like `--no-downloads`.
4. A hub-flow test (temp workspace with poms + stubbed local repo + temp cache,
  no network) asserts both batches' origins and folder sets; the no-pom fallback
  and gating tests cover H2.
5. `src/project.rs` and the three deprecated Maven files are deleted; the crate
  builds (`cargo build --all-targets`), formats (`cargo fmt --all -- --check`),
  and passes `cargo test` with the full suite green.

# Implementation plan

## Approach

The four library sub-tasks land first (their real APIs are the contract this
sub-task compiles against). Wire order: messages origin → index threading →
mod.rs spawn → cli/main wiring → deletions → architecture doc. The spawn does
the heavy work on `spawn_blocking` (like the old `project::spawn`), publishes
notifications through its `HubClient`, and gates on `Cli` flags plus
`settings::offline()`.

## Steps

- [ ] `src/messages.rs`: add `FolderOrigin { Workspace, LibrarySources }`;
  change `FoldersDiscoverred { uri }` to `{ uri, origin }`; update the
  `Notification::summary` arm to match `{ uri, .. }` (H3).
- [ ] `src/index/` : `serve` reads `origin: FolderOrigin` off each batch, maps
  it to `FileOrigin`, and passes it to `scan_sources`; `analyze.rs` threads the
  origin through `scan_sources` → `index_source` → `upsert_file` (no other
  behavior change).
- [ ] `src/maven/mod.rs`: `spawn(&Hub, &Cli)` — subscribe; on `ServerStarted`,
  `spawn_blocking`: `model::discover(&root, &mut resolver)`; publish the
  `Workspace` batch (sorted roots); unless `--no-dependencies`: resolve the
  closure per module (dedup), unless `--no-downloads`/offline: `fetch_and_extract`;
  publish the `LibrarySources` batch; log a summary line.
- [ ] `src/cli.rs`: add `--no-dependencies`, `--no-downloads`; `src/main.rs`:
  `setup(cli)` passes `&cli` to `maven::spawn`, drops `project::spawn`.
- [ ] `src/lib.rs`: declare `pub mod maven;`, remove `pub mod project;`; delete
  `src/project.rs` and `src/deprecated/{project,resolve,sources}.rs`.
- [ ] Tests in `src/maven/mod.rs`: hub-flow (both batches, origins, folder sets),
  no-pom fallback, `--no-downloads`/`--no-dependencies` gating; migrate the
  relevant `src/project.rs` hub tests here.
- [ ] `docs/architecture.md`: Layout — `project.rs` row → `maven/` (settings,
  resolve, model, sources, mod); the rework note drops `project` from the built
  list; the Maven project model / dependency-sources rows and implementation
  notes describe the two `FoldersDiscoverred` batches and the
  `LibrarySource`-tagged scan.
- [ ] `cargo build --all-targets`, `cargo fmt --all -- --check`, `cargo test`
  all green (AC5).