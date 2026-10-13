---
type: ChangeRequest
kind: feature
title: Complete a receiver's members after a dot
description: The live completion module answers nothing after `recv.`; reintroduce type-aware member completion — the receiver's fields (first) then its methods, each overload with its full signature — resolved through a receiver-resolution helper shared with navigation.
state: done
verified: { by: felix, at: 2026-10-13T15:00:00Z }
priority: high
tags: [dev, feature, completion, types]
owner: felix
---

# Problem

`src/completion.rs` — the live `completion` module — deliberately answers nothing
for member access: `if text[..before_prefix].ends_with('.') { return Vec::new(); }`,
commented "the type layer is not yet reintroduced". That is decision D3 of the
just-landed `completion-module` extraction, and the R4 slice's stated limit — names
without types. The practical gap is typing support after a receiver: `list.` offers
nothing, and a call on a receiver (`list.ad…`) cannot be completed at all.

The type-aware member completion that once served this now lives only in the
deprecated `src/deprecated/analysis.rs` (`TreeSitterEngine::member_items`), but
every piece the live modules need already exists:

- The live `navigate` module already resolves a receiver's declared type over the
  hub — `receiver_type_of` with `identifier_type`/`parameter_type`/`local_type`/
  `field_type`/`enclosing_type_name` — private to `src/navigate.rs`.
- The index already models members: `TypeInfo { fields, methods, … }`,
  `query::members_with_overloads`, and `Member { name, kind, ty, params,
  type_params, is_static }` with `Member::signature()`.
- The shell already advertises `completionProvider` and routes
  `textDocument/completion` to the module.

So member completion is a wiring gap: resolve the receiver, offer its members.

# Proposal

When the cursor follows `recv.` (or a partial name after it), resolve the receiver's
declared type from the cursor's file, then offer that type's members — inherited
members included — as completion items. **Fields (and enum constants) come first,
then methods.** Each method overload is its own item, labelled with its full
signature (`Member::signature()`), inserting `name(`, with the declaring type as
`detail`. A receiver that names a *type* offers only its static members; a receiver
whose type cannot be resolved yields nothing — never a guess at what might be there.

To share the receiver step rather than copy it, move `receiver_type_of` and its
helpers out of `src/navigate.rs` into a new `src/util.rs` and have both `navigate`
and `completion` read Java through it. No new hub message family is added for this:
the receiver helpers are plain shared functions, and the index reads they need
already exist. `navigate` keeps its behaviour — the extraction is a move, not a
change.

# Decisions

- **D1 — Served by the `completion` module.** Extend `src/completion.rs`, which
  already owns `textDocument/completion`; no new module.
- **D2 — Receiver resolution is shared through `src/util.rs`, not duplicated.**
  Move `receiver_type_of`/`identifier_type`/`parameter_type`/`local_type`/
  `field_type`/`enclosing_type_name`/`resolvable`/`is_type_parameter` out of
  `navigate.rs` into `util.rs`; `navigate` and `completion` both call them. A move,
  not a rewrite — no behaviour change to navigation. No new hub messaging is
  introduced for this (the user's "no messaging for this"); the reads the helpers
  make already exist.
- **D3 — Fields before methods.** A deliberate ordering: fields and enum constants
  sort before methods (rank prefixes), unlike the old engine's interleaved
  model order.
- **D4 — Overloads are listed individually.** One item per overload, `label =
  Member::signature()`, `filter_text = bare name`, `insert_text = name(`, `detail =
  declaring type` — accepting a method opens its argument list. Fields stay a
  single item with `insert_text = name`. Matches the old engine.
- **D5 — `MembersOf` becomes overload-preserving.** The index's
  `IndexRequest::MembersOf` read answers with `query::members_with_overloads`
  instead of `query::members` (which collapses same-named overloads). `navigate`
  reads the same answer and is unaffected (it filters by name and kind), so no new
  read is added.
- **D6 — Inherited members included, statics filtered by receiver shape.** The
  members read already walks supertypes; a receiver that names a type (a
  `type_identifier`/`scoped_type_identifier`, or a name that `ResolvesType`) offers
  only static members, while `this` and a value receiver offer instance members.
- **D7 — Deliberately out of scope (separate requests / capabilities).** Chained
  receivers (`a.foo().`, `chained-call-completion`); and *type* completion — a type
  receiver's directly nested types (`Greeter.Inn`) and package-qualified type names
  (`java.util.Li`) — which the index does not expose a read for and which is not
  function-call completion.
- **D8 — Reads the current buffer text.** The receiver and scope are read from
  `buffers::current_text`, so a receiver typed in an unsaved edit resolves.

# Acceptance criteria

1. For a receiver of a resolvable type, `recv.` and `recv.pre` offer that type's
   fields first, then its methods; fields appear before methods.
2. Each method overload is a separate item whose label is its full signature and
   whose accepted text is `name(`; the item's `detail` names the declaring type.
   Inherited members appear; an override collapses; distinct overloads stay apart.
3. `this.` offers the enclosing type's members; a field, parameter, or local
   receiver resolves to its declared type; `new T().` resolves to `T`; a static
   qualifier offers only the type's static members; an enum receiver offers its
   constants; a record receiver offers its component accessors.
4. A receiver whose type cannot be resolved (an unresolved name, a type variable,
   an array) yields no items — never a wrong membership. The existing
   `member_access_yields_nothing_without_the_type_layer` test is replaced by the
   positive cases plus a negative case.
5. On `example/`, `ti.` (a `TestInterface` field) offers `TestInterface`'s members.
6. `cargo build`, `cargo fmt --check`, and `cargo test` are clean; the existing
   `navigate` tests are unchanged (the helper extraction is behaviour-preserving).

# Docs

- `docs/architecture.md` — the Layout (`src/util.rs` added; the receiver helpers'
  new home), the module table (the completion row now covers member completion; a
  shared-helpers note), and the completion implementation note (member completion +
  the shared receiver step).
- `docs/requirements.md` — traceability: the deferred member-access half of R4
  lands, advancing R7's type layer into completion.
- `docs/dev/changelog.md` — the shipped entry.
- `docs/log.md` — the reconciliation.

# Implementation plan

## Approach

The receiver step already exists in `src/navigate.rs` but is private and welded
into one entry point (`receiver_type_of`, called with the *name* node of a member
access). Member completion has no name node — the cursor sits right after the dot —
so the work is twofold: (1) extract the receiver machinery into a shared
`src/util.rs` and split `receiver_type_of` into "find the receiver node" +
"classify that node into a type", and (2) add the completion branch that finds the
receiver before the dot and offers its members.

**`src/util.rs` (new).** Move out of `navigate.rs`: `identifier_type`,
`parameter_type`, `local_type`, `field_type`, `enclosing_type_name`,
`enclosing_ancestor`, `member_access_object`, `is_implicit_this_use`, `resolvable`,
`is_type_parameter`, `is_type_parameter_ty`, and the two hub reads they use
(`members`, `resolves_type`). Reshape `receiver_type_of` into
`classify_receiver(client, text, package, object) -> Option<Receiver>` where
`Receiver { ty: Ty, is_type: bool }` (the `is_type` flag distinguishes a value
receiver from a type qualifier, so completion can offer statics only).
`receiver_type_of(node)` becomes `member_access_object(node).and_then(classify_receiver)`.
Add `receiver_before_dot(client, text, package, tree, offset) -> Option<Receiver>`,
porting the deprecated `receiver_before_dot`/`receiver_before_dot_from_text`
(`src/deprecated/analysis.rs`): back up over the typed prefix and whitespace to the
dot, find the node ending there, and classify it. `is_type` needs one addition to
`identifier_type`: report whether the fallback resolved to a *type* (via
`resolves_type`) rather than a value.

**`src/index/read.rs`.** Answer `IndexRequest::MembersOf` with
`query::members_with_overloads` instead of `query::members`, so overloads survive
the hub; update `members_of_lists_the_types_members` accordingly. `navigate` reads
the same answer and is unaffected (it filters by name and kind).

**`src/completion.rs`.** Replace the member-access early return with a member
branch: compute the receiver before the dot via `util::receiver_before_dot`; when
it resolves, read its members over the hub (`MembersOf`), keep those whose name
starts with the typed prefix, order fields and enum constants before methods, and
offer each (fields single, methods one item per overload: `label =
Member::signature()`, `filter_text` = bare name, `insert_text = name(`, `detail` =
declaring type). When `Receiver::is_type`, keep only `Member::is_static` members.
No receiver or no resolution → no items. Reuse the module's existing `offer`
helper; give members their own rank prefixes so fields sort before methods.

## Steps

- [x] `src/util.rs` (new): move the receiver helpers from `navigate.rs`, add
  `classify_receiver`/`Receiver` and `receiver_before_dot`, and make
  `identifier_type` report a type receiver; declare `pub mod util;` in `lib.rs`.
- [x] `src/navigate.rs`: import the helpers from `util` and drop the local copies
  (behaviour-preserving — no `navigate` test changes).
- [x] `src/index/read.rs`: `MembersOf` answers `members_with_overloads`; update
  `members_of_lists_the_types_members`.
- [x] `src/completion.rs`: add the member-completion branch (receiver → members,
  fields before methods, one item per overload, statics-only for a type receiver).
- [x] `src/completion.rs` tests: a field, parameter, local, `this`, `new T()` and
  static-qualifier receiver; fields-before-methods ordering; overloads and
  inherited members; enum constants and record components; a negative
  unresolved-receiver case replacing `member_access_yields_nothing_without_the_type_layer`.
- [x] `docs/architecture.md`: the Layout (`util.rs` added; the receiver helpers'
  new home), the module table (the completion row covers member completion; a
  shared-helpers note), and the completion implementation note.
- [x] `docs/requirements.md`: traceability — the member-access half of R4 lands,
  advancing R7's type layer into completion.
- [x] `docs/dev/changelog.md`: the shipped entry.
- [x] `docs/log.md`: the reconciliation.
- [x] `cargo build --all-targets`, `cargo fmt --all -- --check`, `cargo test`
  green (AC1–AC6).
