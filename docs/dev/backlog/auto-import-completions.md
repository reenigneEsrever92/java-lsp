---
type: ChangeRequest
kind: feature
title: Auto-import on completion accept
description: Completion items from other packages/modules/jars carry an additionalTextEdit adding the import.
state: done
priority: high
tags: [dev, completions, imports]
owner: felix
verified:
  by: cargo test (86 passed: import-edit unit tests, extended Maven integration test)
  at: 2026-09-09T21:20:14Z
---

# Problem

Completions now offer symbols from other packages, other modules, and
dependency jars — but as bare simple names. Accepting `getName` from a jar or
`Util` from another module inserts an identifier the file cannot resolve
until the user hand-writes the import, which is exactly the busywork typing
support exists to remove. The index already knows where every symbol lives;
the missing piece is its package, plus an import edit attached to the item.

# Proposal

Record each indexed symbol's package so its fully qualified name is known at
completion time, then attach an `additionalTextEdits` import to completion
items that need it:

- **Index model**: source entries store the owning file's `package`
  declaration (one per file, extracted during the tree walk); jar entries
  carry the artifact class's internal name (`com/example/lib/Lib`), from
  which the dotted FQCN of the type — and of its members — follows directly.
- **Completion items**: every item sourced from the index whose symbol lives
  outside the current file's package gets `additionalTextEdits` inserting
  `import <fqcn>;` on its own line after the last existing import (else
  after the `package` statement, else at the top of the file). Members
  import their enclosing type's FQCN. The item's `insertText` stays the
  simple name — only the extra edit is added.
- **Never worsen**: no import edit when the symbol is in the same package,
  lives in the same file, is already imported explicitly, is covered by an
  existing `.*` wildcard import of its package, or when its simple name is
  already imported from a different package (a conflict — the insertion
  alone stays honest; the user resolves it).

# Decisions

- **Completion-time only** — `additionalTextEdits` needs no new client
  capability; a `codeAction` "add import" quick-fix for already-unresolved
  references is deliberately deferred to a later CR.
- **Workspace symbols across packages and modules are first-class** — the
  user explicitly wants cross-module imports covered, not just dependency
  jars; both flow through the same FQCN machinery.
- **FQCNs are recorded at index time** (package per source file, internal
  name per jar class) rather than recomputed per completion request.
- **Never worsen** — the server must not introduce a compile error: a
  conflicting simple name suppresses the edit entirely.
- **`.*` wildcard imports count** — an existing `import <pkg>.*;` covering
  the symbol's package suppresses the edit (a redundant explicit import
  would be noise); a `static` import does not make a type nameable.

# Acceptance criteria

- Accepting a dependency type or member completion in a file that lacks the
  import adds the correct `import` line at the correct position; the
  inserted name itself is unchanged.
- Accepting a workspace symbol declared in another package or module adds
  its import the same way.
- No import edit is produced for keywords, locals, same-file symbols,
  same-package symbols, already-imported names, names covered by a matching
  wildcard import, or names conflicting with an existing import from a
  different package.
- Works identically for dependency-jar and workspace-source entries; no new
  client capability is required.
- `docs/architecture.md` documents the FQCN index-model extension and the
  completion pipeline change.

# Implementation plan

## Approach

Two layers: the index learns each symbol's package, and the completion
pipeline turns FQCNs into `additionalTextEdits`. All changes stay in
`src/index.rs`, `src/classfile.rs`, and `src/engine/syntax.rs` (plus tests
and docs) — the shell is untouched.

- **Index model** (`src/index.rs`): `SymbolEntry` gains
  `package: Option<String>`. `extract_entries` first locates the file's
  `package_declaration` in the tree (root declarations only, dotted name,
  `None` for the default package) and stamps it on every entry from that
  file, imports included. `classfile::class_entries` derives the package
  from the class's internal name (`com/example/lib/Lib` → `com.example.lib`).
- **FQCN and import target**: for a type entry, the imported type is
  `pkg + container chain + name`; for a member entry it is
  `pkg + container chain` (the container chain already ends with the
  enclosing type's simple name for both source and jar entries). Entries
  without a package (default package) get no edit — default-package types
  cannot be imported.
- **Completion pipeline** (`src/engine/syntax.rs`): computed once per
  request from the open document's tree — the file's own package, and its
  imports as `(path, is_static, is_wildcard)` triples. For each index-sourced
  item (branch 3 of the pipeline; keywords and locals never carry edits),
  the import edit is attached only when ALL hold: the entry is not from the
  current file (`uri` differs), the entry's package differs from the file's
  package, no existing non-static import equals the type FQCN, no existing
  `pkg.*` wildcard covers the entry's package, no existing import (or
  same-package declaration found via `query_name`) already claims the type's
  simple name under a different FQCN, and the entry has a package. The edit
  is a `TextEdit` inserting `import <fqcn>;\n` at character 0 of the line
  after the last import — else after the `package` statement — else at line
  0. `insertText`/`filterText`/`sortText` stay exactly as they are.
- **Known simplification**: the conflict check covers explicit imports and
  same-package declarations; a same-named type imported via another file's
  wildcard (rare) is not detected.

**Docs touched**: `docs/architecture.md` — the `WorkspaceIndex` bullet
  (package field on entries, jar internal names retained) and the
  completions paragraph of the `TreeSitterEngine` bullet (import-edit rule,
  never-worsen policy).

## Steps

- [x] Extend `src/index.rs`: `SymbolEntry.package`, package extraction from
      the tree's `package_declaration` in `extract_entries`, and the field
      stamped in both entry constructors; unit tests for default-package,
      nested packages, and jar-entry packages via `classfile`. (Groundwork:
      FQCNs available at completion time)
- [x] Extend `src/classfile.rs::class_entries` to derive and stamp the
      dotted package from the internal name; extend its unit tests to assert
      the package on class and member entries. (AC: jar entries carry FQCNs)
- [x] Implement the import-edit pipeline in `src/engine/syntax.rs`:
      per-request file package + import list, the never-worsen rules, insert
      position after last import / package / top, and
      `additional_text_edits` on index-sourced completion items; unit tests
      covering the add case (all three positions) and every no-edit case
      (same file, same package, explicit import, wildcard, conflict, default
      package). (AC: edits added correctly; no-edit cases)
- [x] Extend the Maven integration test in `tests/harness.rs`: the fixture
      gains a second package (`com.other.Other`), a completion for it
      asserts the `import com.other.Other;` edit, and the dependency
      completion asserts `import com.example.lib.Lib;` with the correct
      insert position. (AC: workspace cross-package + jar entries)
- [x] Update `docs/architecture.md` per the doc note above. (Doc step —
      keeps architecture in step with the pipeline)
