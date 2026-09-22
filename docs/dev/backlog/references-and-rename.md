---
type: ChangeRequest
kind: feature
title: Find references and rename
description: Workspace-wide references and a safe rename for types, members, and file-local symbols, behind new trait methods and shell capabilities.
state: done
priority: medium
tags: [dev, navigation, refactor, types]
owner: felix
verified:
  by: cargo test (120 passed - 98 lib, 6 bench bin, 15 harness, 1 stdio)
  at: 2026-09-22T17:55:52Z
---

# Problem

The server can navigate *to* a declaration but cannot answer the reverse
question. R8 lists find-references and rename, and they are the remaining gap
between "a good syntax server" and "a server you can refactor with": renaming a
class or method by hand is exactly the multi-file, easy-to-get-wrong edit an
editor should own.

They were explicitly out of scope for `type-aware-engine`, which required *no*
LSP-layer changes; delivering rename needs new `SemanticEngine` methods and new
shell capabilities, so it needs its own request. They also need something the
index does not have: the index stores *declarations*, while references are
*usages*, which only exist in the source text.

# Proposal

Add `textDocument/references` and `textDocument/rename` over the workspace
source files:

1. **Resolve the target at the cursor** to a name, a kind (type, method, field,
   or local), and — for members — the type that declares it, reusing the index
   lookups and the type layer that hover and completion already use.
2. **Search the workspace sources** for occurrences: the index's known `.java`
   files, each pre-filtered by a plain substring check before any parsing, then
   parsed with the existing tree-sitter parser and scanned for identifier nodes
   matching the name.
3. **Filter each candidate occurrence** so only genuine references survive:
   kind-appropriate nodes, type-layer verification for member accesses (the
   receiver's inferred type must resolve the name to the *same* declaring type),
   and visibility checking for types (the file must be able to see the type).
4. **Rename** builds a `WorkspaceEdit` of one replacement per reference, and
   **refuses** — returning nothing the client can apply — whenever the edit
   cannot be shown to be safe.

# Decisions

- **No result beats a wrong result, applied to a destructive operation.** A
  missing hover is a small loss; a rename that touches the wrong symbol corrupts
  code. Every uncertain case refuses: an unresolved or ambiguous target, a
  library (jar/JDK) declaration, or a new name that is not a valid Java
  identifier. Refusal for references is an empty list; for rename it is `None`,
  which clients render as "cannot rename".
- **Types are only renamed where they are visible.** A candidate occurrence in
  another file counts only if that file is the declaring file, shares the
  declaring package, or imports the type — exactly or via a package wildcard.
  Without this, renaming `a.Foo` would also rewrite unrelated uses of an
  unrelated `b.Foo`.
- **Member references are verified through the type layer.** An access
  `recv.name` counts only when the receiver's inferred type resolves `name` to
  the same *declaring type* as the target (nearest declaration wins, so
  inherited access through a subtype still counts). Unqualified occurrences of
  the name count only inside the declaring type's own span in its own file.
  This is what keeps renaming `size` from touching a different class's `size`.
- **Library declarations are never renamed.** A target whose declaration is a
  dependency entry (jar or JDK) refuses: those files cannot be written, and the
  index deliberately excludes them from navigation.
- **Locals and parameters are supported, file-locally**, and only when the
  enclosing method declares exactly one such local — otherwise which `name` an
  occurrence means cannot be settled. Renaming a local is the most common rename
  and the scope layer already resolves it precisely.
- **The LSP layer changes here, unlike R7.** New `SemanticEngine` methods
  (`references`, `rename`) both get default implementations, so the
  `SyntaxOnlyEngine` stub stays a valid conformance reference, and the shell
  advertises `referencesProvider` and `renameProvider`.
- **`WorkspaceEdit` uses plain `changes`.** The shell tracks versions only for
  open documents, and a heavier `documentChanges` edit with versions for
  unopened files would claim a guarantee the server cannot make. Noted as a
  known limitation.
- **On-demand parsing is a request-path cost, deliberately accepted.** Unlike
  the scan (R6), a references/rename request parses the files that contain the
  name, synchronously. It is user-initiated and rare, the substring prefilter
  keeps it to the files that can match, and it uses the same parser the open
  document already uses.

# Acceptance criteria

- `references` returns the occurrences of the target, including the declaration
  when `include_declaration` is set, and an empty list when the target cannot be
  pinned down.
- `rename` returns a `WorkspaceEdit` with one `TextEdit` per reference, and
  `None` for a library declaration, an unresolved/ambiguous target, or an
  invalid identifier.
- Renaming a type does not touch a same-named type in another package, and
  renaming a member does not touch a same-named member of an unrelated type
  (negative tests included).
- Renaming a local within a method rewrites only that local's occurrences.
- The shell advertises `referencesProvider` and `renameProvider` and routes both
  requests to the engine; the `SyntaxOnlyEngine` stub still conforms.
- `docs/architecture.md` documents the new methods, the resolution and refusal
  rules, and the known limitations; the changelog records the change.

# Implementation plan

## Approach

New trait methods with defaults, the search itself in `engine/syntax.rs` beside
the other type-aware features, a small index accessor, and two shell handlers.
No new dependencies.

- **`WorkspaceIndex::source_files()`** returns the `.java` keys of the index's
  file map — the workspace sources the scan already discovered, which is the
  candidate set for a search, with no second directory walk.
- **Target resolution** mirrors `definition`'s node analysis (import paths,
  declaration names, and the node kind at the cursor) and adds the type layer:
  a member access resolves its receiver type and asks which type declares the
  name, so the target carries the declaring type rather than just a name.
- **Occurrence search** per candidate file: read (or reuse the requested file's
  in-memory text), skip unless the text contains the name, parse, then walk the
  tree collecting identifier nodes that pass the kind-specific rule. Ranges are
  converted with the existing `lsp_range`.
- **Refusal is a first-class outcome.** Resolution returns a `Target` or
  nothing; the caller maps nothing to "no results" / "cannot rename", so an
  uncertain case can never produce an edit.

## Steps

- [x] Add `WorkspaceIndex::source_files()` (the `.java` file URIs) with a unit
      test. (Groundwork for the search set.)
- [x] Add `SemanticEngine::references` and `SemanticEngine::rename` with default
      implementations returning an empty list / `None`, so existing engines and
      the stub conform unchanged. (AC5.)
- [x] Implement target resolution in `src/engine/syntax.rs`: name, kind, and
      (for members) the declaring type, from the cursor's node context and the
      type layer, refusing ambiguous types and dependency declarations.
      (AC1, AC2 — the rules everything else depends on.)
- [x] Implement the occurrences search in `src/engine/syntax.rs`: workspace
      `.java` files, substring prefilter, per-kind node rules (type visibility,
      member ownership verification, unqualified occurrences inside the
      declaring type's span, single-declaration locals), deduplicated and
      `include_declaration`-aware; unit tests per rule including the negatives.
      (AC1, AC3, AC4.)
- [x] Implement `rename`: identifier validation, reference collection, refusal
      paths, and the `WorkspaceEdit`; unit tests for a successful rename and for
      each refusal. (AC2, AC3, AC4.)
- [x] Advertise `referencesProvider`/`renameProvider` and implement both
      handlers in `src/server.rs`. (AC5.)
- [x] Add a harness test driving `textDocument/references` and
      `textDocument/rename` end to end against a fixture workspace. (AC1–AC5.)
- [x] Update `docs/architecture.md`: the two new trait methods and capabilities,
      the resolution/refusal rules, and the limitations (plain `changes`, no
      versioning, on-demand parsing, single-declaration locals). (AC6.)
- [x] Record the change in the changelog. (AC6.)
