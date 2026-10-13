---
type: ChangeRequest
kind: feature
title: Offer each overload with its signature at a call site
description: The unqualified path offers a workspace method name once, name-only, so overloads at a bare call site are indistinguishable; list each overload with its full signature and insert `name(`, matching the member path.
state: done
verified: { by: felix, at: 2026-10-13T15:14:49Z }
priority: medium
tags: [dev, feature, completion, types]
owner: felix
---

# Problem

The live `completion` module's workspace-symbol path offers a method name once,
name-only — `completion-module` D3 records this as a known simplification ("a
method's overloads are offered name-only"). Typing a bare call `add` in a class
that declares `add(int)` and `add(int, int)` therefore offers a single generic
item that cannot be told apart from its sibling, and it inserts the bare name
rather than opening an argument list. The old engine listed each overload with its
real signature (`src/deprecated/analysis.rs`, the `model_overloads` branch),
resolving them from the entry's declaring type in the type model; only the
`member-completion` request restores that for a receiver, leaving the unqualified
case behind.

# Proposal

Apply the overload-aware listing — introduced for member completion — to the
workspace-index method path, so a bare method name offers each overload with its
full signature (`Member::signature()`), inserting `name(`, with the declaring type
as `detail`. Keywords, fields, and types keep their single item; the in-scope
(local/parameter/field) path is unchanged, since it lists no methods.

# Decisions

- **D1 — Reuse the member path's overload listing.** The overload item shape
  (`label = signature`, `filter_text = bare name`, `insert_text = name(`,
  `detail = declaring type`) is the one `member-completion` establishes; this
  request applies it to the index path rather than introducing a second shape.
- **D2 — Overloads resolve through the entry's declaring type.** As the old
  engine's `model_overloads`: the entry's innermost container names the type, and
  the type's same-named methods are the overloads. An entry whose container cannot
  be resolved in the model keeps today's single name-only item — never dropping a
  symbol because its overloads cannot be expanded.
- **D3 — Scope is the workspace-index method path only.** Keywords, fields, and
  types are unchanged; the in-scope path lists locals/parameters/fields and stays
  name-only, because it never contributes methods.
- **D4 — Auto-import is preserved.** Each overload item carries the same
  auto-import edit the single item carried (the entry is the import's anchor).
- **D5 — Depends on `member-completion`** for the shared overload-listing helper
  and the overload-preserving (`members_with_overloads`) index read.

# Acceptance criteria

1. In a class whose own type declares `add(int)` and `add(int, int)`, a bare `add`
   offers two items, each labelled with its full signature and inserting `add(`.
2. A non-overloaded method is offered as a single item, unchanged in shape.
3. A workspace/dependency method with overloads that resolves through its declaring
   type lists each overload; one whose declaring type is not in the model keeps the
   single name-only item (no symbol disappears).
4. Fields, types, and keywords are unchanged; an automatically imported symbol
   still carries its import edit.
5. `cargo build`, `cargo fmt --check`, and `cargo test` are clean.

# Docs

- `docs/architecture.md` — the completion implementation note (both completion
  paths list overloads).
- `docs/dev/changelog.md` — the shipped entry.
- `docs/log.md` — the reconciliation.

# Implementation plan

## Approach

The workspace-index loop in `src/completion.rs` already walks the entries whose
simple name matches the prefix; for a `SymbolKind::Method` entry it currently
falls through to the single name-only `offer`. `member-completion` establishes the
overload item shape there; this request applies the same shape on the index path.
For a method entry, resolve its declaring type — `entry.container.last()` with
`entry.package` — read that type's members over the hub (`MembersOf`, now
overload-preserving), keep the same-name methods, and offer each with
`Member::signature()` / `insert_text = name(` / `detail` = declaring type, carrying
the same auto-import edit. When the declaring type does not resolve to any member
(the container is not in the model), keep today's single name-only item, so no
symbol is ever lost. Factor the item construction into a small helper shared with
the member path so the two paths cannot drift.

## Steps

- [x] `src/completion.rs`: extract the member overload-item construction into a
  helper and use it in the workspace-index loop for `SymbolKind::Method` entries,
  resolving the declaring type's overloads over the hub and falling back to the
  single item when it does not resolve.
- [x] `src/completion.rs` tests: a class with `add(int)`/`add(int, int)` offers two
  full-signature items inserting `add(`; a non-overloaded method stays a single
  item; a method whose declaring type is not in the model keeps the name-only item;
  fields and types are unchanged and auto-import is preserved.
- [x] `docs/architecture.md`: the completion implementation note (both completion
  paths list overloads).
- [x] `docs/dev/changelog.md`: the shipped entry.
- [x] `docs/log.md`: the reconciliation.
- [x] `cargo build --all-targets`, `cargo fmt --all -- --check`, `cargo test`
  green (AC1–AC5).
