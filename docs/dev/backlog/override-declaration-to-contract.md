---
type: ChangeRequest
kind: improvement
title: An override's declaration resolves to the overridden contract
description: A cursor on an overriding member's own declaration resolves to itself, so the class-to-contract direction is unnavigable; resolve an override to the nearest same-named declaration in its type's supertype closure, falling back to self when no source supertype declares it.
state: done
verified: { by: felix, at: 2026-10-12T22:00:00Z }
priority: medium
tags: [dev, improvement, navigation]
owner: felix
---

# Problem

A cursor on an overriding member's own declaration resolves to itself and never
to the declaration it overrides. `find_declaration` (`src/navigate.rs:214-221`)
tries `own_declaration` first, which — when the cursor sits on an indexed
declaration's own name — returns the entry in the cursor's own file whose
`selection_range` matches (`src/navigate.rs:229-264`). On `example/`, a cursor
on `TestImpl.doSomething` therefore returns `TestImpl.doSomething`; there is no
way to reach `TestInterface.doSomething` from the implementation:

```java
public interface TestInterface { void doSomething(); }

public class TestImpl implements TestInterface {
    @Override public void doSomething() {}   // cursor here returns itself
}
```

The self-resolution is deliberate — `declaration-cursor-self-resolution`
(done 2026-10-12) chose it, and its acceptance criteria say "an override's
declaration … likewise resolve to themselves" — and it is right for an ordinary
declaration. But an override has a contract it implements, so the declaration
cursor on it is the one case where the useful answer is the supertype's
declaration. With [go-to-implementation](go-to-implementation.md) restoring the
contract-to-class direction, this leaves the class-to-contract direction
unnavigable.

Falling through to the receiver path would not fix it either: the cursor is on a
`method_declaration` name, not a `recv.name` access, so `receiver_type_of`
returns nothing (`member_access_object` is `None` and `is_implicit_this_use` is
false — `src/navigate.rs:750-785`), the receiver narrowing is skipped, and the
name-only candidate set holds both declarations in two files, which
`unique_candidates` refuses.

# Proposal

For `textDocument/definition` and `textDocument/declaration`, when the cursor is
on an indexed **member** declaration that overrides a member of a supertype,
resolve to the nearest same-named declaration in the enclosing type's supertype
closure instead of to itself. This mirrors the most-derived rule the receiver
path already applies to a *use*: a use resolves to the most-derived
declaration, a declaration resolves to the most-abstract one that declares it.

The declaration cursor's enclosing type and its supertype closure are read the
way the receiver path reads them — `TypeClosure` and `MembersOf` over the hub
(`src/navigate.rs:148-196`) — and the nearest type in the closure, excluding the
enclosing type, that declares the member is the answer. When no supertype
declares it (a first declaration), or the declaring supertype is not a source
declaration (a library/JDK/dependency type, which is never a navigation target),
the cursor falls back to self-resolution. Types, non-overriding members, and
members whose name is not declared by any supertype resolve exactly as today.

# Decisions

- **D1 — Scope: definition and declaration only.** Both route to
  `FindDeclaration`, so one change covers both; `references` and `rename` are
  unchanged (a declaration's own references stay name-group-wide).
- **D2 — Resolution: the nearest overridden declaration in the supertype
  closure.** From the enclosing type's closure, pick the nearest type (excluding
  the enclosing type) that declares the member. Matched by name, and by
  parameter types where the override's parameters are resolvable, so an overload
  does not resolve to a differently-typed supertype overload.
- **D3 — Non-source supertypes fall back to self.** A library, JDK, or
  dependency declaration is not an openable navigation target, so an override of
  such a member — or a supertype name that resolves to no workspace declaration
  — resolves to itself, keeping the standing rule that a jar/JDK declaration is
  never returned.
- **D4 — Everything else keeps self-resolution.** Types, fields that do not
  shadow, non-overriding members, and duplicate-simple-name types resolve to
  themselves, so the `declaration-cursor-self-resolution` interface and
  enum-constant cases stay green. This revises only that request's override
  clause.
- **D5 — Reuse the existing navigation machinery.** The enclosing type and its
  supertype closure are read through the existing `TypeClosure` read, and the
  declaring entries through `EntriesByName`, in `navigate.rs`; no new index read
  is needed (contrast [go-to-implementation](go-to-implementation.md), which
  needs the downward search).
- **D6 — This is an improvement, not a new capability:** it makes existing
  definition/declaration resolve an override to its contract where it resolves
  to itself today, complementing go-to-implementation's contract-to-class
  direction.

# Acceptance criteria

1. On `example/`, a cursor on `TestImpl.doSomething` resolves to
   `TestInterface.doSomething` for both definition and declaration.
2. A cursor on `TestInterface.doSomething` still resolves to itself; the
   duplicate-simple-name type and enum-constant cursors still resolve to
   themselves; `declaration_of_an_interface_method_is_found_though_implemented_elsewhere`
   and `declaration_of_an_enum_constant_is_found_though_the_name_repeats_elsewhere`
   stay green.
3. An override of a library/JDK member (e.g. `run` of `Runnable`), or of a
   supertype that resolves to no workspace declaration, resolves to itself.
4. An override that matches a supertype member by name resolves to it; when the
   override's parameter types are resolvable, an overload resolves to the
   matching parameter-types declaration rather than an arbitrary same-named one.
5. `cargo build`, `cargo fmt --check`, and `cargo test` are clean.

# Docs

- `docs/architecture.md` — the navigation bullet (`:377-386`) currently states
  that a cursor on a declaration resolves to that declaration; note the override
  exception (it resolves to the nearest contract declaration in the supertype
  closure, falling back to itself) and its pairing with go-to-implementation.
- `docs/dev/changelog.md` — the shipped entry, once implemented.

# Implementation plan

## Approach

Resolve an override declaration to the nearest same-named declaration in its
type's supertype closure, using only the existing navigation reads — no new index
read (D5).

In `src/navigate.rs::find_declaration`, after the cursor resolves to its own
indexed declaration (`own_declaration`, `src/navigate.rs:229-264`), check whether
that declaration is a **member** (a method or field) whose enclosing type has a
supertype declaring the same name:

- the enclosing type and package come from the cursor's node
  (`enclosing_type_name`, `file_package`);
- `TypeClosure(enclosing type, package)` gives the ordered supertype identities
  (root first); the enclosing type itself is skipped;
- `EntriesByName(name)` gives every entry declaring the name; the nearest
  identity in the closure whose `(name, package)` has a same-kind entry is the
  overridden declaration, and that entry is the answer;
- when that identity declares several same-named overloads, the one whose
  parameter types match the override's is preferred, read from the supertype
  declaration's source with the module's existing parser and `parameter_type`
  (falling back to the nearest by position when the source cannot be read).

When no supertype declares the name — a first declaration, a type, a non-member,
or an overridden library/JDK member (whose declaration is not an openable target,
D3) — the existing self-resolution stands unchanged.

The step lives beside `own_declaration` and reuses its node/word helpers
(`word_at`, `is_indexed_declaration_name`, `enclosing_type_name`), so the change
is confined to `find_declaration`'s path and leaves `find_references` and the
receiver path untouched (D1).

## Steps

- [x] `src/navigate.rs` — add the overridden-declaration lookup to
      `find_declaration` (types and non-members keep self-resolution; a
      library/JDK supertype falls back to self) (acceptance 1-3); add unit tests
      for the override case, the interface/enum self cases, and a library
      override (acceptance 1-2).
- [x] `src/navigate.rs` — prefer the parameter-types-matching overload when the
      supertype declares several same-named members (acceptance 4).
- [x] `docs/architecture.md` — note the override exception in the navigation
      bullet (`:377-386`): a cursor on a declaration resolves to that declaration
      except an override, which resolves to the nearest contract declaration in
      the supertype closure.
- [x] `docs/dev/changelog.md` — add the shipped entry.
- [x] Verify `cargo build`, `cargo fmt --all -- --check`, and `cargo test`
      (acceptance 5).
