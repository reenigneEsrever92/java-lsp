---
type: ChangeRequest
kind: feature
title: Completions v1
description: Type-free completions from keywords, locals in scope, and workspace index symbols.
state: done
priority: high
tags: [dev, completions]
owner: felix
verified:
  by: cargo test (39 passed)
  at: 2026-09-08T19:29:46Z
---

# Problem

Typing support is the most-wanted day-to-day feature, and v0.1 can deliver a
useful version without any type resolution (R4, UC2).

# Proposal

Implement completions as a mix of three sources, ranked in that order: language
keywords; locals and parameters in scope, extracted from the open document's
syntax tree; and workspace symbols from the index (types, and members shown
with their enclosing type as a prefix). Results carry correct `CompletionItem`
kinds and insert text.

# Decisions

- No member-access resolution in v1 — completions after `x.` are out of scope
  until a type-aware engine exists (recorded as deferred in
  `requirements.md`); they are the part that genuinely needs javac or a Rust
  type checker.
- Sources are deliberately labeled "type-free" so user expectations match
  reality.

# Acceptance criteria

- Completion inside a method body offers keywords, in-scope locals/parameters,
  and matching workspace symbols.
- Results arrive quickly on the `perf-benchmarks` fixture project and never
  block on index warm-up (empty or partial while warming, per R6).
- No wrong-membership claims: the server never implies a member belongs to
  the type before the dot.

# Implementation plan

## Approach

All changes land in `src/engine/syntax.rs` (`TreeSitterEngine::completions`,
which currently returns `None`) plus integration tests in `tests/harness.rs`;
the shell needs no changes — the completion capability and `.` trigger are
already advertised, and the handler already dispatches through the seam.

- **Position → prefix**: add a `byte_offset(text, position)` helper (the
  inverse of the existing `lsp_position`, UTF-16 aware, clamped to the text).
  The word prefix is the run of identifier characters `[A-Za-z0-9_$]` ending
  at the cursor byte offset. An empty prefix returns `None` (no suggestions
  from nothing typed); a prefix directly preceded by `.` returns an empty
  result — member access after `x.` is out of scope until a type-aware
  engine exists, and an empty list claims nothing (AC3).
- **Three ranked sources** (per the proposal), encoded with `sort_text`
  prefixes `0`/`1`/`2` because clients re-sort alphabetically:
  1. **Keywords** — a static list of Java keywords plus `true`/`false`/`null`,
     `CompletionItemKind::KEYWORD`.
  2. **Locals in scope** — from the open document's tree: walk ancestors from
     the node at the cursor; the innermost enclosing method/constructor
     declaration contributes its `formal_parameter` names (kind VARIABLE,
     detail "parameter") and `variable_declarator` names (VARIABLE, detail
     "local"); the enclosing type declaration contributes its
     `field_declaration` names (FIELD, detail "field of <Type>") — fields are
     nameable unqualified inside the type, so this claims no membership the
     code doesn't already grant. Locals declared later in the method are
     included (a known, harmless v1 simplification).
  3. **Workspace symbols** — `WorkspaceIndex::query_prefix(prefix)` on the
     simple name; `IndexKind` maps to Class/Interface/Enum/Struct (record)/
     Method/Field item kinds; members (method/field) get a label of
     `<Container>.<name>` with `insert_text`/`filter_text` set to the simple
     name so typing and inserting stay unqualified; types insert their name.
     `Import` entries are not nameable and are skipped. Queries take only a
     brief read lock, so while warm-up is still running the source simply
     contributes whatever is indexed so far — partial, never blocking (AC2).
- **Dedup**: a workspace symbol whose simple name is already offered by an
  earlier (higher-ranked) source is skipped, so the list never shows the same
  insert text twice.
- **Response**: `CompletionResponse::Array`, each item carrying label, kind,
  detail, `insert_text`, `filter_text`, and the ranking `sort_text`.

**Docs touched**: `docs/architecture.md` — extend the `TreeSitterEngine`
paragraph with the completions pipeline (sources, ranking, the `.`
limitation) so the documented behaviour matches the implementation.

## Steps

- [x] Add the prefix machinery in `src/engine/syntax.rs`: `byte_offset`
      (UTF-16 position → byte offset, clamped), word-prefix extraction over
      identifier characters, the static Java keyword list, and a guard for
      prefixes preceded by `.`; unit tests for offset conversion (multi-line,
      non-ASCII), prefix extraction, and the `.` guard. (Groundwork for AC1,
      AC3)
- [x] Implement in-scope extraction: from the node at the cursor walk parents
      to the innermost method/constructor (params + locals) and enclosing
      type (fields); unit tests on a sample with a nested type and a method
      body asserting names, details, and kinds. (AC1 — locals/params source)
- [x] Assemble `completions`: merge the three sources in rank order with
      `sort_text`, map `IndexKind` to `CompletionItemKind`, label members
      with their container, set insert/filter text, dedupe by simple name,
      and return `CompletionResponse::Array`; unit tests covering the ranked
      merge, member labeling, dedup, and that a `.` prefix yields an empty
      list. (AC1, AC2, AC3)
- [x] Integration tests in `tests/harness.rs`: initialize with a fixture
      `rootUri`, open a document, and assert `textDocument/completion` inside
      a method body offers a keyword, a local, and a fixture workspace symbol
      (with container-prefixed label and simple insert text); assert the same
      request with no workspace root still serves keywords and locals (never
      blocks on warm-up — AC2), and that a completion after `.` returns an
      empty result (AC3).
- [x] Update `docs/architecture.md`: extend the `TreeSitterEngine` bullet with
      the completions pipeline — three ranked sources, `sort_text` ranking,
      warm-up partialness, and the explicit no-member-access limitation.
      (Doc step — keeps architecture in step with the implementation)
