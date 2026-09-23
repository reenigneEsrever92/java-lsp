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
  unambiguous references) — including into a dependency's fetched sources —
  and query workspace symbols.
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
- **In-memory workspace index, with a disk cache for library sources.** The
  index itself is in memory and holds no storage; dependency *sources* are
  fetched from a Maven repository (Maven Central by default) on by default,
  written into the local Maven repository, and extracted under
  `$JAVA_LSP_SOURCES_CACHE` (default `~/.cache/java-lsp/sources`).
  `$JAVA_LSP_OFFLINE` disables all network work. Deployment: native binaries
  built with cargo. A second native binary (analysis daemon) is a possible
  future shape, not built now.
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
| R7 | Type-aware semantic engine: a pure-Rust type layer resolving declared types, members, and receivers | Functional | medium | v0.3 |
| R8 | Gradle project model | Functional | low | deferred |
| R9 | Inlay hints: variable types (including `var` inference), parameter names, and chained-call return types, computed for the requested range | Functional | medium | UC1, R7 |
| R10 | Dependency sources fetched and indexed; go-to-definition opens library declarations | Functional | medium | UC3 |

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
type-aware engine decision (R7) was explicitly pushed behind v0.2; it is
thereafter settled in v0.3.

**v0.3 — type-aware engine**: the server resolves declared types instead of
reasoning about names alone. A pure-Rust type layer (the engine chosen for R7 —
the javac-daemon option was rejected as reintroducing a Java codebase and
annotation-processor risk) models types, their members, and their hierarchies —
a source record's components modelled as the accessors a client calls, and
indexed at the header — and from it hover renders real declarations, completions
after `.` offer the receiver's members, one item per overload with its full
signature and a signature-help request that follows the cursor's argument,
library types carry real signatures and
inherited members
(`jvm-member-descriptors`), unresolved type names are reported as conservative
semantic diagnostics (R7), and find references and rename — R8's first slice,
`references-and-rename` — work for types, members, and file-local symbols,
refusing rather than guessing, and definition and find-references select a
call's overload from its argument types (arity when the types are
inconclusive). Full Java overload resolution, lambdas, and casts stay deferred;
so does Gradle.

**v0.4 — type-aware UX**: the type layer becomes something the developer sees
while reading, not only on demand. Inlay hints render inferred and declared
variable types (resolving `var` from its initializer), parameter names at call
sites, and the return types of intermediate method-chain links, scoped to the
visible range the client requests (R9; see `type-hints` in the backlog). The
`var` inference pulled forward for the hints also types `var` locals in the
scope that hover and completions read, from every supported initializer shape —
a conditional, array creation, `instanceof`, a `switch` expression, an
enhanced-for iterable, or a try-with-resources initializer — and an incomplete
`receiver.` at the end of a line keeps its receiver, so completing it offers
the same members as when the expression continues on the same line
(`dot-completion-and-var-inference`). Type arguments are inferred and
substituted for calls — a method's own type parameters from its arguments and
the receiver's from its own arguments — so `List.of(5)` renders `List<Integer>`
and `list.get(0)` its element type
(`generic-type-argument-inference`).

**v0.5 — library sources**: dependencies stop being opaque. Beyond the class
files it already indexes, the server fetches each resolved artifact's
`-sources.jar` from Maven Central (writing it into the local repository and
extracting it into a cache) and indexes the Java sources, so hover and
completions carry real signatures and parameter names and **go-to-definition
opens a library declaration** instead of answering nothing. On by default, with
`JAVA_LSP_OFFLINE` to opt out; an artifact without published sources or an
unreachable repository degrades to the class-file behavior (R10; see
`maven-source-indexing` in the backlog). `references` and `rename` stay
workspace-only — the cache is never searched or edited.

**Deferred**: Gradle project model support (what remains of R8) stays listed
here so a later change request can pick it up.
