---
type: ChangeRequest
kind: feature
title: Go to implementation for types and members
description: Answer textDocument/implementation for a type or member cursor with the workspace subtype declarations, and the overriding member declarations, across the whole workspace.
state: done
priority: medium
tags: [dev, navigation, types, lsp]
owner: felix
verified:
  by: cargo test --all-targets (274 passed - 240 lib, 6 bench bin, 27 harness,
    1 stdio)
  at: 2026-09-25T17:50:55Z
---

# Problem

The server answers most navigation requests but not
`textDocument/implementation`. `src/server.rs` advertises hover, definition,
completion, signature help, document symbols, workspace symbols, references,
rename, code actions, folding, semantic tokens, and inlay hints — there is no
`implementation_provider`, and no `goto_implementation` handler. So a Java
developer cannot:

- jump from an interface or abstract method to the workspace methods that
  override it — `definition` on `I.m()` lands on the interface's own
  declaration, never on an implementation;
- list the workspace types that implement or extend a type.

The information is already in the declared-type model. `TypeInfo.supertypes`
(`src/types.rs`) carries a type's `extends`/`implements` targets for both source
and class-file types, and `subtype_of` (same file) already walks that hierarchy
breadth-first, cycle-guarded, to answer "is X a subtype of Y" — but it is
private, used only internally for assignability, and never surfaced as a
navigation result. `TreeSitterEngine::overlay_model()` yields every workspace
source type, so the answer is a matter of exposing what the model already knows.

# Proposal

Add the implementation provider end to end, reusing the existing binding and
hierarchy machinery.

- **Model** (`src/types.rs`) — expose the hierarchy the resolution step needs:
  a public predicate for "is `from` a subtype of `name`" (today's private
  `subtype_of`), and iteration over a `TypeModel`'s types (its `by_name` map is
  private, and the engine needs to walk every workspace source type).
- **Resolution** (`src/analysis.rs`) — a new `implementation(uri, position) ->
Vec<Location>` resolves the cursor to a _contract_ and collects its
  implementations. The contract is the enclosing type of a member cursor or the
  type under a type cursor:
  - a **type** contract returns the declaration of every workspace source type
    whose supertype closure reaches it;
  - a **member** contract returns the declaration of every workspace source
    subtype that declares an overriding member of the same signature.
    Binding reuses `resolve_target` for the workspace case; because
    `resolve_target` deliberately refuses a library owner, a library-tolerant
    fallback resolves a type position and a member receiver through the model
    (which carries jar/JDK types) to a `(name, package)` identity.
- **Boundary** (`src/engine.rs`) — `Command::Implementation` and
  `EngineHandle::implementation`, dispatched through the existing `read`
  helper like every other read-only query.
- **Shell** (`src/server.rs`) — advertise
  `implementation_provider: Simple(true)` and implement `goto_implementation`,
  mapping a non-empty result to `GotoImplementationResponse::Array` and an empty
  one to `null`.

# Decisions

- **Both a type and a member cursor are supported.** A type cursor lists
  workspace subtypes; a member cursor lists overriding member declarations. A
  field, constructor, or local cursor answers nothing — those have no
  "implementations", and treating them as contracts would be a guess. This
  matches what Java tooling conventionally offers and gives the feature both of
  its useful entry points.
- **A library/JDK type may be the contract owner.** Go-to-implementation on
  `Runnable` listing the workspace classes that implement it is the headline use
  case, and every _result_ the feature returns is still a workspace source
  declaration (openable), so the rule that a jar/JDK declaration is never a
  navigation target holds. This needs a resolution path that admits a library
  owner, since `resolve_target` refuses one today (`type_target_for` and
  `member_target` both require a workspace declaration).
- **The hierarchy match is name-based, as the model already is.** `subtype_of`
  matches a supertype by simple name, so a contract whose simple name is shared
  by two packages could pick up extra subtypes. This is the existing behaviour
  of the type layer's assignability, and it is accepted here rather than
  re-plumbing hierarchy edges to carry packages; the typed-tie case is rare and
  over-reporting a navigable declaration is preferable to missing one.
- **The result is the whole transitive closure.** A sub-interface, an abstract
  intermediate class, and a concrete class all count as implementations, for
  both contract kinds; only the contract type itself is excluded. A developer
  following a contract wants every workspace type bound by it, not one hop.
- **A member override is matched by name and parameter types.** When the
  contract overload can be pinned down (`Target.overload`), an override must
  declare the same parameter-type list (the `params_key` overload key); when the
  name is not overloaded, the name alone matches. A subtype that merely inherits
  the member without declaring it contributes nothing — an "implementation" is a
  declaration, not an inherited slot. A library member whose parameter types are
  absent from the model falls back to name matching.
- **Dependency and JDK declarations are never results.** A class-file jar or JDK
  location cannot be opened by an editor, so only workspace sources are listed,
  consistent with `definition`'s rule that no result beats a wrong result.
- **The response is an array of locations, or `null`.** An empty result is a
  refusal as much as a "none found", exactly as `references` reports it. No
  `LocationLink` (the feature has no origin range to supply) and no partial
  results.

# Acceptance criteria

- `initialize` advertises `implementationProvider`, and
  `textDocument/implementation` is answered (not `MethodNotFound`).
- On a workspace interface or abstract class, go-to-implementation returns the
  declaration of every workspace type that implements or extends it, transitively
  — including a sub-interface and an abstract intermediate — and not the
  contract type itself.
- On an interface or abstract method, go-to-implementation returns the
  declaration of every workspace subtype that overrides it, matching the
  selected overload's parameter types; a subtype that only inherits it is not
  listed.
- On a member of a library/JDK type (e.g. `Runnable` or `Runnable.run`),
  go-to-implementation returns the workspace types or overriding methods bound
  by that contract; a contract with no workspace implementation returns `null`.
- A field, constructor, or local cursor returns `null`, as does a cursor on a
  symbol that cannot be resolved.
- Dependency/JDK declarations never appear in the result.
- Unit tests cover the type contract, the member contract (overload-narrowed and
  name-only), transitivity, the library-owner fallback, the empty-result
  refusal, and the excluded cursor kinds in `src/analysis.rs`; a model test for
  the exposed subtype predicate and iteration in `src/types.rs`; and a harness
  test that the capability is advertised and the request round-trips in
  `tests/harness.rs`.

# Documentation impact

- `docs/architecture.md` — the LSP-shell bullet (add `implementation` to the
  advertised request list) and the navigation paragraph (go-to-implementation:
  the two contract kinds, transitivity, library owners, workspace-only results).
- `docs/requirements.md` — a new requirement **R13** under UC3 (navigate the
  workspace) for implementation navigation, and its milestone mention.
- `README.md` — the **Navigation** feature bullet gains go-to-implementation.

# Implementation plan

## Approach

No new dependencies and no LSP-shape change beyond one request handler. Work
lands in `src/types.rs`, `src/analysis.rs`, `src/engine.rs`, and
`src/server.rs`, plus tests and three docs. The feature is a read-only query, so
it threads through the existing `Command`/`read` seam exactly like
`references`.

### `src/types.rs` — expose the hierarchy

- Promote the private `subtype_of` to a public `pub fn is_subtype_of(from: &Ty,
target: &str, model: &dyn TypeLookup, package: Option<&str>) -> bool`
  (keeping its breadth-first, cycle-guarded walk and simple-name matching);
  `ref_assignable` keeps calling it.
- Add `pub fn types(&self) -> impl Iterator<Item = &TypeInfo>` to `TypeModel`
  (`by_name.values().flatten()`), so the engine can enumerate every workspace
  source type; `by_name` stays private.

### `src/analysis.rs` — resolve the contract, collect implementations

- A private `enum Contract { Type { name, package }, Method { owner,
owner_package, name, params: Option<Vec<Ty>> } }`.
- `fn implementation_contract(&self, uri, document, offset) -> Option<Contract>`
  reuses `resolve_target`:
  - `TargetKind::Type` → `Contract::Type { name, package }`;
  - `TargetKind::Method` → `Contract::Method { owner, owner_package: package,
name, params: overload }`;
  - `Field`/`Constructor`/`Local` → `None`.
  - When `resolve_target` is `None` (a library/JDK owner, which it refuses), a
    fallback resolves the same cursor through the model: a type position via
    `types::type_from_node` + `query.lookup`, a member receiver via
    `types::receiver_type` + `types::member_owner` (+ `member_for_arguments`
    when overloaded), yielding the owner's `(name, package)` identity.
- `pub fn implementation(&self, uri, position) -> Vec<Location>`:
  - build `base`/`overlay`/`query` as the other features do, resolve the
    contract, and return empty when there is none;
  - iterate `overlay.types()`, skipping the contract's own type; for each source
    type `S` that `is_subtype_of` the contract root (by simple name), a **Type**
    contract emits `S`'s declaration (its index entry via
    `workspace_type_entry`), and a **Method** contract emits the declaration of
    each of `S`'s own methods matching the name and, when `params` is set, the
    parameter-type list, located with `member_declarations` + `match_declaration`
    (falling back to the sole candidate);
  - sort and deduplicate, mirroring `references`.
- Helper: `fn override_location(&self, uri, document, name, contract_params,
info) -> Option<Location>` finds `info`'s override — its own method matching
  the name and, when `contract_params` is set, the declared parameter types —
  and locates its declaration.

### `src/engine.rs` — the boundary

- `Command::Implementation { uri, position, reply: Reply<Vec<Location>> }`,
  `EngineHandle::implementation(uri, position) -> Vec<Location>` (mirroring
  `references`), and a `read` dispatch arm.

### `src/server.rs` — the shell

- Add `implementation_provider: Some(ImplementationProviderCapability::Simple(true))`
  to `ServerCapabilities`, and `goto_implementation` returning
  `GotoImplementationResponse::Array` for a non-empty result, else `None`.
- New imports: `GotoImplementationParams`, `GotoImplementationResponse`,
  `ImplementationProviderCapability`.

### Tests

- `src/types.rs`: `is_subtype_of` (direct, transitive, cycle-guarded,
  name-only) and `TypeModel::types()` enumeration.
- `src/analysis.rs`: a type contract (direct + transitive, sub-interface and
  abstract intermediate, self excluded), a member contract (overload-narrowed
  and name-only, inherited-only subtype excluded), a library owner
  (`Runnable`), the empty-result refusal, and the excluded cursor kinds.
- `tests/harness.rs`: `initialize` advertises `implementationProvider` and a
  `textDocument/implementation` request round-trips over stdio.

## Steps

- [x] `src/types.rs`: public `is_subtype_of`, `TypeModel::types()`, and their
      unit tests. (AC7)
- [x] `src/analysis.rs`: the `Contract` type and `implementation_contract`
      (workspace path plus the library-owner fallback). (AC2, AC3, AC4)
- [x] `src/analysis.rs`: `implementation` and `override_location`, collecting
      type and member declarations across the workspace. (AC2, AC3, AC5, AC6)
- [x] `src/analysis.rs` unit tests: both contract kinds, transitivity,
      overload narrowing, the library owner, the empty refusal, and the
      excluded cursor kinds. (AC2–AC5, AC7)
- [x] `src/engine.rs`: `Command::Implementation`, `EngineHandle::implementation`,
      and the dispatch arm. (AC1)
- [x] `src/server.rs`: advertise `implementation_provider` and implement
      `goto_implementation`. (AC1)
- [x] `tests/harness.rs`: the advertised capability and a round-trip
      `textDocument/implementation`. (AC1)
- [x] `docs/architecture.md`: the LSP-shell bullet and the navigation paragraph.
      (all)
- [x] `docs/requirements.md`: requirement **R13** and its milestone mention.
      (all)
- [x] `README.md`: the **Navigation** bullet. (all)
- [x] Run `cargo test --all-targets` and confirm the suite passes. (all)
