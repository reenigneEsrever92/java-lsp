---
type: ChangeRequest
kind: bug
title: A dot at line end loses its receiver, and `var` locals frequently infer no type
description: Completing `receiver.` returns nothing when the next line starts a new statement (the reported case is a following `var` line), and `var` bindings infer no type for several common initializers.
state: proposed
priority: high
tags: [dev, completions, types, var]
owner: felix
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
