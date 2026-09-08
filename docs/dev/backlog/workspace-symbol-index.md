---
type: ChangeRequest
kind: feature
title: Non-blocking workspace symbol index
description: A background, incrementally updated in-memory index of all Java declarations in the workspace.
state: proposed
priority: high
tags: [dev, index, performance]
owner: felix
---

# Problem

Completions and navigation need symbols from across the workspace (R4, R5).
Scanning per request would be O(project) and would block the request path —
directly violating the standing performance requirement R6: project open →
responsive, with the initial scan never blocking text sync or request handling.

# Proposal

A background indexer walks the workspace's `.java` files, extracts
declarations from the tree-sitter AST (classes, interfaces, enums, records,
methods, fields, imports), and maintains an in-memory index. Edits update only
the affected file's entries. Features served by the index report themselves
unavailable until warm-up completes — brief unavailability is agreed to be
acceptable; blocking is not.

# Decisions

- Pure Rust, in process — no semantic engine yet; this is the v0.1 engine.
- Declarations only, no type resolution — type-aware resolution belongs to the
  postponed engine decision (see `type-aware-engine`).
- Built off the request path (background task) so opening a project never
  waits for the scan.

# Acceptance criteria

- Index construction never delays document sync or any LSP request; syntax
  features stay responsive while a large workspace is scanned (verified with
  the `perf-benchmarks` harness).
- Edits update the affected file's symbols incrementally, not by re-scanning
  the workspace.
- Memory stays proportional to workspace size (no unbounded growth).
