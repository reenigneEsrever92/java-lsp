---
type: ChangeRequest
kind: feature
title: Maven source download and library navigation
description: Download dependency `-sources.jar` files on its own, index their sources, and let go-to-definition open library declarations.
state: done
priority: high
tags: [dev, maven, index, navigation, network]
owner: felix
verified:
  by: cargo test --all-targets (193 passed)
  at: 2026-09-23T20:27:02Z
---

# Problem

Dependencies are indexed from their `.class` files only, so a library symbol has
no readable declaration anywhere the editor can open. `String`, `List`, Gson's
`toJson`, Guava's `ImmutableList.of` — all of them complete, hover with
approximate signature, and then **cannot be navigated to**: `definition` and
`workspace_symbols` deliberately drop every entry flagged `dependency` because a
jar location is not openable (`src/engine/syntax.rs:658-692`, `:747-751`), and
`rename` refuses library targets. A Java developer expects to jump into a
dependency's source the way they jump into their own code.

The pieces to close the gap mostly exist. `jdk.rs::src_zip_entries`
(`src/jdk.rs:184-236`) already parses Java **sources** with tree-sitter, producing
entries with real ranges and `TypeInfo`s with real signatures and parameter names,
flagged `dependency: true`. `TypeModel::insert` already overlays a source-derived
type on a class-derived one by `(name, package, kind, nested)`
(`src/types.rs:415-428`). What is missing is getting the sources: the resolver is
strictly offline (`src/resolve.rs:1-7`) and reads `~/.m2/repository` only, and
nothing in the crate performs network I/O.

# Proposal

Teach the analyzer to fetch and index dependency sources, and let navigation
reach them:

1. **Fetch** — for every artifact in a module's resolved dependency closure,
   preferably reuse an existing `<a>-<v>-sources.jar` from the local repository;
   otherwise download it from a Maven repository (Maven Central by default) and
   write it into the local repository in Maven's own layout, so Maven and other
   IDEs reuse it. All of this happens on the background warm-up task, never on
   the request path (R6).
2. **Extract + index** — unpack each sources jar into a dedicated cache directory
   and index its `.java` entries through the same tree-sitter path the JDK
   `src.zip` uses (real ranges, real signatures, parameter names). Source-backed
   entries replace that artifact's class-file entries and class-derived types, so
   the index never holds two declarations of the same type.
3. **Navigate** — `definition` opens the extracted source file at the
   declaration, because its `file://` URI is one the editor can open; hover,
   `.`-completion, and inlay hints improve for free from the source-derived
   types. `references` and `rename` are unchanged for libraries: references
   does not resolve or search library symbols, rename keeps refusing them, and
   the cache files are never searched or edited.
4. **Degrade honestly** — the feature is on by default and opt-out. With no
   network, with `JAVA_LSP_OFFLINE=1`, or for an artifact that publishes no
   sources, the server behaves exactly as today: class-file entries, approximate
   signatures, and no library definition.

# Decisions

- **On by default, explicitly opt-out** — the request is that the analyzer fetch
  sources "on its own", so no configuration is needed to get navigation; the
  escape hatch is `JAVA_LSP_OFFLINE=1` (any non-empty value), which skips all
  network work and restores today's behavior exactly.
- **All artifacts in the closure** — direct, transitive, BOM-imported, and
  parent-inherited dependencies are all eligible; there is no direct-only or
  size cap. A classpath missing most of a starter's sources would be as useless
  for navigation as it was for completions (`maven-project-model`).
- **Local repository first, download only what is missing** — a
  `<a>-<v>-sources.jar` already in `$MAVEN_REPO`/`~/.m2/repository` is reused;
  only its absence triggers a fetch. Downloading writes the jar back into the
  same repository at Maven's standard path
  (`<repo>/<g as path>/<a>/<v>/<a>-<v>-sources.jar`), keeping the cache
  reusable by `mvn` and other tools.
- **Extract to a java-lsp cache** — unpacked sources live under
  `$JAVA_LSP_SOURCES_CACHE`, defaulting to
  `$XDG_CACHE_HOME/java-lsp/sources/<g>/<a>/<v>/` (`~/.cache/…` when unset), one
  real file per source, so a definition location is an ordinary openable
  `file://` URI. The cache is derived data: a stale or missing entry is
  re-extracted, and nothing there is ever written by the server.
- **Async HTTP with `reqwest`** — non-blocking, so downloads never stall the
  warm-up thread or the runtime; `reqwest` with `rustls-tls` and default
  features off (pure-Rust TLS, no OpenSSL). Bounded concurrency (a small
  semaphore) keeps the fan-out over a large closure from opening one socket per
  artifact. One new dependency for the checksum, `sha1`.
- **Repository URL is configurable** — default
  `https://repo1.maven.org/maven2`, overridable with
  `JAVA_LSP_MAVEN_CENTRAL_URL` for mirrors and private repositories. No
  credential handling and no `settings.xml` mirror parsing in v1: an artifact
  absent from the reachable repository is skipped with a warning, never
  retried into a failure.
- **Verify the published checksum when present** — the sibling `.sha1` is
  checked; a mismatch discards the download and warns rather than indexing
  corrupted bytes. A missing `.sha1` is not fatal.
- **Source entries replace class entries per artifact** — when an artifact's
  sources index, its class-file entries are dropped from the index and its
  source-derived `TypeInfo`s overlay the class-derived ones. Leaving both would
  make `definition` see two declarations of one type (a class-file entry at a
  zero range and a source entry at a real one) and, by the "no result beats a
  wrong result" rule, answer nothing. An artifact whose sources are unavailable
  keeps its class-file entries untouched.
- **A distinct "navigable" flag** — `SymbolEntry` keeps `dependency: true` for
  everything library-sourced (completions, `workspace/symbol`, and rename all
  treat it as a library), and gains a flag that marks entries whose location is
  an openable extracted `.java` file. `definition` admits a library entry only
  when that flag is set; `WorkspaceIndex::source_files()` — the candidate set
  for references and rename (`src/engine/syntax.rs:436`) — excludes all
  library-sourced URIs, so a references search or a rename can never read or
  rewrite the cache.
- **Navigation scope is definition, not references or rename** — references
  keeps searching workspace sources only and does not resolve library symbols
  (there is nothing useful to search in a third-party library's own sources),
  and rename keeps refusing library targets because the cache is not
  writable-back. Hover, completions, and inlay hints improve as a side effect of
  the source-derived types; `workspace/symbol` stays workspace-only.
- **Readiness flips before downloads** — warm-up indexes the workspace and the
  class-file jars, sets `ready`, and only then downloads and indexes sources,
  upgrading the index in place and logging a second line
  (`library sources indexed`). The existing `workspace index warm-up complete`
  line keeps its meaning so the `perf-benchmarks` baseline stays comparable, and
  first open stays as responsive as it is today.
- **Tests and the bench stay hermetic** — the downloader is exercised against a
  local HTTP fixture (a tiny in-process server over a synthetic repository),
  never the real network, and the suite's default is downloads disabled
  (`JAVA_LSP_OFFLINE=1` set under the existing `jdk::env_lock`), so no test
  depends on Maven Central. The committed `example/` (with its Gson dependency)
  must not trigger a fetch during `cargo test`.
- **Supersedes three earlier decisions** — "Offline, local repository only"
  (`maven-project-model`), "jars feed completions only" (`maven-project-model`,
  `jdk-standard-library`), and requirements.md's "in-memory index, no storage,
  no external services" are all changed by this request; the docs steps below
  record the change rather than leaving a contradiction.
- **Maven only** — Gradle stays out of scope exactly as in
  `maven-project-model`.

# Acceptance criteria

- On a Maven workspace whose dependency sources are not cached, with the network
  reachable, the server downloads `<a>-<v>-sources.jar` into the local
  repository in Maven's layout, extracts it under the cache directory, and
  indexes it — entirely on the background task, with requests answered
  throughout (R6).
- A dependency served from sources carries **real** signatures and parameter
  names: hover renders the declared member and `.`-completion shows real
  parameters (the `jvm-member-descriptors` fidelity, now for dependencies).
- Go to definition on a type or member of such a dependency returns a location
  inside the extracted cache that the editor opens on the declaration.
- Find-references and rename never return or edit a location under the cache;
  rename still refuses a library target.
- With `JAVA_LSP_OFFLINE=1`, nothing is fetched and every feature behaves
  exactly as it does today (class-file entries, approximate signatures, no
  library definition).
- An artifact with no published sources, or one whose download or checksum
  fails, keeps its class-file entries and logs a warning; no feature breaks and
  no error surfaces to the client.
- The existing warm-up log line is not delayed by downloads, and the bench
  records the download pass's cost separately from first-open responsiveness.
- `cargo test` passes with no network access; the downloader's own tests drive a
  local HTTP fixture.

# Docs touched

- `docs/requirements.md` — technology choices (network fetch for sources with an
  offline opt-out; a disk cache for extracted sources), a new requirement for
  library-source navigation, and a **v0.5 — library sources** milestone.
- `docs/architecture.md` — the layout tree (`src/sources.rs`), the Maven
  project-model bullet (fetch / extract / index, the repository and cache
  locations, the env vars, degradation), the `WorkspaceIndex` and `TreeSitterEngine`
  bullets (the navigable flag, the `source_files` exclusion, readiness flipping
  before downloads), the data-flow diagram, and retirement of the "jars feed
  completions only" limitation sentence.
- `docs/dev/changelog.md` — the shipped entry (written by `fawi-implement`).

# Implementation plan

## Approach

One new module (`src/sources.rs`), two new dependencies (`reqwest` with
`rustls-tls` and default features off, and `sha1`), one field on `SymbolEntry`,
and a split of the warm-up into a sync core plus an async wrapper. The request
path never changes (R6): downloads run on the runtime, extraction and parsing on
the blocking pool.

- **`src/sources.rs` — fetch, extract, index.** Env helpers: `enabled()`
  (`JAVA_LSP_OFFLINE` unset or empty), `base_url()` (`JAVA_LSP_MAVEN_CENTRAL_URL`,
  default `https://repo1.maven.org/maven2`, trailing slash trimmed), `cache_dir()`
  (`JAVA_LSP_SOURCES_CACHE`, else `$XDG_CACHE_HOME/java-lsp/sources`, else
  `~/.cache/java-lsp/sources`). Path helpers mirror `Resolver`: the class jar and
  its sibling `<a>-<v>-sources.jar` under the repository root
  (`pub(crate) index::local_repository()`). `index_sources(index, artifacts)` is
  the async entry: skip when disabled or empty; for each artifact reuse an
existing sources jar, otherwise download it with a bounded
  (`Semaphore` + `JoinSet`) fan-out — GET the jar, verify the sibling `.sha1`
  when present, write to a temp file and rename into the repository. It returns
  the artifacts whose sources are now on disk, then `index_extracted` (on
  `spawn_blocking`) reads each sources jar with
  `classfile::for_each_zip_entry`, writes every `.java` entry under the cache
  (skipping a file it cannot write), parses it with the shared tree-sitter
  parser, and produces entries (via `index::extract_entries`, then
  `dependency = true`, `library_source = true`, attributed to the extracted
  `file://` URI) and types (`types::collect_type_infos` per
  `types::file_package`). For each artifact it first `remove_file`s the class
  jar's URI, then upserts the source files and overlays the types, and finally
  re-sets the type model and logs `library sources indexed`.
- **`SymbolEntry::library_source`** (`src/index.rs`) — `false` everywhere except
  extracted dependency sources. `definition` admits an entry when
  `!dependency || library_source` (`src/engine/syntax.rs`); every other
  navigation path is unchanged, and `WorkspaceIndex::source_files()` drops files
  whose entries are library-sourced, so references and rename can never read or
  rewrite the cache.
- **Warm-up split** (`src/index.rs`, `src/engine/syntax.rs`) — `scan_workspace`
  keeps its signature (the inline, no-runtime path tests use) and delegates to a
  new `scan_workspace_core`, which does today's work and returns the resolved
  artifact list, setting `ready` exactly as before so the `workspace index
  warm-up complete` line keeps its meaning. A new `scan_workspace_async` runs
  the core on `spawn_blocking` and then `sources::index_sources`; the engine
  spawns it on the runtime and falls back to the sync path with no runtime.
- **Testing** — `src/sources.rs` unit tests drive a tiny in-process HTTP server
  (`std::net::TcpListener`) over a synthetic repository, so no test touches the
  real network: download + extract + index (entry is `library_source`, its URI
  is under the cache), `.sha1` present and matching, `.sha1` absent (tolerated),
  `.sha1` mismatching (discarded), and the offline no-op. Engine tests cover the
  `definition` predicate and the `source_files` exclusion. The suite's existing
  tests run with `JAVA_LSP_OFFLINE=1` under `jdk::env_lock` so the committed
  `example/`'s Gson dependency is never fetched.

## Steps

- [x] Add `reqwest` (rustls-tls, no default features) and `sha1` to
      `Cargo.toml`. (Groundwork for the fetch step)
- [x] Add `SymbolEntry::library_source` (`src/index.rs`) with every existing
      constructor — workspace `entry`, the import entry, `classfile::class_entries`,
      and the test literals — setting it `false`; make `source_files()` exclude
      library-sourced files. (AC: references/rename never touch the cache)
- [x] Implement `src/sources.rs` (env helpers, path helpers, the async bounded
      downloader with SHA-1 verification and atomic write into the repository,
      and `index_extracted`), register it in `src/lib.rs`, and add the local-HTTP
      unit tests. (AC: fetch, checksum, degrade, no network in tests)
- [x] Split the warm-up (`src/index.rs`): `scan_workspace_core` returns the
      artifact list and keeps `ready`'s current timing; `scan_workspace_async`
      runs the core then the source pass; wire `TreeSitterEngine::set_workspace_root`
      to the async variant with the inline fallback. (AC: replace class entries,
      readiness before downloads, R6)
- [x] Admit library-source entries in `definition` (`src/engine/syntax.rs`) and
      add the engine tests (a `library_source` entry resolves, a plain jar entry
      does not, a library-sourced file is absent from `source_files`). (AC:
      definition navigates; references/rename unchanged)
- [x] Run `cargo test --all-targets` with the offline default and confirm no
      test reaches the network. (AC: hermetic suite)
- [x] Update `docs/requirements.md`: technology choices (network fetch with an
      offline opt-out, a disk cache), a new requirement for library-source
      navigation, and the **v0.5 — library sources** milestone.
- [x] Update `docs/architecture.md`: `src/sources.rs` in the layout tree, the
      project-model bullet (fetch/extract/index, repository and cache locations,
      env vars, degradation), the index/engine bullets (the `library_source`
      flag, the `source_files` exclusion, readiness before downloads), the
      data-flow diagram, and retirement of the "jars feed completions only"
      limitation.
- [x] Append the changelog entry and mark the request done with its
      `verified` block.
