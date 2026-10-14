---
type: ChangeRequest
kind: feature
title: Maven module — parse pom.xml projects, fetch dependency sources into a home-folder cache, publish source roots via FoldersDiscoverred
description: Reintroduce the Maven machinery (project model, offline dependency resolution, source fetching) as a `maven` hub module that publishes the extracted dependency sources to the index through the FoldersDiscoverred notification, superseding the generic project.rs source-folder walk.
state: done
verified: { by: felix, at: 2026-10-14T12:00:29Z }
priority: medium
tags: [dev, maven, project-model, sources]
owner: felix
---

# Problem

The hub rework is mid-flight: the Maven machinery (project model, resolve, sources)
sits deprecated under `src/deprecated/` (out of the build), and the live `project.rs`
only walks the workspace tree for folders that directly hold a `.java` file — it
parses no `pom.xml`, so Maven modules' declared source roots are ignored, and the
server is blind to dependencies: nothing resolves the closure, and no library
sources reach the index (R10 is unimplemented in the live tree).

What is wanted: a `maven` module that parses Maven projects, downloads the
dependency sources into a cache in the home folder, and adds those sources to the
index via the `FoldersDiscoverred` notification — the transport the index already
consumes, so dependency sources ride the same scan path as workspace sources.

# Proposal

New hub module `src/maven.rs`, reintroduced from `src/deprecated/{project,resolve,sources}.rs`
and reworked onto the current hub API (register/subscribe/notify, `main.rs::setup`
spawns it). On `Notification::ServerStarted` it:

1. **parses the Maven projects** under the workspace root — every `pom.xml`
   becomes a module with its effective pom (parent chains, property
   interpolation, `dependencyManagement` incl. import-scoped BOMs) and its
   declared source roots (`src/main/java`/`src/test/java`, honoring `<build>`
   overrides); a workspace with no poms falls back to scanning the whole root,
   so non-Maven projects keep working;
2. **downloads the dependencies to a cache in the home folder** — resolves the
   full dependency closure strictly offline (transitive walk, nearest-wins
   mediation, exclusions, optionals) from the local repository, then for each
   artifact reuses `<a>-<v>-sources.jar` from the local repository or downloads
   it (Maven Central by default, settings.xml mirror, or
   `$JAVA_LSP_MAVEN_CENTRAL_URL`), `.sha1`-verified, writing it into the local
   repository, and extracts it under the sources cache
   (`$JAVA_LSP_SOURCES_CACHE` → `$XDG_CACHE_HOME/java-lsp/sources` →
   `~/.cache/java-lsp/sources`); and
3. **publishes the sources to the index via `FoldersDiscoverred`** — two
   batched notifications: the workspace source roots with `FolderOrigin::Workspace`,
   and the extracted dependency-source roots with `FolderOrigin::LibrarySources`.
   The index's existing serve loop scans both; the new per-batch origin maps to
   the existing `FileOrigin::LibrarySource`, so extracted sources are completable
   and openable (go-to-definition reaches a library declaration) while
   `WorkspaceFiles` — references, workspace symbols, rename — stays workspace-only,
   per the documented v0.5 constraint.

`src/project.rs` is superseded and deleted; its generic walk survives only as
maven's no-pom fallback. Binary dependency-jar indexing stays out (separate
future request; `jars.rs`, `classfile.rs`, `base_cache.rs` remain deprecated).

# Decisions

- **D1 — One `maven` module, reintroducing the deprecated pieces.** The
  deprecated `project.rs` (pom discovery, source roots), `resolve.rs` (effective
  poms, closure, `local_repository()`), and `sources.rs` (fetch/extract, `offline()`,
  `base_url()`, `cache_dir()`) hold almost all the needed logic; the request
  reworks them onto the current hub instead of writing from scratch. They are
  removed from `src/deprecated/` once the reintroduced code replaces them.
- **D2 — `maven` supersedes `project.rs`; the fallback keeps non-Maven
  workspaces working.** The live `src/project.rs`'s only consumer is
  `main.rs::setup`; its topmost-`.java`-folder walk becomes maven's no-pom
  fallback (whole root as one source root, matching deprecated
  `ProjectModel::discover`), the hub tests adapt into maven's, and `project.rs`
  is deleted.
- **D3 — Caches in the home folder, per the established defaults.** Sources jars
  are written into the local Maven repository (default `~/.m2/repository`);
  extraction lives under `$JAVA_LSP_SOURCES_CACHE` → `$XDG_CACHE_HOME/java-lsp/sources`
  → `~/.cache/java-lsp/sources` (requirements R10, deprecated `cache_dir()`), one
  directory per artifact. Reuse-before-download in both.
- **D4 — Honor the local `~/.m2/settings.xml`.** Read Maven's user-level
  settings and apply: `localRepository` (precedence `$MAVEN_REPO` > settings
  `localRepository` > `~/.m2/repository` — the env var stays the explicit
  java-lsp override), `mirrors` (the first mirror whose `mirrorOf` matches
  `central`/`*`/`external:*` replaces the download base, below an explicit
  `$JAVA_LSP_MAVEN_CENTRAL_URL`), and `offline: true` (treated like
  `JAVA_LSP_OFFLINE`). Global settings (`$M2_HOME/conf/settings.xml`), `<servers>`
  credentials, and `<proxies>` are out of scope; reqwest's proxy env handling is
  left alone.
- **D5 — `FoldersDiscoverred` carries a per-batch origin.** The message gains a
  small `FolderOrigin` field (`Workspace` | `LibrarySources`) so the index can
  tag scanned files with the matching `FileOrigin`. This keeps the message name
  the request names while making the existing origin semantics work:
  `read.rs::source_files` (Workspace-only) and `navigate.rs::declaration_candidates`
  (excludes only `Dependency`) then fall out correctly — extracted sources are
  definition targets but never references/rename/workspace-symbol candidates.
- **D6 — Sources only, binaries later.** This request indexes the downloaded
  *sources*; indexing the resolved class jars (deprecated `jars.rs`/`classfile.rs`/
  `base_cache.rs`) is a separate future change request.
- **D7 — CLI toggles return with the module.** `--no-downloads` (resolve, but
  never fetch/extract) and `--no-dependencies` (skip resolution and library
  sourcing entirely) are reintroduced in `cli.rs`, per the architecture note that
  the toggles "return together with the modules they switch off"; the env vars
  (`JAVA_LSP_OFFLINE`, `JAVA_LSP_MAVEN_CENTRAL_URL`, `JAVA_LSP_SOURCES_CACHE`,
  `MAVEN_REPO`) keep working. Workspace source-root discovery is never gated by
  them.
- **D8 — Resolution is strictly offline.** Only `-sources.jar` is ever fetched;
  poms and binaries are read from the local repository or pruned with a warning
  (degraded completions, never a broken server) — the deprecated resolver's
  contract, unchanged.
- **D9 — Split into five sub-tasks under an umbrella, implemented in parallel by
  sub-agents.** The change spans five independently testable pieces (settings,
  resolve, model, sources, hub integration) that map one-to-one onto the
  deprecated file boundaries; each lands as its own `maven-module-<part>`
  change request so the work proceeds in parallel with disjoint write scopes and
  is reviewed slice by slice. The coordinator scaffolds `src/maven/` with
  compiling API contracts (stub signatures) first so the crate builds while the
  pieces land; each sub-agent owns exactly one file. The dependency chain
  settings → resolve → model → sources → hub determines the order; the hub
  sub-task supersedes `project.rs`, deletes the deprecated files, and wires the
  module in.

# Acceptance criteria

1. On `ServerStarted` in a workspace with poms, maven discovers every module's
   effective pom and source roots (`src/main/java`/`src/test/java`, honoring
   `<build>` overrides) and publishes them in one `FoldersDiscoverred` batch with
   `Workspace` origin; a pom-less workspace publishes the whole root instead —
   the `project.rs` behavior keeps working.
2. The full dependency closure resolves from the local repository (parent chains,
   property interpolation, `dependencyManagement` incl. import-scoped BOMs,
   transitive walk with nearest-wins mediation, path-scoped exclusions, optional/
   test pruning); an artifact missing from the repository is pruned with a
   warning and never fetched as a binary.
3. For each resolved artifact, `-sources.jar` is reused from the local repository
   when present; otherwise it is downloaded (default Maven Central; the settings.xml
   mirror when one matches; `$JAVA_LSP_MAVEN_CENTRAL_URL` overriding both), verified
   against the published `.sha1` when there is one, and written into the local
   repository.
4. Extraction lands under `$JAVA_LSP_SOURCES_CACHE` → `$XDG_CACHE_HOME/java-lsp/sources`
   → `~/.cache/java-lsp/sources`, one directory per artifact.
5. The extracted roots are published as a second `FoldersDiscoverred` batch with
   `LibrarySources` origin; the index scans them, so a library type completes and
   `textDocument/definition` on a library type opens its declaration, while
   `textDocument/references`, `workspace/symbol`, and rename never search the
   cache (the existing `WorkspaceFiles`/`declaration_candidates` filters hold).
6. `~/.m2/settings.xml` is honored: `localRepository` (below `$MAVEN_REPO`, above
   `~/.m2/repository`), mirrors for the download base, and `offline: true`
   disabling all network work like `JAVA_LSP_OFFLINE`.
7. `--no-downloads` resolves but never fetches or extracts; `--no-dependencies`
   skips resolution and library sourcing; `JAVA_LSP_OFFLINE` disables all network
   work.
8. Every test runs offline — fixture poms, a stubbed local repository and a temp
   sources cache, no live network — and covers: settings.xml parsing,
   pom discovery/effective models, closure resolution, extract-and-publish (the
   batch carries `LibrarySources` and points into the cache), offline gating, and
   the no-pom fallback. `cargo build --all-targets`, `cargo fmt --all -- --check`,
   and `cargo test` are clean.
9. `src/project.rs` is deleted and `docs/architecture.md` documents the `maven`
   module and the `FoldersDiscoverred` origin.

# Docs

- `docs/architecture.md` — Layout: `project.rs` row becomes the `maven.rs` row
  (project model, closure resolution, source fetching, `FoldersDiscoverred`
  batches with origin); the "Rework in progress" note drops `project` from the
  built list; the warm-up/dependency rows and implementation notes describe the
  maven module and the `LibrarySource`-tagged index entries.
- `docs/dev/changelog.md` — the shipped entry (at implementation).
- `docs/requirements.md` — no change: R10 already states the behavior being
  implemented; v0.5's "references and rename stay workspace-only" is upheld by
  D5.

# Implementation plan

## Approach

Maven support is reintroduced as a directory module `src/maven/` with five
pieces that mirror the deprecated file boundaries, so each rework is a mechanical
port plus the new behaviour (settings.xml, `FoldersDiscoverred`-with-origin),
and each piece is a standalone sub-task with a disjoint write scope:

- `src/maven/settings.rs` — Maven user settings + local-repository resolution
  (new: `~/.m2/settings.xml` `localRepository`/`mirrors`/`offline`).
- `src/maven/resolve.rs` — effective poms and the offline dependency closure
  (rework of `src/deprecated/resolve.rs`, minus the old hub driver).
- `src/maven/model.rs` — pom discovery, modules, source roots, no-pom fallback
  (rework of `src/deprecated/project.rs`; supersedes the live `src/project.rs`).
- `src/maven/sources.rs` — sources-jar reuse/download/extract into the cache
  (rework of `src/deprecated/sources.rs`, extraction only — no direct index
  writes, no progress vocabulary).
- `src/maven/mod.rs` — the hub module: `spawn`, the two `FoldersDiscoverred`
  batches with origins, and the CLI/env gating (new).

Data flow: `ServerStarted` → `model::discover` over the settings-aware local
repository → `resolve::resolve_closure` per module → `sources::fetch_and_extract`
→ two `FoldersDiscoverred` batches (`Workspace`, then `LibrarySources`), picked
up by the index's existing serve loop, which tags scanned files via the
per-batch origin (`FileOrigin::Workspace` / `FileOrigin::LibrarySource`).

The sub-tasks form a dependency chain (settings → resolve → model → sources →
hub). The coordinator scaffolds `src/maven/` with compiling stub signatures
first, so the crate builds while the parallel agents fill their files; each
agent reworks exactly one deprecated file into exactly one new file, verifies
with `cargo test`, and does not touch any other module.

## Sub-tasks

Ordered by the dependency chain; the hub sub-task is last and closes the loop.

- [x] [Maven module: Maven settings and local repository](maven-module-settings.md)
- [x] [Maven module: effective poms and dependency closure](maven-module-resolve.md)
- [x] [Maven module: project model with pom-aware source roots](maven-module-model.md)
- [x] [Maven module: dependency sources fetch and extraction](maven-module-sources.md)
- [x] [Maven module: hub integration (spawn, FoldersDiscoverred origin, wiring)](maven-module-hub.md)