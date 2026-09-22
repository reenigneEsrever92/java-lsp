---
type: ChangeRequest
kind: bug
title: Record components are not modelled, so member completion on a record is empty
description: A source record's components are ignored by the type model and the index, so `.`-completion, hover, and navigation on a record expose no components or accessors.
state: done
priority: high
tags: [dev, types, completions, records]
owner: felix
verified:
  by: cargo test --all-targets (167 passed - 142 lib, 6 bench bin, 18 harness,
    1 stdio)
  at: 2026-09-22T21:00:16Z
---

# Problem

Dot-completion on a value whose type is a source-declared record returns an
empty list. In Java a record's components are, for a client, its accessor
methods: given `record Point(int x, int y) {}`, `p.` must offer `x()` and
`y()`. The server offers nothing, so a record behaves like a member-less type.

Both extractors read only the record's `body` and never the component list:

- `src/types.rs` — `type_info_from_declaration` collects members from the
  declaration's `body` only, and `collect_members` handles `field_declaration`
  and `method_declaration`, so a record's `parameters` (`int x, int y`) add
  nothing. Because a record's `supertypes` is empty (and the implicit
  `java.lang.Object` is deliberately not recorded), the member set comes out
  empty outright.
- `src/index.rs` — `collect_entries` likewise descends only into the record's
  `body`, so components are never indexed.

The consequences reach past completion: hover on `p.x()` renders nothing, and
definition, references, rename, and `workspace/symbol` cannot see a component.
Records reached from a jar or the JDK are unaffected — the compiled class file
really does carry the `x()` accessors — so only source records are broken,
which is exactly the case a developer is editing.

# Reproduction

```java
record Point(int x, int y) {}
class Use {
    void m() {
        Point p = null;
        p.x();
    }
}
```

Completing immediately after `p.` returns an empty list; `x` and `y` are
expected. (Verified against the current tree with a throwaway unit test in
`src/engine/syntax.rs` alongside the existing member-completion tests.)

# Proposal

Teach both extractors about a record's components, treating each component as
a member of the record:

- **Type model** (`src/types.rs`) — a record declaration's `parameters`
  contribute a component member whose access type is the component's declared
  type. To an outside receiver that member is the accessor `x()`, so it must
  be offered and resolved as a method; hovering `p.x()` must render a
  signature, and hovering the record declaration must show the components.
- **Scope** (`src/types.rs`) — `collect_field_members`, which builds the
  enclosing type's in-scope names, must also include a record's components, so
  code inside the record (a compact constructor, a custom method) resolves the
  bare component names.
- **Index** (`src/index.rs`) — each component is indexed at its position in
  the record header, so go-to-definition, find-references, rename, and
  `workspace/symbol` can target it and a `p.x()` usage resolves to that
  declaration.

# Decisions

- **Components are accessors for outside access, not fields** — Java makes the
  backing field private, so `.`-completion and hover must show `x()`, never a
  private `x` field. The bare component name is only a nameable value *inside*
  the record.
- **Full surface, per the report** — completion, hover, and
  navigation/rename are all in scope, not completion alone.
- **Source records only** — jar/JDK records already work through the class
  file's real accessor methods, so `src/classfile.rs` is untouched.
- **The `Object`/`Record` members stay out of completion lists** — this
  request does not revisit the deliberate decision to omit `java.lang.Object`
  members (see `jvm-member-descriptors`); it adds only the header's
  components. (`p.x()` typing as `String` via `toString()` is handled by the
  separate `dot-completion-and-var-inference` request.)

# Acceptance criteria

- Completing after `p.` on a source record `Point(int x, int y)` offers `x` and
  `y`, and narrows by the typed prefix.
- A record with no components offers nothing extra; a record with explicitly
  declared methods still offers them as before.
- Hovering a component accessor (`p.x()`) renders its signature; hovering the
  record declaration shows the component list.
- Go-to-definition on `p.x()`, find-references, and rename of a component
  target the component in the record header, and the component appears in
  `workspace/symbol` results.
- A component name used unqualified inside the record (compact constructor or
  method) resolves to the component.
- Regression tests cover each of the above in `src/engine/syntax.rs`,
  `src/types.rs`, and `src/index.rs`.

# Documentation impact

- `docs/architecture.md` — the `types.rs` and `WorkspaceIndex` bullets: note
  that record components are modelled as accessor members and indexed at the
  header.
- `docs/requirements.md` — the record case of declarations should be reflected
  wherever the member/type model is described.

# Implementation plan

## Approach

No new dependencies and no LSP-shape changes. Work lands in three source files,
their unit tests, and two docs. The grammar is confirmed: a
`record_declaration` carries its components in a `parameters` field (a
`formal_parameters` of `formal_parameter`s, each with `type` and `name`), and a
record body is a normal `class_body`, so the existing body walk already sees
any explicitly declared methods.

- **`src/types.rs` — the type model and the scope.** A shared helper
  `record_components(node, text)` reads a `record_declaration`'s `parameters`
  into `(name, ty)` pairs, reusing `parameter_name`/`type_from_node`.
  - `type_info_from_declaration` (~L709): after the body's members are
    collected, a record's components are pushed onto `info.methods` as
    method-like accessors — `IndexKind::Method`, the component's declared type
    as the return type, no parameters, not static. That is what a client sees:
    `.`-completion offers `x`, `p.x()` hovers and resolves as a method. Appending
    after the body walk lets an explicitly declared accessor win the name-dedupe
    in `members`. The private backing field is *not* added, so it is never
    offered to an outside receiver.
  - `scope_at` (~L1050): when the enclosing type is a record, the same
    components are added to `scope.fields` as `IndexKind::Field` members — the
    private-field view — so `resolve_name` resolves a bare component name inside
    the record (a compact constructor or a custom method).
- **`src/index.rs` — the flat index.** `collect_entries` (~L282) additionally
  indexes each record component at its source position in the header: the
  `formal_parameter` node as the full range and its name as the selection range,
  kind `IndexKind::Method` (the accessor), with the record as the container. That
  is what lets definition, references, rename, and `workspace/symbol` target the
  component and lets a `p.x()` use resolve to the header declaration.
- **`src/engine/syntax.rs` — declaration hover.** `declaration_text` (~L1826)
  appends the record's `parameters` text to the rendered declaration, so hovering
  a record's name shows `record Point(int x, int y)`. Only a record supplies a
  `parameters` field, so class/interface/enum rendering is unchanged.
- **Tests.** One unit test per acceptance criterion, in the file the criterion
  exercises: `src/types.rs` (component accessor members, an empty record adding
  nothing, and an unqualified component resolving inside the record),
  `src/index.rs` (component entries at their header positions with the record as
  container), and `src/engine/syntax.rs` (`.`-completion offers and prefix
  narrowing, hover of `p.x()` and of the declaration, go-to-definition,
  references, rename, and `workspace/symbol`).
- **Docs.** `docs/architecture.md`: the type-layer bullet notes record components
  as accessor members and the scope view inside the record; the `WorkspaceIndex`
  bullet notes components indexed at the header. `docs/requirements.md`: the v0.3
  milestone notes the record case of the member/type model.

## Steps

- [x] Model a record's components as accessor members in
      `type_info_from_declaration` via a shared `record_components` helper; leave
      the backing field out. (AC1, AC2.)
- [x] Add the same components to `scope.fields` in `scope_at` so a bare component
      name resolves inside the record. (AC5.)
- [x] Index each record component at its header position in `collect_entries`,
      kind `Method`, container the record. (AC4.)
- [x] Include a record's `parameters` in `declaration_text` so declaration hover
      shows the component list. (AC3.)
- [x] Add the `src/types.rs` unit tests: component accessor members, empty record,
      and unqualified resolution inside the record. (AC6.)
- [x] Add the `src/index.rs` unit test: component entries at the header with the
      record as container. (AC6.)
- [x] Add the `src/engine/syntax.rs` unit tests: completion offers/narrows,
      `p.x()` hover, declaration hover, definition, references/rename, and
      `workspace/symbol`. (AC6.)
- [x] Update `docs/architecture.md`: the type-layer bullet (record components as
      accessor members, scope view inside the record) and the `WorkspaceIndex`
      bullet (components indexed at the header). (AC1, AC4.)
- [x] Update `docs/requirements.md`: the v0.3 milestone notes the record case of
      the member/type model. (AC1.)
- [x] Run `cargo test --all-targets` and confirm the suite passes. (all ACs.)
