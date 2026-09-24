---
type: Changelog
title: Changelog
description: Shipped changes, newest first.
tags: [dev, changelog]
status: draft
---

Entries that say "Verified by N tests" quote the whole suite's size at that
point (the cumulative `cargo test` total), not the number of tests covering the
named change.

## 2026-09-24

- **Create-symbol quick fixes** — unresolved symbols now offer full create
  actions: class / interface / enum / record (a scaffolded file), and a method,
  field, or local variable with the signature inferred from the usage
  (parameter types from the call's arguments, the return type from the
  assignment/declaration/`return` context), including on a workspace receiver's
  type. Verified by `cargo test --all-targets` (229 tests). See
  [Create-symbol quick fixes for unresolved symbols](backlog/create-stub-quick-fixes.md).

- **Unresolved-symbol diagnostics and quick fixes** — the server now reports
  unresolved types, members, bare identifiers, and imports as `ERROR`
  diagnostics (gated on a clean parse and an indexed `java.lang`, disabled by
  `JAVA_LSP_SEMANTIC_DIAGNOSTICS=0`), and serves `textDocument/codeAction`
  (`codeActionProvider`, kind `quickfix`) offering "Add import", a did-you-mean
  rename, and create-stub actions — the create-type fix a `CreateFile` resource
  operation, withheld unless the client advertises it. Verified by
  `cargo test --all-targets` (225 tests). See
  [Unresolved-symbol diagnostics and quick fixes](backlog/unresolved-symbol-diagnostics.md).

## 2026-09-23

- **Float literals and parameter hints honour overloads** — a floating literal
  is now typed by its suffix (`5.0f` is `float`, `5.0` is `double`), so
  `data.test(5.0f)` selects the `float` (or widening `double`) overload instead
  of falling back to the first same-arity one; and inlay parameter hints select
  the callee from the argument types, emitting no hint rather than a wrong name
  when the overload cannot be pinned down. Verified by
  `cargo test --all-targets` (213 tests). See
  [Float literals and parameter hints ignore overload resolution](backlog/float-literals-and-overload-hints.md).

- **Overload-aware completions and signature help** — method completions list
  each overload of a method as its own item, labelled with the full signature and
  inserting `name(`, in both `.`-member completion and the workspace-index
  source; the shell advertises `signatureHelpProvider` and serves
  `textDocument/signatureHelp` from the type layer, marking the argument the
  cursor is in as `activeParameter`. Verified by `cargo test --all-targets`
  (203 tests). See
  [Overload-aware completions and signature help](backlog/overload-completions.md).

- **Argument-aware overload resolution for navigation** — go-to-definition and
  find-references now select the overload a call targets from its argument types
  (arity when the types are inconclusive): a new `assignable` relation in the
  type layer plus `member_for_arguments` narrow a callee's overloads, and a
  reference is attributed only when its call accepts the selected overload's
  parameters. `rename` stays name-group-wide. Verified by
  `cargo test --all-targets` (208 tests). See
  [Argument-type overload resolution for navigation](backlog/overload-navigation.md).

- **Warm-up and source-fetch progress** — the server now reports the background
  warm-up to the client: one work-done progress item (`window/workDoneProgress/
  create` + `$/progress`) titled `java-lsp` whose message names each phase with
  counts (`Indexed N source files`, `Indexed N dependency jars`, `Indexed N JDK
  classes`, `Fetching N dependency sources` with a rising percentage), plus a
  single `window/showMessage` notice when `JAVA_LSP_OFFLINE` disables source
  fetching on a workspace that has dependencies. Gated on the client's
  `window.workDoneProgress` capability; emitted through a new `Reporter` and the
  `Progress`/`Message` engine events. Verified by `cargo test --all-targets`
  (198 tests). See
  [Warm-up and source-fetch progress reporting](backlog/warmup-progress-reporting.md).

- **Message-based engine boundary** — the shell and the engine now meet at a
  `tokio` channel boundary (`src/engine.rs`: `Command`, `EngineEvent`,
  `EngineHandle`, and a dispatcher task) instead of a read-locked
  `Box<dyn SemanticEngine>` trait. Mutations are applied inline in arrival
  order while read-only queries run on spawned tasks, and diagnostics arrive as
  `EngineEvent`s the shell's drain task publishes. The `SemanticEngine` trait,
  `SyntaxOnlyEngine`, and `src/engine/` are gone; the analysis core moved to
  `src/analysis.rs` unchanged. Verified by `cargo test --all-targets` (193
  tests). See [Message-based engine boundary](backlog/message-based-engine.md).

- **Maven dependency sources** — the server now fetches each resolved
  dependency's `<a>-<v>-sources.jar` from a Maven repository (Maven Central by
  default; `JAVA_LSP_MAVEN_CENTRAL_URL` overrides, `JAVA_LSP_OFFLINE=1` opts
  out), writes it into the local Maven repository, extracts it under
  `JAVA_LSP_SOURCES_CACHE` (default `~/.cache/java-lsp/sources`), and indexes
  the Java sources — so hover and `.`-completion carry real signatures and
  parameter names and **go-to-definition opens the extracted source file**. A
  new `library_source` entry flag admits those declarations to `definition`
  while `WorkspaceIndex::source_files` excludes the cache, so references and
  rename never read or edit it. Verified by `cargo test --all-targets` (193
  tests). See
  [Maven source download and library navigation](backlog/maven-source-indexing.md).

## 2026-09-22

- **Dot receiver recovery and wider `var` inference** — an incomplete
  `receiver.` at the end of a line now keeps its receiver (the dot can parse
  into the next token, so `gson.` before a `var` line read `gson.var` as a
  scoped type identifier and completed to nothing), and `var` bindings infer
  from more initializer shapes: an enhanced-for iterable's element type, a
  try-with-resources binding, and a conditional, array creation, `instanceof`,
  or `switch` expression. Inherited `java.lang.Object` methods now resolve for
  typing, hover, and `var` inference without rejoining `.`-completion listings.
  Verified by `cargo test --all-targets` (184 tests). See
  [A dot at line end loses its receiver, and `var` locals frequently infer no type](backlog/dot-completion-and-var-inference.md).

- **Record components as accessors** — a source record's header components are
  now modelled and indexed. `.`-completion on a record value offers each
  component as its accessor (`x()`), narrowed by the typed prefix, and a record
  with no components adds nothing; hover renders the accessor's signature and,
  on the declaration, the component list (`record Point(int x, int y)`); and
  go-to-definition, references, rename, and `workspace/symbol` target the
  component at its position in the header, while the bare component name
  resolves inside the record. Verified by `cargo test --all-targets` (167
  tests). See
  [Record components are not modelled, so member completion on a record is empty](backlog/record-members.md).

- **Type-aware review follow-ups** — fixes the defects found reviewing the
  type-aware work. References and rename now identify a member's declaring type
  by simple name *and* package, so renaming `a.Widget.run` no longer touches
  `b.Widget.run`; they report a declaration only when `include_declaration`
  asks, target nothing on a non-terminal import segment, count a
  fully-qualified use as visibility, and refuse (rather than drop a reference)
  when a candidate file cannot be read or parsed. A qualified supertype and a
  nested class-file type (`Map$Entry`) now resolve, and an `implements` list is
  diagnosed once per unresolved name instead of twice. A call whose argument
  count matches no overload gets no parameter hint, hints never fall outside
  the requested range, `rename` rejects restricted identifiers
  (`var`/`record`/`yield`), the harness no longer prints debug output, and the
  committed `example/` is back to valid Java with no build artifacts in the
  change. Verified by `cargo test --all-targets` (157 tests); bench with a real
  JDK indexed (Temurin 25, `java-lsp-bench --files 5`, release): warm-up ≈ 6.2 s,
  peak RSS ≈ 197 MB, warm-up hover RTT ≤ 1.1 ms, post-warm-up hover RTT 1.2 ms.
  See [Type-aware review follow-ups](backlog/type-aware-review-followups.md).

- **Generic type-argument inference** — `receiver_type` now binds and substitutes
  type arguments for calls: a method's own type parameters from its argument
  types (`List.of(5)` renders `List<Integer>`, primitives boxed) and the
  receiver's parameters from its own arguments (`List<String> l; l.get(0)`
  renders `String`), with fields substituted the same way. Calls pick their
  overload by arity (a new `member_for_call`), which also gives parameter hints
  the right names, and literals now carry a type. A call that cannot be pinned
  down keeps the written form; erased class-file signatures stay as they are.
  Verified by 141 tests and against a real JDK (`: List<Integer>`,
  `: String`). See
  [Generic type-argument inference](backlog/generic-type-argument-inference.md).
- **Separate completions for same-named symbols** — workspace-index completions
  no longer collapse every symbol that shares a simple name into one item. The
  dedupe key is now the insert text plus the import the item carries, so typing
  `Li` offers both `class of java.awt` and `interface of java.util`, each with
  its own `import` edit, while duplicate entries for one symbol and a local
  shadowing a same-named field still collapse. Previously the surviving item was
  arbitrary — accepting `List` inserted `import java.awt.List;`. Verified by 137
  tests. See
  [Separate completions for same-named symbols](backlog/same-name-completions.md).
- **Import-aware type-name resolution** — `resolve_name` now resolves a simple
  type name through the file's imports (an exact single-type import, then the
  package, then a wildcard import, then a unique match) and returns a
  package-qualified reference; `TypeLookup::lookup`/`members`/`member_owner`
  honour that qualifier, and `receiver_type` qualifies a local's or field's
  declared type name. This fixes names shared across packages (a real JDK indexes
  both `java.util.List` and `java.awt.List`), so hover, `.`-completions, and
  inlay hints work for them — `var x = List.of(3);` now hints. Verified by 136
  tests. See
  [Import-aware type-name resolution](backlog/import-aware-type-resolution.md).
- **Type and parameter inlay hints** — the shell advertises `inlayHintProvider`
  and `SemanticEngine` gains `inlay_hints` (defaulting to none), answered from
  the open document's tree plus the type model for the client's requested range
  only. Three families: variable type hints (a `var` local's initializer is
  inferred through `receiver_type`, which also types `var` locals in the scope
  hover and completions read), parameter-name hints at call sites, and the
  return types of intermediate method-chain links. `Member` now carries
  parameter names for source-declared methods, so hover signatures include them
  while class-file members stay name-less and produce no parameter hint.
  Verified by 128 tests. See
  [Type and parameter inlay hints](backlog/type-hints.md).
- **Find references and rename** — `SemanticEngine` gains `references` and
  `rename` (defaulting to an empty list and `None`), and the shell advertises
  `referencesProvider`/`renameProvider`. Resolution reuses the type layer —
  including a member's declaring type from the receiver's type — then searches
  the workspace's `.java` files (substring-prefiltered, parsed on demand) for
  occurrences it can attribute with confidence: types only in files that can
  see them, member accesses only where the receiver resolves the name to the
  same declaring type, unqualified names only inside the declaring type's span,
  and locals only when their method declares the name once. `rename` refuses
  with `null` for an unresolved or ambiguous target, a library declaration, or
  an invalid identifier. Verified by 120
  tests. See [Find references and rename](backlog/references-and-rename.md).
- **Library type signatures from JVM descriptors** — `classfile.rs` now parses
  each member's descriptor into a type (so a jar/JDK type carries
  `String getName()`, not a bare name) and records the superclass and
  interfaces (skipping the implicit `java.lang.Object`), and the JDK's
  `lib/src.zip` path feeds the model through `collect_type_infos`; library
  receivers therefore offer inherited members exactly like workspace types, and
  each jar is read once via the new `jar_outputs`. Costs ~14 MB more peak RSS
  with a JDK indexed (192 MB vs 178 MB) and ~0.8 s more warm-up. Verified by 113
  tests. See [JVM member descriptors](backlog/jvm-member-descriptors.md).
- **Type-aware engine (pure Rust)** — a new `src/types.rs` models declared
  types, members, and hierarchies, built during warm-up from the source trees
  (full signatures) over name-only types synthesized from the indexed jars and
  JDK. It now backs `TreeSitterEngine::hover` (previously hardcoded `None`, now
  rendering a member's signature, a type declaration, or a local's type),
  member completions after `.` (the receiver's inferred type, inherited members
  included, still empty for an uninferrable receiver), and conservative
  `type X cannot be resolved` warnings that never fire without an indexed JDK or
  for imported, same-package, or type-parameter names. `SemanticEngine` and the
  LSP shell are unchanged. Costs ~13 MB more peak RSS with a JDK indexed
  (178 MB vs 165 MB). Verified by 109 tests; the bench's post-warm-up hover
  probe now asserts resolved content. See
  [Type-aware semantic engine](backlog/type-aware-engine.md).

## 2026-09-09

- **Standard library indexing** — the installed JDK's `java.*`/`javax.*`
  declarations are indexed during warm-up from `jmods` class files,
  `lib/src.zip` sources (tree-sitter parsed), or `rt.jar`, discovered via
  `$JAVA_LSP_JDK`/`$JAVA_HOME`/SDKMAN and read with a streaming ZIP walk;
  JDK types appear in completions with auto-import edits (none for
  `java.lang`), stay out of navigation, and a missing JDK is a graceful
  no-op. Real-JDK baseline (Temurin 25 via src.zip, ~4.2k files): warm-up
  5.5 s, peak RSS 165 MB, hover RTT during warm-up still ≤ 1.6 ms. Verified
  by 92 tests. See [Standard library indexing](backlog/jdk-standard-library.md).
- **Auto-import on completion accept** — the index now records each symbol's
  package (from the source file's `package_declaration` or the jar class's
  internal name), and completion items from other packages, modules, or jars
  carry an `additionalTextEdits` adding the `import` line (after the last
  import, else after the package statement, else at the top); never-worsen
  rules suppress the edit when the name already resolves or would conflict.
  Verified by 86 tests. See
  [Auto-import on completion accept](backlog/auto-import-completions.md).
- **Maven project model and dependency indexing** — the warm-up now builds a
  Maven model from statically parsed `pom.xml` files (multi-module, `<build>`
  directory overrides) and scans exactly the declared source roots, then
  resolves each module's full dependency closure offline (parent chains,
  property interpolation, `dependencyManagement` with import-scoped BOMs,
  transitive walk with nearest-wins mediation, exclusions, optionals) from
  `$MAVEN_REPO`/`~/.m2` and indexes the jars' class files via a minimal
  reader; dependency types are offered in completions while definition and
  `workspace/symbol` stay source-only. Verified by 79 tests; the bench's new
  `--maven` mode keeps all five features under 1 ms during a 500-file scan.
  See [Maven project model and dependency indexing](backlog/maven-project-model.md).

## 2026-09-08

- **Performance benchmark harness** — `src/bin/java-lsp-bench.rs` generates a
  configurable fixture Java workspace, drives the real `java-lsp` binary over
  stdio JSON-RPC, and reports per-feature first-response time, hover RTT
  during index warm-up (detected via the server's readiness log line), and
  peak RSS (`VmHWM`); v0.1 baseline at `--files 500 --methods-per-class 10`
  on aarch64 / 8 cores: first response 0.6–0.7 ms from didOpen (≤1.6 ms from
  process start) for all five features, warm-up 116 ms (5 hover samples,
  max 0.1 ms), post-warm-up RTT 0.1 ms, peak RSS 11.8 MB. See
  [Performance benchmark harness](backlog/perf-benchmarks.md).
- **Go-to-definition and workspace symbols** — `TreeSitterEngine::definition`
  now resolves the word under the cursor through the workspace index (import
  targets via the dotted path's last segment, declaration-name lookups
  narrowed by the node kind at the cursor, same-file overload collapse,
  `null` on multi-URI ambiguity) and the shell serves `workspace/symbol`
  from a new `SemanticEngine::workspace_symbols` prefix query with
  `workspaceSymbolProvider` advertised. Verified by 52 tests. See
  [Navigation v1](backlog/navigation-v1.md).
- **Type-free completions** — `TreeSitterEngine::completions` now merges three
  ranked sources (`sort_text` `0`/`1`/`2`): Java keywords, names in scope from
  the open document's tree (parameters, locals, enclosing fields), and
  workspace index symbols (members labeled `Container.name`, inserted under
  their simple name); results are partial rather than blocked during index
  warm-up, and a completion directly after `.` returns an empty list. See
  [Completions v1](backlog/completions-v1.md).
- **Workspace symbol index** — `TreeSitterEngine` now owns a `WorkspaceIndex`
  (`src/index.rs`): a background scan of the workspace's `*.java` files
  extracts declarations and imports into in-memory entries, edits update only
  the edited file's entries, and `index_ready()` gates index-backed features
  during warm-up without ever blocking the request path. Verified by 31
  tests. See [Non-blocking workspace symbol index](backlog/workspace-symbol-index.md).
- **Syntax features via tree-sitter** — `TreeSitterEngine` now parses each
  open document with tree-sitter-java and publishes parse-error diagnostics
  (which clear on fix), hierarchical document symbols, folding ranges, and
  semantic tokens behind the `SemanticEngine` seam; the shell advertises the
  three new capabilities. Verified by 22 tests. See
  [Syntax features via tree-sitter](backlog/syntax-features.md).
- **Zed extension** — `zed-java-lsp/` provides Java support in Zed around
  java-lsp: tree-sitter-java grammar, the Java language definition (queries
  vendored from `zed-extensions/java`), and the java-lsp binary as language
  server, resolved from settings override, `$PATH`, or the repository's
  `target/` builds. See [Zed extension](zed-java-lsp/README.md) and
  [LSP shell skeleton](backlog/lsp-shell-skeleton.md).
- **LSP shell skeleton** — `java-lsp` now builds a tower-lsp server over
  stdio with incremental versioned text sync, `SemanticEngine`/`SyntaxOnlyEngine`
  seam, and a `LspService` test harness; verified with 12 unit/integration
  tests and a stdio JSON-RPC smoke test. See
  [LSP shell skeleton](backlog/lsp-shell-skeleton.md).
