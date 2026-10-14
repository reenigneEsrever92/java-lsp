---
type: ChangeRequest
kind: feature
title: "Maven module: project model with pom-aware source roots"
description: Rework src/deprecated/project.rs into src/maven/model.rs — pom discovery, modules with effective poms, declared source roots, no-pom whole-root fallback — superseding the live src/project.rs walk.
state: done
verified: { by: felix, at: 2026-10-14T12:00:29Z }
priority: medium
tags: [dev, maven, project-model]
owner: felix
parent: maven-module
---

# Problem

The live `src/project.rs` (232 lines) discovers source folders by walking the
tree for folders that directly hold a `.java` file — it parses no `pom.xml`, so
Maven modules' declared source roots are ignored and build outputs leak into the
candidate set. The deprecated `src/deprecated/project.rs` already has the
pom-aware model (`ProjectModel::discover`, `source_roots`, no-pom fallback) that
the maven module needs.

# Proposal

Rework `src/deprecated/project.rs` into `src/maven/model.rs`:
- `Module { directory, effective, source_roots }`, `ProjectModel { modules,
  maven }` with `source_roots()` (deduped, sorted).
- `discover(root, &mut Resolver) -> ProjectModel` — every `pom.xml` under the
  root (hidden dirs, `target`/`build` skipped) becomes a module with its
  effective pom and existing source roots (`src/main/java`/`src/test/java`,
  honoring `<build>` overrides); with no poms at all, the whole root is one
  module with the root as its only source root (the non-Maven fallback).
- `find_poms`/`source_roots` stay module-private; `collect_java_files` /
  `walk_project` from the deprecated file are dropped — the index scans
  `FoldersDiscoverred` folders itself.

# Decisions

- **M1 — Model only, no file walk.** `walk_project`/`collect_java_files` are not
  ported: the index's `workspace_java_files` walk already handles scanning, and
  the maven spawn publishes folders, not files.
- **M2 — Same discovery semantics as the live fallback.** The no-pom fallback
  (whole root as one source root) preserves the live `project.rs` behavior for
  non-Maven workspaces (umbrella D2, AC1).

# Acceptance criteria

1. `discover` on a multi-module workspace returns one module per `pom.xml`, each
   with its effective pom and only the source roots that exist on disk
   (`<build>` overrides honored; missing roots dropped).
2. A workspace without poms yields one module with the root as its source root
   (`maven == false`), so non-Maven projects keep working.
3. The walk skips hidden directories and `target`/`build` trees.
4. The discovery tests from `src/deprecated/project.rs` pass under
   `src/maven/model.rs`, adapted to the new module path; the hub-flow test that
   publishes the folders lives in the [hub](maven-module-hub.md) sub-task.
5. `cargo test` green for the module; no other file touched.

# Implementation plan

## Approach

Copy `src/deprecated/project.rs`, minus `walk_project`/`collect_java_files`;
keep `discover`, `find_poms`, `source_roots`, `Module`, `ProjectModel`. Import
`crate::maven::resolve::{EffectivePom, Resolver}` instead of
`crate::resolve::…`, and drop the `crate::resolve::local_repository()` call —
callers pass the root (the hub spawn resolves it via `settings::local_repository()`).
The scaffold holds the public contract.

## Steps

- [ ] `src/maven/model.rs`: port `Module`, `ProjectModel` (+`source_roots`),
  `discover`, `find_poms`/`find_poms_recursive`, `source_roots(directory,
  effective)` from `src/deprecated/project.rs`; drop `walk_project` and
  `collect_java_files`; switch imports to `crate::maven::resolve`.
- [ ] Make `discover` take the repository root or a prebuilt `Resolver` — keep
  the deprecated shape `discover(root, &mut Resolver)` so the hub spawn can reuse
  one resolver across modules (shared pom/BOM cache).
- [ ] Port the deprecated discovery tests, adapted to the new path, including the
  multi-module and no-pom fallback cases and the hidden/`target` skip.
- [ ] `cargo test` green for the module; no other file touched.

## Next steps

The [hub](maven-module-hub.md) sub-task calls `model::discover` on
`ServerStarted`, publishes the workspace roots, then drives
[sources](maven-module-sources.md) for the closure.