---
type: ChangeRequest
kind: improvement
title: Stop storing import declarations for library sources
description: Import entries are ~2.9M of the flat index and are read by no feature, so the dependency-source and JDK passes drop them; workspace files keep theirs.
state: done
priority: low
tags: [dev, improvement, memory, indexing]
owner: felix
verified:
  by: "cargo build/cargo fmt --check clean; the 22 `index` tests (including the two that pin workspace import entries) and the `bus`/`base_cache`/non-network `sources` tests pass, and `tests/example_features.rs` is green over the `example/` workspace."
  at: 2026-10-01T00:00:00Z
---

# Problem

The flat entry index stores one `SymbolEntry` per declaration, imports included.
On a large Maven workspace the `index composition:` line reports
`imports=2904383` of `9,396,381` entries — roughly 31 % of the flat index, about
0.39 GB — yet no feature reads an `Import` entry:

- `definition` filters `IndexKind::Import` out on both branches
  (`src/analysis.rs`, `TreeSitterEngine::definition`);
- `completion_kind`/`symbol_kind` answer `None` for it, so it is never offered;
- `ambiguous_names` skips it;
- `references`/`rename` walk `index.source_files()` (workspace files) and parse
  their text — they never turn a library `Import` entry into a location;
- the import helpers (`preferred_type_package`, `type_visible_in`,
  `has_import_ancestor`) read the file's parsed tree, not the index.

So over a library tree the entries are pure cost.

# Proposal

`src/index.rs` gains `drop_import_entries(&mut Vec<SymbolEntry>)`, which retains
every entry whose kind is not `Import`. `src/sources.rs`'s `index_one` and
`src/jdk.rs`'s `src_zip_entries` call it on each file's extracted entries before
storing them, so the two library passes index no import declarations. Workspace
extraction (`index.rs`) is untouched and keeps its imports.

# Decisions

- **D1 — Drop only in the library passes, not in `extract_entries` itself.**
  Workspace import entries are few, two `index` tests pin them
  (`extraction_covers_every_kind_and_container_chains`,
  `wildcard_and_static_imports_keep_their_dotted_names`), and keeping them keeps
  the change's blast radius to the two library call sites.
- **D2 — A shared helper on `index.rs`, not an inline `retain`.** Both library
  passes need it, and the doc comment records why imports are safe to drop.
- **D3 — `kind: improvement`.** It removes wasted work/storage without changing
  what any feature answers, not a redesign.

# Acceptance criteria

1. The dependency-source and JDK passes store no `IndexKind::Import` entries;
   workspace extraction still stores them.
2. The two `index` tests that assert workspace import entries pass unchanged.
3. `cargo build`/`cargo fmt --check` clean; the `index`, `bus`, `base_cache` and
   non-network `sources` tests pass; `tests/example_features.rs` stays green.
4. On the real corpus the `index composition:` line's `imports=` count drops from
   ~2.9M towards zero, with the other counts and the base model unchanged.
