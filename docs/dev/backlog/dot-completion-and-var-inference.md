---
type: ChangeRequest
kind: bug
title: A dot at line end loses its receiver, and `var` locals frequently infer no type
description: Completing `receiver.` returns nothing when the next line starts a new statement (the reported case is a following `var` line), and `var` bindings infer no type for several common initializers.
state: done
priority: high
tags: [dev, completions, types, var]
owner: felix
verified:
  by: cargo test --all-targets (184 passed - 159 lib, 6 bench bin, 18 harness,
    1 stdio)
  at: 2026-09-22T21:15:16Z
---

# Problem

Two related failures make completion and typing feel broken around `var`.

**1. A dot at the end of a line loses its receiver.** Completing `gson.`
returns nothing when the next line begins a new statement — the reported case
is a following `var` line:

```java
class Use {
    Gson gson;
    void m() {
        gson.                    // completes to nothing
        var test = gson.toString();
    }
}
```

`receiver_before_dot` (`src/engine/syntax.rs`) finds the receiver by walking up
from the dot's node to a `field_access`/`method_invocation`. An incomplete
`gson.` whose following line starts a new statement is not recovered into such
a node, so the walk finds nothing and the member list is empty. This is **not**
specific to `var`: a following `int n = 1;` or `Widget w = null;` behaves the
same, while a following `run();` (which can continue the same expression)
still works.

**2. `var` takes its type from an initializer, but several common initializers
infer nothing** (`Ty::Unknown`), so `.`-completion on the binding is empty:

- a call to a method inherited from `java.lang.Object` — the reported
  `var test = gson.toString();`. `Object` is deliberately absent from the
  model, so `toString()` has no known result type; an explicit
  `String test = gson.toString();` works because the declared type is used
  directly.
- an enhanced-for binding, `for (var w : ws) { w. }` — the binding's declared
  type is stored verbatim as the literal `var` (an explicit `Widget w` works).
- a ternary initializer, `var w = c ? new Widget() : null;` — `receiver_type`
  does not handle `ternary_expression`; the same gap covers array creation,
  lambda, `switch` expressions, and `instanceof`.

**3. Adjacent cases, not `var`-specific (in scope per the report):**

- try-with-resources bindings are never collected — `try (Widget w = open())
  { w. }` returns nothing even with an explicit type, because `collect_locals`
  handles only `local_variable_declaration` and `enhanced_for_statement`, not
  the `resource` node.

# Reproduction

The following were verified against the current tree with a throwaway unit
test in `src/engine/syntax.rs` (completion immediately after the `.`; the
explicit-type controls exist to separate the `var`-specific causes from
pre-existing gaps):

| Code | `.`-completion | `var`-specific? |
|------|----------------|-----------------|
| `gson.` (same line) / `gson.` + following `run();` | members offered | — |
| `gson.` + following `var test = ...;` / `int n = 1;` / `Widget w = null;` | empty | no (general) |
| `var x = gson.run();` (own/inherited workspace method) | members offered | — |
| `var x = gson.toString();` | empty (no inlay hint: type unknown) | yes |
| `for (var w : ws) { w. }` vs `for (Widget w : ws)` | empty vs members | yes |
| `try (var w = open()) { w. }` vs explicit type | empty vs empty | no |
| `var w = c ? new Widget() : null;` | empty | yes |

# Proposal

- **Recover the receiver from the source, not only from the tree**
  (`src/engine/syntax.rs`): when the dot is incomplete, derive the receiver
  from the token/expression immediately before it instead of requiring a
  `field_access`/`method_invocation` node, so `gson.` at the end of a line
  still resolves `gson`.
- **Infer `var` from the initializer in every supported position**
  (`src/types.rs`): teach `collect_locals` the enhanced-for and
  try-with-resources bindings, and extend `receiver_type` to the initializer
  shapes it currently ignores (ternary, array creation, lambda, `switch`,
  `instanceof`), keeping `Unknown`/refusal where no answer can be given.
- **Resolve `java.lang.Object`'s methods** so a call like `toString()` yields a
  type (`String`) for `var` inference and for hover/definition, without adding
  those members to `.`-completion listings.

# Decisions

- **The dot failure is general, not `var`-specific** — the fix belongs at the
  receiver lookup, so it applies whatever the following statement is; the
  reported `var` line is one instance.
- **`Object` members are resolve-only** — they must be available for typing
  calls and `var` inference but must not re-introduce `toString`/`equals`/
  `hashCode`/`wait`/... noise into `.`-completion lists (the reason they are
  absent today); if resolve-only turns out to be impractical in the type
  layer, this point is revisited before implementing, since it is a deliberate
  existing design decision.
- **Refusal stays the failure mode** — an initializer whose type genuinely
  cannot be inferred still yields no type and no completion, rather than a
  guess, consistent with `type-aware-review-followups`.
- **Everything the report surfaced is in scope** — the enhanced-for and
  try-with-resources bindings and the unhandled initializer shapes are the
  same defect (a `var` that infers nothing), so they are fixed together.

# Acceptance criteria

- Completing `gson.` with a `var` line (or any new statement) on the next line
  offers `Gson`'s members, exactly as when the expression continues on the
  same line.
- `var x = gson.toString();` infers `String` (an inlay hint renders and `x.`
  offers `String`'s members when a JDK is indexed).
- `for (var w : ws) { w. }` infers the element type; the explicit-type form is
  unchanged.
- `try (Widget w = open()) { w. }` and the `var` form both offer `w`'s
  members.
- `var w = c ? new Widget() : null;` (and the other newly handled initializer
  shapes) infers the type where a single answer exists.
- `.`-completion lists do not gain `toString`/`equals`/`hashCode`/`wait` etc.
  from the new `Object` resolution.
- Regression tests cover each case above in `src/engine/syntax.rs` /
  `src/types.rs`.

# Documentation impact

- `docs/architecture.md` — the completions paragraph of the `TreeSitterEngine`
  bullet (receiver recovery) and the `types.rs` bullet (`var` inference and the
  `Object` member set).
- `docs/requirements.md` — the `var`-inference wording in the v0.4 milestone,
  extended to the newly inferred positions.

# Implementation plan

## Approach

Three defects, three seams; no new dependencies and no LSP-shape changes. The
acceptance criteria are referred to as AC1–AC7 below, in their listed order.

- **Receiver recovery (`src/engine/syntax.rs`).** `receiver_before_dot` keeps its
  `field_access`/`method_invocation` walk and gains a fallback that reads the
  source: take the last non-whitespace byte before the dot, find the tree node
  there, and climb to the outermost expression ending exactly at that byte
  (an `is_receiver_kind` allow-list mirroring the shapes `receiver_type`
  handles). The returned node is a real node of the open document's tree, so
  `scope_at`/`receiver_type` keep working unchanged. `member_items` passes the
  document text and treats `type_identifier` like `identifier` for the
  static-receiver check, so `Widget.` still offers only statics. A following
  `var` line parses `gson.var` as a `scoped_type_identifier`, which is why the
  walk alone finds nothing.
- **`var` inference (`src/types.rs`).**
  - `collect_locals`: an enhanced-for binding written `var` stores the iterable's
    element type (new `element_type`: an array's element, or a `Ref`'s single
    type argument) instead of the literal `var`; a new `resource` arm collects
    try-with-resources bindings (explicit and `var`).
  - `receiver_type_unqualified`: new arms for `type_identifier` and
    `scoped_identifier`/`scoped_type_identifier` (a name receiver),
    `ternary_expression` (unify the branches, a lone `null` yielding the other),
    `array_creation_expression` (an array of the element type),
    `instanceof_expression` (`boolean`), and `switch_expression` (unify the rule
    bodies and `yield`ed values). A lambda has no target type here, so it stays
    `Unknown`, as does any shape with no single answer.
  - `enhanced_for_hint` renders a type hint for a `var` binding too, now that the
    element type is inferred (it needs the tree and model, so its signature
    gains them).
- **Resolve-only `java.lang.Object` (`src/types.rs`).** `member_of` and
  `member_for_call` fall back to the model's `java.lang.Object` (qualified
  lookup, then a simple-name lookup for a model that keys it without a package)
  when the normal hierarchy walk finds nothing, and only for a known
  reference-ish receiver (`Ref`/`Var`/`Array`) so an `Unknown` receiver never
  gains members. `TypeLookup::members` — which `.`-completion reads directly —
  is untouched, so `toString`/`equals`/`hashCode`/`wait`/... do not reappear in
  listings; `member_owner` is untouched, so a library `Object` member still
  refuses navigation, exactly like other library members.

## Steps

- [x] Recover the receiver from the source text before an incomplete dot in
      `receiver_before_dot` (+ `is_receiver_kind`), and pass the text from
      `member_items`. (AC1.)
- [x] Teach `collect_locals` the enhanced-for `var` element type and the
      try-with-resources `resource` binding. (AC3, AC4.)
- [x] Add the `receiver_type_unqualified` arms: `type_identifier`/`scoped_*`,
      ternary, array creation, `instanceof`, and `switch` expression. (AC5.)
- [x] Add the resolve-only `java.lang.Object` fallback to `member_of` and
      `member_for_call`. (AC2, AC6.)
- [x] Render a type hint for a `var` enhanced-for binding in `enhanced_for_hint`.
      (AC3.)
- [x] Add `src/types.rs` unit tests: enhanced-for `var` element type, resource
      binding, ternary/array/`instanceof`/`switch` inference, and `toString()`
      through a modelled `java.lang.Object`. (AC2–AC5, AC7.)
- [x] Add `src/engine/syntax.rs` unit tests: `gson.` with a following `var` line
      (and other new statements) offers members like the same-line form, the
      explicit-type controls, `var x = gson.toString()` hint/`x.` members with a
      modelled `Object`, and that a record's `.`-list gains no `Object` members.
      (AC1–AC3, AC6, AC7.)
- [x] Update `docs/architecture.md`: the `TreeSitterEngine` completions paragraph
      (receiver recovery) and the `types.rs` bullet (`var` inference and the
      resolve-only `Object` member set). (AC1, AC2, AC6.)
- [x] Update `docs/requirements.md`: the v0.4 `var`-inference wording, extended to
      the newly inferred positions. (AC3–AC5.)
- [x] Run `cargo test --all-targets` and confirm the suite passes. (all ACs.)
