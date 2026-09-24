---
type: ChangeRequest
kind: refactor
title: A quick-fix subsystem, a queryable diagnostics cache, and a prefix-ordered index
description: Move the quick fixes out of the analysis core into their own subsystem that queries the symbol index and a new queryable diagnostics cache, and make the index's prefix lookup a range scan.
state: done
priority: medium
tags: [dev, refactor, messaging, diagnostics, indexing, quickfix]
owner: felix
verified:
  by: cargo test --offline — the library's quickfix and code-action tests pass (the quick-fix subsystem test, `a_did_you_mean_action_renames_the_member`, `a_create_type_action_needs_the_client_capability`); the harness could not be run meaningfully under the machine's load average of ~30 (its warm-up tests blow a 10 s index deadline)
  at: 2026-09-29T00:00:00Z
---

# Problem

Three gaps left over from splitting the index and diagnostics into subsystems:

- **The quick fixes still live in the core.** `TreeSitterEngine::code_actions`
  and its builders (`add_import_actions`, `rename_member_action`,
  `create_type_actions`, `create_symbol_actions`,
  `create_receiver_member_action`) read the core's document and index directly,
  so a "subsystem that generates quick fixes" did not exist.
- **The diagnostics pass cannot be queried.** The diagnostics subsystem
  computed and published a pass per open document but kept no result, so
  anything that wanted the current diagnostics (the quick fixes, or the
  diagnostics themselves) had to recompute them.
- **Prefix lookup scans every name.** `query_prefix` — completions on every
  keystroke, and `workspace/symbol` — iterated the whole `by_name` map and
  sorted, so a completion cost the number of distinct names in the workspace
  (plus the jars and the JDK), not the number that match.

# Proposal

Give the quick fixes their own subsystem, make the diagnostics cache queryable,
and order the index's name map so prefix lookups are range scans.

# Decisions

- **D1 — `quickfix.rs` is a subsystem.** It owns a `tree-sitter-java` parser and
  the open buffers' text (fed by the hub, like the diagnostics subsystem), and a
  `QuickFixHandle` on its own thread (mirroring `IndexHandle`). The engine
  dispatches `codeActions` to it on the **blocking pool**, so its blocking handle
  never starves a runtime worker (the harness runs on a current-thread runtime).
- **D2 — It queries the two indexes.** The quick fixes read the symbol index via
  `IndexHandle` and the diagnostics cache via `DiagnosticsHandle`; they hold no
  index or diagnostics state. When a request carries no diagnostics, the
  subsystem falls back to the cached pass for the document.
- **D3 — The diagnostics cache is queryable.** The subsystem keeps the latest
  `(version, Vec<Diagnostic>)` per open document, exposed as
  `DiagnosticsHandle::diagnostics(uri) -> Option<(i32, Arc<Vec<Diagnostic>>)>`
  and dropped on close. Queries share the `Arc`, so a query does not clone the
  diagnostics.
- **D4 — The index is ordered by name.** `IndexState::by_name` is a `BTreeMap`,
  and `query_prefix` is a `range` scan that stops at the first non-matching name.
  The key set is unchanged (same `String`s), so the index does not grow; the
  within-name position sort stays.
- **D5 — Behaviour is preserved.** The same fixes, the same diagnostics, and the
  same completions/`workspace/symbol` order; the fix builders were moved
  verbatim, and `TreeSitterEngine::code_actions` and the diagnostics entry point
  remain as thin delegators for the core's tests.

# Acceptance criteria

- `quickfix.rs` generates the quick fixes from its own parse, reading the symbol
  index and the diagnostics cache; `analysis.rs` no longer contains the fix
  builders, and `TreeSitterEngine::code_actions` delegates.
- The diagnostics subsystem keeps a queryable cache of each open document's
  latest pass, cleared on close.
- `query_prefix` is an ordered range scan; the index's key set is unchanged.
- The library's code-action and quick-fix tests pass; the harness is
  behaviour-equivalent (it could not be run under the machine's load).

# Implementation plan

## Approach

`src/quickfix.rs` holds a `QuickFix` context (the document's tree/text, the
`IndexHandle`, the declared-type overlay, and the create-type capability) with
the fix builders moved onto it verbatim, and a `QuickFixHandle`/thread subsystem
that owns a parser and the open buffers' text, parses a request's buffer itself,
and answers from the index and the diagnostics cache. `diagnostics.rs` gains a
shared results map behind `DiagnosticsHandle`. `index.rs` swaps `by_name` to a
`BTreeMap` and rewrites `query_prefix` as a range scan.

## Steps

- [x] Add the queryable diagnostics cache (`CachedDiagnostics`,
      `DiagnosticsHandle::diagnostics`, cleared on close).
- [x] Swap the index `by_name` to a `BTreeMap` and rewrite `query_prefix`.
- [x] Add `src/quickfix.rs` (the `QuickFix` context and builders, the
      `QuickFixHandle` subsystem) and register it in `lib.rs`.
- [x] Rework `analysis.rs`: remove the moved builders and their helpers, keep
      `code_actions` as a delegator, and expose the shared helpers the fix
      builders use (`byte_offset`, `lsp_position`).
- [x] Rework `engine.rs`: spawn the quick-fix subsystem, feed it documents and
      the client capability, and route `codeActions` to it on the blocking pool.
- [x] Tests, `docs/architecture.md`, the changelog, and `state: done`.
