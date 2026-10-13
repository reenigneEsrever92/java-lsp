---
type: ChangeRequest
kind: bug
title: A declaration's own name is constrained by what it declares
description: Find references returns nothing when the cursor sits on a declaration's own name that is also declared in the same file as another kind — a class and its constructor share a simple name — because the declaration name is left unconstrained and the mixed type/member candidate set is rejected by the cross-file agreement check.
state: done
verified: { by: felix, at: 2026-10-13T11:35:33Z }
priority: medium
tags: [dev, bug, navigation]
owner: felix
---

# Problem

`find_references` resolves the cursor's word to the index entries of that name
and narrows them with `name_constraint` (`src/navigate.rs`). That predicate
recognises a type position only when the node is a `type_identifier`, and a
member position only when the node is the `name`/`field` of a method invocation
or field access. A **declaration's own name** is neither: a `class_declaration`'s
name node is an `identifier`, so the constraint is `Any` and the candidate set
holds every entry of that name — the type and any same-named member.
`unique_candidates` admits them (they share one file), but `Target::from`
requires the entries to agree on `is_type` and `container`, so the mixed set is
refused and the search returns nothing.

Every class has a constructor named after it, indexed as a member of the class,
so this makes find-references fail on the declaration of **every** class — and on
any declaration whose name is also a member's. Observed on `example/`: a cursor
on `Greeter` in
`example/greeting-lib/src/main/java/com/example/greeting/Greeter.java` — which
declares both the class `Greeter` and its constructor `Greeter(String)` — returns
`[]`. The hub log shows `EntriesByName { name: "Greeter" }` answering a `Class`
and a `Method`, then no file scan following — the early return before
`source_files`.

`find_declaration` does not fail here: it resolves a declaration cursor through
`own_declaration` before `declaration_candidates`, so only references is
affected. This is a distinct defect from
[a type name resolves against what the cursor's file can see](type-name-visibility.md):
that narrowed a type candidate set by visibility; this narrowed a declaration
name by kind, and both are needed.

# Reproduction

`navigate::tests::references_on_a_class_declaration_survive_a_named_constructor`
(`src/navigate.rs`) mirrors the example: `com/example/greeting/Greeter.java`
declares `class Greeter` and its constructor `public Greeter(String name)`, and
`com/example/app/Main.java` imports `Greeter` and uses it in a field type,
`new Greeter(...)`, and `new Greeter.Inner()`. A cursor is placed on the class
declaration's own name.

- Observed: `find_references` returns `[]` — the `Class` and the constructor
  `Method` reach `Target::from` together and are refused.
- Expected: with `includeDeclaration`, the class declaration plus the four
  `Greeter` occurrences in `Main.java`; without, the four occurrences.

Uses of the type (a field type, `new T(...)`, a scoped `T.Inner`, an import
segment) were already unaffected and resolved.

# Proposal

Classify a declaration's own name by what it declares, in `name_constraint`:

- a type declaration's name (`class`/`interface`/`enum`/`record`/
  `annotation_type`) constrains to `Type`;
- a `method`/`constructor`/`enum_constant`/`variable_declarator`/
  `formal_parameter`/`spread_parameter`/`enhanced_for` name constrains to
  `Member`.

The candidate set is then pre-filtered to the right kind before the cross-file
agreement check, so a same-named member never mixes into a type's set.

# Decisions

- **D1 — Constrain declaration names by kind.** The node kind alone cannot say
  whether a declaration name is a type or a member (a `class_declaration` name and
  a `method_declaration` name are both `identifier`s); its parent kind can.
  Chosen over teaching `Target::from` to prefer one kind, which would leave the
  candidate read ambiguous.
- **D2 — Reuse the declaration-name kinds.** The parent kinds mirror
  `is_declaration_name`, plus `enum_constant`, which only
  `is_indexed_declaration_name` lists (the shared predicate omits it because it
  also governs references). Keeping the lists in step avoids a second source of
  truth.
- **D3 — Definition is unchanged.** A declaration cursor still resolves through
  `own_declaration` first, so this only changes `declaration_candidates` for a
  declaration cursor `own_declaration` does not own.
- **D4 — A constructor's own references stay sparse.** `new T(...)` is attributed
  to the type, not the constructor, so a cursor on a constructor's name still
  finds little; that is the separate constructor-model gap, left out of scope.

# Acceptance criteria

1. A cursor on a class declaration's own name resolves references to the class
   even when the class shares its simple name with its constructor; the
   declaration is reported only with `includeDeclaration`.
2. A cursor on a type use, and go-to-definition from a declaration, are
   unchanged.
3. `cargo build`, `cargo fmt --check`, and `cargo test` are clean.

# Docs

None. The architecture's navigation bullet already states that a cursor on a
declaration's own name resolves to that declaration. The `docs/dev/changelog.md`
entry records the fix.

# Implementation plan

## Approach

`name_constraint` (`src/navigate.rs`) already classifies a `type_identifier` as
`Type` and a member-access name as `Member`; declaration names fell through to
`Any`. Extending its `identifier` branch to also match a declaration's parent
kind — type declarations to `Type`, the rest to `Member` — narrows the candidate
set by kind before `unique_candidates`/`Target::from` run, so the type and its
same-named constructor never reach the agreement check together. No new index
read or helper is needed.

## Steps

- [x] Extend `name_constraint` (`src/navigate.rs`) to classify a declaration's
  own name by its parent kind (D1, D2).
- [x] Add `references_on_a_class_declaration_survive_a_named_constructor` as the
  regression test; confirm the existing declaration and use tests stay green
  (D3).
- [x] Verify: `cargo fmt --all -- --check`, `cargo build --all-targets`, and
  `cargo test` clean.

## Progress

Implemented in `src/navigate.rs` (`name_constraint` and the regression test).
Verified: `cargo fmt --all -- --check` clean; `cargo test` green — 60 library
tests and 7 bench tests. Not committed.
