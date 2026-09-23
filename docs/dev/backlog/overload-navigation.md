---
type: ChangeRequest
kind: feature
title: Argument-type overload resolution for navigation
description: Go-to-definition and find-references pick the overload a call targets from its argument types, with arity as the backstop.
state: done
priority: medium
tags: [dev, navigation, types]
owner: felix
verified:
  by: cargo test --all-targets (208 passed - 178 lib, 6 bench, 23 harness, 1 stdio)
  at: 2026-09-23T22:05:00Z
---

# Problem

Go-to-definition and find-references ignore a call's arguments, so overloaded
methods are conflated:

- `definition` (`analysis.rs::definition`) resolves the word under the cursor by
  name and node kind alone. Same-file overloads collapse to the first
  declaration, and a name spread across files returns nothing.
- `references` (and the occurrence search behind it, `collect_member_occurrences`
  / `member_access_matches`) targets a member by its declaring type and name
  only, so every overload's call sites are reported together.

A call site carries exactly what is missing: the arguments. The type layer can
already infer their types (`receiver_type`) and `member_for_call` already narrows
by parameter count — only the type match itself does not exist.

# Proposal

Select the callee's overload from the call's arguments, and use it for
navigation:

1. Add an assignability relation to the type layer (`assignable(from, to)`),
   covering the conversions Java applies at a call: identity, primitive
   widening, boxing/unboxing, `null` to a reference, subtyping through the
   existing hierarchy walk, arrays, and generics by erasure.
2. At a call site, narrow the same-named methods by arity (the existing
   `member_for_call` policy), then keep those whose parameter types each accept
   the corresponding argument's inferred type; among several, take the most
   specific; when the types are inconclusive, keep the arity-level answer.
3. `definition` reuses the receiver-aware target resolution `references` already
   uses (`resolve_target` / `member_target`), narrowed to the selected overload,
   so the two finally agree.
4. `references` reports only the selected overload's occurrences.

# Decisions

- **Argument types first, arity as the backstop** (per the request): arity narrows
  the candidate set, argument types refine it, and an inconclusive type match
  keeps the arity-level answer rather than returning nothing.
- **`assignable` is a conservative approximation** — the conversions listed
  above and no more. An argument whose type cannot be inferred leaves a candidate
  *unconfirmable*, and an unconfirmable candidate is never preferred over a
  determinate one.
- **Ambiguity refuses, as everywhere else.** A call the layer cannot attribute
  to one overload gets the current conservative answer: `definition` keeps
  today's best-candidate behaviour, `references` keeps its name-based set, and
  neither invents a location.
- **`definition` is unified with `references`.** Both resolve through
  `resolve_target` / `member_target` (receiver type plus declaring type), then
  narrow by the selected overload. This replaces `definition`'s separate
  name-constraint path, so the two features answer the same question the same
  way.
- **`rename` is deliberately left name-group-wide.** It keeps renaming every
  overload of the name. A partial rename that misses a call site silently breaks
  code, and argument types cannot always attribute every occurrence (a method
  reference has no argument list), so the precise overload is a *find* concern
  here, not a *rename* one. This is a documented limitation, not an oversight;
  the occurrence search becomes overload-aware for `references` only.
- **No index schema change.** An occurrence's arguments are read from the tree
  already parsed for the search, the same on-demand parsing `references` uses
  today.

# Acceptance criteria

- With `add(int)` and `add(String)` declared, go-to-definition on `x.add(1)`
  resolves to `add(int)` and on `x.add("a")` to `add(String)`; find-references on
  `add(int)` returns only its call sites.
- When argument types cannot disambiguate (unresolvable arguments), arity
  decides; when arity cannot either, `definition` and `references` keep their
  current conservative results — never a wrong location.
- A name still spread across several files returns no location.
- `rename` continues to rename every overload of the name, with a test asserting
  it.
- Unit tests cover the assignability relation (widening, boxing, `null`,
  subtype, array, erasure) and each navigation outcome; a harness test drives
  definition and references over a fixture with overloaded calls.

# Documentation

- `docs/architecture.md` — the definition and references bullets (overload
  selection by argument types, the arity backstop, the rename limitation) and the
  type-layer bullet (the new assignability relation).
- `docs/requirements.md` — the v0.3/v0.4 notes that say overload resolution by
  argument types "stays deferred": it is now delivered for navigation, with the
  sibling completion/signature-help request covering the UX side.

# Implementation plan

## Approach

The new type-layer primitives land in `src/types.rs`; the resolution and
occurrence filtering in `src/analysis.rs`. No shell or engine-boundary change —
`definition` and `references` already exist.

- **Assignability (`src/types.rs`).** `assignable(from, to, model, package)` is a
  conservative approximation of Java's call conversions: identity, primitive
  widening, boxing/unboxing, `null` to a reference, subtyping through the
  model's hierarchy (with `java.lang.Object` implicit), arrays, and generics by
  erasure. An `Unknown` operand makes it `false`, so an unconfirmable candidate
  is never preferred over a determinate one. `member_for_arguments` narrows
  same-named methods by arity, then keeps those whose parameters accept the
  argument types, takes the most specific when several apply, and falls back to
  the arity-level `member_for_call` when the types are inconclusive.
- **The target carries the overload.** `Target` gains `overload` (the selected
  overload's parameter types) and `overload_declaration` (its located
  declaration). `member_target` sets them from the call's argument types via
  `member_for_arguments`, locating the declaration from the declaring file's own
  parameter list (`entry_method_params`/`method_params_in`/`match_declaration`);
  `declaration_target` sets them from the declaration under the cursor. A name
  with a single declaration is left un-narrowed, so single methods and record
  component accessors keep their existing bare-name behaviour.
- **Occurrence filtering.** `member_access_matches` and the unqualified branch of
  `collect_member_nodes` call `occurrence_matches_overload`: with an overload
  selected, an occurrence counts only when it is a call whose argument types
  accept the parameters (a method reference has no argument list and is left
  out). `add_declarations` reports only the selected overload's declaration.
- **`definition` is call-aware.** A method-call name resolves through the
  receiver's declaring type and the call's argument types (`call_definition`),
  returning the selected overload's location, and otherwise falls back to the
  existing name-and-kind lookup — so a bare name and the library/import paths
  are unchanged.
- **`rename` stays name-group-wide.** It clears the overload fields after
  resolving, so it renames every overload of the name and never a partial set.

## Steps

- [x] Add `assignable`, the widening/boxing helpers, `member_for_arguments`, and
      `most_specific` in `src/types.rs`; unit tests for the conversions and for
      type-then-arity overload selection.
- [x] Extend `Target` with `overload`/`overload_declaration`; thread the call's
      argument types through `member_target` and `declaration_target`, locating
      the selected declaration from the declaring file's parameter list
      (`entry_method_params`, `method_params_in`, `match_declaration`).
- [x] Filter occurrences by the selected overload (`occurrence_matches_overload`
      in `member_access_matches` and `collect_member_nodes`) and report only the
      selected declaration (`add_declarations`); keep `rename` name-group-wide.
- [x] Make `definition` call-aware (`call_definition`) for both qualified and
      unqualified calls; unit tests for type selection and the arity fallback.
- [x] Add a harness test driving `textDocument/definition` over an overloaded
      fixture.
- [x] Update `docs/architecture.md` (definition and references bullets, the type
      layer's `assignable`/`member_for_arguments`, and the removed limitation)
      and `docs/requirements.md` (overload selection no longer deferred).
- [x] Run `cargo test --all-targets` and confirm the suite passes.
