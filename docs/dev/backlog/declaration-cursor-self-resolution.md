---
type: ChangeRequest
kind: bug
title: A cursor on a declaration resolves to that declaration
description: Go-to-definition/declaration returns nothing when the cursor sits on a declaration name whose name is also declared in another file (an interface method with an implementation, an override, or two types sharing a simple name), because the cross-file ambiguity policy is applied to a cursor that is on the declaration itself.
state: done
verified: { by: felix, at: 2026-10-12T16:40:54Z }
priority: medium
tags: [dev, bug, navigation]
owner: felix
---

# Problem

`textDocument/definition` and `textDocument/declaration` resolve a cursor by
looking the word up in the flat symbol table and refusing when the name is
declared in more than one file — "a name spread across files is no answer (no
result beats a wrong result)", `src/index/navigate.rs::unique_candidates`. That
policy is right for a _use_: an ambiguous reference has no single answer. It is
wrong for a cursor that sits **on a declaration name**, where the answer is the
declaration the cursor is already on.

The interface case makes the defect common: an interface method is almost always
also implemented, so its name has two declarations in two files and navigation
to it returns nothing. The same holds for an overridden method and for two
unrelated types that share a simple name across packages. On `example/` (with
`TestInterface`/`TestImpl` present), a cursor on `doSomething` in
`TestInterface.java` — and on the `@Override` in `TestImpl.java` — returns no
location.

# Reproduction

`index::navigate::tests::declaration_of_an_interface_method_is_found_though_implemented_elsewhere`
(`src/index/navigate.rs`): an interface declares `void solo()`, a class
implements it with `solo()`, and the cursor is placed on the interface's own
method declaration.

- Observed: `find_declaration` returns `None`.
- Expected: the interface method's own location.

`cargo test --lib` currently reports 18 passed, this test failing.

# Proposal

When the cursor sits on an **indexed declaration's name** — the `name` field of
a class/interface/enum/record, a method or constructor, a field, or an enum
constant — resolve to that declaration directly (the entry in the cursor's file
whose selection range matches the name), instead of applying the cross-file
ambiguity policy. The shape is recognised by `is_declaration_name`, extended
locally to `enum_constant` because the shared predicate also governs
`references`, which D1 leaves unchanged.

The change is narrow: it only alters what a cursor _on_ a declaration returns.
A cursor on a _use_ keeps the current policy (an ambiguous use still yields
nothing), and `references` is unchanged — a declaration's own references remain
ambiguous across the name group.

# Decisions

- **D1 — Fix the declaration path only.** Scope to `find_declaration`; leave
  `find_references` as it is. A _use_ of a name spread across files stays
  ambiguous; only a cursor on the declaration resolves to itself. Reason: the
  defect is applying the use-ambiguity policy to a declaration cursor, and
  references has no self-case to make.
- **D2 — Only indexed declarations.** Types (class/interface/enum/record),
  methods, constructors, fields, and enum constants have index entries; locals
  and parameters do not (and annotation types are not indexed at all), so their
  declaration cursors stay out of scope and unchanged.
- **D3 — Keep the reproduction test as the regression test**, in
  `src/index/navigate.rs` alongside the existing navigation tests.
- **D4 — Both shell requests are fixed together.** `textDocument/definition`
  and `textDocument/declaration` both route to `IndexRequest::FindDeclaration`,
  so one fix covers both.

# Acceptance criteria

1. A cursor on an interface method's declaration resolves to that declaration
   even when another file declares an implementation of the same name; an
   override's declaration and a duplicate-simple-name type's declaration
   likewise resolve to themselves.
2. `declaration_of_an_ambiguous_name_is_none` still passes: a _use_ of a name
   spread across files still yields no location.
3. `cargo build`, `cargo fmt --check`, and `cargo test --lib` are clean.

# Docs

`docs/architecture.md` — the navigation bullets describe the single-candidate
rule ("A single candidate … anything spread across multiple files returns no
location rather than a wrong one") in the engine-core bullet and the "Navigation
is the index's own" bullet; both must note the declaration-cursor exception: a
cursor on a declaration resolves to that declaration, and only uses keep the
cross-file ambiguity policy.

# Implementation plan

## Approach

The defect is a policy applied to the wrong node. `find_declaration`
(`src/index/navigate.rs`) resolves the cursor by looking the word up in the flat
symbol table and then running `unique_candidates`/`unique_location`, whose
cross-file ambiguity policy is right for a _use_ but wrong for a cursor that sits
_on a declaration name_ — there the answer is the declaration already under the
cursor.

The fix adds a pre-check in `find_declaration` only: when the cursor node is a
declaration's `name` field (the names `is_declaration_name` recognises, extended
locally to enum constants), return the entry in the
cursor's own file whose `selection_range` equals the name node's — the
declaration the cursor is on. When no such entry exists (a local, a parameter, an
annotation type — declarations the symbol table does not hold) or the cursor is
on a use, the function falls through to the existing
`unique_location(declaration_candidates(...))` path unchanged, so uses keep the
cross-file ambiguity policy and non-indexed declaration cursors behave exactly as
before. `find_references` is untouched (D1).

No new index or type-layer machinery is needed: `selection_range` is already the
name node's range (`index/analyze.rs::entry`), so the match is exact and cheap.

## Steps

- [x] In `find_declaration` (`src/index/navigate.rs`), when the cursor sits on an
  indexed declaration's name (the names `is_declaration_name` recognises, with a
  local extension for `enum_constant`), resolve to the entry in the cursor's own
  file whose selection range matches the name node; fall back to
  `unique_location(declaration_candidates(...))` otherwise (D1, D2, D4). Update
  the function's and the module's doc comments to note the declaration-cursor
  exception.
- [x] Keep `unique_candidates`/`unique_location` and `find_references` unchanged,
  so a use of a name spread across files still returns no location (D1).
- [x] Keep `declaration_of_an_interface_method_is_found_though_implemented_elsewhere`
  and `declaration_of_an_enum_constant_is_found_though_the_name_repeats_elsewhere`
  as the regression tests (D3); confirm `declaration_of_an_ambiguous_name_is_none`
  stays green.
- [x] Update `docs/architecture.md`: note the declaration-cursor exception in the
  engine-core navigation bullet and the "Navigation is the index's own" bullet —
  a cursor on a declaration resolves to that declaration, and only a use keeps the
  cross-file ambiguity policy.
- [x] Run `cargo build`, `cargo fmt --check`, and `cargo test --lib` clean.
