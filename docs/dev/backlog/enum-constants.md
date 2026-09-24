---
type: ChangeRequest
kind: bug
title: Source enum members (constants and declared members) are not modelled, so completion, navigation, and member resolution miss them
description: A source enum's constants and its members declared after the `;` are ignored by the index and the type model, so `DataType.` offers nothing, definition/references/rename cannot target a constant, and a reference like `DataType.TYPE_1` is reported unresolved.
state: done
priority: high
tags: [dev, types, completions, enums]
owner: felix
verified:
  by: cargo test --all-targets (313 passed; the only failures are 4 sources
    lib tests and 1 harness test that need a loopback socket this sandbox
    forbids)
  at: 2026-09-28T13:21:07Z
---

# Problem

Members declared in a workspace source enum are invisible to every type-aware
feature. Given

```java
public enum DataType {
    TYPE_1, TYPE_2
}
```

the server offers nothing after `DataType.`, cannot navigate to `TYPE_1` from a
reference, and reports `cannot resolve \`TYPE_1\``.

Both extractors read only the parts of an enum they already understand and skip
the rest:

- `src/index.rs` — `collect_entries` handles `class/interface/enum/record`
  declarations, methods, constructors, fields, and imports, but has no
  `enum_constant` arm, so `TYPE_1`/`TYPE_2` are never indexed. (It recurses into
  every node, so a member declared after the `;` _is_ indexed.)
- `src/types.rs` — `collect_members` reads `field_declaration` /
  `constant_declaration`, `method_declaration`, and `constructor_declaration`,
  never `enum_constant`, and never descends into an enum's
  `enum_body_declarations` — the `;`-introduced section holding the members
  declared after the constants. So an enum's `TypeInfo` holds neither its
  constants nor any field, method, or constructor declared after the `;`.
- `src/analysis.rs` — `collect_type_fields`, which names the enclosing type's
  fields for unqualified completion, has the same gap for constants.

The consequences follow from those: `.`-completion (`member_items`) is built
from the model's members and returns empty; go-to-definition/references/rename
(`member_target`) need both the model member and the index entry, so a constant
finds nothing; and `SemanticCheck::check_member` reports `cannot resolve
\`TYPE_1\``because`member_of`finds no such member on the enum receiver. The
missing`enum_body_declarations`walk additionally drops an enum's own methods
and fields from the model, so`c.rank()` on an enum value completes nothing
either.

Enums reached from a jar or the JDK are unaffected: their constants and members
are real fields/methods in the class file and already come through the
class-file parser, so only workspace **source** enums — exactly the ones being
edited — are broken. (Semantic tokens already emit `enumMember` for an
`enum_constant`; only the index and the type model never learned about them.)

# Reproduction

```java
// p/DataType.java
public enum DataType {
    TYPE_1, TYPE_2
}

// p/Use.java
class Use {
    void m() {
        var x = DataType.TYPE_1;   // "cannot resolve `TYPE_1`"
    }
}
```

Completing immediately after `DataType.` returns an empty list; go-to-definition
on `TYPE_1` returns nothing; the reference is flagged unresolved. Confirmed
against the current tree with a throwaway harness. An enum with a declared
member (`enum Color { RED; int rank() {…} }`) likewise completes nothing for
`c.`.

Expected: `DataType.` offers `TYPE_1` and `TYPE_2`; `TYPE_1` is not flagged;
definition/references/rename target the constant's declaration; an enum's
declared methods and fields complete as well.

# Proposal

Model a source enum's members as members of the enum, in all three places that
model declarations:

- **Index** (`src/index.rs`) — `collect_entries` gains an `enum_constant` arm
  that indexes each constant at its declared name, kind `IndexKind::EnumConstant`,
  with the enum as the container, so definition, references, rename, and
  `workspace/symbol` can target it.
- **Type model** (`src/types.rs`) — `collect_members` adds an `enum_constant`
  arm pushing a `Member` whose kind is `IndexKind::EnumConstant`, whose access
  type is the enum itself (so `DataType.TYPE_1.rank()` chains), and which is
  static; and it descends into an enum's `enum_body_declarations`, so the
  members declared after the `;` are collected too.
- **Scope** (`src/analysis.rs`) — `collect_type_fields` adds `enum_constant`, so
  a constant is nameable unqualified inside its own enum body.

The new `IndexKind::EnumConstant` is threaded through the kind maps
(`completion_kind` → `ENUM_MEMBER`, `symbol_kind` → `ENUM_MEMBER`, `kind_word`)
and through member-target resolution, so a constant behaves as a member for
navigation and diagnostics.

# Decisions

- **D1 — A dedicated `IndexKind::EnumConstant`.** Constants are not plain
  fields: they complete as `ENUM_MEMBER`, render as "enum constant" in hover,
  and deserve their own icon. A distinct kind keeps that visible rather than
  conflating them with `Field`.
- **D2 — A constant's type is its enum.** `DataType.TYPE_1` has type `DataType`,
  so a chained access such as `TYPE_1.rank()` resolves; the member is static,
  matching Java.
- **D3 — Qualified and unqualified.** Both `DataType.TYPE_1` and a bare `TYPE_1`
  inside the enum body resolve.
- **D4 — Arguments and bodies are kept.** A constant written `TYPE_1(1) { … }`
  is still indexed by its name; only the name matters here.
- **D5 — The declaration section is walked.** `collect_members` descends into
  `enum_body_declarations` so an enum's own fields, methods, and constructors
  are modelled — without it an enum value's members are missing from completion
  and hover.
- **D6 — Implicit `values()`/`valueOf(String)` are out of scope.** They are
  synthesized methods, not constants; adding them is a separate concern.
- **D7 — Document-symbol outline entries for constants are out of scope.** The
  outline lists types/methods/fields today; listing constants there is a
  presentation change beyond this defect.
- **D8 — Source enums only.** Jar/JDK enums already yield their members through
  the class file; `src/classfile.rs` is untouched.

# Acceptance criteria

- Completing after `DataType.` offers `TYPE_1` and `TYPE_2` as
  `CompletionItemKind::ENUM_MEMBER`, narrowed by the typed prefix.
- `DataType.TYPE_1` produces no `cannot resolve` diagnostic, while a missing
  constant (`DataType.NOPE`) is still reported.
- Go-to-definition, find-references, and rename on `DataType.TYPE_1` target the
  constant's declaration; the constant appears in `workspace/symbol` results.
- A bare `TYPE_1` used inside its own enum body resolves.
- A constant declared with arguments/body (`TYPE_1(1) { … }`) is still offered
  and resolvable by name.
- A member declared after the enum's `;` (a method or field) completes on an
  enum value and resolves.
- Regression tests cover each of the above in `src/index.rs`, `src/types.rs`,
  and `src/analysis.rs`.

# Documentation impact

- `docs/architecture.md` — the `types.rs` bullet and the `WorkspaceIndex`
  bullet: note that enum constants are modelled as static members (kind
  `EnumConstant`) and indexed at their declaration.

# Implementation plan

## Approach

No new dependencies and no LSP-shape changes. The grammar is confirmed: an
`enum_declaration`'s `body` is an `enum_body` whose named children are
`enum_constant` nodes (each with a `name` field) followed by an optional
`enum_body_declarations` (a `;` then ordinary class-body declarations). One
variant is added to `IndexKind`; every exhaustive match on it is updated, and a
constant is non-`Method` so the existing field-like member handling applies.

- **`src/index.rs` — the flat index.** Add `IndexKind::EnumConstant`. In
  `collect_entries` an `enum_constant` arm indexes the constant at its name,
  kind `EnumConstant`, the enum its container. The existing recursion already
  covers `enum_body_declarations`.
- **`src/types.rs` — the type model.** `collect_members` gains an
  `enum_constant` arm (a static `Member` typed as the enum) and an
  `enum_body_declarations` arm that recurses, so declared members are collected.
- **`src/analysis.rs` — the scope, the kind maps, and resolution.**
  `completion_kind`/`symbol_kind`/`kind_word` map the new kind
  (`ENUM_MEMBER`/`ENUM_MEMBER`/"enum constant"); `member_hover` reports "enum
  constant"; `member_items` offers a constant as `ENUM_MEMBER`. `member_declarations`
  takes a set of acceptable kinds, and a field access admits
  `Field | EnumConstant` (a call admits `Method`); `declaration_target` gains an
  `enum_constant` arm; the plain-lookup definition path's `Member` constraint and
  the completion label/detail/import helpers admit `EnumConstant`;
  `collect_type_fields` handles `enum_constant`.
- **Tests.** One unit test per acceptance criterion: `src/index.rs` (constants
  indexed at their declarations, including a nested enum's chain), `src/types.rs`
  (a constant is a static member typed as the enum, declared members are
  modelled), and `src/analysis.rs` (`. `-completion, no false diagnostic plus a
  true one, unqualified resolution, and definition).
- **Docs.** `docs/architecture.md`: the `types.rs` and `WorkspaceIndex` bullets.

## Steps

- [x] Add `IndexKind::EnumConstant` and index each `enum_constant` in
      `collect_entries`. (AC1, AC3.)
- [x] Model a constant as a static member of its enum in `collect_members`, typed
      as the enum, and recurse into `enum_body_declarations`. (AC1, AC2, AC5,
      AC6.)
- [x] Thread the kind through `completion_kind`, `symbol_kind`, `kind_word`,
      `member_hover`, `member_declarations`, `declaration_target`, the definition
      `Member` constraint, and the completion label/detail/import helpers. (AC1,
      AC2, AC3.)
- [x] Add `enum_constant` to `collect_type_fields`. (AC4.)
- [x] Add the `src/index.rs`, `src/types.rs`, and `src/analysis.rs` unit tests.
      (AC7.)
- [x] Update `docs/architecture.md` (`types.rs` and `WorkspaceIndex` bullets).
      (AC1, AC3.)
- [x] Run `cargo test --all-targets` and confirm the suite passes. (all ACs.)
