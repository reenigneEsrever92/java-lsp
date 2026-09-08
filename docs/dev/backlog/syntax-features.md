---
type: ChangeRequest
kind: feature
title: Syntax features via tree-sitter
description: Document symbols, folding ranges, semantic tokens, and parse-error diagnostics from tree-sitter-java.
state: proposed
priority: high
tags: [dev, syntax, tree-sitter]
owner: felix
---

# Problem

The server must deliver immediate value on file open, independent of any
semantic engine (R3, UC1, UC4). Parse errors are the cheapest real diagnostics
available without a type checker.

# Proposal

Integrate `tree-sitter` with `tree-sitter-java`. Keep one incremental syntax
tree per open document, reused across requests. Implement document symbols,
folding ranges, semantic tokens, and publish parse errors as diagnostics on
open and on change.

# Decisions

- `tree-sitter-java` over a hand-written parser — mature, incremental, and
  error-recovering; a lossless custom parser is deferred until the pure-Rust
  semantic work demands one.
- Per-document trees only; no workspace-wide reparsing on a single edit.

# Acceptance criteria

- Opening a Java file produces correct document symbols, folding ranges, and
  semantic tokens for a representative sample file.
- Parse errors appear as diagnostics and clear when fixed.
- Edits update the affected document's features without reparsing the
  workspace.
