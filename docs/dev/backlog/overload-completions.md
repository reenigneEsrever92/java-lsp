---
type: ChangeRequest
kind: feature
title: Overload-aware completions and signature help
description: List each method overload separately in completions, render its full signature, and serve textDocument/signatureHelp.
state: done
priority: high
tags: [dev, completions, signature-help]
owner: felix
verified:
  by: cargo test --all-targets (203 passed - 174 lib, 6 bench, 22 harness, 1 stdio)
  at: 2026-09-23T21:30:00Z
---

# Problem

Method completion cannot tell overloads apart. Member completion after `x.`
(`analysis.rs::member_items`) dedupes by `(name, is_method)` and pushes one item
per name, and the workspace-index source dedupes by name plus import; underneath
both, `TypeLookup::members` (`src/types.rs`) itself collapses same-named members
by `(name, is_method)`. A receiver declaring `add(int)` and `add(int, int)`
therefore offers a single `add`, with the full signature hidden in `detail` and
no way to choose between them. The client also cannot show argument help: the
server never advertises `textDocument/signatureHelp`, so the editor has no
signature to display while the cursor is inside a call.

# Proposal

Three related changes to method completion:

1. Member completion after `x.` offers one item per distinct overload, each
   labeled with its full signature (`int add(int a, int b)`), inserting `add(`
   on accept.
2. The workspace-index completion source — which is where unqualified method
   names come from, since the scoped source covers only parameters, locals,
   fields, and type names — splits method overloads the same way, rendering each
   overload's parameter list from the type model by the entry's owning type.
3. The server serves `textDocument/signatureHelp`: the callee's overloads
   rendered as `foo(int a, int b)`, with `activeParameter` derived from the
   cursor's position within the argument list.

# Decisions

- **Both completion paths** (per the request): member completion
  (`member_items`) and the workspace-index source (`completions`, source 3) both
  list overloads. The index holds one `SymbolEntry` per declaration and no
  parameter list, so the index path looks each overload up in the type model by
  the entry's container type.
- **Overloads must become visible to completion.** `TypeLookup::members` folds
  same-named members by `(name, is_method)`, which is why one `add` appears
  today. Add an overload-preserving listing that walks the hierarchy as
  `members` does but dedupes by name, kind, and parameter signature, so a true
  duplicate (an override) still collapses while distinct overloads stay
  separate.
- **Label, filter, and insertion.** `label` = `Member::signature()` (the full
  signature, which already omits an unknown return type rather than rendering
  `?`); `filter_text` = the bare name, so typing the name still filters;
  `insert_text` = `name(`, so accepting the item opens the argument list and the
  client's signature help can engage. `detail` carries the declaring type, which
  is what separates an inherited or imported overload from an own one. Note that
  the shared `offer` helper currently sets `filter_text` to `insert_text`; method
  items need the two to differ.
- **Fields and non-overloaded members are unchanged** — one item, label = name.
- **Ordering** stays ranked (`sort_text` `0`/`1`/`2`); items that share a name
  are ordered by parameter count so the list is stable.
- **Signature help is conservative.** It renders from the type model, so it
  works for workspace, dependency-source, and class-file types alike; when the
  callee or the receiver's type cannot be resolved it returns no signatures
  rather than a guess. `activeParameter` is the argument index the cursor sits
  in, counted from the call's argument list; the provider triggers on `(` and
  `,`.
- **No `SemanticEngine` trait to touch.** The engine sits behind the channel
  boundary (`src/engine.rs`), so signature help adds a `Command`, an
  `EngineHandle` method, a `dispatch` arm, and a shell handler, like every other
  query.

# Acceptance criteria

- Member completion after `.` on a type declaring `add(int)` and `add(int, int)`
  offers two items, each showing its full signature, and accepting one inserts
  `add(`.
- The workspace-index source likewise offers each distinct overload of a method
  (distinct signatures), not one collapsed entry.
- An inherited overload and an own overload with the same signature are still
  deduplicated.
- Fields and non-overloaded methods produce exactly one item, as today.
- `textDocument/signatureHelp` returns the callee's overload signatures with the
  correct `activeParameter`, and nothing when the callee cannot be resolved; the
  shell advertises `signatureHelpProvider`.

# Documentation

- `docs/architecture.md` — the completions bullet (per-overload items, labels,
  insertion) and the shell capability list, plus a new signature-help paragraph.
- `docs/requirements.md` — the wording around completions, which now render real
  method signatures rather than names alone.

# Implementation plan

## Approach

The analysis lives in `src/analysis.rs` and the type layer in `src/types.rs`;
signature help adds one command to the engine boundary (`src/engine.rs`) and one
handler plus capability to the shell (`src/server.rs`). No new dependency.

- **Overload-preserving member listing (`src/types.rs`).** `TypeLookup::members`
  folds same-named members by `(name, is_method)`, which is why overloads never
  reach completion. Add a provided trait method `members_with_overloads` that
  walks the hierarchy exactly as `members` does but dedupes by name, kind, and
  parameter types; rebuild `members` on top of it by collapsing to the first per
  name, so typing semantics are unchanged.
- **Member completion (`member_items`).** Iterate `members_with_overloads`; for
  a method emit `label` = `Member::signature()`, `filter_text` = the bare name,
  `insert_text` = `name(`, no `detail`; for a field keep label/insert/filter =
  name and `detail` = `signature()`. The dedup key becomes the signature for
  methods and the name for fields.
- **Workspace-index completion (source 3 of `completions`).** For a `Method`
  entry, resolve the owning type in the type model by `entry.container.last()`
  and `entry.package` and emit one item per declared overload (signature label,
  `name(` insertion, `method of <container>` detail, the same auto-import edit),
  falling back to the current single name-only item when the model cannot name
  the owner. `offer` gains a separate `filter_text` so a method's filter is its
  name while its insertion adds `(`.
- **Signature help.** `signature_help(uri, position)` finds the innermost
  `method_invocation` whose argument list contains the cursor, infers the
  receiver type (the enclosing type for an unqualified call), and renders every
  overload of the callee's name as a `SignatureInformation` labelled with
  `Member::signature()`. `activeParameter` is the count of argument nodes ending
  before the cursor, clamped to the arity. Unresolved receiver or callee →
  `None`; constructors are not modelled, so `new T(...)` answers nothing.
- **Boundary wiring.** `src/engine.rs` gains `Command::SignatureHelp`, an
  `EngineHandle::signature_help`, and a `dispatch` arm; `src/server.rs`
  advertises `signatureHelpProvider` (trigger characters `(` and `,`) and
  implements `signature_help`.

## Steps

- [x] Add `TypeLookup::members_with_overloads` in `src/types.rs` and rebuild
      `members` on it; unit tests that overloads stay apart, an override and an
      inherited duplicate still collapse, and `members` is unchanged.
- [x] Rework `member_items` in `src/analysis.rs` for per-overload items with
      signature labels and `name(` insertion; unit tests for two overloads, the
      insertion/filter texts, and an unchanged field.
- [x] Rework the workspace-index source in `completions` to expand method
      overloads from the model (fallback to the name-only item), giving `offer`
      a separate `filter_text`; unit tests for the expansion and the fallback.
- [x] Add `signature_help` (with `enclosing_call`/`active_parameter` helpers) to
      `src/analysis.rs`; unit tests for overload signatures, `activeParameter`
      at two positions, and an unresolved-callee `None`.
- [x] Add `Command::SignatureHelp` to `src/engine.rs`, and the provider plus
      `signature_help` handler to `src/server.rs`; extend the harness capability
      assertion and add an end-to-end harness test.
- [x] Update `docs/architecture.md`: the completions bullet (per-overload items,
      signature labels, `name(` insertion), the shell capability list, and a new
      signature-help paragraph. Update `docs/requirements.md`'s completions
      wording to note real method signatures.
- [x] Run `cargo test --all-targets` and confirm the suite passes.
