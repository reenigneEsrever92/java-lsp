---
type: ChangeRequest
kind: bug
title: Member completion after a dot is empty for dotted nested-type receivers and partial type names
description: Completion after `.` returns nothing when the receiver is a dotted nested-type name (`Outer.Inner.`) or when the name being typed parses as a type (`SumType.T…`, `new SumType.T…`); the tree at HEAD also fails to compile.
state: done
priority: high
tags: [dev, bug, completions, types]
owner: felix
verified:
  by: cargo test --all-targets (283 lib, 28 harness, 1 stdio, 7 bench passed;
    the only failures are 4 sources lib tests and 1 harness test that need a
    loopback socket this sandbox forbids)
  at: 2026-09-28T14:50:20Z
---

# Problem

`.`-completion works only when the receiver is a single identifier — a local,
field, or simple type name — and either the cursor sits immediately after the dot
or the partially typed name reads as a _value_ member (`field_access` /
`method_invocation`). Two reachable shapes return an empty list instead, which is
what "auto-complete only works right behind the dot" describes.

**A dotted receiver is not resolved as a type.** A receiver written the Java way
for a nested type — `Greeter.Inner.`, `SumType.Type1.` — is read by
`receiver_type_unqualified` (`src/types.rs`) as a _member chain_: its
`scoped_identifier`/`scoped_type_identifier` arm, and its `field_access` arm (the
shape a dotted name takes in expression position), both treat the last segment as
a _member_ of the qualifier's type. Nested types are deliberately not modelled as
members (they are listed separately by `member_items`), so `member_of(Greeter,
"Inner")` finds nothing and the receiver's type comes out `Unknown` — an empty
list, even with the cursor directly after the dot. This covers inner classes
(`Greeter.Inner.`) and any static member reached through one
(`Greeter.Inner.CONST`, `SumType.Type1.<static>`).

**A partially typed dotted name loses its receiver.** `receiver_before_dot`
(`src/analysis.rs`) locates the receiver at `offset - 1` — the cursor — rather than
at the `.` that precedes the typed prefix. When the partial name parses as a value
member the climb still recovers a `field_access`/`method_invocation` object and
works; when the parser reads the whole dotted name as a _type_ — which is what an
uppercase-first name after a type name produces — the node is a
`scoped_type_identifier` with no member-access ancestor, and the source fallback
`receiver_before_dot_from_text` uses the same cursor-relative `end`, returning the
segment _after_ the dot instead of the qualifier. So `Greeter.Inn` in a declaration,
and `new Greeter.Inn` / `new SumType.T…`, complete to nothing.

**The tree at HEAD does not compile.** `src/messages.rs:207` declares
`model: Arc<Projectodel>` instead of `ProjectModel`, introduced by the last commit
(`5f8ecc7`), so `cargo build` fails and no behaviour can be verified. It is folded
into this request as a required first step.

**Distinct same-kind types in one package collapse in a model-wide scan.**
`all_types` identifies a type by `(package, kind, owner)` — without its name — in
`SourceLayerIndex` and `TypeQuery`, so a scan that lists a package or a type's
nested types drops all but one of several same-kind siblings: `java.util.Li` would
offer only one of `List`/`Map`, and `Greeter.` only one of two nested classes. The
package listing this request adds is built on that scan, so its key gains the name.

With the reported sum type (a `sealed interface SumType` with nested records
`Type1` through `Type4`), `SumType.` lists the four nested records, but typing any
of them out (`SumType.T…`, `new SumType.T…`) completes to nothing.

# Reproduction

`cargo build` fails on `Projectodel`. With that one-character fix applied, in the
open document:

```java
class Greeter {
    static class Inner {
        static final String CONST = "c";
        int getVal() { return 1; }
    }
    static int count(int n) { return n; }
    void m() {
        Greeter.Inner i = new Greeter.Inner(); // `i.` completes (works)
        int c = i.getV;                         // completes `getVal` (works)
        int d = Greeter.cou;                    // completes `count` (works)
        Greeter.Inn x1;                         // nothing (should offer Inner)
        var made = new Greeter.Inn;             // nothing (should offer Inner)
        int h = Greeter.Inner.CON;              // nothing (should offer CONST)
    }
}
```

Verified against the tree with a throwaway unit test in `src/analysis.rs`,
replaced by the regression tests below. The controls all pass — `Greeter.` offers
`count` and `Inner`, `i.` offers members, `i.getV` narrows to `getVal`,
`Greeter.cou` to `count` — which isolates the two causes: a dotted receiver read as
a type, and a partial name read as a type.

# Proposal

- **Fix the build.** Correct `ProjectModel` in `src/messages.rs`.
- **Recover the receiver at the dot, not the cursor** (`src/analysis.rs`).
  `receiver_before_dot` (and its `member_items` caller) must locate the receiver
  ending at the `.` that precedes the typed prefix — the byte the prefix starts
  after — instead of at `offset - 1`. The prefix is already known to `completions`,
  so the dot index is `offset - prefix.len() - 1`; taking the outermost
  receiver-kind node that ends there yields the qualifier in every shape
  (`i.getV` → `i`, `Greeter.Inn` → `Greeter`, `list.get(0).ru` → `list.get(0)`),
  removing the off-by-one the text fallback inherits from the cursor-after-dot
  assumption. `member_items` also treats _any_ receiver that resolves to a type as
  a type for the static-only filter — including the `field_access` shape a dotted
  nested name takes in expression position — so `Greeter.Inner.` offers only statics
  (and its nested types), never instance members.
- **Resolve a dotted receiver as a type when it names no member** (`src/types.rs`).
  `receiver_type_unqualified` keeps the member-chain reading of a dotted name (its
  `scoped_*` and `field_access` arms), but when that finds no member it resolves
  the _whole_ dotted name as a type — nested-first, through the same
  `resolve_type_info`/`TypeLookup::lookup` path — so `Greeter.Inner` and
  `SumType.Type1` type-resolve and `.`-completion lists their members.
- **Key `all_types` by name too** (`src/types.rs`). `SourceLayerIndex::all_types`
  and `TypeQuery::all_types` add the type's name to their dedupe key, so a scan
  keeps every distinct same-kind sibling instead of collapsing them per package.
- **Complete a package-qualified name** (`src/types.rs`/`src/analysis.rs`). When the
  receiver before the dot is a package rather than a type (`java.util.Li`), offer
  that package's types filtered by the prefix, so a qualified reference completes
  like an unqualified one.

# Decisions

- **All points ship as one request**, per the report; the build fix is a
  prerequisite, not a separate change.
- **The two completion symptoms are one defect family** — a dotted name read as a
  type — split across the receiver lookup (position) and type resolution
  (semantics); both are fixed, since either alone still leaves a reported case
  empty.
- **Package-qualified completion is in scope**, confirmed in discussion; it shares
  the dotted-receiver lookup and must not regress the nested-type path. If listing a
  package's types needs a new model query that turns out impractical, this point is
  revisited before implementing rather than dropped silently.
- **Refusal stays the failure mode** for a receiver whose type genuinely cannot be
  inferred (an unknown receiver still yields no members), consistent with
  `dot-completion-and-var-inference`.
- **A type's name is part of its identity in a scan** — `all_types` deduped
  distinct same-kind siblings (two nested classes, two package members) because its
  key omitted the name; including it is what lets the package listing and
  `nested_types` report every match.
- **The example's package/directory mismatch is left as-is** — `SumType.java`
  declares `package one.example.com` while sitting under `com/example/greeting/`;
  it is self-consistent with `Main`'s imports and unrelated to the defect.

# Acceptance criteria

- `cargo build --all-targets` (and `cargo test --all-targets`) compiles cleanly.
- `Greeter.Inner.` offers `Inner`'s static members (e.g. `CONST`), and
  `Greeter.Inner.CON…` narrows to `CONST`; an instance receiver of the same type
  (`i.`) still offers instance members.
- While typing a nested-type name after a dot — `Greeter.Inn`, `SumType.T…`, and
  the `new Greeter.Inn` / `new SumType.T…` forms — the qualifier's nested types and
  members are offered, narrowed by the prefix, exactly as when the cursor sits on
  the dot.
- `SumType.`, `SumType.T…`, and `new SumType.T…` all offer `Type1..Type4`; an
  instance of a nested record (`sumType.`) offers its component accessor (`val`).
- `java.util.Li` (a package-qualified prefix) offers `List` and the package's other
  matching types.
- The existing controls stay green: `Greeter.` offers `count` and `Inner`, `i.`
  offers `Inner`'s members, `i.getV` narrows to `getVal`, `Greeter.cou` to `count`,
  and an unknown receiver still yields an empty list.
- Regression tests cover each case in `src/analysis.rs` (receiver recovery,
  completion) and `src/types.rs` (dotted type receiver, package receiver).
- Two same-kind nested types under one owner are both offered (`Greeter.` lists
  `Inner` and `Innermost`), and a package's several types are all listable.

# Documentation impact

- `docs/architecture.md` — the `TreeSitterEngine` completions paragraph (receiver
  recovery at the dot) and the `types.rs` bullet (a dotted receiver resolves as a
  type nested-first; a package receiver lists its types).
- `docs/requirements.md` — the completions wording, extended to named types
  (nested and package-qualified) after a dot.

# Implementation plan

## Approach

No new dependencies and no LSP-shape changes. Work lands in `src/messages.rs` (the
build), `src/analysis.rs` (receiver recovery and completion), `src/types.rs`
(dotted-receiver typing and a package query), their unit tests, and two docs.

- **Build (`src/messages.rs`).** `Projectodel` → `ProjectModel`; nothing else.
- **Receiver recovery (`src/analysis.rs`).** `receiver_before_dot` locates the `.`
  before the typed prefix — `offset - word_prefix(text, offset).len() - 1` —
  instead of the cursor, so a partial name the parser reads as a type no longer
  hides its qualifier. The `field_access`/`method_invocation` walk uses that dot,
  returning the object only when it ends exactly at the dot;
  `receiver_before_dot_from_text` takes the dot index as the receiver's exclusive
  end and drops the off-by-one (its `end` is the exclusive end `parent.end_byte()`
  is compared against), so `Greeter.Inn` → `Greeter` and `new Greeter.Inn` →
  `Greeter`.
- **Completion (`src/analysis.rs`).** `member_items` treats any receiver that
  resolves to a type as a _type_ for the static-only filter, whatever node shape
  the parser gave it — a plain name, a scoped name, or the `field_access` a dotted
  nested name becomes in expression position (so `Greeter.Inner.` offers only
  statics and nested types, never instance members) — and when the receiver
  resolves to no type it offers the types of a package of that name, filtered by
  the prefix (`java.util.Li` → `List`), inserted as the simple name since the
  qualifier is already written.
- **Dotted-receiver typing (`src/types.rs`).** `receiver_type_unqualified`'s
  `scoped_*` and `field_access` arms keep their member-chain reading, but when it
  finds no member a new `dotted_name_type` helper resolves the whole dotted name as
  a type through `resolve_type_info` (nested-first via `TypeLookup::lookup`),
  preserving the dotted spelling so lookup disambiguates a nested type. This is what
  makes `Greeter.Inner`/`SumType.Type1` a typed receiver.
- **Package query (`src/types.rs`).** A default `TypeLookup::types_in_package`
  over `all_types()` (built beside `nested_types`), so `member_items` can list a
  package's types without a new name index.
- **Docs.** `docs/architecture.md` — the `TreeSitterEngine` completions paragraph
  (receiver recovery at the dot) and the `types.rs` bullet (a dotted receiver
  resolves as a type nested-first; a package receiver lists its types).
  `docs/requirements.md` — the `var`/completions wording, extended to named-type
  receivers.

## Steps

- [x] Fix the `ProjectModel` typo in `src/messages.rs`. (AC1.)
- [x] Recover the receiver at the dot in `receiver_before_dot` and
      `receiver_before_dot_from_text` (`src/analysis.rs`). (AC3, AC4.)
- [x] Make `member_items` treat a type-receiver (whatever its node shape) as a
      type and fall back to a package's types when the receiver resolves to none.
      (AC2, AC5.)
- [x] Resolve a dotted receiver as a type when the member-chain reading finds
      nothing, in `receiver_type_unqualified` (`src/types.rs`). (AC2, AC4.)
- [x] Add the type's name to the `all_types` dedupe key in `SourceLayerIndex` and
      `TypeQuery`. (AC8.)
- [x] Add the `types_in_package` default to `TypeLookup` (`src/types.rs`). (AC5.)
- [x] Add `src/analysis.rs` unit tests: `Greeter.Inn` and `new Greeter.Inn` offer
      `Inner`; `Greeter.Inner.` offers `CONST` but not `getVal`; `Greeter.Inner.CON`
      narrows to `CONST`; two same-kind nested types are both offered; a partial
      member narrows; `java.util.Li` offers `List`; the controls stay green.
      (AC2–AC8.)
- [x] Add `src/types.rs` unit tests: a dotted nested type and a package-qualified
      name type as receivers, and `types_in_package` lists only its package's
      top-level types. (AC2, AC4, AC5, AC7, AC8.)
- [x] Update `docs/architecture.md`. (AC1, AC2, AC3, AC5.)
- [x] Update `docs/requirements.md`. (AC3, AC4.)
- [x] Run `cargo test --all-targets`: 283 lib, 28 harness, 1 stdio, and 7 bench
      tests pass; the only failures are the 5 that need a loopback socket this
      sandbox forbids. (all ACs.)
