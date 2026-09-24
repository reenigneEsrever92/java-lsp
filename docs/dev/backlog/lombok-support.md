---
type: ChangeRequest
kind: feature
title: Lombok annotation support
description: Synthesize Lombok-generated members (accessors, setters, builders, log fields, and constructors) into the source type model and index, with no annotation processor.
state: done
priority: high
tags: [dev, types, lombok, completions, navigation]
owner: felix
verified:
  by: cargo test --all-targets (267 passed - 234 lib, 6 bench bin, 26 harness,
    1 stdio)
  at: 2026-09-25T17:26:04Z
---

# Problem

Lombok generates members at compile time from annotations. This server is pure
Rust with no annotation processing — a deliberate choice recorded in
`type-aware-engine`, which rejected the javac-daemon option precisely to avoid
"annotation-processor (Lombok) risk". The consequence is that while a developer
edits a source type carrying `@Getter`, `@Data`, `@Builder`, or the like, none
of the generated members exist as far as the server is concerned:

- `.`-completion on a receiver of that type omits every generated accessor,
  setter, and builder step.
- hover and signature help render nothing for a generated member.
- the `unresolved-member` diagnostic fires a false positive on a call to a
  generated accessor — `order.getTotal()` goes red although it compiles.
- go-to-definition and find-references cannot reach a generated member.

Types reached from a jar or the JDK are unaffected: Lombok has already run, and
the generated members are real class-file members of that type. The gap is
exactly the file the developer is editing.

# Proposal

Synthesize the members Lombok would generate into the source type model and the
flat index, statically and syntactically, with no annotation processor and no
JVM.

- **Type model** (`src/types.rs`) — after a declaration's real members are
  collected, a shared Lombok pass reads the declaration's annotations and
  appends the members Lombok would generate, so `.`-completion, hover,
  signature help, `var` inference, inlay hints, and the unresolved-member
  diagnostic all see them.
- **Index** (`src/index.rs`) — the same pass contributes synthetic entries
  anchored at the annotated field (or the type's own annotation), flagged so
  they never surface in `workspace/symbol` or ordinary completion; they exist so
  `definition` and `references` resolve a generated member.
- **Coverage in this request**: field- and class-level `@Getter`, `@Setter`,
  `@With`; the composed `@Data` and `@Value`; `@Accessors` (fluent and chain,
  read from the annotation); `@Builder`; the log-field family (`@Slf4j` and
  friends); and the constructor annotations `@NoArgsConstructor`,
  `@RequiredArgsConstructor`, `@AllArgsConstructor`, which build on the separate
  [constructors](constructors.md) request.

# Decisions

- **Syntactic detection by simple annotation name.** There is no classpath or
  `lombok.config` check: dependency resolution is static and offline and could
  silently disengage the whole feature, and the model already resolves names
  best-effort everywhere. `lombok.config` — `accessors.chain`,
  `getter.noIsPrefix`, custom log field names, `@Accessors(prefix = ...)` — is
  ignored in this request and documented as such. A non-Lombok annotation that
  happens to be named `@Getter` is vanishingly rare and behaves no worse than
  today.
- **One shared synthesis helper.** The model and the index must agree exactly,
  so a single helper in `src/types.rs` turns a type declaration plus its text
  into the generated members, and both `collect_type_infos` and
  `collect_entries` call it. It reads annotations from the declaration's
  `modifiers` child (`annotation` / `marker_annotation`, taking the `name`
  field; `element_value_pair` for `@Accessors(fluent = true)`).
- **Builders are in scope.** `@Builder` synthesizes the nested `T.TBuilder`
  type — one setter per field returning the builder, a `build()` returning `T` —
  plus a static `builder()` on `T` (and `toBuilder()` when the annotation asks
  for it); the type model already carries nested types via `TypeInfo::nested`.
  `@Builder` also implies an all-args constructor. `@SuperBuilder` is deferred.
- **Constructors are in scope but blocked by the constructor request.**
  `@NoArgsConstructor` / `@RequiredArgsConstructor` / `@AllArgsConstructor`
  cannot be modelled until `TypeInfo` carries constructors, so this request
  depends on [constructors](constructors.md) and must be implemented after it.
- **Navigation: definition and references, not rename.** A generated member has
  no source of its own; its index entry anchors at the annotated field, so
  definition lands there and references find the call sites. `rename` keeps
  refusing (`null`) for a generated member — there is no declaration to rewrite,
  and rewriting call sites of a member the user never wrote is not a rename this
  server can make safe.
- **`equals` / `hashCode` / `toString` are not synthesized.** `@Data` and
  `@Value` generate them, but the model already resolves them through the
  `java.lang.Object` fallback and deliberately keeps `Object` members out of
  `.`-completion listings; synthesizing them would only add noise.
- **Annotations that generate no member contribute none.** `@NonNull`,
  `@SneakyThrows`, `@Synchronized`, `@Cleanup`, `@val` / `@var`, and
  `@Getter(lazy = true)` are recognized as Lombok but add nothing to model or
  index.

# Acceptance criteria

- `.`-completion on a receiver of a source class with `@Getter private int
count;` offers `getCount()`; with `@Setter` it offers `setCount(int)`; a
  `boolean` field `active` offers `isActive()` and a `Boolean` field offers
  `getActive()`; a field already named `isActive` keeps `isActive()` (never
  `isIsActive`).
- A class annotated `@Data` offers both accessors and setters for its instance
  fields; `@Value` offers accessors and no setters; `@With` offers
  `withField(value)`.
- `@Accessors(fluent = true)` renames accessors and setters to the bare field
  name; `chain = true` makes setters return the declaring type.
- `@Builder` on a class offers a static `builder()` on the type and a nested
  `TBuilder` whose completion offers one setter per field (`name(String)`)
  returning the builder, and a `build()` returning `T`.
- `@Slf4j` and the covered log family offer a static `log` field.
- The `unresolved-member` diagnostic no longer fires on a call to a generated
  accessor, setter, or builder step, while a genuinely unknown member still
  does.
- Go-to-definition and find-references on a generated accessor resolve to the
  annotated field and to its call sites; `rename` on a generated member refuses
  (`null`).
- Once [constructors](constructors.md) has landed, the constructor annotations
  synthesize constructors: `@RequiredArgsConstructor` over the `final` and
  `@NonNull` fields, `@AllArgsConstructor` over every field,
  `@NoArgsConstructor` over none, each resolvable by `new T(...)`.
- A class with no Lombok annotation is unchanged: no synthesized members in the
  model, the index, or diagnostics.
- Unit tests cover each of the above in `src/types.rs`, `src/index.rs`, and
  `src/analysis.rs`.

# Documentation impact

- `docs/architecture.md` — the type-layer bullet (Lombok annotations synthesize
  members into a source file's model, and which annotations are covered) and the
  `WorkspaceIndex` bullet (synthesized entries anchored at the annotated field,
  excluded from `workspace/symbol`, and what definition/references do with them).
- `docs/requirements.md` — a new requirement for static Lombok support, naming
  the covered annotation set and the detection / `lombok.config` / rename
  limitations, traced to UC2, UC3, and UC4.

# Implementation plan

## Approach

No new dependencies and no LSP-shape change. One synthesis helper in
`src/types.rs` is shared by the type model and the flat index; `src/index.rs`
and `src/analysis.rs` carry the entries and the navigation wiring; `SymbolEntry`
gains one flag.

### `src/types.rs` — the shared synthesis

- `annotations_of(node, text)` reads a declaration's `modifiers` child into
  `Annotation { name, args }`, taking the annotation's simple name and each
  `element_value_pair`'s key/value (a bare value keyed `value`).
- `lombok_generated(node, text, type_name) -> Vec<GeneratedMember>` is the one
  shared pass. It walks the body's `field_declaration`s and emits the members
  Lombok would generate: getters (`getX` / `isX`, `@Accessors(fluent)`), setters
  (`setX`, `chain`-aware return type), `withX`, the log field, `builder()` /
  `toBuilder()`, the `TBuilder` nested type with its per-field setters and
  `build()`, and constructors (`@NoArgsConstructor` /
  `@RequiredArgsConstructor` / `@AllArgsConstructor`, the required-args one also
  implied by `@Data`, the all-args one by `@Value` and `@Builder`). Each
  `GeneratedMember` carries its name, kind, return type, parameters,
  static-ness, whether it is a constructor, the builder it belongs to, and its
  anchor (the field it derives from, or the type's own name).
- `type_info_from_declaration` returns the declared `TypeInfo` plus any
  synthetic nested types; it appends the generated members (constructors into
  `constructors`, methods/fields into `methods`/`fields`) and builds the
  `TBuilder` `TypeInfo`. `collect_types_in` pushes both.
- Naming: first character capitalized; a primitive `boolean` field is `isX` (a
  field already named `isX` stays `isX`); a `Boolean` wrapper is `getX`;
  `@Accessors(fluent = true)` uses the bare name; `chain = true` returns the
  declaring type. Class-level `@Getter`/`@Setter`/`@With` skip static fields; a
  field-level annotation applies regardless. `getName`-style collisions with an
  explicitly declared member are left to the existing name-dedupe.
- Excluded: `equals`/`hashCode`/`toString`; `@Accessors(prefix)`;
  `lombok.config`; `@SuperBuilder`.

### `src/classfile.rs`, `src/sources.rs`, tests — the flag

- `SymbolEntry` gains `synthetic: bool` (false for every real entry), so the
  index can carry generated members without their surfacing as ordinary symbols.
  Every literal `SymbolEntry` construction is updated.

### `src/index.rs` — the synthetic entries

- `collect_entries` calls `lombok_generated` for each type declaration and
  appends `synthetic: true` entries whose container is the type (or the builder
  type), `full_range`/`selection_range` the anchoring field (or the type name).
  A Lombok-generated constructor is a `Method` entry named after the type,
  exactly like a declared one.

### `src/analysis.rs` — resolution and diagnostics

- `workspace_symbols` and index-driven `completions` drop `synthetic` entries;
  `rename` refuses a target whose declarations are synthetic.
- `call_definition` short-circuits a lone synthetic candidate to its anchor, so
  definition on a generated accessor lands on the field. Hover, signature help,
  member completion, and the `unresolved-member` diagnostic already read the
  model, so they need no change.

### Tests

One unit test per acceptance criterion: `src/types.rs` (naming, `@Data` /
`@Value` / `@With`, `@Accessors`, `@Builder`, log field, constructors, the
no-annotation case), `src/index.rs` (synthetic entries and their anchors),
`src/analysis.rs` (completion, definition, references, rename refusal, the
diagnostic clearing).

### Docs

- `docs/architecture.md`: the type-layer bullet (Lombok synthesis and the
  covered set) and the `WorkspaceIndex` bullet (synthetic entries).
- `docs/requirements.md`: a new requirement for static Lombok support.

## Steps

- [x] `src/types.rs`: `Annotation` reading, `GeneratedMember`, the naming
      helpers, and `lombok_generated`. (AC1–AC5, AC8)
- [x] `src/types.rs`: wire the pass into `type_info_from_declaration` /
      `collect_types_in`, returning the synthetic nested builder type. (AC1–AC4, AC8)
- [x] `src/index.rs`: add `SymbolEntry.synthetic` and `collect_lombok_entries`,
      called from `collect_entries`. (AC6, AC7)
- [x] `src/classfile.rs`, `src/sources.rs`, test helpers: set `synthetic: false`. (build)
- [x] `src/analysis.rs`: filter synthetic entries from `workspace_symbols` and
      `completions`; refuse `rename`; short-circuit `call_definition`. (AC6, AC7)
- [x] `src/types.rs`, `src/index.rs`, `src/analysis.rs` unit tests. (AC1–AC9, AC10)
- [x] `docs/architecture.md`: type-layer and `WorkspaceIndex` bullets. (all)
- [x] `docs/requirements.md`: the Lombok requirement. (all)
- [x] Run `cargo test --all-targets` and confirm the suite passes. (AC10)
