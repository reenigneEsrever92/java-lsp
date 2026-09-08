---
type: ChangeRequest
kind: feature
title: Navigation v1
description: Go-to-definition and workspace symbols backed by the workspace index.
state: done
priority: medium
tags: [dev, navigation]
owner: felix
verified:
  by: cargo test (52 passed)
  at: 2026-09-08T19:49:46Z
---

# Problem

A Java developer needs to jump to declarations (R5, UC3). Without type
resolution, navigation can still be accurate for declarations, imports, and
unambiguous references.

# Proposal

Implement go-to-definition on top of the workspace index: definitions in the
same file, targets of import statements, and same-name declaration matches
across the workspace. Implement `workspace/symbol` as a query over the index.
Ambiguous references resolve to a best candidate or return no result — never a
silently wrong location.

# Decisions

- Honest limitation: references that require type inference (e.g.
  `x.foo()` on an inferred receiver) may be unresolved in v1; accurate
  type-aware navigation belongs to the postponed engine decision (see
  `type-aware-engine`).
- Ambiguity policy: no result beats a wrong result.

# Acceptance criteria

- Go-to-definition works for class names, import targets, and method/field
  declarations reachable through the index.
- `workspace/symbol` finds types and members by prefix across the workspace.
- Ambiguous cases return no location rather than a wrong one, and the
  limitation is documented.

# Implementation plan

## Approach

Everything needed already exists: `WorkspaceIndex` stores every declaration
and import with `name`/`kind`/`container`/`full_range`/`selection_range` per
URI, `query_name` answers exact-name lookups, `query_prefix` answers prefix
lookups, and scanned files are indexed with the same shape of entries as open
documents (the engine's `open`/`change` upsert the open file's entries, and
`scan_workspace` upserts every `*.java` on disk once — so definition over a
file that was never opened works through the exact same index entries). The
work is therefore confined to three places:

1. **`TreeSitterEngine::definition`** (`src/engine/syntax.rs`, currently
   returns `None`). Resolution pipeline, in order, all synchronous:
   - Lock `documents`; if the URI is not open, return `None` (definition
     requests only arrive for open documents).
   - Convert the LSP position to a byte offset with the existing
     `byte_offset`, and extract the **full identifier under the cursor** (a
     new `word_at` helper that expands in both directions over
     `[A-Za-z0-9_$]`, complementing the backward-only `word_prefix`).
   - **Import-qualified case**: if the tree (`descendant_for_byte_range` +
     ancestor walk) places the cursor inside an `import_declaration`, take the
     dotted path from the statement text (same slicing as `import_name`); use
     the **last segment** as the simple name (an empty segment from a `.*`
     wildcard yields `None`) and resolve it via `query_name` against
     declaration kinds only (Class/Interface/Enum/Record — or Method/Field for
     `static` imports). A unique hit returns its `selection_range` `Location`;
     zero or several distinct-file hits return `None`. JDK/library imports
     (`java.util.List`) are simply not indexed, so they honestly resolve to
     `None`.
   - **Identifier case** for a plain word:
     1. `query_name(word)` filtered to declaration kinds (Import entries are
        never navigation targets).
     2. *Best-candidate narrowing* (this is how "best candidate or no result"
        is made concrete): (a) filter by the node kind at the cursor — a
        `type_identifier` restricts to type kinds, a method-invocation or
        field-access name restricts to Method/Field, no constraint otherwise;
        (b) if several candidates remain and all share one URI (method
        overloads, same-file repeats), return the first by position — that is
        a correct answer, not a guess; (c) anything still spread across
        multiple URIs returns `None`. That is the ambiguity policy: no result
        beats a wrong result.
   - A cursor sitting on the declaration itself naturally resolves to its own
     `selection_range` through the same path. Keywords, empty words, and
     unindexed names fall out as `None`.
   No gating on `index_ready()`, mirroring completions: a warm-up-in-progress
   query contributes whatever is indexed so far — partial, never blocking (R6).
2. **`workspace_symbols` on the `SemanticEngine` trait** (`src/engine/mod.rs`):
   `fn workspace_symbols(&self, query: &str) -> Vec<SymbolInformation>` with a
   default empty `Vec` so `SyntaxOnlyEngine` stays the minimal conformance
   reference untouched. `TreeSitterEngine` implements it as
   `index.query_prefix(query)` mapped to `SymbolInformation` — `name`, a new
   `IndexKind` → `SymbolKind` mapping (sibling of the existing
   `completion_kind`), `Location { uri, range: selection_range }`, and
   `container_name` from the last container entry. Import entries are excluded
   (they are not workspace symbols). An empty query returns everything
   indexed, ordered as `query_prefix` orders it; the client does the filtering.
3. **Shell wiring** (`src/server.rs`): advertise
   `workspace_symbol_provider: Some(OneOf::Left(true))` in `initialize`, and
   add the `symbol` handler dispatching straight through to the engine, like
   every other query handler.

The honest type-inference limitation stays as decided: `x.foo()` on an
inferred receiver is a plain word lookup for `foo` in v1 — it may hit a same-
or cross-file declaration by name or return `None`; nothing is silently
invented.

## Steps

- [x] 1. Add a `word_at(text, offset) -> &str` helper next to `word_prefix`
      in `src/engine/syntax.rs` (expands the identifier run in both
      directions from the cursor) plus a unit test alongside the existing
      `word_prefix` tests. *Supports AC1 (word extraction for definition).*
- [x] 2. Implement `TreeSitterEngine::definition` per the resolution pipeline
      above (import-qualified → kind-filtered `query_name` → best-candidate
      narrowing → `None` on multi-URI ambiguity), with unit tests in the
      `syntax.rs` test module: same-file class/method/field target; import
      target declared in another `engine.open`-ed file (opening a document
      upserts its index entries, so no scan is needed); unique cross-file
      class reference; ambiguous same-name in two files → `None`; JDK import
      → `None`; cursor on the declaration itself → its own range.
      *AC1 + AC3.*
- [x] 3. Add `workspace_symbols(&self, query: &str) -> Vec<SymbolInformation>`
      to the `SemanticEngine` trait in `src/engine/mod.rs` with an empty
      default, and implement it in `TreeSitterEngine` via `query_prefix` with
      an `IndexKind` → `SymbolKind` mapping and `container_name`; unit tests
      for prefix hits, kind mapping, container name, and no Import entries.
      *AC2.*
- [x] 4. Wire the shell in `src/server.rs`: advertise
      `workspace_symbol_provider` in `initialize` and add the `symbol`
      handler delegating to the engine; extend the
      `initialize_advertises_...` harness assertion with
      `capabilities["workspaceSymbolProvider"] == true`. *AC2.*
- [x] 5. Add integration tests to `tests/harness.rs` following the
      `workspace_index_...` fixture pattern (temp-dir fixture project,
      `LspService::new` **with the ClientSocket drained** in a spawned task,
      `wait_for_index`): `textDocument/definition` resolving a type usage in
      an open file to a scanned fixture file's `selection_range`, an import
      target, an ambiguous reference returning `null`, and
      `workspace/symbol` returning prefix matches for a scanned file without
      it ever being opened. *AC1 + AC2 + AC3.*
- [x] 6. Update `docs/architecture.md`: extend the `SemanticEngine` bullet
      with `workspace_symbols`; replace the `definition` stub sentence in the
      `TreeSitterEngine` bullet with the resolution order and the
      best-candidate/`None` ambiguity policy; change the `WorkspaceIndex`
      bullet's "navigation arrives with the navigation CR" note to describe
      `query_name`/`query_prefix` as the definition and workspace-symbol
      backends; and record the documented v1 limitations (no type-aware
      receiver resolution — see `type-aware-engine` — JDK/library imports and
      scanned-file ranges reflect the last disk scan, not a live watcher).
      *AC3 (limitation documented).*
- [x] 7. Run `cargo test` and confirm the full suite passes with the new
      unit and integration tests. *Verifies all acceptance criteria.*
