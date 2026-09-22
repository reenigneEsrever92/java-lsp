---
type: ChangeRequest
kind: feature
title: Type-aware semantic engine
description: A pure-Rust type layer resolving declared types and members, bringing type-aware hover and conservative semantic diagnostics.
state: done
priority: medium
tags: [dev, engine, architecture, types]
owner: felix
verified:
  by: cargo test (109 passed - 88 lib, 6 bench bin, 14 harness, 1 stdio) +
    java-lsp-bench --files 5 (post-warm-up hover resolves the fixture class)
  at: 2026-09-22T15:42:31Z
---

# Problem

Syntax and index features have a ceiling: type-aware hover, accurate
diagnostics, member completions after `x.`, and rename all require real type
resolution (R7). The init conversation deliberately postponed choosing the
approach so that v0.1 ships pure Rust first.

# Proposal

Build a pure-Rust type layer over the existing syntax tree and workspace index,
implemented behind the `SemanticEngine` trait with **no changes to the LSP
shell**. It resolves declared types, their members, and (for the common cases)
the type of a receiver expression, and uses that to deliver:

1. **Type-aware hover** — the symbol under the cursor resolves to its
   declaration and is rendered with its real signature/type, instead of the
   current hardcoded `None`.
2. **Member completions after `.`** — the receiver's type is inferred and its
   members (including inherited ones) are offered; an unresolvable receiver
   still returns an empty list.
3. **Conservative semantic diagnostics** — unresolved type references are
   reported alongside parse errors, using the index so imported, same-package,
   JDK, and dependency types are never flagged.

# Decisions

- **Chosen engine: the pure-Rust type checker** (option 2). The javac-daemon /
  GraalVM Native Image option is rejected: it would reintroduce a Java codebase
  and annotation-processor (Lombok) risk into a project whose reason to exist is
  a JVM-free, single-language server.
- **The written evaluation is dropped by decision**, not overlooked: the owner
  settled the choice directly. This request records the decision instead of the
  comparison. The original acceptance criterion requiring the evaluation is
  removed accordingly.
- **No JVM at runtime** — the standing project constraint is unaffected: this
  adds Rust modules to the existing single crate, no second binary.
- **Conservative by default.** No result beats a wrong result, as everywhere
  else in this server: an ambiguous or unknown type yields no hover, no
  membership, and no diagnostic rather than a guess. Diagnostics fire only when
  a name is provably unresolvable against every source the server has (imports,
  same package, type parameters, `java.lang`, and the indexed workspace, jars,
  and JDK).
- **Phased, and honest about the ceiling.** Generics substitution, overload
  resolution by argument types, `var` inference, lambdas, casts, and
  static-import member resolution are **non-goals of this request** and belong
  to follow-up CRs. Type variables are treated as resolvable-but-opaque.
- **Member signatures for jar/JDK types are name-only in this slice** — they are
  synthesized from the existing flat index entries (which carry names, not
  descriptors), so a receiver of a library type offers its directly declared
  members but no inherited ones and shows no field/return types. Parsing JVM
  member descriptors out of `classfile.rs` is a follow-up CR.
- **Off the request path (R6).** The workspace type model is built inside the
  existing warm-up scan; per-request work is bounded to the open document.
- **Memory:** signatures and hierarchies extend the deliberately flat index.
  The model keys by simple name and keeps a member list per type, so memory
  stays proportional to declared members, not to source text.

# Acceptance criteria

- Type-aware hover: the symbol under the cursor resolves to a declaration and
  is rendered with its resolved signature/type for workspace-declared symbols
  and members; unresolved/ambiguous symbols return no hover.
- Member completions after `.`: the receiver's type is inferred via
  locals/parameters/fields, `this`/`super`, `new T()`, and chained member
  access, and that type's members (inherited included, for source types) are
  offered; an uninferrable receiver returns an empty list.
- Semantic diagnostics: a type name that resolves nowhere is reported as a
  warning, and a type provided by an import, the same package, a type
  parameter, `java.lang`, or the indexed workspace/jars/JDK is **never**
  reported (negative tests included).
- The engine implements the above behind `SemanticEngine` with no changes to
  `server.rs` or the LSP layer.
- Verified by the test suite (unit tests plus `tests/harness.rs`) and by the
  `perf-benchmarks` harness, whose post-warm-up hover probe now asserts resolved
  content. Semantic diagnostics are covered by unit and harness tests rather
  than the bench fixture, which contains no unresolved types; they also
  deliberately require an indexed JDK.
- `docs/architecture.md` and `docs/requirements.md` are updated to match.

# Implementation plan

## Approach

One new module, `src/types.rs`, holding the type layer, plus wiring in
`src/index.rs` (build the workspace model during warm-up) and
`src/engine/syntax.rs` (serve hover, `.`-completions, and diagnostics from it).
No new dependencies: `tree-sitter` already gives the syntax tree, and the flat
`WorkspaceIndex` already gives type and member names for jars/JDK.

- **Type representation (`src/types.rs`).** `Ty` covers primitives, `void`,
  `null`, arrays, named reference types with generic arguments kept as written,
  type variables, and `Unknown`. Named types are keyed by simple name; a
  reference names its simple name plus its arguments only.
- **Declared-type model (`TypeModel`/`TypeInfo`).** Per type: simple name,
  package, kind, type parameters, supertypes (`extends`/`implements`), and its
  fields and methods with declared types (`Member`). `TypeModel` indexes by
  simple name and answers `members(ty)` by walking supertypes breadth-first,
  cycle-guarded, first declaration winning — so inherited members appear.
- **Two sources for the model.** Source files contribute full `TypeInfo`s
  (member types, supertypes) extracted from the same trees the scan already
  parses. Jar/JDK types contribute name-only `TypeInfo`s synthesized from their
  index entries. Source entries overlay the name-only ones.
- **Binding (`bind` helpers in `src/types.rs`).** From a position and the open
  document's tree, collect the visible names (locals and parameters with their
  declared types, enclosing fields, enclosing type parameters, imports
  including `.*`, the file's package) and then resolve a simple name to a
  declaration, and infer the type of a `.`-receiver: an identifier (local,
  parameter, field, or type name), `this`/`super`, `new T(...)`, and chained
  member access through a field's type or a method's return type. Anything
  else is `Unknown`, which callers treat as "no answer".
- **Engine wiring (`src/engine/syntax.rs`).** Hover resolves the node under the
  cursor and renders the declaration signature; `.`-completions use the
  inferred receiver type; diagnostics add unresolved-type warnings on top of the
  parse errors, gated on the model being ready so warm-up never squiggles.
- **Known approximations**, to be documented rather than hidden: a package-less
  simple-name match prefers the context package and then a unique model match;
  generic arguments are preserved for display but not substituted; overloads are
  matched by name only (multiple candidates → no claim).

## Steps

- [x] Add `src/types.rs`: `Ty`/`Prim` with rendering and JVM-descriptor
      parsing, `TypeInfo`/`Member`, and `TypeModel` (insert by simple name,
      `find_unique` preferring a context package, `members` walking supertypes
      breadth-first with cycle guard and first-wins, `from_entries` building
      name-only types from index `SymbolEntry`s), with unit tests for type
      rendering, descriptor parsing, inheritance-aware member lookup, and
      shadowing. (Groundwork for every AC.)
- [x] Add source extraction to `src/types.rs`: `collect_type_infos` turning a
      tree-sitter document into `TypeInfo`s — type kind, type parameters,
      `extends`/`implements`, field types, method parameter and return types,
      and static flags — with unit tests over representative sources.
      (AC1/AC2 — real signatures for workspace types.)
- [x] Add `src/types.rs` binding: visible names at a position (locals,
      parameters, enclosing fields, type parameters, imports incl. `.*`,
      package) and receiver-type inference (identifier, `this`/`super`,
      `new T(...)`, chained member access), returning `None`/`Unknown` when no
      single answer exists; unit tests per case including the negative ones.
      (AC1/AC2/AC3 — the resolution rules.)
- [x] Wire the workspace model into warm-up: `src/index.rs` gains a `types`
      slot (`set_types`/`types`) populated by `scan_workspace` from the source
      trees it already parses, overlaid on `TypeModel::from_entries` for the
      jars and JDK entries it indexes; still entirely off the request path.
      (AC5 — model available to features; R6.)
- [x] Implement `SemanticEngine::hover` in `src/engine/syntax.rs`: resolve the
      node at the cursor (declaration name, type reference in a declaration /
      `new` / cast / `extends` / `implements`, or a member access through an
      inferred receiver) and render the resolved declaration and its
      signature as Markdown; `None` when unresolved or ambiguous. Unit tests
      plus a harness test. (AC1, AC5.)
- [x] Implement `.`-member completions in `src/engine/syntax.rs`: when the
      character before the cursor is `.`, infer the receiver type from the open
      document and offer its members (inherited included for source types) with
      `Container.name` labels; keep the current empty list when the receiver is
      `Unknown`; unit tests plus a harness test. (AC2, AC5.)
- [x] Implement conservative semantic diagnostics in `src/engine/syntax.rs`: a
      type name in a declaration, `new`, cast, `extends`, or `implements` that
      resolves nowhere becomes a warning, checked against type parameters,
      imports, the file's package, `java.lang`, and the model, and only once the
      model is ready; unit tests including the jar/JDK/imported negative cases.
      (AC3, AC5.)
- [x] Extend `src/bin/java-lsp-bench.rs` so the post-warm-up hover probe
      asserts the fixture class name resolves (type-aware hover returns content
      where the type-free engine returned nothing) and run it. Diagnostics are
      not asserted on the fixture: it contains no unresolved types and the
      diagnostic gate requires an indexed JDK.
      (AC5 — hover verified on the fixture; diagnostics verified by tests.)
- [x] Update `docs/architecture.md`: add `types.rs` to the layout tree, a
      component paragraph (model, two sources, binding rules, the conservative
      policy, and the listed approximations/non-goals), and refresh the
      completions/navigation/hover/diagnostics bullets to describe what the
      type layer now answers. (AC6.)
- [x] Update `docs/requirements.md`: move R7 out of "deferred" into a v0.3
      milestone entry naming the pure-Rust choice and this request, and adjust
      the R7 row so it no longer reads "decision postponed". (AC6.)
