---
type: ChangeRequest
kind: bug
title: Dotted nested-type references (Outer.Inner) are misread as package-qualified names
description: A source-written nested type reference such as `Outer.Inner` is treated as package `Outer` plus type `Inner`, so the type does not resolve — no member completion on an instance of it, no completion of nested types on the enclosing type, and navigation by type is lost.
state: done
priority: high
tags: [dev, types, completions, resolution]
owner: felix
verified:
  by: cargo test --all-targets (313 passed; the only failures are 4 sources
    lib tests and 1 harness test that need a loopback socket this sandbox
    forbids)
  at: 2026-09-28T13:21:07Z
---

# Problem

A nested type reference written the Java way — `Outer.Inner` — does not
resolve. `Ty::qualified_package` (`src/types.rs`) splits a `.`-separated name as
`package.Type`, so for `Greeter.Inner` (which has no `$`) it returns
`Some("Greeter")` — treating the **outer type as a package**. `TypeLookup::lookup`
then runs `find_in_package("Inner", Some("Greeter"))`, which fails because
`Inner`'s package is the real package, not `Greeter`. `resolve_type_info`
(`src/types.rs`) made the same `.`-only assumption.

The user-visible result:

- `Greeter.Inner i = new Greeter.Inner();` leaves `i`'s type unresolved, so
  completing after `i.` offers nothing.
- Completion after the enclosing type — `Greeter.` — offers nothing either: a
  nested type is not a member of its enclosing type in the model, so
  `member_items` never lists `Inner`.
- Go-to-definition for `i.getVal()` only "works" by accident: with no resolved
  receiver it falls back to a unique global-name search, which breaks as soon as
  the member's name is not globally unique.

Nested types from a jar/JDK escape the first symptom only because their
descriptors carry `$` (`java/util/Map$Entry`), which `qualified_package` splits
correctly. Source code has no `$`, so a dotted source reference — and a dotted
jar reference such as `Map.Entry` — is the broken case. No diagnostic is
reported (the resolution simply yields nothing), so this is a silent miss rather
than a false positive.

# Reproduction

```java
// p/Greeter.java
public class Greeter {
    public static class Inner {
        public int getVal() { return 1; }
    }
}

// p/Use.java
class Use {
    void m() {
        Greeter.Inner i = new Greeter.Inner();
        i.getVal();   // no completion after `i.`
        Greeter.      // `Inner` not offered
    }
}
```

Expected: `i.` offers `getVal`; `Greeter.` offers `Inner`; definition of
`i.getVal()` resolves through the receiver's type. Confirmed against the current
tree with a throwaway harness.

# Proposal

Teach resolution that a dotted name may be a nested type, and teach completion
to list a type's nested types:

- **`src/types.rs` — resolution.** In `lookup`, resolve a dotted, non-`$` name
  `A.B` nested-first: resolve `A` as a type in the current context, then find `B`
  among `A`'s directly nested types; fall back to the existing package-qualified
  interpretation (`A` as a package) when that fails. `resolve_type_info`'s
  qualified branch delegates to `lookup`, so scoped name resolution follows.
  This fixes `new Greeter.Inner()`, `Greeter.Inner x`, and therefore `i.` member
  completion and navigation. A dotted jar reference (`Map.Entry`) is fixed by
  the same path.
- **`src/types.rs` — enumerate nested types.** Add `nested_types(owner, package)`
  to the looked-up model, built on a new `all_types()` enumeration on the
  `TypeLookup` trait (implemented for `TypeModel`, `SourceLayerIndex`,
  `ModelLayers`, and `TypeQuery`), returning types whose recorded `nested` chain
  names `owner`.
- **`src/analysis.rs` — completion.** In `member_items`, when the receiver is a
  type, also offer that type's nested types (kind CLASS/INTERFACE/ENUM/STRUCT,
  inserting the simple name) alongside its static members.

# Decisions

- **D1 — Nested-first for a dotted name.** Java gives no syntactic signal
  separating `Outer.Inner` from `package.Type`; `Outer` resolving as an in-scope
  type takes precedence, which is Java's own rule, with the package reading as
  the fallback.
- **D2 — Reuse the recorded `nested` chain.** The model already stores each
  nested type's enclosing chain (source uses `Outer$Inner`, class files use
  `java.util.Map$Entry`), so no new identity is introduced.
- **D3 — `$` names keep their current meaning.** A name containing `$` is a
  class-file nested type and resolves exactly as today; only `.`-dotted names
  gain the new path.
- **D4 — Completion lists nested types on a type receiver.** `Greeter.` offers
  `Inner`; it is not offered on an instance receiver (`greeter.`), since the
  receiver is known to be a type at that point (`static_only`).
- **D5 — Out of scope.** Local and anonymous classes, and static imports of
  nested types, are left as they are.

# Acceptance criteria

- `i.getVal()` with `i` of type `Outer.Inner` completes `getVal` and resolves
  its definition through the receiver's type.
- `Greeter.` offers the nested type `Inner` (and still offers the enclosing
  type's static members).
- `Greeter.Inner i = new Greeter.Inner();` type-resolves; a variable declaration
  and an object creation with a dotted nested type both work.
- A dotted reference to a jar/JDK nested type (`Map.Entry`) resolves.
- A plain package-qualified name (`java.util.List` / `q.List`) still resolves,
  and a `$`-form nested name is unchanged.
- Regression tests cover each of the above in `src/types.rs` and
  `src/analysis.rs`.

# Documentation impact

- `docs/architecture.md` — the `types.rs` bullet: note that a dotted name is
  resolved nested-first (a nested type before a package), and that `.`-completion
  on a type receiver lists its nested types.

# Implementation plan

## Approach

No new dependencies and no LSP-shape changes. Resolution lands in
`src/types.rs`; the completion surface lands in `src/analysis.rs`.

- **`src/types.rs` — nested-first resolution.** `TypeLookup::lookup` first tries
  the nested reading for a name with a `.` and no `$` — resolve the prefix as a
  type (`self.lookup(&Ty::reference(prefix), package)`), then take the nested
  type named by the last segment — before falling back to
  `find_in_package(simple, Some(prefix))`. Names without a `.` and names with `$`
  keep their behaviour. `resolve_type_info`'s qualified branch delegates to
  `lookup`.
- **`src/types.rs` — nested-type enumeration.** Add required `all_types()` and a
  default `nested_types(owner, package)` to `TypeLookup`. `all_types` is
  implemented for `TypeModel` (its own `types()`), `SourceLayerIndex` (layers
  highest-precedence-first, slot-deduped), `ModelLayers` (its existing `types()`),
  and `TypeQuery` (overlay then base, slot-deduped). `nested_types` filters
  `all_types()` by package and by the innermost `$`-segment of `nested` equal to
  `owner`.
- **`src/analysis.rs` — completion.** `member_items`, after the static-member
  loop, when `static_only` and the receiver resolves to a type, lists
  `query.nested_types(name, package)` as items whose kind is the nested type's
  completion kind, inserting the simple name.
- **Tests.** `src/types.rs`: a dotted nested reference resolves through its
  owner; a package-qualified name still resolves; a `$`/dotted class-file nested
  name resolves; `nested_types` lists only direct children. `src/analysis.rs`:
  `i.` completion for an `Outer.Inner` variable; `Greeter.` offering the nested
  type; definition of `i.getVal()` through the receiver.
- **Docs.** `docs/architecture.md`: the `types.rs` bullet.

## Steps

- [x] Add `all_types()` to `TypeLookup` (for `TypeModel`, `SourceLayerIndex`,
      `ModelLayers`, `TypeQuery`) and the default `nested_types`. (AC3, AC4.)
- [x] Make `TypeLookup::lookup` resolve a dotted, non-`$` name nested-first, with
      the package reading as fallback. (AC1, AC3, AC4, AC5.)
- [x] Delegate `resolve_type_info`'s qualified branch to `lookup`. (AC3.)
- [x] List a type receiver's nested types in `member_items`. (AC2.)
- [x] Add the `src/types.rs` and `src/analysis.rs` unit tests. (AC6.)
- [x] Update `docs/architecture.md` (the `types.rs` bullet). (AC1, AC2.)
- [x] Run `cargo test --all-targets` and confirm the suite passes. (all ACs.)
