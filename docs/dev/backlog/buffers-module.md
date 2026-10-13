---
type: ChangeRequest
kind: refactor
title: Own the editor's unsaved text in a buffers module
description: Move the open-buffer store out of the deprecated document module into a live `buffers` hub module, and have completion and navigation read a file's current text through it, so unsaved edits and untitled buffers are visible instead of stale disk content.
state: done
verified: { by: felix, at: 2026-10-13T14:00:00Z }
priority: medium
tags: [dev, refactor, buffers, completion, navigation]
owner: felix
---

# Problem

Completion and navigation read the file they answer about from disk
(`tokio::fs::read_to_string` in `completion.rs` and `navigate.rs`). The editor's
unsaved text is never consulted, so an unsaved edit is invisible: a local or
field typed but not saved is not offered, the word under the cursor is computed
against stale text, and an untitled buffer — which has no file path — yields
nothing at all. The shell advertises incremental text sync but implements no
`didOpen`/`didChange`/`didClose`, so the editor's edits never reach the hub; the
open-buffer store lives in the deprecated `document.rs` (`DocumentStore`) and the
`DocumentOpened`/`DocumentChanged`/`DocumentClosed` notifications the shell would
send are defined in `messages.rs` but produced by no one.

# Proposal

Reintroduce the document store as a live top-level hub module, `src/buffers.rs`,
labelled `buffers`: it owns a [`DocumentStore`] and consumes the three document
lifecycle notifications — `DocumentOpened` (full text), `DocumentChanged` (the
incremental changes, which the module applies itself), and `DocumentClosed` — and
answers a new `BuffersRequest::Text` read. The shell forwards
`didOpen`/`didChange`/`didClose` as those notifications. Completion and navigation
read a file's current text through `buffers::current_text` — the open buffer when
the editor has it open, otherwise the file on disk — instead of reading the file
themselves, so an unsaved edit (or an untitled buffer) is visible.

# Decisions

- **D1 — The module is `buffers`, not `document`.** It owns the editor's
  *buffers* (URI → text), so the module and its label say so, and the request
  family is `BuffersRequest`/`BuffersResponse` (`Request::Buffers`). Chosen over
  keeping the deprecated `document` name.
- **D2 — `current_text` is the read a consumer calls.** It resolves the open
  buffer first, then disk, so a consumer needs one call and untitled buffers work.
  Chosen over a bare `BuffersRequest::Text` at each call site, which would make
  every consumer re-implement the "open, else disk" policy.
- **D3 — The module applies the incremental changes itself.** `DocumentChanged`
  carries the changes (as `messages.rs` already documents: "the owner of the
  document's text applies them"), so the shell holds no document state and does
  no text splicing.
- **D4 — Ordering: a queued edit is applied before a read.** Notifications and
  requests arrive on separate channels, and `tokio::select!` polls ready branches
  at random, so a read could be served against pre-edit text. The module's
  `select!` is `biased` with the notification branch first, so a document event
  already queued is always applied before a request is served.
- **D5 — Both consumers read through the module.** Completion's requested file
  and navigation's cursor file and each candidate source go through
  `current_text`, so a cross-file unsaved edit is visible to find-references too.

# Acceptance criteria

1. `src/buffers.rs` exists, labels itself `buffers`, owns a `DocumentStore`,
   consumes `DocumentOpened`/`DocumentChanged`/`DocumentClosed`, and answers
   `BuffersRequest::Text`.
2. `Messages`: `Request::Buffers`/`Response::Buffers`, `BuffersRequest::Text`,
   `BuffersResponse::Text(Option<Arc<String>>)`; `hub::owner_of` routes `Buffers`
   to `buffers`; `main.rs::setup` spawns it; `lib.rs` declares it.
3. The shell implements `did_open`/`did_change`/`did_close` and notifies the
   three document lifecycle notifications.
4. `completion.rs` and `navigate.rs` read text through `buffers::current_text`,
   falling back to disk; an unsaved edit and an untitled buffer are visible.
5. `cargo build`, `cargo fmt --check`, and `cargo test` are clean, with no new
   clippy warnings.

# Docs

- `docs/architecture.md` — the rework note, the Layout (`buffers.rs` added;
  `document.rs` dropped from `deprecated/`), the module table (the document-store
  row becomes the buffers row; the shell and completion/navigate rows), and a
  buffers implementation note.
- `docs/dev/changelog.md` — the shipped entry.
- `docs/log.md` — the reconciliation.

# Implementation plan

## Approach

Port the deprecated `DocumentStore` (`open`/`change`/`close`/`get`,
`position_to_offset`, `text_of`) into `src/buffers.rs` against the new hub
vocabulary, spawn it, add the message family and the `current_text` helper, wire
the shell's lifecycle handlers, and repoint completion and navigation.

## Steps

- [x] `messages.rs` — `Request::Buffers`, `BuffersRequest::Text`,
      `BuffersResponse::Text(Option<Arc<String>>)` (default `None`),
      `Response::Buffers`.
- [x] `hub.rs` — `owner_of` routes `Request::Buffers` to `buffers`.
- [x] `src/buffers.rs` (new) — the ported `DocumentStore`, `spawn` (biased
      `select!`), `current_text`, and its tests; declared in `lib.rs` and spawned
      in `main.rs`.
- [x] `server.rs` — `did_open`/`did_change`/`did_close` notify the three document
      lifecycle notifications.
- [x] `completion.rs` / `navigate.rs` — read through `buffers::current_text`; a
      test shows completion using an unsaved buffer over disk.
- [x] Docs and `cargo fmt --all -- --check`, `cargo build --all-targets`,
      `cargo test` green.

## Progress

Landed as above. Verified: `cargo fmt --all -- --check` clean; `cargo test` green
— 72 library tests and 7 bench tests; no new clippy warnings (the pre-existing
`never_loop` in `src/bin/java-lsp-bench.rs` is unchanged). Not committed.
