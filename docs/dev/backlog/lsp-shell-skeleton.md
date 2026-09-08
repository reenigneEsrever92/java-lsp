---
type: ChangeRequest
kind: feature
title: LSP shell skeleton
description: A tower-lsp server over stdio with lifecycle, incremental text sync, and the SemanticEngine trait seam.
state: proposed
priority: high
tags: [dev, lsp, shell]
owner: felix
---

# Problem

Nothing exists yet. Every other feature — syntax or semantic — needs a server
that speaks LSP over stdio, tracks document state, and separates the editor
facing layer from whatever analysis backend comes later (R1, R2).

# Proposal

Create the cargo crate and the server skeleton: `tower-lsp` over stdio with
initialize/shutdown lifecycle, incremental `didOpen`/`didChange`/`didClose`
handling storing versioned documents, and `tracing` logging. Define the
`SemanticEngine` trait (open/change/diagnostics/hover/definition/completions)
and a `SyntaxOnlyEngine` stub that returns empty results, so every later
backend slots in without touching the LSP layer.

# Decisions

- Rust with `tower-lsp`, over `lsp-server` (sync loop) — ergonomics first;
  revisit if it gets in the way of cancellation or backpressure control
  (recorded in `requirements.md`).
- stdio transport only — the universal mechanism every LSP client supports.
- The trait seam exists from the first commit — the architecture principle
  agreed during project init.

# Acceptance criteria

- The server starts as a stdio binary and completes the LSP handshake with
  Zed, Neovim, and VS Code.
- Incremental `didChange` events are applied to a versioned document store.
- Hover/definition/completions/diagnostics respond (with empty results) from
  the `SyntaxOnlyEngine` stub.
- Graceful shutdown on `shutdown`/`exit`.
- A test harness can drive the server without a real editor.
