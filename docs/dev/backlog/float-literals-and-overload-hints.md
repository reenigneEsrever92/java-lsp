---
type: ChangeRequest
kind: bug
title: Float literals and parameter hints ignore overload resolution
description: A float literal is typed double and parameter hints match by arity, so a call to an overloaded method can land on the wrong overload.
state: done
priority: high
tags: [dev, bug, types, hints]
owner: felix
verified:
  by: cargo test --all-targets (213 passed - 183 lib, 6 bench, 23 harness, 1 stdio)
  at: 2026-09-23T22:40:00Z
---

# Problem

Overload selection is argument-type-aware for go-to-definition,
find-references, completions, and signature help, but two paths still ignore the
argument's type, so a call to an overloaded method can be attributed to the
wrong overload:

1. **Inlay parameter hints** (`analysis.rs::parameter_hints`) choose the callee
   with `types::member_for_call`, which matches **by arity only**. When two
   overloads share an arity and a return type — `void test(int count)` and
   `void test(double number)` — `member_for_call` treats them as one answer and
   returns the first, so `data.test(5.0f)` is hinted as `count:`.
2. **A float literal is typed `double`.** `types.rs::receiver_type_unqualified`
   maps every `decimal_floating_point_literal`/`hex_floating_point_literal` to
   `Ty::Prim(Prim::Double)`, ignoring the `f`/`F` suffix. With
   `void test(float number)` and `void test(int count)`, a call
   `data.test(5.0f)` therefore infers a `double` argument that fits neither
   overload, so the type-based selection finds no applicable candidate and falls
   back to the arity answer — the first same-arity overload, `test(int count)`.
   Go-to-definition and find-references then land on `test(int)`.

# Reproduction

With

```java
record Data(int number) {
    void test() {}
    void test(int count) {}
    void test(float number) {}
}
// ...
var data = new Data(5);
data.test(5);
data.test(5.0f);
```

- The parameter hint on `5.0f` reads `count:` (the `int` overload) instead of
  `number:`.
- Go-to-definition and find-references on `test` at `data.test(5.0f)` resolve to
  `test(int count)` instead of `test(float number)`.

Expected: a `5.0f` argument is a `float`, so it selects `test(float number)`
(and, with no `float` overload, `test(double)` by widening); the hint names that
overload's parameter and navigation lands on it. `data.test(5)` keeps selecting
`test(int count)`.

# Proposal

Two fixes in the type layer and the hint path:

1. Type a floating literal by its suffix: `float` when it ends in `f`/`F`, else
   `double` (`src/types.rs`).
2. Select the hint's callee from the call's argument types with the same policy
   navigation uses, and emit **no** hint when the overload cannot be pinned down
   (several same-arity candidates and inconclusive argument types) rather than
   naming a possibly-wrong parameter (`src/analysis.rs::parameter_hints`).

# Decisions

- **The float-literal fix is in scope** (per the report): the reporter's
  signature used `float` and resolution failed; the literal's type is the root
  cause, not a separate concern. This also covers `5.0` versus `5.0f` in any
  future `float`/`double` overload pair, where Java picks `float` for a `float`
  literal.
- **Hints refuse rather than guess.** When the argument types do not single out
  an overload and more than one candidate shares the arity, no parameter hint is
  emitted — the same "no result beats a wrong result" rule the rest of the
  server follows. A single same-arity candidate is still used, and a call whose
  arity matches nothing still yields no hint.
- **Navigation keeps its arity fallback.** Definition and references continue to
  fall back to the arity-level answer when the argument types are inconclusive;
  this change only makes the types decisive for float literals.
- **No new dependency and no shell change** — both fixes are inside
  `src/types.rs` and `src/analysis.rs`.

# Acceptance criteria

- With `test(int)` and `test(float)`, `data.test(5.0f)` resolves to
  `test(float number)` for definition and references, its hint reads `number:`,
  and `data.test(5)` still resolves to `test(int count)` with the hint `count:`.
- With `test(int)` and `test(double)` and no `float` overload,
  `data.test(5.0f)` resolves to `test(double)` (a float widens to a double) and
  its hint names that overload's parameter.
- A call whose argument types are inconclusive and whose name has several
  same-arity overloads gets **no** parameter hint.
- A `5.0f` literal is typed `float` and a `5.0` literal `double` (unit test).
- Existing navigation and completion behaviour is unchanged.

# Documentation

- `docs/architecture.md` — the type-layer approximations (a floating literal's
  type by suffix) and the inlay-hint bullet (the callee is chosen by argument
  types, and no hint is emitted when the overload cannot be pinned down).

# Implementation plan

## Approach

Both fixes are in the type layer and the hint path; no shell or engine-boundary
change.

- **Floating literals (`src/types.rs`).** Add `floating_literal_type(node, text)`
  returning `Ty::Prim(Prim::Float)` when the literal ends in `f`/`F`, else
  `Ty::Prim(Prim::Double)`, and use it for `decimal_floating_point_literal` /
  `hex_floating_point_literal` in `receiver_type_unqualified`.
- **A confirmed selection (`src/types.rs`).** Extract the candidate gathering
  into `call_candidates`, then add `member_for_arguments_confirmed`, which
  returns an overload only when the argument types select exactly one applicable
  candidate, or a single same-arity candidate exists; several same-arity
  candidates with inconclusive types yield `None`. Rebuild `member_for_arguments`
  on it (confirmed, then the arity-level `member_for_call` fallback) so
  navigation's behaviour is unchanged.
- **Hints (`src/analysis.rs::parameter_hints`).** Infer the call's argument types
  with `receiver_type` and select the callee with
  `member_for_arguments_confirmed`; keep the arity guard, so an unpinned overload
  emits nothing.

## Steps

- [x] Add `floating_literal_type` and use it in `receiver_type_unqualified`; unit
      test that `2.5f` is `float` and `3.5` is `double`.
- [x] Add `call_candidates` and `member_for_arguments_confirmed`, rebuild
      `member_for_arguments` on them; unit test the confirmed selection and the
      unpinned `None`.
- [x] Switch `parameter_hints` to the argument-type selection; unit tests for the
      `float`/`int` and `double`/`int` pairs and for the withheld-hint case.
- [x] Extend the reproduction test to a `test(float)` signature (definition,
      references, and the hint) alongside `data.test(5)`.
- [x] Update `docs/architecture.md` (the literal-type approximation and the
      inlay-hint bullet).
- [x] Run `cargo test --all-targets` and confirm the suite passes.
