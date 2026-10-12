---
type: ChangeRequest
kind: feature
title: Reintroduce go-to-implementation in the navigate module
description: textDocument/implementation stopped being answered when the hub rework deleted the shell's goto_implementation and disabled the analysis engine that computed it; restore it as a navigate-module query backed by a new coarse index read.
state: done
verified: { by: felix, at: 2026-10-12T22:00:00Z }
priority: medium
tags: [dev, feature, navigation]
owner: felix
---

# Problem

The server no longer answers `textDocument/implementation`. During the hub
rework the shell's handler and capability were deleted — commit `47e8b5f`
("wip") removes both `implementation_provider: Some(…Simple(true))` and
`async fn goto_implementation` from `src/server.rs` — and the engine that
computed the answer lives in `src/deprecated/analysis.rs`
(`TreeSitterEngine::implementation`/`implementation_contract`, ~L1331), which
`src/lib.rs:24-38` keeps out of the build. So the capability survives only as
dead source:

- `src/server.rs:115-123` advertises `textDocumentSync`, `definition`,
  `declaration`, and `references` — no `implementationProvider`, and the
  `LanguageServer` impl has no `goto_implementation` (it ends at
  `references`, `src/server.rs:179-196`).
- `NavigateRequest`/`NavigateResponse` (`src/messages.rs:38-61`) carry only
  `FindDeclaration` and `FindReferences`, and `navigate.rs::serve`
  (`src/navigate.rs:90-103`) matches only those two.

A developer therefore cannot jump from a contract — an interface or abstract
type, or one of its members — to the workspace types that implement or override
it. The docs still describe the feature as shipping (`docs/architecture.md:391-402`,
`docs/requirements.md:78-82` R13 / milestone v0.8), and its agreed behaviour is
preserved in the condensed entry
[Go to implementation for types and members](summary.md#go-to-implementation-for-types-and-members),
which is the spec to reintroduce against.

# Proposal

Bring `textDocument/implementation` back as a first-class navigation query in
the `navigate` module (`src/navigate.rs`), the module that now owns definition
and references — not by reviving the deprecated analysis engine. The shell
advertises `implementationProvider` and routes `textDocument/implementation`
to `navigate` as a new `NavigateRequest::FindImplementation`, answered by a
new `NavigateResponse` variant.

The answer reuses the contract resolution definition already performs (the
symbol under the cursor: a type, or a member narrowed by its receiver), then
reads the declared-type hierarchy:

- a **type contract** → every workspace source type whose supertype closure
  reaches it, transitively (sub-interfaces and abstract intermediates
  included), the contract itself excluded;
- a **member contract** → every workspace subtype that declares an override of
  the same-named member — matched by name and parameter types when the overload
  is pinned, name-only otherwise — excluding subtypes that only inherit it.

The contract may be a library or JDK type (a cursor on `Runnable` lists its
workspace implementors), since every result is a workspace declaration.

Because the implementation search walks the hierarchy the index owns, and the
index's current read vocabulary only offers the upward `TypeClosure`, add a
coarse read for the downward search (`SubtypesOf { ty, package }` → the
workspace type entries whose supertype closure reaches the contract) rather
than having `navigate` enumerate `WorkspaceFiles`, parse each file, and test
every type itself (D3).

# Decisions

- **D1 — Land in `navigate.rs`, not the deprecated analysis engine.** Navigation
  is its own hub module now, and go-to-implementation is a navigation query like
  definition and references. Reviving `analysis.rs` would undo the rework that
  made navigation self-contained and testable.
- **D2 — Restore the shell wiring.** Advertise `implementationProvider` in
  `initialize` and add a `goto_implementation` handler that routes a new
  `NavigateRequest::FindImplementation { uri, position }` to `navigate`; the
  reply is a `NavigateResponse` variant carrying `Vec<Location>` (an empty list
  renders as `null`).
- **D3 — Add a coarse index read for the downward hierarchy search.** The index
  owns the declared-type model and its closure walks; the new read (e.g.
  `SubtypesOf { ty, package }`) returns light, owned values in the existing
  coarse vocabulary, so no model type crosses the hub and `navigate` never
  parses the workspace to find implementors. Chosen over enumerating files from
  `navigate` (chatty, and it duplicates the index's hierarchy knowledge).
- **D4 — Keep the retired feature's matching decisions.** Both a type and a
  member cursor are supported; the closure is transitive and excludes the
  contract itself; a member override is matched by name and parameter types
  when the overload is pinned and name-only otherwise; an inherited-only member
  contributes nothing; a library/JDK contract may be the owner while dependency
  and JDK declarations are never results; a field, constructor, local, or
  unresolvable cursor yields nothing.
- **D5 — The response is an array of locations or `null`** — no `LocationLink`,
  no partial results — matching the other navigation answers.
- **D6 — This is a reintroduction, not a new design.** The behaviour is already
  documented (R13 / milestone v0.8) and its decisions were settled; the work is
  to re-implement it on the current architecture, so the docs need only be
  reconciled with its new home (see Docs).

# Acceptance criteria

1. `initialize` advertises `implementationProvider`, and `textDocument/implementation`
   requests are answered.
2. A type contract returns every transitively implementing/extending workspace
   type, excluding the contract itself; a library/JDK type contract returns its
   workspace implementors; a contract with no workspace implementation returns
   `null`.
3. A member contract returns every overriding subtype matching the pinned
   overload (name and parameter types), excluding inherited-only members;
   name-only when the overload is not pinned.
4. A field, constructor, or local cursor, and an unresolvable cursor, return
   `null`; dependency and JDK declarations never appear.
5. On `example/`, from `TestInterface` and from `TestInterface.doSomething`,
   implementation returns `TestImpl` and `TestImpl.doSomething` respectively.
6. Unit tests in `src/navigate.rs` and read tests in `src/index/read.rs` cover
   each case; `cargo build`, `cargo fmt --check`, and `cargo test` are clean.
   (The `tests/` harness stays `#![cfg(any())]`-disabled during the rework, so
   its implementation cases cannot run yet.)

# Docs

- `docs/architecture.md` — the navigation bullet (`:391-402`) already documents
  go-to-implementation but attributes it to the analysis core; reconcile it with
  its new `navigate`-module home and the `SubtypesOf` read, and add
  implementation to the `navigate` row of the module table.
- `docs/requirements.md` — R13 / milestone v0.8 (`:78-82`, `:172-179`) are
  unchanged in substance; verify they now match the implemented behaviour.
- `docs/dev/changelog.md` — the shipped entry, once implemented.

# Implementation plan

## Approach

Reintroduce go-to-implementation as a `navigate`-module query backed by one new
coarse index read; no part of the deprecated analysis engine is revived.

**The read** (`src/messages.rs`, `src/index/read.rs`). Add
`IndexRequest::ImplementationsOf { contract: Ty, package: Option<String>, member:
Option<MemberSelector> }` with `MemberSelector { name: String, params:
Option<Vec<Ty>> }`, answered with `IndexResponse::Entries(Vec<Arc<SymbolEntry>>)`
(the response variant `EntriesByName` already uses; a `Vec<Ty>` is existing hub
vocabulary, so no model type crosses). `read::answer` resolves the contract's
`(simple name, package)` with `query::lookup`, then walks the workspace **type**
entries in the symbol table (`origin == Workspace`, `kind.is_type()`), keeping
each whose supertype closure — the existing `type_closure` walk — reaches the
contract, excluding the contract itself. With `member: None` those type entries
are the answer; with `member: Some(sel)` each subtype contributes its own
*declared* member matching `sel.name` (and `sel.params` when pinned) — looked up
in the subtype's `TypeInfo` through `query::lookup`, so an inherited-only member
contributes nothing — and the member's entry is returned. Iterating workspace
entries (not `query::all_types`, which also holds jar/JDK types) keeps every
result a workspace declaration; matching `Member.params` inside the index keeps
the model type off the hub.

**The query** (`src/navigate.rs`). Add `find_implementation` and the request and
reply (`NavigateRequest::FindImplementation { uri, position }`,
`NavigateResponse::Implementations(Vec<Location>)`). Resolve the cursor to a
contract: a `type_identifier`/`scoped_type_identifier` names a type; a member
name is resolved through its receiver exactly as definition does
(`receiver_type_of`), with the dependency-origin filter dropped so a library or
JDK type may be the contract owner (`Runnable`). A field, constructor, local, or
unresolvable cursor names no contract and answers empty. For a member contract,
resolve the owner type and the selected overload (its name, plus the pinned
parameter types when the cursor is a call with resolvable arguments); a type
contract carries no member selector. Call the read once, map each returned entry
to `Location { uri, selection_range }`, sort by uri/line/character, and dedup.

**The shell** (`src/server.rs`). Advertise `implementationProvider` in
`initialize` and implement `goto_implementation`, routing
`NavigateRequest::FindImplementation` and rendering the locations (an empty list
as `null`).

## Steps

- [x] `src/messages.rs` — add `MemberSelector`, `NavigateRequest::FindImplementation`,
      `NavigateResponse::Implementations`, and `IndexRequest::ImplementationsOf`.
- [x] `src/index/read.rs` — answer `ImplementationsOf` (the supertype-closure
      subtype set; the optional declared-member match) and add read tests for a
      type contract, a member contract, an inherited-only member, and a shared
      simple name across packages (acceptance 2-4).
- [x] `src/navigate.rs` — add `find_implementation`, the serve arm, and the
      contract resolution (type and receiver-resolved member, library owner
      allowed); add unit tests on `TestInterface`/`TestImpl` for the type and
      member cases and for a field/constructor/unresolvable cursor (acceptance
      3-5). A cursor on a type declaration's own name also names the type.
- [x] `src/server.rs` — advertise `implementationProvider` and implement
      `goto_implementation` (acceptance 1).
- [x] `docs/architecture.md` — re-point the go-to-implementation prose
      (`:391-402`) at the `navigate` module and the `ImplementationsOf` read, and
      add implementation to the `navigate` row of the module table; verify
      `docs/requirements.md` R13 / milestone v0.8 (`:78-82`, `:172-179`).
- [x] `docs/dev/changelog.md` — add the shipped entry.
- [x] Verify `cargo build`, `cargo fmt --all -- --check`, and `cargo test`
      (acceptance 6).
