---
type: ChangeRequest
kind: bug
title: Record components are not modelled, so member completion on a record is empty
description: A source record's components are ignored by the type model and the index, so `.`-completion, hover, and navigation on a record expose no components or accessors.
state: proposed
priority: high
tags: [dev, types, completions, records]
owner: felix
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
