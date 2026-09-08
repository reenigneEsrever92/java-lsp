---
type: Changelog
title: Changelog
description: Shipped changes, newest first.
tags: [dev, changelog]
status: draft
---

## 2026-09-09

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
