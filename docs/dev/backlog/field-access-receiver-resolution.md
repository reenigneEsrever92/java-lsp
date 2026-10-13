---
type: ChangeRequest
kind: improvement
title: Resolve a member through a field-access receiver
description: The receiver classification has no `field_access` arm, so member completion and navigation answer nothing after a field access (`a.field.`, `this.field.`) even though a call receiver (`a.foo().`) resolves — and the navigation note now implies chains resolve in general.
state: done
verified: { by: felix, at: 2026-10-13T15:36:21Z }
priority: medium
tags: [dev, review, completion, navigation, types]
owner: felix
---

# Problem

Found reviewing the uncommitted member-completion work.

`classify_receiver` (`src/util.rs`) classifies `this`, an identifier, a
`scoped_type_identifier`, a `new T(…)`, and — since `chained-call-completion` — a
`method_invocation`; a `field_access` falls to `_ => None`. But `is_receiver_kind`
lists `field_access`, and both recovery paths return a `field_access` node when the
receiver is a field access, so the node is handed to a classifier that ignores it.
The result:

1. `a.field.` and `this.field.` offer no members (whereas `a.foo().` resolves) —
   and the same receiver step leaving nothing means `textDocument/definition` and
   `textDocument/references` on `a.field.member` answer nothing.
2. A chain that passes through a field (`a.field.c()`) breaks at the field link,
   even though the call links resolve.

Compounding it, `chained-call-completion` dropped "a chained call" from the
`navigate` implementation note's v1-limitation list in `docs/architecture.md`, so
the note now implies chains resolve in general while field links silently do not.

# Proposal

Add a `field_access` arm to `classify_receiver`, symmetric to the
`method_invocation` arm: resolve the accessed object's type (recursively,
depth-guarded), read its members, and return the named field's declared type
(`Member::access_type()`) as a value receiver; an unresolvable link returns `None`.
Then `a.field.` composes, navigation through a field access resolves, and a chain
through a field resolves left to right. Correct the navigation and completion notes
to state the remaining limitation precisely (rather than implying all chains
resolve).

# Decisions

- **D1 — `improvement`, not `bug`.** The shipped behaviour never claimed
  field-access receivers; this adds resolution where none existed, symmetric to the
  `method_invocation` work (D1 of `chained-call-completion`). The doc-revision
  oversell (Problem, last paragraph) is fixed here too, but the dominant nature is
  an enhancement.
- **D2 — Extend `classify_receiver` in `src/util.rs`.** No new hub vocabulary —
  it reuses the existing `MembersOf` read, exactly as `method_invocation` does.
- **D3 — Same depth guard and fail-safe.** An unresolvable link anywhere yields
  nothing (no wrong membership), matching the call-chain path.
- **D4 — Deliberately out of scope (unchanged):** nested-type and package
  completion, `super`, and generic type-argument substitution — the limitations the
  completion note already documents.

# Acceptance criteria

1. `a.field.` and `this.field.` offer the field's type's members; navigation on a
   member reached through a field access resolves for definition/declaration and
   references, where it previously returned nothing.
2. A chain through a field (`a.field.c()`) resolves left to right; an unresolvable
   link yields nothing.
3. `docs/architecture.md` states the field-access limitation (or its removal)
   accurately, and the `navigate`/`completion` notes no longer imply all chains
   resolve.
4. `cargo build --all-targets`, `cargo fmt --all -- --check`, and `cargo test` are
   clean.

# Docs

- `docs/architecture.md` — the navigation v1-limitation prose and the completion
  implementation note (a field-access receiver resolves like a call receiver; the
  remaining chain limitations stated exactly).

# Implementation plan

## Approach

`classify_receiver_at` (`src/util.rs`) classifies a receiver node. `method_invocation`
is handled by `method_invocation_receiver` (resolve the callee's receiver, then the
method's return type), but `field_access` falls to `_ => None`. Add a symmetric
`field_access` arm: take the node's `object` and `field` children, resolve the
object's receiver type by recursing `classify_receiver_at` (boxed, `depth + 1`), read
that type's members, find the member named by the field text whose kind is `Field`
(or `EnumConstant`), and return `Receiver { ty: member.access_type(), is_type: false }`.
An unresolvable object, or a field the type does not declare, returns `None` — the
same fail-safe the call arm uses. Because the helper is shared, `completion` and
`navigate` both gain it.

## Steps

- [x] `src/util.rs`: add the `field_access` arm (boxed recursion into
  `classify_receiver_at`, depth-guarded), mirroring `method_invocation_receiver`.
- [x] `src/completion.rs` tests: `a.field.` offers the field's type's members;
  `this.field.` resolves; a chain through a field (`a.field.c()`) resolves; an
  unresolvable link yields nothing.
- [x] `src/navigate.rs` tests: a member reached through a field access resolves for
  declaration and references.
- [x] `docs/architecture.md`: the navigation v1-limitation prose and the completion
  implementation note state the field-access resolution and the exact remaining
  chain limitations.
- [x] `docs/dev/changelog.md`: the shipped entry.
- [x] `docs/log.md`: the reconciliation.
- [x] `cargo build --all-targets`, `cargo fmt --all -- --check`, `cargo test` green.
