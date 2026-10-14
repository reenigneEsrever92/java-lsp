---
type: ChangeRequest
kind: feature
title: "Maven module: effective poms and dependency closure"
description: Rework src/deprecated/resolve.rs into src/maven/resolve.rs — effective-pom assembly (parents, properties, dependencyManagement/BOMs) and the offline transitive closure — minus the old hub driver.
state: done
verified: { by: felix, at: 2026-10-14T12:00:29Z }
priority: medium
tags: [dev, maven, resolve]
owner: felix
parent: maven-module
---

# Problem

The offline resolver lives in `src/deprecated/resolve.rs` (1032 lines: `Dep`,
`EffectivePom`, `Resolver`, `assemble`, `interpolate`, `resolve_closure`,
`Artifact`, plus an old hub driver and tests). The maven module needs that
resolver as a pure library — the closure driver moves into the hub spawn, and
`local_repository()` moves into [settings](maven-module-settings.md).

# Proposal

Rework `src/deprecated/resolve.rs` into `src/maven/resolve.rs`:
- Keep unchanged in behavior: `Dep`, `EffectivePom::managed`, `Resolver`
  (cache + `pom_path`/`jar_path`/`effective_from_repo`), `assemble` (parent
  chains, property interpolation, `dependencyManagement` incl. import-scoped
  BOMs), `interpolate`, `interpolate_dep`, `resolve_closure` (nearest-wins
  mediation, path-scoped exclusions, optional/test pruning, missing poms prune
  their branch with a warning).
- Drop: the old `DriverMessage`/`next_notification` driver blocks — `resolve_artifacts`
  and `spawn` — and the `local_repository()` helper (now settings-owned). The
  hub spawn calls `resolve_closure` per module itself.
- Adapt: `Resolver::new(repo_root)` keeps taking the root; callers pass
  `settings::local_repository()`.

# Decisions

- **R1 — Pure library, no hub vocabulary.** No `crate::hub`, no `DriverMessage`;
  the closure is a plain `fn resolve_closure(&EffectivePom, &mut Resolver) ->
  Vec<Artifact>` the hub spawn invokes. Warnings use `tracing` only.
- **R2 — Keep the deprecated test suite.** The `TestRepo` fixture (temp local
  repository from `{g/a/p}/{a}/{v}/` pom texts) and the effective-pom/closure
  tests port over as-is, adapted to the new module path.

# Acceptance criteria

1. `assemble` produces the same effective poms as the deprecated code: parent
   chain via `relativePath` then repository, inherited `groupId`/`version` /
   `packaging` / properties / management / dependencies, interpolation of
   `${…}` incl. `project.*`/`pom.*` built-ins, `dependencyManagement` chain
   beats imported BOMs (import scope stripped after expansion).
2. `resolve_closure` expands direct + transitive dependencies with nearest-wins /
   first-declared mediation, path-scoped exclusions, optional and test pruning;
   a dependency without a resolvable version or a missing pom prunes its branch
   with a warning — never an error.
3. The closure tests from `src/deprecated/resolve.rs` pass under
   `src/maven/resolve.rs`; `Resolver` reads only the local repository (offline
   by construction) and no test touches the network.
4. `cargo test` green for the module; no other file touched.

# Implementation plan

## Approach

Move `src/deprecated/resolve.rs` to `src/maven/resolve.rs` and strip the hub
parts: delete `crate::hub`/`crate::messages` imports, the
`-- the dependency driver --` `spawn`, and `resolve_artifacts`; delete
`local_repository()` (superseded by `crate::maven::settings`); keep everything
else verbatim, re-testing with the ported `TestRepo` suite. The scaffold already
holds the public contract; the agent fills bodies and tests.

## Steps

- [ ] `src/maven/resolve.rs`: port `Dep`, `EffectivePom` (+`managed`),
  `Artifact`, `Coordinates`, `RawPom`, `parse_pom`/`parse_dependencies`,
  `child`/`child_text`/`trim`, `interpolate`/`interpolate_dep`,
  `assemble`/`strip_import_scope`, `Resolver`, `resolve_closure` from
  `src/deprecated/resolve.rs` verbatim (behavior unchanged).
- [ ] Drop the old driver: `resolve_artifacts`, `spawn`, `next_notification`,
  `DriverMessage` imports; `Resolver::new` callers now pass
  `crate::maven::settings::local_repository()` (no `local_repository()` here).
- [ ] Port the `TestRepo` fixture and the effective-pom/closure tests; add a
  sanity test using `settings::local_repository()` precedence only if cheap
  (otherwise leave precedence to the settings tests).
- [ ] `cargo test` green for the module; no other file touched.

## Next steps

[model](maven-module-model.md) builds on `resolve::{EffectivePom, Resolver}`;
[sources](maven-module-sources.md) on `resolve::Artifact`; the
[hub](maven-module-hub.md) sub-task drives `resolve_closure`.