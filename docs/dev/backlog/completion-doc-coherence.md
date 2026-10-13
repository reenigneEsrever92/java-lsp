---
type: ChangeRequest
kind: improvement
title: Reconcile the completion and signature-help architecture notes
description: The deprecated "Engine core (analysis.rs)" note now describes the live hub implementation and duplicates the live completion note, which itself omits signature help; and the `MembersOf` message contract still says "members", not that overloads are kept separate.
state: done
verified: { by: felix, at: 2026-10-13T15:47:54Z }
priority: low
tags: [dev, review, docs, completion]
owner: felix
---

# Problem

Found reviewing the uncommitted member-completion work; each is documentation
drift introduced while landing the completion requests.

1. `docs/architecture.md`'s `- **Engine core** (`analysis.rs`, `TreeSitterEngine`)`
   implementation note now carries live-hub vocabulary — `util.rs`, `MembersOf`,
   `ConstructorsOf`, `signatureHelpProvider` — describing the live `completion`
   module inside a note about the deprecated `analysis.rs` engine, so it duplicates
   the live `- **Completion is its own hub module**` note. Meanwhile that live note
   does **not** mention signature help, which is described only in the module table
   and in the engine-core bullet.
2. `src/messages.rs`'s `IndexRequest::MembersOf` doc still reads "A type's members,
   inherited ones included", but the read now answers
   `query::members_with_overloads` and keeps same-named overloads separate
   (`read.rs`'s module doc and the test were updated; the message contract was not).
3. `src/navigate.rs`'s module doc still lists "a chained call (`a.b().c()`)" among
   the receiver shapes "not resolved through" — false since `chained-call-completion`
   (its `a_member_access_resolves_through_a_chained_receiver` test passes) — and is
   now also silent on a field-access receiver, which
   `field-access-receiver-resolution` resolves.

# Proposal

Make the live completion note the single home for completion **and** signature
help: move the signature-help prose there, and describe the field- and call-chain
limitations exactly. Strip the live-hub vocabulary from the deprecated engine-core
note so it describes only the deprecated engine (or reduce it to a short stub
pointing at the live modules). Update the `MembersOf` message doc to state that the
answer keeps same-named overloads separate. Correct `src/navigate.rs`'s module doc so
its receiver-limitation list matches the code.

# Decisions

- **D1 — `improvement`.** Documentation accuracy and coherence only.
- **D2 — One home for the live completion behaviour.** The live completion note
  carries completion and signature help; the deprecated engine-core note stops
  describing live modules, so the two cannot drift.
- **D3 — No functional code change.** The only non-`docs/` edits are doc comments
  in `src/messages.rs` and `src/navigate.rs`.

# Acceptance criteria

1. In `docs/architecture.md`, signature help is described in the live completion
   implementation note, and the engine-core note contains no live-module vocabulary
   (`util.rs`, `MembersOf`, `ConstructorsOf`, `signatureHelpProvider`) and does not
   duplicate the live note.
2. `src/messages.rs`'s `MembersOf` doc states that the read keeps same-named
   overloads separate.
3. `src/navigate.rs`'s module doc no longer lists a chained call as unresolved and
   reflects the field-access resolution.
4. `cargo build --all-targets`, `cargo fmt --all -- --check`, and `cargo test` are
   clean.

# Docs

- `docs/architecture.md` — the engine-core implementation note and the live
  completion implementation note.
- `src/navigate.rs` — the module doc's receiver-limitation list.
- `docs/log.md` — the reconciliation entry.

# Implementation plan

## Approach

In `docs/architecture.md`, move the signature-help prose out of the deprecated
`- **Engine core** (\`analysis.rs\`, `TreeSitterEngine`)` implementation note into
the live `- **Completion is its own hub module**` note — the single home for
completion and signature help — and remove live-hub vocabulary (`util.rs`,
`MembersOf`, `ConstructorsOf`, `signatureHelpProvider`) from the engine-core bullet
so it describes only the deprecated engine. In `src/messages.rs`, extend the
`IndexRequest::MembersOf` doc comment to say the answer keeps same-named overloads
separate. In `src/navigate.rs`, correct the module doc's receiver-limitation list to
match the code.

## Steps

- [x] `docs/architecture.md`: move the signature-help prose into the live completion
  implementation note; strip the live-module vocabulary from the engine-core bullet.
- [x] `src/messages.rs`: state on `IndexRequest::MembersOf` that same-named overloads
  stay separate.
- [x] `src/navigate.rs`: correct the module doc's receiver-limitation list (a chained
  call resolves; a field-access receiver resolves).
- [x] `docs/log.md`: the reconciliation entry.
- [x] `cargo build --all-targets`, `cargo fmt --all -- --check`, `cargo test` green.
