---
type: ChangeRequest
kind: bug
title: Separate completions for same-named symbols
description: Keep completion items for symbols that share a simple name but are imported differently as distinct, package-labeled entries.
state: done
priority: high
tags: [dev, completions, index]
owner: felix
verified:
  by: cargo test (137 passed - 112 lib, 6 bench bin, 18 harness, 1 stdio)
  at: 2026-09-22T19:48:30Z
---

# Problem

Reported while chasing a missing inlay hint: accepting the `List` completion
inserted `import java.awt.List;` rather than `import java.util.List;`, so
`List.of(5)` still did not resolve and no hint appeared.

The root cause is in the workspace-index completion loop, which deduplicated by
the *insert text* alone (`offer`, `src/engine/syntax.rs`). Every symbol sharing a
simple name therefore collapsed into one item, and the first one encountered won.
`WorkspaceIndex::query_prefix` orders by name and then position (`src/index.rs`)
— not by package — so with a real JDK, which indexes `java.util.List` *and*
`java.awt.List`, the surviving item was an arbitrary one: `java.awt.List`.

The user could not reach `java.util.List` from the completion list at all, and
the item they did get carried a wrong import — a wrong result, against the
server's own "no result beats a wrong result" rule. The same collapse also hit
members: two same-named methods on different types became one item with an
arbitrary enclosing type.

# Proposal

Keep symbols that share a simple name but are imported differently as separate
completion items, each labeled with its owning package.

# Decisions

- **Deduplicate by symbol, not by insert text.** The dedupe key is the insert
  text plus the import the item carries, so two `List` types from different
  packages are two items, while duplicate index entries for one symbol (the real
  JDK indexes `java.awt.List` several times) collapse to one.
- **A shared name is labeled with its owner.** When more than one symbol claims
  the same simple name, each item's detail names the owner (`class of java.awt`,
  `interface of java.util`, `method of java.util.List`). An unambiguous name
  keeps its existing detail, so nothing else changes.
- **A local still shadows a same-named workspace symbol.** A scope name and an
  index symbol with the same simple name and no import need still collapse to one
  item (the local), preserving the existing ranking.
- **No guessing.** The server does not choose a package for the user; it offers
  the candidates with the information needed to tell them apart.

# Acceptance criteria

- Two types with the same simple name in different packages are offered as two
  items, each with its own import edit and its owning package in the detail.
- Duplicate index entries for one symbol collapse to a single item.
- A local and a same-named, same-file field still yield one item (negative test).
- Verified by a unit test in `src/engine/syntax.rs` and confirmed against a real
  JDK: typing `Li` now offers `class of java.awt` and `interface of java.util`,
  each with its own `import` edit.

# Implementation plan

## Steps

- [x] Give `offer` an explicit dedupe key instead of using the insert text.
- [x] Key index items by insert text plus their import, so distinct imports stay
      separate while one symbol's duplicates collapse.
- [x] Add `qualified_owner`/`symbol_identity`/`ambiguous_names` and label a shared
      name with its owner.
- [x] Unit test: two same-named types in different packages, each with its import.
- [x] Confirm against the real JDK; update `docs/architecture.md` and the
      changelog.
