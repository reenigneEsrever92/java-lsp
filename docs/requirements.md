---
type: Requirements
title: Requirements
description: Users, use cases, technology choices, and the numbered requirement list for java-lsp.
tags: [requirements, users, performance]
status: draft
---

## Users

- **Java developer** (primary) — edits Java projects in any LSP-capable editor
  (Zed, Neovim, VS Code, Helix, Emacs); mid-to-expert technical level. Cares
  about: being able to work the instant a project opens, low memory footprint,
  no JVM install or runtime, and features that are correct for what they claim
  to do. No secondary users were identified.

## Use cases

All use cases belong to the Java developer.

- **UC1 — Open a project and edit immediately.** Given a workspace containing
  Java files, when the server starts and a file is opened, then syntax-level
  features (document symbols, folding, semantic tokens, parse-error
  diagnostics) work without waiting for any full-workspace scan to finish.
- **UC2 — Get typing support.** When completing inside a Java file, receive
  language keywords, locals in scope, and symbols from the workspace index.
- **UC3 — Navigate the workspace.** Jump to declarations via
  go-to-definition backed by the symbol index (declarations, imports, and
  unambiguous references), and query workspace symbols.
- **UC4 — See syntax diagnostics.** When a file has parse errors, they are
  reported as diagnostics on open and on edit.

## Technology choices

- **Rust** — for performance and the no-JVM-at-runtime constraint. Alternative
  considered: a Java-based server; rejected because a JVM runtime is precisely
  what this project exists to avoid.
- **`tower-lsp`** — async LSP framework with server-side types. Alternative
  considered: `lsp-server` (synchronous loop, as used by rust-analyzer);
  rejected for now in favor of tower-lsp's ergonomics, revisitable if it gets
  in the way of cancellation or backpressure control.
- **`tree-sitter` + `tree-sitter-java`** — syntax layer: incremental parsing
  with error recovery. Alternative considered: a hand-written parser; deferred
  until the pure-Rust semantic work needs a lossless tree.
- **In-memory workspace index, no storage, no external services.** Deployment:
  native binaries built with cargo. A second native binary (analysis daemon) is
  a possible future shape, not built now.
- **Constraints**: no JVM at runtime; performance — especially project
  open → responsive — is a standing general concern (no fixed numeric targets
  were agreed); stdio transport only, so any LSP client works.

## Requirements

| ID | Requirement | Type | Priority | Traced to |
|----|-------------|------|----------|-----------|
| R1 | LSP server over stdio usable from any LSP client | Functional | high | UC1 |
| R2 | Lifecycle and incremental text sync with versioned documents | Functional | high | UC1 |
| R3 | Syntax features from tree-sitter: document symbols, folding ranges, semantic tokens, parse-error diagnostics | Functional | high | UC1, UC4 |
| R4 | Completions without type resolution: keywords, locals in scope, workspace index symbols | Functional | high | UC2 |
| R5 | Workspace symbol index built without blocking the request path; go-to-definition and workspace symbols backed by it | Functional | high | UC3 |
| R6 | Project open → responsive: the initial scan never blocks text sync or request handling; individual features may be briefly unavailable while warming up | Non-functional | high | UC1 |
| R7 | Full type-aware semantic engine (javac daemon via GraalVM Native Image vs. pure-Rust type checker — decision deliberately postponed) | Functional | medium | deferred |
| R8 | Find references, rename, type-aware hover, Maven/Gradle project models | Functional | low | deferred |

## Milestones

**v0.1 — usable syntax server** (shipped): R1–R6. A Java developer can open
a project in any editor and immediately get symbols, folding, highlighting,
parse errors, and typing/navigation support from the workspace index — all
pure Rust, no semantic engine. R6 was treated as a design constraint from day
one, verified by the benchmark harness (`perf-benchmarks`).

**v0.2 — project model**: the server understands the shape of a Java project
instead of treating it as a file pile — Maven source roots and modules
statically parsed from `pom.xml`, and the full dependency closure (direct,
transitive, BOM-imported, parent-inherited) resolved offline from the local
repository and indexed from their jars so external types show up in
completions (R8, first slice; see `maven-project-model` in the backlog). The
type-aware engine decision (R7) is explicitly pushed behind v0.2: its CR
stays `proposed` until this milestone lands.

**Deferred**: R7 (the engine decision) and the rest of R8 (find references,
rename, type-aware hover, Gradle, transitive dependency resolution) stay
listed here so later change requests can pick them up.
