---
type: ChangeRequest
kind: refactor
title: Split the index subsystem into analyze, query, and util modules
description: Move every function out of index/mod.rs, index/extract.rs, and index/types.rs into analyze.rs (Java analysis and extraction), query.rs (index insert and query), and util.rs (shared value and source helpers), leaving types.rs as data only and mod.rs as the module root.
state: done
verified: { by: felix, at: 2026-10-12T00:00:00Z }
priority: medium
tags: [dev, refactor, index]
owner: felix
---

# Problem

The index subsystem's behaviour is spread across three files that each mix
concerns. `src/index/types.rs` opens by claiming the intended design — "Pure data
with its derives; every operation on it lives in `crate::index`" — yet it also
carries the intrinsic value behaviour (`Ty::display`/`substitute`,
`Member::signature`, `Prim::from_keyword`, descriptor parsing) and all of the
declared-type store logic (`TypeModel::*`, `SourceLayerIndex::*`,
`ModelLayers::*`). `src/index/mod.rs` interleaves three unrelated jobs: Java
extraction (`extract_entries` and its collectors), index mutation and
declared-type queries (`upsert_file`, `lookup`, `members`, …), and the scan
driver (`index_source`, `scan_sources`, `spawn`). Only `src/index/extract.rs` has
a single, coherent role. The file a function lives in therefore says nothing
about whether it reads Java, inserts into or queries the index, or is a shared
helper — exactly the distinction the module split should make obvious.

# Proposal

Partition every function by role and leave the type definitions behind as pure
data:

- **`analyze.rs`** — everything that reads Java: `extract.rs` wholesale, plus
  `java_parser`, `LineIndex`, the entry extractors, and the workspace `.java`
  walk.
- **`query.rs`** — everything that inserts into or reads the index data
  structures: symbol-index mutation, the declared-type store updates, the
  context rebuild, and every declared-type query (the free functions in
  `mod.rs` and the `TypeModel`/`SourceLayerIndex`/`ModelLayers` methods in
  `types.rs`).
- **`util.rs`** — the helpers that fit neither: the value behaviour on index
  data (`Ty`, `Prim`, `Param`, `Member`, `TypeInfo` methods and descriptor
  parsing) and the shared predicates/conversions (`is_type_kind`,
  `java_parser`, `params_key`).
- **`types.rs`** — the type definitions and derives only.
- **`mod.rs`** — module declarations, re-exports, `spawn`, and tests.

# Decisions

- **D1 — `types.rs` becomes data-only.** Every `impl` block and free function
  leaves it; only the `struct`/`enum` definitions and `#[derive]`s remain.
  Chosen because the file's own header already declares this intent.
- **D2 — `analyze.rs` owns all Java reading.** All of `extract.rs`; from
  `mod.rs`: `LineIndex::{new, position, range}`, `extract_entries` and its
  collectors (`collect_entries`, `collect_record_components`,
  `collect_lombok_entries`, `entry`, `import_name`), `index_source`,
  `scan_sources`, and the `.java` walk (`workspace_java_files`,
  `collect_java_files`, `is_skipped_dir`).
- **D3 — `query.rs` owns all index insert and query.** From `mod.rs`:
  `upsert_file`, `remove_from_name`, `remove_from_package`, `put`, `set_source`,
  `source_layer_index`, `rebuild_context`, `set_ready`, `find_unique`,
  `find_in_package`, `contains`, `all_types`, `nested_types`, `types_in_package`,
  `lookup`, `constructors`, `members`, `members_with_overloads`. From `types.rs`:
  every `TypeModel`, `SourceLayerIndex`, and `ModelLayers` method — the mutators
  (`insert`, `extend`, `merge`, `from_entries`, `add_entries`) as well as the
  queries (`find_unique`, `find_in_package`, `contains`, `all_types`, `types`,
  `ordered`, `new`, `layered`, `is_empty`, `stats`, `declaring`, …) — plus
  `ModelLayers`'s `Default`, and the store-only helpers `layer_candidates` and
  `synthetic_layer_uri`.
- **D4 — `util.rs` holds the shared value helpers.** From `types.rs`: the `Prim`,
  `Ty`, `Param`, `Member`, and `TypeInfo` methods (`display`, `signature`,
  `reference`, `substitute`, `simple_name`, `qualified_package`, `access_type`,
  `constructor_signature`, `bytes`, `from_keyword`, `name`, `unnamed`, `new`,
  `members`, …), the descriptor parsers (`parse_descriptor`,
  `Ty::from_descriptor`, `Ty::method_from_descriptor`, `Prim::from_descriptor`),
  `ty_bytes`, and `params_key`. From `mod.rs`: `is_type_kind` and
  `java_parser`. Chosen because these are neither index operations nor
  Java-tree analysis, and both `analyze.rs` and `query.rs` need them.
- **D5 — `mod.rs` keeps the module root and the driver.** The `mod`/`pub use`
  declarations, `spawn` (the hub subscriber that owns the thread and runs the
  scan), and a `#[cfg(test)] mod tests`. Module-specific tests live next to the
  code they cover where that is cleaner (extraction tests in `analyze.rs`, index
  and query tests in `query.rs`), but some may stay in `mod.rs`.
- **D6 — The public paths are preserved.** `mod.rs` keeps `pub use
  self::types::{FileOrigin, IndexKind, SymbolEntry, SymbolKind, TypeContext,
  WorkspaceIndex}` and re-exports the moved functions with matching visibility
  (`pub use`/`pub(crate) use`), so `crate::index::{extract_entries, java_parser,
  upsert_file, find_unique, …}` and `crate::index::types::{Ty, Member, Prim,
  TypeInfo, …}` still resolve. `extract.rs` is replaced by `analyze.rs`. The
  `src/deprecated/` modules import these paths but are not compiled; of the
  paths they use, only `crate::index::types::params_key` moves (to `util.rs`) and
  is repathed when those modules are reintroduced.
- **D7 — Behaviour is unchanged.** A move-only refactor: no signature, logic,
  visibility, or data-shape change beyond what the module boundary requires.
- **D8 — Docs are updated with the code.** `docs/architecture.md`'s Layout block,
  the "Index subsystem" and "Type layer" module rows, and the Implementation
  notes bullets that describe `index.rs` internals are rewritten for
  `index/{mod,analyze,query,util,types}.rs`; `docs/dev/changelog.md` gets the
  shipped entry.

# Acceptance criteria

1. `src/index/types.rs` contains only type definitions and derives — no `impl`
   blocks and no free functions — and compiles.
2. Every function of the subsystem lives in `analyze.rs`, `query.rs`, or
   `util.rs`, per D2–D4; `mod.rs` holds only module declarations, re-exports,
   `spawn`, and tests.
3. No behaviour change: `cargo build`, `cargo fmt --check`, and `cargo test` (the
   `index` tests) pass, and `tests/example_features.rs` is green.
4. `crate::index::{SymbolEntry, SymbolKind, IndexKind, FileOrigin, TypeContext,
   WorkspaceIndex}`, the re-exported `crate::index::*` functions, and
   `crate::index::types::*` still resolve.
5. `docs/architecture.md` reflects the split.

# Implementation plan

## Approach

A move-only split of `src/index/`. `types.rs` keeps only the type definitions
(the symbol table, the declared-type store and model, and the shared identity
types); `analyze.rs` takes all Java reading (the whole of `extract.rs` plus the
`LineIndex` impl, the entry extractors, and the `.java` walk from `mod.rs`);
`query.rs` takes every function that inserts into or reads the index (the
symbol-table upsert and removal, the declared-type store updates and context
rebuild, the declared-type queries, and the `TypeModel`/`SourceLayerIndex`/
`ModelLayers` methods); and `util.rs` takes the value behaviour on the data
(`Ty`/`Prim`/`Param`/`Member`/`TypeInfo` methods and the descriptor parsers)
plus the shared predicates and keys (`is_type_kind`, `java_parser`,
`params_key`). `mod.rs` becomes the module root — declarations, re-exports,
`spawn`, and the scan test. Because the moved `TypeModel`/`SourceLayerIndex`/
`ModelLayers` methods now live in `query.rs`, their previously private fields
and `TypeInfo::bytes` widen to `pub(crate)` (the only visibility change the
boundary requires), and the flat `crate::index::<fn>` paths are preserved with
re-exports.

## Steps

- [x] Create `src/index/analyze.rs` (all of `extract.rs` plus the `LineIndex`
      impl, the entry extractors, `index_source`, `scan_sources`, and the
      `.java` walk) and delete `extract.rs`.
- [x] Create `src/index/query.rs` (symbol-table upsert and removal, the
      declared-type store updates and context rebuild, the declared-type
      queries, and the `TypeModel`/`SourceLayerIndex`/`ModelLayers` methods).
- [x] Create `src/index/util.rs` (the `Prim`/`Ty`/`Param`/`Member`/`TypeInfo`
      methods, descriptor parsing, `params_key`, `is_type_kind`, `java_parser`).
- [x] Reduce `src/index/types.rs` to the type definitions and derives; widen the
      private fields the moved methods need to `pub(crate)`.
- [x] Reduce `src/index/mod.rs` to declarations, re-exports, `spawn`, and the
      scan test; move the extraction tests to `analyze.rs`.
- [x] Update `docs/architecture.md` (Layout, the index/type module rows, and the
      implementation notes) for `index/{mod,analyze,query,util,types}.rs`.
- [x] Verify: `cargo build`, `cargo fmt --check`, `cargo test index`, and
      `cargo test`.
