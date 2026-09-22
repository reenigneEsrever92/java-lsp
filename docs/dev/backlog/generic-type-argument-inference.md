---
type: ChangeRequest
kind: feature
title: Generic type-argument inference
description: Infer and substitute generic type arguments so a resolved type renders as List<Integer> rather than List or List<E>.
state: done
priority: medium
tags: [dev, types, engine]
owner: felix
verified:
  by: cargo test (141 passed - 116 lib, 6 bench bin, 18 harness, 1 stdio)
  at: 2026-09-22T19:55:21Z
---

# Problem

Inlay hints (and hover) render a generic type without its arguments. For
`var x = List.of(3);` the hint is `x: List` when the JDK is indexed from class
files (descriptors are erased) or `x: List<E>` when it is indexed from
`lib/src.zip` (the source's type variable, unsubstituted) — never
`List<Integer>`. `type-aware-engine` recorded generics substitution as a
non-goal, and `docs/architecture.md` documents "generic arguments are preserved
for display but not substituted". The gap is visible now that `List.of(...)`
resolves (see `import-aware-type-resolution`): the hint appears but is the least
informative form.

# Proposal

Infer type arguments for the common cases and substitute them when a resolved
type is rendered or when a receiver's members are looked up.

# Decisions

- **Scope (settled).** Infer and substitute type arguments for **method calls** —
  a method's own type parameters from its argument types (`List.of(3)` →
  `List<Integer>`), and the receiver type's parameters from the receiver's
  arguments (`List<String> l; l.get(0)` → `String`) — and for a **field** read
  through a receiver. Applied where an expression's type is computed
  (`receiver_type`), which is what the inlay hints consume; hover signatures and
  completion details keep the written form in this slice.
- **Overloads are picked by arity.** `member_of` matches by name only, so
  `List.of` collapses to its first (parameterless) overload, which cannot bind
  `E`. Call sites therefore use a new `member_for_call`, which prefers the
  overload whose parameter count matches the call.
- **The representation already exists.** `Ty::Ref` keeps its arguments as
  written, so the work is inference and substitution, not a new type model.
- **Conservative as everywhere.** Only substitute when a single argument type is
  provable; otherwise keep the written form (`List<E>`) or the erased name
  (`List`) rather than guessing.
- **Class-file sources are erased.** A JDK indexed from bytecode has no type
  parameters, so the ceiling there is the erased name — inference cannot recover
  `E` from a descriptor. A `lib/src.zip` (or workspace) source is where
  substitution is possible.

# Acceptance criteria

- `var x = List.of(3);` renders `List<Integer>` with a JDK indexed from
  `lib/src.zip` or declared in the workspace.
- An argument that cannot be inferred leaves the written form unchanged (no
  guess).
- The feature is recorded in `docs/architecture.md` (which currently lists
  generics substitution as a non-goal) and the changelog.

# Implementation plan

## Approach

All of the analysis lives in `src/types.rs`; the only engine change is using the
arity-aware call lookup where a call's argument count is known.

- **`Member::type_params`.** The method's declared type parameter names (empty
  for fields and for class-file members, whose signatures are erased), filled
  from a method's `type_parameters` in `collect_members`.
- **`Ty::substitute`.** Replaces a reference to a bound type-parameter name,
  recursing through generic arguments and array element types. Primitives are
  boxed when bound (`int` → `java.lang.Integer`), matching Java's inference for
  `List.of(3)`.
- **`member_for_call`.** Prefers the overload whose parameter count equals the
  call's argument count (and, when several share that arity, requires that they
  agree on return type and kind, as `member_of` does); falls back to
  `member_of` when no arity matches.
- **Binding.** The receiver's type parameters bind from the receiver's arguments
  (only when the member is declared by the receiver's own type — inherited
  members are left alone), and the method's type parameters bind positionally
  from the argument types. Only a single provable binding is substituted;
  anything unresolved keeps the written form.
- **Where it applies.** `receiver_type`'s method-invocation and field-access
  arms, so `var`, chained-call, and parameter hints all see the inferred type.

## Steps

- [x] Add `type_params` to `Member` and fill it from a method's
      `type_parameters` in `collect_members`; update the `Member` literals in
      `src/types.rs` and `src/classfile.rs`. (Groundwork.)
- [x] Add `Ty::substitute` plus the primitive-boxing helper, with unit tests.
      (AC: `List.of(3)` → `List<Integer>`.)
- [x] Add `member_for_call` (arity-preferred selection, conservative) with unit
      tests covering an arity match, agreement among same-arity overloads, and
      the fallback. (AC: overloads bind the right parameters.)
- [x] Wire binding and substitution into `receiver_type`'s method-invocation and
      field-access arms, with unit tests for a factory call, a literal argument,
      a receiver-argument chain, an uninferrable argument, and an erased
      (class-file) member. (AC: inference and the no-guess cases.)
- [x] Use `member_for_call` in `parameter_hints` so the arity-matched overload's
      parameter names are shown. (Improves the earlier `List.of` finding.)
- [x] Update `docs/architecture.md` (generics substitution is no longer a
      non-goal) and `docs/requirements.md` (the v0.3 remainder), and record the
      change in `docs/dev/changelog.md`.
