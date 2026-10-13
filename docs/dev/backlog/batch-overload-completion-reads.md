---
type: ChangeRequest
kind: improvement
title: Batch the workspace-index overload reads
description: The workspace-index completion path issues one `MembersOf` request per matching method entry, sequentially, so a short prefix can fire many per-keystroke index round-trips; resolve the overloads in one batched read.
state: done
verified: { by: felix, at: 2026-10-13T15:42:08Z }
priority: low
tags: [dev, review, completion, performance]
owner: felix
---

# Problem

Found reviewing the uncommitted member-completion work.

In `src/completion.rs`'s workspace-index loop, each entry whose kind is `Method`
calls `method_overloads`, which issues a separate `util::members`
(`IndexRequest::MembersOf`) hub request — **sequentially, one per matching entry**.
The entries come from `EntriesByPrefix`, which returns every entry whose simple name
starts with the typed prefix, so a short prefix can match many method entries across
the workspace and a single completion can fire many index round-trips. Completion
runs on every keystroke and R6 requires it to stay responsive and never block; the
deprecated engine resolved these overloads in-process (its `model_overloads`), so
this is new per-entry traffic on a hot path.

# Proposal

Resolve the overloads for the matching method entries in one batched index read
(`IndexBatchRequest`), instead of a request per entry inside the loop. Deduplicate
by declaring type so several entries of the same type share one `MembersOf`, and
keep the item shape and the name-only fallback exactly as they are.

# Decisions

- **D1 — `improvement`.** A performance enhancement to shipped behaviour, with no
  user-visible change; classified `improvement` rather than `refactor` because it
  does not restructure the module.
- **D2 — Reuse `IndexBatchRequest`.** The vocabulary already exists (`Request::IndexBatch`),
  so no new hub message type is added.
- **D3 — Behaviour-preserving.** The overload item shape and the single name-only
  fallback (when a declaring type does not resolve) are unchanged; the existing
  `call-site-overload-completion` tests must stay green.
- **D4 — Out of scope:** caching overload results across completion requests.

# Acceptance criteria

1. The workspace-index path makes at most one batched index request for its
   overload reads — no per-entry `MembersOf` inside the loop.
2. Completion output is unchanged: the `call-site-overload-completion` tests
   (two-overload listing, single non-overloaded item, name-only fallback,
   auto-import on overload items) still pass.
3. `cargo build --all-targets`, `cargo fmt --all -- --check`, and `cargo test` are
   clean.

# Docs

- `docs/architecture.md` — the completion implementation note, only if it describes
  the read pattern (otherwise no change).

# Implementation plan

## Approach

In `complete` (`src/completion.rs`), the workspace-index loop calls
`method_overloads` per matching `Method` entry, and each call is its own `MembersOf`
hub request — sequential per-entry reads. Replace that with one batched read:
collect the distinct declaring types (`entry.container.last()` with `entry.package`)
of the prefix-matching method entries, issue a single `IndexBatchRequest` (one
`MembersOf` per distinct type), and look the results up per entry in the loop. Add a
small `members_batch` helper mirroring `navigate.rs`'s `read_batch`. The item shape
and the single-name-only fallback are unchanged.

## Steps

- [x] `src/completion.rs`: add a batched members read over `IndexBatchRequest` and
  resolve the workspace loop's overloads from it instead of per-entry requests
  (dedupe by declaring type + package).
- [x] `src/completion.rs` tests: the `call-site-overload-completion` tests
  (two-overload listing, single non-overloaded item, name-only fallback,
  auto-import) stay green; output is unchanged.
- [x] `cargo build --all-targets`, `cargo fmt --all -- --check`, `cargo test` green.
