---
type: ChangeRequest
kind: refactor
title: Shrink the indexed entry and type model
description: Each declaration is stored twice (a SymbolEntry and a Member), the container chain is re-allocated per entry, and names/types are String-heavy; share what repeats and stop storing the same declaration twice, cutting both the in-memory index and the on-disk cache.
state: in-progress
priority: medium
tags: [dev, refactor, memory, indexing]
owner: felix
---

# Problem

The index is large and so is its cache — the class-file base cache reached 3.7 GB
before it was split per archive
([base-cache-per-archive](base-cache-per-archive.md)), and the in-memory index
(the bench's peak RSS) is of the same order. The size comes from the shape of what
is stored:

1. **Every declaration is stored twice.** A method/field is a `SymbolEntry` (in
   `by_name`, for name lookup, `definition`, references, `workspace/symbol`) _and_
   a `Member` inside a `TypeInfo` (in the base, for `.`-completion, hover,
   signatures). Two representations of the same declaration.
2. **`container: Arc<[String]>` is rebuilt per entry** (`Arc::from(container.to_vec())`
   in `entry`/import/Lombok) — a fresh array plus `String` clones for every member,
   though all of a type's members share one enclosing chain.
3. **Names and types are `String`-heavy.** `SymbolEntry.name`, `Ty::Ref { name:
String }`, `Member.params: Vec<Param>`, `type_params: Vec<String>` — many small
   allocations, with the same simple names and type names repeated across a corpus.

# Proposal

Reduce what is stored, in increasing order of blast radius:

1. **Share the per-type container chain.** Thread one `Arc<[String]>` per type
   (built when entering the type, cloned per entry) instead of allocating
   `Arc::from(container.to_vec())` per entry. Local to `index.rs`.
2. **Intern repeated names and type names**, so identical `name`/`Ty::Ref.name`
   values are one `Arc<str>` rather than many `String`s. Reduces RSS (not the JSON
   cache, which serializes each `Arc`'s contents).
3. **Stop storing each declaration twice** — the big one: keep the declaration in
   one place and derive the other view (or have both views read one store). This
   touches `SymbolEntry`, `TypeInfo`, `Member`, `Ty`, and every consumer
   (index queries, completions, hover, definition, references, rename), so it needs
   its own careful pass.

# Decisions

- **D1 — Do (1) first.** It is contained to `index.rs`, removes a per-entry
  allocation, and shrinks both RSS and the JSON cache (the containers are stored).
- **D2 — (2) is RSS-only.** `Arc` interning does not shrink JSON, since each `Arc`
  serializes its contents; a deduplicating cache format would be needed for that.
- **D3 — (3) is the structural win and is planned, not rushed.** It changes the
  shape of the index and must keep every feature's answers identical.
- **D4 — Measure before and after.** Use the per-phase/`RSS` numbers (the bench's
  peak RSS, and the entry counts) so the reduction is demonstrated, not assumed.
- **D5 — Validate end to end on `example/`.** `tests/example_features.rs` drives
  the real binary over stdio against the `example/` Maven workspace and asserts
  `definition`, `documentSymbol`, `hover`, `.`-completion, `references`, and
  `workspace/symbol` answers — the features a reshaped index would break, checked
  over the wire rather than only in unit tests. Fast (~1 s) and environment-free
  (JDK indexing disabled, assertions only over workspace sources). Every slice
  keeps it green, alongside the fast `index` tests and the (slow, run
  individually) `references`/`rename` tests.

# Acceptance criteria

1. (1) No `Arc::from(container.to_vec())` per entry: a type's members share one
   container `Arc`; entry ranges/containers and every feature's answers unchanged.
2. (2) Repeated names share one allocation; RSS drops measurably on a large
   corpus; JSON cache size is unchanged by this step.
3. (3) Each declaration is stored once; RSS and the JSON cache both drop; all
   existing tests pass unchanged.
4. `cargo build`/`cargo fmt --check` clean.

# Implementation plan

## Steps

- [x] `src/index.rs`: build one `Arc<[String]>` container per type and thread it
      through `collect_entries`/`entry`/`collect_record_components`/
      `collect_lombok_entries`, replacing the per-entry `Arc::from(to_vec())`.
- [ ] Measure RSS and the per-archive cache size before/after (1).
- [ ] Intern repeated `name`/`Ty::Ref.name` values (`Arc<str>`), measured.
- [ ] Plan and implement (3) — the single representation — behind the existing
      tests, feature by feature.

## Progress

- **(1) done.** One `Arc<[String]>` container is built per type and shared by all
  of its entries (members, record components, Lombok members) instead of an
  `Arc::from(container.to_vec())` allocation per entry. `container: &mut
Vec<String>` is gone from the collectors. Verified: `cargo build`/`cargo fmt
--check` clean and all 21 `index` tests pass, including the container-sensitive
  ones (`a_nested_enums_constants_carry_the_full_container_chain`,
  `extraction_covers_every_kind_and_container_chains`,
  `record_components_are_indexed_at_their_header_positions`,
  `lombok_members_are_synthetic_entries_anchored_at_the_field`,
  `wildcard_and_static_imports_keep_their_dotted_names`).
- **(1) done for the class-file path too.** `src/classfile.rs::class_entries`
  built `Arc::from(member_container.clone())` for every member — the same
  per-entry re-allocation, and the jar and JDK paths both route through it
  (`jdk.rs::class_archive_entries`). Now one `Arc<[String]>` is built per type
  and shared by all of its members. Verified: `cargo build`/`cargo fmt --check`
  clean, the `classfile`/`base_cache`/`index` tests pass, and
  `tests/example_features.rs` is green.
- **Validation in place.** `tests/example_features.rs` (see D5) passes over the
  `example/` workspace, so each remaining slice can be checked end to end.
- **Next:** the unification itself — one declaration record + by-name/by-type
  indexes over it — in slices, keeping the above green.
