---
type: ChangeRequest
kind: feature
title: Syntax features via tree-sitter
description: Document symbols, folding ranges, semantic tokens, and parse-error diagnostics from tree-sitter-java.
state: done
priority: high
tags: [dev, syntax, tree-sitter]
owner: felix
verified:
  by: cargo test (15 unit + 6 harness + 1 stdio smoke, all passing)
  at: 2026-09-08T16:30:00Z
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

# Implementation plan

## Approach

New `TreeSitterEngine` implements `SemanticEngine`, replacing
`SyntaxOnlyEngine` in the shell. The engine module becomes a directory:

```
src/engine/
  mod.rs    — the SemanticEngine trait (extended with the three query methods)
  stub.rs   — SyntaxOnlyEngine, kept for trait-conformance tests
  syntax.rs — TreeSitterEngine: parsing + the four feature extractors
```

- **Dependencies**: `tree-sitter = "0.27"`, `tree-sitter-java = "0.23"`.
- **Tree cache, not incremental InputEdit parsing**: the engine keeps one
  `Tree` per open `Url` (`HashMap<Url, Tree>`), reused by every query. On
  `open`/`change` it reparses the full document (`parser.parse(bytes, None)`).
  The request's decisions and acceptance criteria only require per-document
  trees with no workspace-wide reparse; forwarding edit ranges to drive
  `tree.edit` + incremental reparse is a later optimization (the shell does
  not forward ranges today). Full-file parses of single files are
  microseconds — the R6 responsiveness concern is unaffected.
- **Positions**: tree-sitter `Point`s carry byte columns; LSP wants UTF-16.
  The engine converts via a shared helper (inverse of the store's
  `position_to_offset`) that walks the line's UTF-8 chars and counts UTF-16
  units.
- **Parse-error diagnostics**: if `root.has_error()`, walk the tree collecting
  `ERROR` and missing nodes into diagnostics (`ERROR` → Error severity;
  missing nodes get a synthetic range) with the message from the node kind.
  Returns an empty vec when the tree is clean — republishing on every change
  clears fixed errors.
- **Document symbols**: hierarchical `DocumentSymbol[]` from declaration
  nodes: `class_declaration` → Class, `interface_declaration` → Interface,
  `enum_declaration` → Enum, `method_declaration`/`constructor_declaration`
  → Method (via the `name` field), `field_declaration` → Field (from
  `variable_declarator` children). Children recurse, so nesting mirrors the
  source. Driven by a `TreeCursor`, no workspace knowledge.
- **Folding ranges**: `FoldingRange[]` for declaration bodies and `block`
  nodes that span more than one line; range covers the node minus one line
  (LSP fold conventions).
- **Semantic tokens**: flat `SemanticTokens` (delta-encoded) over a curated
  node-kind mapping — declaration names (method → `method`, class → `class`,
  interface → `interface`, enum → `enum`, enum constant → `enumMember`,
  field → `property`, parameter → `parameter`, local variable → `variable`),
  `type_identifier` → `type`, plus `string_literal`, comment, and numeric
  literal tokens. The legend lists exactly the types emitted.
- **Shell wiring**: `server.rs` adds the `document_symbol_provider`,
  `folding_range_provider`, and `semantic_tokens_provider` capabilities and
  `document_symbol`/`folding_range`/`semantic_tokens_full` handlers that read
  the engine under the existing `RwLock`. The engine constructor is where the
  tree-sitter language gets registered once.

**Docs touched**: `docs/architecture.md` (engine internals: `TreeSitterEngine`,
the per-document tree cache, the full-reparse-per-change tradeoff) and the
backlog index. No API/CLI docs exist.

## Steps

- [x] Add `tree-sitter = "0.27"` and `tree-sitter-java = "0.23"` to
      `Cargo.toml`; restructure `src/engine.rs` into `src/engine/{mod,stub,
      syntax}.rs`, extend the `SemanticEngine` trait with `document_symbols`,
      `folding_ranges`, and `semantic_tokens` (all `&Url -> Option<...>`, LSP
      types out), and keep `SyntaxOnlyEngine` answering empty; shell
      constructors now build a `TreeSitterEngine`. (AC: seam supports the
      new features)
- [x] Implement parsing and per-document tree cache in `syntax.rs`:
      `open`/`change` parse the full text into a stored `Tree`, `close` drops
      it; unit tests for cache lifecycle (parse error-free file, edit,
      close). (AC: edits update only the affected document)
- [x] Implement byte→UTF-16 position conversion and parse-error diagnostics
      in `syntax.rs`; unit tests: broken file yields `ERROR`-node diagnostics,
      fixing the text yields an empty set. (AC: parse errors appear and
      clear)
- [x] Implement `document_symbols` and `folding_ranges` in `syntax.rs`; unit
      tests on a representative sample file (nested class/method/field
      hierarchy, fold ranges match brace blocks). (AC: correct symbols and
      folding)
- [x] Implement `semantic_tokens` in `syntax.rs` with the curated kind
      mapping and delta-encoded output; unit test asserting expected token
      types/positions on the sample file. (AC: correct semantic tokens)
- [x] Wire the three capabilities + handlers into `src/server.rs` and extend
      `tests/harness.rs`: `documentSymbol`, `foldingRange`, and
      `semanticTokens/full` requests return the expected results for an
      opened file; diagnostics notification reflects a parse error and its
      fix. (AC: features reachable from a real client)
- [x] Update `docs/architecture.md` engine section (TreeSitterEngine, tree
      cache, full-reparse tradeoff) and note the syntax backend in the Zed
      integration section. (Doc step)
- [ ] Manual verification (left to the user): `zed: restart language server` in Zed on
      `Hello.java` — symbols in the outline panel, folds on class/method
      bodies, parse-error squiggle when the file is broken; record in
      `docs/log.md`. (AC: opening a Java file produces the features)
