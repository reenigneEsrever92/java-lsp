---
type: ChangeRequest
kind: feature
title: Maven project model and dependency indexing
description: Static pom.xml parsing for source roots/modules plus local-repo jar indexing feeding completions.
state: done
priority: high
tags: [dev, maven, project-model, index]
owner: felix
verified:
  by: cargo test (79 passed) + java-lsp-bench --maven (500 files, all features < 1 ms during warm-up)
  at: 2026-09-09T18:37:07Z
---

# Problem

The server treats the workspace as an unstructured pile of `.java` files: the
scan walks everything under the root (including `target/` and generated
trees), there is no notion of modules or source roots, and external
dependencies are invisible — completions and navigation only see symbols whose
source happens to be in the workspace (R8, first slice). A Java developer
working on a Maven project expects the server to know its source roots,
modules, and classpath.

# Proposal

Add a Maven project model, built entirely by static parsing (the build tool is
never invoked):

1. **Source roots and modules** — detect `pom.xml` files under the workspace,
   parse them (XML), and compute each module's source roots
   (`src/main/java`/`src/test/java`, honoring `<build>` directory overrides),
   recursing into `<modules>`. The workspace scan, the index, and the
   close-policy ("disk truth wins") operate on declared roots instead of the
   whole root directory.
2. **Dependency jar indexing** — resolve each module's full dependency
   closure by static evaluation of the Maven model: parent POM chains,
   `<properties>` interpolation, `<dependencyManagement>` (including
   `import`-scoped BOMs), direct and transitive dependencies (via each
   artifact's POM in the local repository), Maven's conflict mediation
   (nearest wins, first declaration wins at equal depth), exclusions, and
   optionals. Artifacts come from the local repository (`~/.m2/repository`)
   only — nothing is ever downloaded or fetched. Index the resolved jars'
   `.class` files with a minimal class-file reader (constant pool, access
   flags, fields, methods). Jar declarations feed completions; navigation
   targets stay workspace sources. Missing artifacts are skipped with a
   warning, pruning that resolution branch.

Jar indexing runs off the request path like the workspace scan (R6).

# Decisions

- **Maven only**; Gradle is deliberately excluded for now (a follow-up CR can
  add it) — one build system done honestly beats two done approximately.
- **Static parsing only, never invoke `mvn`** — keeps the server JVM-free and
  startup instant. Consequence: exotic configurations (roots added by
  plugins, profile-activated directories) are not seen; the standard
  directory layout plus `<build>` overrides covers the overwhelming majority.
- **Full dependency resolution from day one** — direct, transitive,
  BOM-imported, and parent-inherited dependencies are all resolved, because a
  classpath missing most of a starter's jars makes completions useless in
  real projects. This means implementing Maven's model subset properly:
  parent chains, property interpolation, `dependencyManagement`/BOM imports,
  nearest-wins mediation, exclusions, optionals — the CR's largest single
  piece, phased inside the implementation (source roots land first).
- **Offline, local repository only** — resolution reads `~/.m2/repository`
  (path overridable) and never touches the network; a developer who built the
  project once has everything cached, and anyone else gets a graceful
  degradation (missing artifacts pruned with warnings), not a broken server.
- **Dependency scopes ignored in v1** — every resolved artifact is indexed
  regardless of scope (test-scope types offered in main sources is accepted
  noise for now).
- **Jars feed completions only** — go-to-definition and `workspace/symbol`
  stay workspace-source-only in v1: editors cannot open jar-declared
  locations, and claiming them would violate "no result beats a wrong
  result".
- **Minimal class-file reader, hand-rolled** — declarations (types, methods,
  fields) need only the constant pool and member tables; std-only except
  DEFLATE for jar entries (`flate2` with its pure-Rust backend — the one new
  dependency, plus `roxmltree` (zero-dependency) for `pom.xml` parsing).
- **No file watching** (existing limitation) — `pom.xml` changes apply on
  restart; jar re-indexing likewise.
- **This CR opens milestone v0.2** in `docs/requirements.md`; the
  type-aware-engine evaluation (R7) is explicitly pushed behind it.

# Acceptance criteria

- A Maven project's scan covers exactly the declared source roots of all
  detected modules (multi-module included); `target/` and non-root trees are
  not indexed; overridden `<sourceDirectory>`/`<testSourceDirectory>` are
  honored.
- The full declared dependency closure resolves statically: parent-inherited
  dependencies, `dependencyManagement`-managed versions, `import`-scoped
  BOMs, and transitive dependencies are all indexed from their jars, with
  conflicts mediated like Maven does (nearest wins) and exclusions/optionals
  honored.
- A declared dependency's type — including one pulled in only transitively
  or via a BOM — appears in completions in a module that declares it.
  Unresolvable artifacts are skipped with a warning and every feature still
  works; nothing is ever fetched from the network.
- The build tool is never invoked; the server binary stays JVM-free.
- Project open stays responsive (R6): jar indexing never blocks the request
  path, verified with the `perf-benchmarks` harness on a Maven fixture.
- `docs/requirements.md` records the v0.2 milestone; `docs/architecture.md`
  documents the project-model component and data flow.

# Implementation plan

## Approach

Three new modules in the single crate, zero new runtime threads; two new
dependencies (`roxmltree` for `pom.xml`, `flate2` with its pure-Rust backend
for jar DEFLATE). All Maven work runs inside the background task the engine
already spawns — the request path never changes (R6).

- **`src/project.rs` — model and roots**: walks the workspace root for
  `pom.xml` files (hidden dirs skipped, like the scanner). Each raw pom
  (roxmltree) yields coordinates, parent reference, properties,
  `dependencyManagement`, `dependencies`, `<modules>`, and `<build>`
  directories. Output is a `ProjectModel { modules: Vec<Module> }` where each
  `Module { root_dir, source_roots: Vec<PathBuf>, pom: EffectivePom }`. If no
  `pom.xml` exists anywhere, the model falls back to today's behaviour
  (whole-root walk) — non-Maven projects keep working.
- **`src/resolve.rs` — effective poms and dependency closure**: two layers.
  *Effective model*: a pom is materialized by loading its parent (from
  `relativePath` if it exists on disk, else the local repo), interpolating
  `${...}` against the merged property map (plus the built-ins
  `project.groupId/version/artifactId`, `project.parent.*`, `pom.*`), and
  merging parent-first (child wins for properties and management; parent
  `dependencies` and build directories are inherited). *Resolution*: for each
  module, direct dependencies get versions from the module's effective
  `dependencyManagement` — including `import`-scoped BOMs, whose own
  effective depMgmt merges in (direct management beats imported; first
  imported wins) — then a breadth-first transitive walk loads each artifact's
  effective pom from the local repo and repeats. Mediation is Maven's:
  nearest depth wins, first declaration wins at equal depth; exclusions
  accumulate along the traversal path; `optional` and `test`-scoped
  transitive deps are pruned. Unresolvable properties resolve to the literal
  string and the artifact is then missing → pruned with a warning. Depth cap
  (64) and a visited set on (group, artifact, version, role) guard cycles,
  including parent cycles. Repo layout is the standard
  `{repo}/{g/a/p}/{artifact}/{version}/...-.{pom,jar}`; `packaging=pom`
  artifacts contribute management/parents but no jar. `-SNAPSHOT` files are
  used as found on disk.
- **`src/classfile.rs` — jars and class files**: a minimal ZIP reader
  (EOCD scan, central directory, local headers; STORED passthrough, DEFLATE
  via flate2) and a minimal class-file parser — magic/versions, full constant
  pool (all tags, double-width entries), access flags, `this_class`, then the
  field and method tables (name + flags; attributes skipped). Extraction per
  class: internal name → kind (interface flag, enum flag, superclass
  `java/lang/Record` → record, else class), nested-name path → container
  chain, declared methods and fields → `SymbolEntry`s. Members keep name only
  (descriptors are a later refinement).
- **Index flow**: each resolved jar's entries are `upsert_file(jar_uri,
  entries)` into the existing `WorkspaceIndex` — completions and prefix
  queries pick them up unchanged. `SymbolEntry` gains a `dependency: bool`
  field (false for source entries): `definition` and `workspace_symbols`
  filter dependency entries out (jar locations are not openable — "no result
  beats a wrong result"), completions include them. The close-policy
  ("disk truth wins") applies to files under any source root, not just under
  the raw workspace root; the engine holds the `ProjectModel` for that check.
- **Sequencing inside the background task**: build model → scan source roots
  (or fallback root) → resolve dependency closure per module → index jars →
  flip ready. A huge dependency tree delays `index_ready` — accepted: the
  flag means "everything indexed", and warm-up requests are already proven
  non-blocking (bench).
- **Testing without a repo**: resolution tests build a synthetic local
  repository in a temp dir (hand-written poms) covering inheritance,
  interpolation, depMgmt, BOM import, transitivity, mediation, exclusions,
  optionals, and cycles. Jar/class tests construct class bytes programmatically
  and zip them with a STORED-entry test helper (hand-rolled CRC32 — std has
  no zip writer). The bench gains a `--maven` fixture mode (generated poms
  and source roots, no external deps) so the R6 check runs on a Maven layout.

**Docs touched**: `docs/architecture.md` — layout tree (`project.rs`,
`resolve.rs`, `classfile.rs`), component paragraphs (model, resolver with its
mediation/offline policy, class-file reader), data-flow diagram (engine →
model → scan + jars → index), and the completions/navigation bullets updated
for dependency entries.

## Steps

- [x] Add `roxmltree` and `flate2` to `Cargo.toml`; implement `src/classfile.rs`
      (ZIP central-directory reader + STORED/DEFLATE entry decoding + minimal
      class-file parser producing kind, container chain, methods, fields) with
      unit tests using hand-built class bytes and a STORED-zip test helper
      (CRC32). (Groundwork for AC2 — jars become parseable)
- [x] Implement `src/resolve.rs`: raw pom parsing (roxmltree), effective-model
      assembly (parent chain from `relativePath` or local repo, property
      interpolation with built-ins, parent-first merge), and dependency-
      management application (module depMgmt, import-scoped BOMs, version
      defaults); unit tests with synthetic poms for inheritance,
      interpolation, and BOM import. (AC: parent-inherited deps and
      depMgmt-managed versions resolve)
- [x] Implement the transitive closure in `src/resolve.rs`: breadth-first
      walk over artifact poms from the local repo with nearest-wins/first-
      declared mediation, path-scoped exclusions, optional/test pruning,
      depth cap and cycle guards, missing-artifact warnings and branch
      pruning; unit tests covering each rule plus a cycle. (AC: transitive
      closure resolves; conflicts mediated like Maven)
- [x] Implement `src/project.rs`: pom discovery, `ProjectModel`/`Module`
      construction with source roots (defaults + `<build>` overrides,
      `<modules>` recursion) and the no-pom fallback to the whole-root walk;
      unit tests on a generated multi-module tree including a directory
      override. (AC: scan covers exactly the declared roots; overrides
      honored)
- [x] Wire the engine (`src/engine/syntax.rs`, `src/index.rs`): the background
      task builds the model, scans source roots, resolves each module's
      closure, and indexes resolved jars (entries marked `dependency: true`
      — the new `SymbolEntry` field); `definition` and `workspace_symbols`
      filter dependency entries, completions keep them; the close-policy
      re-index check uses source roots. Integration tests in
      `tests/harness.rs`: a Maven fixture with a dependency jar in a
      synthetic local repo — fixture source roots scanned (`target/`
      excluded), the dependency's type offered in completions, definition
      and `workspace/symbol` still return no jar locations. (AC1–3; R6 —
      all off the request path)
- [x] Extend `src/bin/java-lsp-bench.rs` with a `--maven` fixture mode
      (generated poms, modules, and source roots, no external dependencies);
      run the bench on it and record the numbers in the changelog baseline
      bullet's context. (AC: project open stays responsive on a Maven
      fixture, verified with the harness)
- [x] Update `docs/architecture.md`: the three new modules in the layout
      tree, component paragraphs (model + fallback, resolver with offline/
      mediation policy and known gaps — no plugin-added roots, no profile
      activation, literal-property degradation —, class-file reader scope),
      the data-flow diagram (engine → model → scan + jars → index), and the
      completions/navigation bullets for dependency entries. (Doc step —
      keeps architecture in step with the new components)
