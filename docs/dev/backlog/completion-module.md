---
type: ChangeRequest
kind: refactor
title: Reintroduce completion as its own module
description: Extract the first-slice completions out of the deprecated analysis engine into a live `completion` hub module that serves `textDocument/completion` and reads the index over the hub, offering keywords, in-scope names, and workspace symbols without type resolution.
state: done
verified: { by: felix, at: 2026-10-13T13:00:00Z }
priority: medium
tags: [dev, refactor, completion, index]
owner: felix
---

# Problem

Completions live in the deprecated `analysis.rs` (`TreeSitterEngine::completions`),
welded to the engine core and the type model, so no live module serves
`textDocument/completion` and the shell advertises no `completionProvider`. The
first slice the requirements describe (R4) needs no types at all — keywords, the
names in scope, and the workspace index symbols — so it can be a module of its own
that reads the index over the hub, like `navigate`.

# Proposal

Extract the R4 slice into a top-level hub module, `src/completion.rs`, labelled
`completion`: it serves the shell's `textDocument/completion` as a
`CompletionRequest::Complete`, reading the requested file itself and the index
only through a new coarse `EntriesByPrefix` read. It merges three ranked sources
as `sort_text` prefixes `0`/`1`/`2`: Java keywords; the locals, parameters, and
enclosing fields nameable unqualified at the cursor; and the workspace symbols
whose simple name starts with the typed prefix, each workspace symbol carrying an
auto-import edit when the file does not already resolve the name. Member
completion after a `.` needs the receiver's declared type — the type layer is not
yet reintroduced — so it yields nothing rather than a guess.

# Decisions

- **D1 — Completion is its own hub module.** `src/completion.rs`, labelled
  `completion`, serves `CompletionRequest`; the index neither contains nor serves
  completion. Chosen for separation of concerns, matching the navigation
  extraction.
- **D2 — The index read is coarse and prefix-shaped.** `EntriesByPrefix { prefix }`
  range-scans the name-ordered symbol table; the response reuses
  `IndexResponse::Entries`. Chosen over a broad mirror of the index's query
  surface.
- **D3 — The first slice resolves names without types (R4).** Keywords, in-scope
  names, and workspace symbols only. Member access yields nothing until the type
  layer returns; a method's overloads are offered name-only.
- **D4 — The auto-import check uses the prefix entries.** The same-package clash
  test reads the candidates already fetched by `EntriesByPrefix`, since any entry
  with the item's exact simple name starts with the prefix.
- **D5 — The shared identifier helpers move to `parser.rs`.** `byte_offset`,
  `word_prefix`, and `word_at` move out of `navigate.rs` into the shared,
  stateless `parser.rs`, so both modules read Java through one set of helpers.

# Acceptance criteria

1. `src/completion.rs` exists, labels itself `completion`, serves
   `CompletionRequest::Complete`, reads the requested file itself, and reads the
   index only through hub requests.
2. `Request::Completion`/`Response::Completion`, `CompletionRequest`,
   `CompletionResponse`, and `IndexRequest::EntriesByPrefix` exist; `owner_of`
   routes `Completion` to `completion`; `server.rs` sends `Request::Completion`
   and advertises `completionProvider`; `main.rs::setup` spawns `completion`.
3. `byte_offset`, `word_prefix`, and `word_at` live in `src/parser.rs` and are
   used by both `navigate` and `completion`.
4. `cargo build`, `cargo fmt --check`, and `cargo test` are clean.

# Docs

- `docs/architecture.md` — the rework note, the Layout (`completion.rs` added;
  the identifier helpers' new home), the module table (shell and a new completion
  row, plus the `EntriesByPrefix` read), and a completion implementation note.
- `docs/dev/changelog.md` — the shipped entry.
- `docs/log.md` — the reconciliation.

# Implementation plan

## Approach

Add the vocabulary and the `EntriesByPrefix` read, then the module (a port of the
deprecated completion helpers: `word_prefix`, the scoped-name collector, the
auto-import helpers, `offer`, `completion_kind`), then wire the shell and move the
shared identifier helpers into `parser.rs`.

## Steps

- [x] `messages.rs` — `CompletionRequest`/`CompletionResponse`,
      `Request::Completion`, `Response::Completion`, `IndexRequest::EntriesByPrefix`.
- [x] `hub.rs` — `owner_of` routes `Completion` to `completion`.
- [x] `index/read.rs` — answer `EntriesByPrefix` from the name-ordered symbol
      table; add its test.
- [x] `src/completion.rs` (new) — the module, its helpers, and its tests;
      declared in `lib.rs` and spawned in `main.rs`.
- [x] `server.rs` — advertise `completionProvider` and answer
      `textDocument/completion`.
- [x] `src/parser.rs` / `src/navigate.rs` — move `byte_offset`/`word_prefix`/
      `word_at` into `parser.rs` and repoint `navigate`.
- [x] Docs and `cargo fmt --all -- --check`, `cargo build --all-targets`,
      `cargo test` green.

## Progress

Landed as above. Verified: `cargo fmt --all -- --check` clean; `cargo test` green
— 59 library tests and 7 bench tests at the time (before the buffers module added
more); no new clippy warnings. Not committed.
