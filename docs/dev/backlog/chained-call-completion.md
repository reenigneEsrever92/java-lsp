---
type: ChangeRequest
kind: feature
title: Complete members on a call's result
description: A chained receiver (`a.foo().`) is deliberately unresolvable today, so completion — and navigation — after a call's result answers nothing; resolve a method-invocation receiver's return type through the shared receiver helper so `a.foo().` completes on the returned type.
state: done
verified: { by: felix, at: 2026-10-13T15:06:50Z }
priority: medium
tags: [dev, feature, completion, types]
owner: felix
---

# Problem

The receiver step resolves the shapes listed in `receiver-typed-member-navigation`
D1 and returns `None` for a chained receiver — a `field_access` or
`method_invocation` before the `.` — which was an explicit, documented limitation.
The consequence is that `a.foo().` (and `Foo.bar().`) resolve nothing: member
completion after a call's result is empty, and navigation of a member reached
through a call finds no location. Chaining is common in Java (a builder or a
fluent API is written as a chain), so this is a real day-to-day gap left by
`member-completion`.

# Proposal

Extend the shared receiver-resolution helper (the `src/util.rs` introduced by
`member-completion`) to resolve a `method_invocation` receiver to the callee's
return type, recursively for the receiver before the callee so a longer chain
resolves left-to-right. Then `a.foo().` completes on `foo`'s return type, and a
member reached through a chain navigates. Because the helper is shared, completion
and navigation both gain the resolution.

# Decisions

- **D1 — The chained shape is `method_invocation`.** A `recv.foo(args)` receiver
  resolves to the member `foo`'s declared return type; the receiver before `foo`
  is resolved the same way, so `a.b().c().` resolves left to right.
- **D2 — Shared helper, so navigation gains it too.** Landed in the
  `member-completion` `util.rs`; both completion and navigation read through it.
  This deliberately narrows `receiver-typed-member-navigation` D1's "chained calls"
  out-of-scope note — recorded as an intended extension, not an accident.
- **D3 — Return types are taken as the model holds them.** No generic
  substitution beyond what the index already carries: a call whose return type is
  a type variable stays unresolved (matching `member-completion`'s generic
  limitation), rather than guessing.
- **D4 — Fail-safe.** A chain with any unresolvable link yields no members —
  never a wrong membership — matching the existing refusal policy.
- **D5 — Depends on `member-completion`.** Its `util.rs` extraction and the
  overload-preserving members read are prerequisites.

# Acceptance criteria

1. `list.get(0).` completes on the element type where the model resolves it;
   `Foo.bar().` completes on `bar`'s return type; a longer chain resolves
   left-to-right.
2. A member reached through a chained receiver navigates: `textDocument/definition`
   and `textDocument/declaration` on it resolve, and `textDocument/references`
   reports its occurrences, where they previously returned nothing.
3. A chain with an unresolvable link yields nothing (completion empty, navigation
   `null`).
4. `cargo build`, `cargo fmt --check`, and `cargo test` are clean; the existing
   `navigate`/`completion` tests are unchanged except where a chained-receiver case
   is added.

# Docs

- `docs/architecture.md` — the navigation limitation prose (the "remaining
  limitations" list drops chained calls) and the completion implementation note.
- `docs/dev/changelog.md` — the shipped entry.
- `docs/log.md` — the reconciliation.

# Implementation plan

## Approach

The classification added by `member-completion` (`util::classify_receiver`) maps a
receiver *node* to a type. It handles `this`, an `identifier` (a value or a type
qualifier), a `type_identifier`/`scoped_type_identifier`, and a `new T(...)`;
everything else — a chained `field_access`/`method_invocation` — falls to `None`.
This request adds the `method_invocation` arm: resolve the callee's own receiver
(its `object`, or the enclosing type for an unqualified call), read that
receiver's members over the hub, find the callee by name, and return the member's
`access_type()` (its declared return type). Because the helper is shared, both
`completion` and `navigate` gain the resolution; the recursion into the callee's
receiver is what makes `a.b().c().` resolve left to right, so cap it with a small
depth guard. An unresolvable link anywhere returns `None`, keeping the fail-safe.

## Steps

- [x] `src/util.rs`: add the `method_invocation` arm to `classify_receiver` —
  resolve the callee's receiver recursively, find the callee member by name, and
  return `Member::access_type()`; add a recursion-depth guard.
- [x] `src/completion.rs` tests: `list.get(0).` and `Foo.bar().` complete on the
  returned type; an unresolvable chain yields nothing.
- [x] `src/navigate.rs` tests: a member reached through a chained receiver
  resolves for declaration and references (previously nothing).
- [x] `docs/architecture.md`: drop chained calls from the navigation
  "remaining limitations" prose and note the shared helper's extension in the
  completion implementation note.
- [x] `docs/dev/changelog.md`: the shipped entry.
- [x] `docs/log.md`: the reconciliation.
- [x] `cargo build --all-targets`, `cargo fmt --all -- --check`, `cargo test`
  green (AC1–AC4).
