---
type: ChangeRequest
kind: feature
title: Model constructors so new T(...) resolves
description: Add constructors to the declared-type layer and resolve new T(...) against them for signature help, hover, definition, references, and parameter hints.
state: done
priority: high
tags: [dev, types, constructors, navigation]
owner: felix
verified:
  by: cargo test --all-targets (253 passed - 220 lib, 6 bench bin, 26 harness,
    1 stdio)
  at: 2026-09-25T17:17:16Z
---

# Problem

`new T(...)` is never resolved. Constructors are absent from the declared-type
model:

- `src/types.rs` — `collect_members` reads `field_declaration` and
  `method_declaration` off a type's body, and `type_info_from_declaration` adds
  a record's header components; a `constructor_declaration` contributes nothing,
  and `TypeInfo` has no place to keep one.
- `src/classfile.rs` — `read_members` drops every member whose name starts with
  `<`, which removes `<init>` (the only way a compiled constructor appears)
  along with `<clinit>`.
- `src/analysis.rs` — signature help is documented as answering nothing for
  `new T(...)`: `enclosing_call` matches only `method_invocation`, and a
  constructor has no `name` field to read the way a method call does.

Because the constructor is invisible, a developer gets nothing at a
`new T(...)`: no signature help, no hover, no go-to-definition into the
constructor, no find-references from a constructor declaration to its call
sites, and no parameter-name hints at the call. Source constructors are already
in the flat index — `collect_entries` records a `constructor_declaration` as a
`Method` entry named after the type — but no model-backed feature can reach
them.

# Proposal

Model constructors as first-class parts of a declared type and resolve
`new T(...)` against them.

- **Type model** (`src/types.rs`) — `TypeInfo` gains a
  `constructors: Vec<Member>` collection, kept separate from `methods` so a
  constructor never leaks into `.`-member listings. `collect_members` reads
  `constructor_declaration`s; a record's canonical constructor is synthesized
  from its header components when no constructor is declared explicitly, and a
  class that declares no constructor gets the implicit no-arg constructor. A
  constructor is represented as a member of its declaring type — the type's own
  simple name plus the declared parameters — with signature rendering that omits
  a return type (whether that is a dedicated index kind or a rendering flag on
  `Member` is settled in the plan), so a constructor renders and reads as
  `T(int x)` rather than as a method.
- **Class files** (`src/classfile.rs`) — `read_members` parses a public
  `<init>` into the enclosing type's constructors (named after the type), while
  still skipping `<clinit>`.
- **Resolution** (`src/analysis.rs` + `src/types.rs`) — `new T(...)` resolves
  against T's constructors, selecting an overload by the call's argument types
  through the existing `assignable` relation, so signature help, hover,
  go-to-definition, find-references, and source-parameter inlay hints work at a
  constructor call. Signature help extends `enclosing_call` to match
  `object_creation_expression` and reads the created type instead of a `name`
  field; the parameter-hint path gets the same `object_creation_expression`
  treatment.
- **Index** (`src/index.rs`) — source constructors are already indexed; the
  model's constructor is matched to its owner so definition lands on the
  `constructor_declaration`.

# Decisions

- **Constructors are a separate collection, not methods.** Member completion
  reads `members_with_overloads`; a constructor is reachable only through `new`,
  so a dedicated collection prevents offering `t.T()` while letting every other
  feature treat a constructor as an overloaded, parameterized member.
- **Implicit constructors are synthesized.** A class with no declared
  constructor has an implicit no-arg constructor, and a record's canonical
  constructor follows from its components, so `new T()` and
  `new Point(1, 2)` resolve without the developer writing a constructor.
  Interfaces and enums contribute none (neither is `new`-able).
- **Class-file constructors are modelled, but jar/JDK navigation is not.**
  Resolving `<init>` gives a library type real constructor signatures for
  signature help and hover; definition still yields nothing for a library
  constructor because its location is not openable — the rule that a jar/JDK
  declaration is never a definition target holds unchanged.
- **Diagnostics are out of scope.** This request makes `new T(...)` resolvable;
  reporting an unknown or wrong-arity constructor (a new diagnostic family, with
  varargs and implicit conversions to get right) is a deliberate follow-up.
- **Known limitation: inner-class construction is not fully modelled.** A
  non-static inner class has an implicit enclosing-instance parameter in its
  constructor; the model resolves a call against the declared or synthesized
  parameter list only, so an inner class constructed with the enclosing
  instance is not matched. This is documented rather than guessed at.
- **Lombok's constructor-generating annotations depend on this.**
  `@NoArgsConstructor` / `@RequiredArgsConstructor` / `@AllArgsConstructor`
  synthesize constructors, so they land in [Lombok annotation
  support](lombok-support.md), which builds on this request and must be
  implemented after it. The accessor- and builder-generating annotations do not
  depend on it.

# Acceptance criteria

- Signature help inside `new T(...)` lists T's constructor overloads with their
  declared parameters and marks the active argument; it returns `None` when T or
  its constructors cannot be resolved.
- Go-to-definition on `new T(...)` for a source constructor lands on T's
  `constructor_declaration`; find-references on a source constructor
  declaration returns its call sites, and with `include_declaration` also the
  declaration.
- A class with no declared constructor resolves `new T()`; a record
  `record Point(int x, int y)` resolves `new Point(1, 2)`; an overloaded
  constructor, or the same arity with different parameter types, is selected by
  the call's argument types, falling back to arity and then to name.
- Source constructors keep their parameter names, so a call site renders
  parameter-name inlay hints; a library constructor (name-less parameters)
  produces no parameter hint but still answers signature help and hover.
- Constructors never appear in `.`-completion, `workspace/symbol`, or as a
  callable member (`t.T()` stays unresolved).
- Definition of a library (jar/JDK) constructor returns no location, consistent
  with other library declarations.
- Unit tests cover each of the above in `src/types.rs`, `src/classfile.rs`,
  `src/index.rs`, and `src/analysis.rs`.

# Documentation impact

- `docs/architecture.md` — the type-layer bullet (constructors on `TypeInfo`,
  implicit/canonical synthesis, the inner-class limitation), the class-file
  bullet (`<init>` parsed), and the engine-core bullet (signature help no longer
  says "constructors are not modelled"; `new T(...)` resolves and is a
  definition/references target).
- `docs/requirements.md` — record constructor modelling against the type-aware
  engine (R7), extending its description or adding a requirement ID if the
  `new`-expression surface warrants one.

# Implementation plan

## Approach

No new dependencies and no LSP-shape change. Work lands in three source files
(`src/types.rs`, `src/classfile.rs`, `src/analysis.rs`) plus a regression test in
`src/index.rs`, and two docs. `src/index.rs` needs no code change: a source
`constructor_declaration` is already indexed as a `Method` entry named after the
enclosing type, which is exactly what the model is matched against.

### `src/types.rs` — the model

- `TypeInfo` gains `pub constructors: Vec<Member>`, empty in `TypeInfo::new`.
  `TypeInfo::members()` (fields and methods, the hierarchy walk's input) is
  unchanged, so a constructor never reaches member lookup or `.`-completion.
- A constructor is a `Member` with `kind: IndexKind::Method`, `ty: Ty::Void`, the
  declaring type's simple name, and the declared parameters. Rendering is
  dedicated: a new `Member::constructor_signature()` renders `Name(params)`
  without a return type, which the generic `Member::signature()` would add.
- `collect_members` gains a `constructor_declaration` arm pushing into
  `info.constructors` (its `name` field is the type name).
- `type_info_from_declaration` synthesizes after the body walk: a record always
  has its canonical constructor (component types as parameters), inserted only
  when no declared constructor shares its parameter types; a class with no
  declared constructor gets a no-arg one; interfaces and enums add none.
- `TypeLookup` gains `fn constructors(&self, ty, package) -> Vec<Member>` — the
  looked-up type's own constructors (constructors are not inherited, so no
  hierarchy walk); `TypeQuery` inherits the default.
- The body of `member_for_arguments_confirmed` is factored into
  `confirmed_overload(candidates, args, model, package)`, reused by the new
  `constructor_for_arguments(ty, args, model, package)`: the confirmed overload,
  else the single same-arity constructor, else the first constructor
  (name-level) — the constructor mirror of `member_for_arguments`.

### `src/classfile.rs` — jar/JDK constructors

- `ClassInfo` gains `pub constructors: Vec<Member>`.
- `read_members` stops dropping `<init>` from the method table: the skip rule
  becomes table-aware — the field table skips a name starting with `<`, the
  method table skips only `<clinit>`; synthetic (`$`) and private members stay
  skipped.
- `parse_class` splits `<init>` out of the method table, renames each to the
  enclosing simple name (innermost `$` segment), and stores them in
  `constructors`, so the method name list and `members` stay constructor-free.
- `class_type_info` copies `info.constructors` into the `TypeInfo`.

### `src/analysis.rs` — resolution

- `TargetKind` gains `Constructor`. `declaration_target` gains a
  `constructor_declaration` arm with the enclosing type as owner and name,
  `declarations` from `member_declarations(name, Method, owner, package,
index)`, and an overload selected from the declaration's parameters when the
  name is overloaded.
- `resolve_target` maps a cursor on the `type` of an `object_creation_expression`
  to a `Constructor` target (a new `constructor_target` resolves the created
  type, refuses a non-workspace owner, and selects the overload from the call's
  argument types) rather than the plain type target; `rename` refuses a
  `Constructor` target.
- `collect_occurrences` gains a `Constructor` arm calling a new
  `collect_constructor_occurrences`, which parses the candidate files and
  records each `object_creation_expression` whose created type resolves to the
  target's owner (same simple name and package), narrowed to the selected
  overload by the call's argument types — the `new T(...)` analog of
  `collect_member_nodes`.
- `definition` gains `constructor_definition`, tried before the name-only
  lookup: on the `type` of a `new T(...)` it locates the matching source
  constructor through the same `match_declaration` path `call_definition` uses.
  A library or implicit constructor has no declaration entry and falls back to
  the existing type target.
- `signature_help` handles `object_creation_expression`: when the cursor sits in
  a `new T(...)` argument list (a new `enclosing_object_creation`), it offers
  the created type's constructor overloads, labelled with
  `Member::constructor_signature`, and tracks the active argument via the
  existing `active_parameter`.
- `collect_inlay_hints` gains an `object_creation_expression` arm calling a new
  `constructor_parameter_hints`, which resolves the created type's constructor
  from the call's argument types and emits the same `name:` hints a method call
  does (source constructors keep parameter names; class-file ones do not).
- `workspace_symbols` and index-driven `completions` drop constructor entries
  through a shared `is_constructor_entry` predicate (`kind == Method` and
  `name == container.last()` — a method can never share its class's name, so
  this identifies a constructor exactly).

### Tests

One unit test per acceptance criterion, in the file it exercises:
`src/types.rs` (the model, no `.`-membership, implicit no-arg, record canonical,
overload selection), `src/classfile.rs` (`<init>` modelled; `<clinit>`, private,
and synthetic members still skipped), `src/index.rs` (a source constructor
entry carries the type as its container), and `src/analysis.rs` (signature help,
definition, references, parameter hints, `.`-completion and `workspace/symbol`
exclusion, library definition refusal).

### Docs

- `docs/architecture.md`: the type-layer bullet (constructors on `TypeInfo`,
  implicit/canonical synthesis, the inner-class limitation), the class-file
  bullet (`<init>` parsed), and the engine-core bullet (signature help no longer
  says "constructors are not modelled").
- `docs/requirements.md`: record constructor modelling under R7.

## Steps

- [x] `src/types.rs`: add `TypeInfo.constructors`, `Member::constructor_signature`,
      the `constructor_declaration` arm in `collect_members`, and the
      implicit/canonical synthesis in `type_info_from_declaration`. (AC3, AC5)
- [x] `src/types.rs`: add `TypeLookup::constructors`, factor
      `confirmed_overload`, and add `constructor_for_arguments`. (AC1, AC3)
- [x] `src/types.rs` unit tests: model, no `.`-membership, implicit/record
      synthesis, overload selection. (AC3, AC5, AC7)
- [x] `src/classfile.rs`: add `ClassInfo.constructors`, table-aware `<init>`
      handling in `read_members`, the split in `parse_class`, the copy in
      `class_type_info`; update the existing skip test. (AC4, AC5, AC6)
- [x] `src/index.rs` test: a source constructor entry carries the type as its
      container. (AC7)
- [x] `src/analysis.rs`: `TargetKind::Constructor`, the `declaration_target`
      arm, `constructor_target` in `resolve_target`, and
      `collect_constructor_occurrences`. (AC2, AC5)
- [x] `src/analysis.rs`: `constructor_definition` in `definition`. (AC2, AC6)
- [x] `src/analysis.rs`: `object_creation_expression` handling in
      `signature_help`. (AC1)
- [x] `src/analysis.rs`: `constructor_parameter_hints` and the
      `collect_inlay_hints` arm. (AC4)
- [x] `src/analysis.rs`: `is_constructor_entry` filtering in
      `workspace_symbols`/`completions` and the `rename` refusal. (AC5)
- [x] `src/analysis.rs` unit tests: signature help, definition, references,
      parameter hints, `.`-completion and `workspace/symbol`, library refusal.
      (AC1, AC2, AC4, AC5, AC6, AC7)
- [x] `docs/architecture.md`: type-layer bullet, class-file bullet, engine-core
      signature-help sentence. (all)
- [x] `docs/requirements.md`: record constructor modelling under R7. (AC1)
- [x] Run `cargo test --all-targets` and confirm the suite passes. (AC7)
