---
type: ChangeRequest
kind: bug
title: Complete a var-declared local's members from its inferred type
description: The shared receiver step reads a local_variable_declaration's written type node, so a `var x = …` binding resolves to the non-existent type "var" and member completion after `x.` offers nothing.
state: done
verified: { by: felix, at: 2026-10-13T16:10:48Z }
priority: high
tags: [dev, bug, completion, types]
owner: felix
---

# Problem

Member completion after a dot on a local declared with `var` offers nothing. In
the example workspace, `var data = new Data(5); data.` completes to an empty list
— the live completion request at `example/greeting-app/.../Main.java` (31, 13)
answers no items — while `greeter.` and `gson.` (explicitly typed locals)
resolve.

`src/util.rs::local_type`, the shared receiver step behind member completion,
reads the `local_variable_declaration`'s written `type` node and turns it into a
`Ty`. For a `var` declaration that node's text is `"var"`, which parses to
`Ty::reference("var")` — a type that exists nowhere — so the receiver's `MembersOf`
read answers nothing and completion yields an empty list. The deprecated engine
(`src/deprecated/types.rs` `collect_locals`, the "var locals frequently infer no
type" bundle) inferred a `var` binding's type from its initializer; the live
hub-based `completion` module never received that behaviour.

# Reproduction

In `example/greeting-app/src/main/java/com/example/app/Main.java` (or any file
with a `Widget`-like type available):

```java
var data = new Data(5);
data.   // completion request at the dot answers []
```

The regression test `completion::tests::a_var_local_receiver_infers_its_initializers_type`
(added in `src/completion.rs`) sends a `Complete` at the dot after `var w = new
Widget();` and asserts `size` and `void run()` are offered — it fails today with
`[]`.

**Expected**: `w.` (a `var` binding) resolves to the initializer's inferred type
and offers that type's members exactly as `Widget w = …; w.` does.

# Proposal

In `src/util.rs`, teach `local_type` the `var` form: when the declaration's type
node text is `"var"`, return the named declarator's initializer expression node
instead of a declared type, and have `identifier_type` resolve that initializer
through the existing async receiver classifier (`classify_receiver_at`,
depth-guarded) — so `new T()`, a method call's return type, a field access, a
chain, and `this` all infer, and an initializer the classifier cannot type (a
lambda, an unresolvable call) yields nothing rather than a guess. Because the
receiver step is shared, member completion and signature help on `var` locals
gain the behaviour together with no new hub vocabulary.

# Decisions

- **D1 — Bug, not a feature gap.** The shipped behaviour never documented `var`
  as unresolved, and the deprecated engine this live module replaces supported it;
  the report names a concrete failing site in the example workspace.
- **D2 — Local declarations only.** The reported and reproduced case is a
  `var` local. Enhanced-for `var` bindings (`for (var w : ws)`), try-with-resources
  `var` bindings, and record-pattern `var` bindings (`instanceof Type1(var val)`)
  stay out: the live hub does not resolve try-with-resources bindings at all (even
  with explicit types) and has no array/generic element-type machinery, so they are
  separate pre-existing gaps, not this defect.
- **D3 — Infer through the shared receiver classifier.** `local_type` stays a
  plain function; it hands the initializer's node up to `identifier_type`, which
  classifies it with the existing `classify_receiver_at` (depth `+ 1`, so the chain
  guard still bounds a self-referential `var x = x;`). No new index read and no new
  hub message; the mechanics are the ones `new T()`, method-call, and field-access
  receivers already use.
- **D4 — Refusal stays the failure mode.** An initializer the classifier cannot
  type (a lambda, an unresolvable call) leaves the local with no inferred type and
  completion empty — matching the documented "a receiver it cannot type yields
  nothing" and the old engine's decision.
- **D5 — The reproduction test is the regression test.** `a_var_local_receiver_infers_its_initializers_type`
  in `src/completion.rs` stays, extended with a method-call initializer
  (`var x = maker.get();`) so the inference path beyond `new T()` is covered.

# Acceptance criteria

1. `var w = new Widget(); w.` offers `Widget`'s members (fields first, then
   method overloads) exactly as `Widget w = …; w.` does; the example site
   `data.` on `var data = new Data(5);` offers `Data`'s members.
2. A `var` binding whose initializer is a resolvable call (`var x = holder.get();`
   with `get()` returning a workspace type) completes on the return type; a chain
   (`var x = holder.get().nested;`) resolves left to right.
3. An initializer the classifier cannot type (a lambda, an unresolvable call)
   yields no items — no wrong membership.
4. Signature help on a call through a `var`-declared receiver resolves like the
   explicitly typed form (`var c = calc; c.add(` lists `add`'s overloads).
5. `cargo build --all-targets`, `cargo fmt --all -- --check`, and `cargo test`
   are clean.
6. `docs/architecture.md` states that the shared receiver step resolves a
   `var` local through its initializer, and `docs/dev/changelog.md` records the
   fix.

# Docs

- `docs/architecture.md` — the Layout's `util.rs` row (a parameter or local — a
  `var` binding through its initializer's inferred type) and the completion and
  navigation implementation notes' receiver lists.
- `docs/dev/changelog.md` — the shipped entry.

# Implementation plan

## Approach

All the inference machinery already exists; only the local-declaration path
reads the wrong node. `src/util.rs::local_type` returns a `Ty` built from the
written `type` node, which for `var` is the non-type `Ty::reference("var")`.

The fix: `local_type` gains a small result shape that can hand back the
named declarator's initializer `Node` in place of a declared `Ty`, and
`identifier_type` — already async and client-carrying — classifies that node
with the existing `classify_receiver_at` (boxed, `depth + 1`), mapping the
initializer's receiver onto the binding as a value receiver (`is_type: false`;
a `var` holds a value, never names a type). The depth guard already bounds the
adversarial self-reference `var x = x;`, since each hop is `depth + 1`.

`identifier_type` gains a `depth: usize` parameter so the initializer hop is
counted against the same guard; its two callers pass the depth they hold
(`classify_receiver_at` passes its own, `navigate.rs::argument_type` passes
`0`, since it is a top-level entry). No new hub message, no new index read.

## Steps

- [x] `src/util.rs`: give `local_type` a `LocalType` result (`Declared(Ty)` for
  an explicit type — the enhanced-for arm unchanged — or `Var(Node)` for a
  `var` declaration's named declarator, taking the declarator's `value` child);
  keep `parameter_type` and the field/type fallbacks in `identifier_type` as-is.
- [x] `src/util.rs`: add the `depth` parameter to `identifier_type`; for
  `LocalType::Var` classify the initializer with
  `classify_receiver_at(client, text, package, initializer, depth + 1)` (boxed,
  like the other recursive arms) and map the resulting receiver to
  `is_type: false`, refusing (`None`) when the initializer resolves nothing.
- [x] `src/navigate.rs`: pass `0` as the depth in `argument_type`'s `identifier_type`
  call.
- [x] `src/completion.rs` tests: keep `a_var_local_receiver_infers_its_initializers_type`
  (the `new Widget()` regression test); add a method-call initializer
  (`var x = maker.get(); x.` completes on the return type) and a refusal case
  (a lambda or unresolvable-call initializer yields nothing).
- [x] `docs/architecture.md`: the Layout's `util.rs` row and the completion /
  navigation implementation notes list the `var`-initializer resolution; the
  receiver cannot-type lists stay accurate.
- [x] `docs/dev/changelog.md`: the shipped entry, and `docs/log.md` the
  reconciliation.
- [x] `cargo build --all-targets`, `cargo fmt --all -- --check`, and `cargo test`
  green (AC1–AC6).