---
type: ChangeRequest
kind: feature
title: Serve parameter completion inside a call (signature help)
description: The shell never advertises textDocument/signatureHelp, so completing a call's arguments offers nothing; serve the callee's overloads with their declared parameters and the cursor's active parameter, plus constructor overloads for `new T(...)`.
state: done
verified: { by: felix, at: 2026-10-13T15:21:20Z }
priority: medium
tags: [dev, feature, completion, signatures]
owner: felix
---

# Problem

The shell advertises no `textDocument/signatureHelp` and no live module answers it,
so once a call's argument list is open (`calc.add(`), the editor shows no parameter
guidance — the "parameter completion" half of typing a call. The old engine served
it (`src/deprecated/analysis.rs`, `TreeSitterEngine::signature_help`): the callee's
overloads rendered with their declared parameters and the active parameter taken
from the cursor's position among the arguments, and the created type's constructor
overloads for a `new T(...)`. None of it is live.

# Proposal

Serve signature help. For the call the cursor sits in, resolve the callee's
overloads through the receiver's declared type (the shared receiver helper) and
answer with each overload's declared signature and the active parameter counted
from the cursor's argument position; a `new T(...)` answers with the created type's
constructor overloads. Advertise `signatureHelpProvider` with `(` and `,` as the
retrigger characters.

# Decisions

- **D1 — Served by the `completion` module.** A new
  `CompletionRequest::SignatureHelp` variant on the existing `Completion` family,
  sharing the receiver-resolution helper (`util.rs`) and the index's member reads;
  this mirrors `navigate` serving four LSP methods from one request family. A
  separate module was rejected — it would duplicate the receiver step.
- **D2 — Conservative: no guess.** No help when the callee or its receiver cannot
  be resolved, matching the old engine and the project's refusal policy.
- **D3 — Constructors are answered too.** `new T(...)` returns `T`'s constructor
  overloads. This needs one index read the hub does not expose yet — a constructors
  read alongside the members read (`TypeInfo::constructors`, already modelled) —
  added as part of this request.
- **D4 — Rendering matches the old engine.** `label = Member::signature()` for a
  method and `Member::constructor_signature()` for a constructor,
  `active_signature = 0`, and each signature carrying `active_parameter` counted
  from the call's arguments (skipping comment nodes).
- **D5 — No parameter offsets or labels beyond the signature string** (as before);
  the signature text is the whole help.
- **D6 — Depends on `member-completion`/`chained-call-completion`** for the shared
  receiver helper.

# Acceptance criteria

1. `calc.add(` where `add` has two overloads returns both signatures, and the
   reported `activeParameter` follows the cursor across `,` boundaries.
2. `new Point(` returns the constructor overloads; an implicit no-arg constructor
   answers where the model holds it.
3. A callee or receiver that cannot be resolved returns no help (no wrong
   signature).
4. The shell advertises `signatureHelpProvider` (triggers `(` and `,`) and answers
   `textDocument/signatureHelp`.
5. `cargo build`, `cargo fmt --check`, and `cargo test` are clean.

# Docs

- `docs/architecture.md` — the module table (the completion row also serves
  signature help), the shell row's advertised capabilities, and the completion
  implementation note.
- `docs/requirements.md` — traceability: signature help completes the typing
  support the v0.3 milestone describes (one item per overload with a
  signature-help request that follows the cursor's argument).
- `docs/dev/changelog.md` — the shipped entry.
- `docs/log.md` — the reconciliation.

# Implementation plan

## Approach

Signature help is the same shape of query as completion — find the call at the
cursor, resolve the callee's type, read its members — so it rides the `completion`
module and the shared receiver helper rather than a new module. The rendering is a
port of the deprecated `TreeSitterEngine::signature_help`
(`src/deprecated/analysis.rs`) onto the hub vocabulary: `enclosing_call`,
`enclosing_object_creation`, and `active_parameter` move to `src/completion.rs`.
The vocabulary grows by one request on each side: `CompletionRequest::SignatureHelp`
→ `CompletionResponse::SignatureHelp(Option<SignatureHelp>)`, and
`IndexRequest::ConstructorsOf { ty, package }` →
`IndexResponse::Constructors(Vec<Member>)` answered by `query::constructors`
(already modelled, `TypeInfo::constructors`). Methods are read through the now
overload-preserving `MembersOf`; the callee name and receiver come from the shared
`util` helper, so a `recv.name(` call resolves through the receiver's declared type
and an unqualified call through the enclosing type. The shell advertises
`signatureHelpProvider` with `(` and `,` retrigger characters and forwards
`textDocument/signatureHelp`.

## Steps

- [x] `src/messages.rs`: `CompletionRequest::SignatureHelp { uri, position }`,
  `CompletionResponse::SignatureHelp(Option<SignatureHelp>)`,
  `IndexRequest::ConstructorsOf { ty, package }`,
  `IndexResponse::Constructors(Vec<Member>)`.
- [x] `src/index/read.rs`: answer `ConstructorsOf` with `query::constructors`; add
  its test.
- [x] `src/completion.rs`: an `enclosing_call`/`enclosing_object_creation` reader
  and `active_parameter`, and a `signature_help` entry point that resolves the
  callee's overloads through the shared receiver helper (methods via `MembersOf`,
  constructors via `ConstructorsOf`) and renders them with the active parameter;
  `serve` routes `CompletionRequest::SignatureHelp`.
- [x] `src/server.rs`: advertise `signatureHelpProvider` (triggers `(`/`,`) and
  implement `textDocument/signatureHelp`; update the initialize comment.
- [x] `src/completion.rs` tests: overloads with a moving `activeParameter`;
  `new Point(` constructor overloads (and an implicit no-arg constructor); no help
  for an unresolvable callee.
- [x] `docs/architecture.md`: the module table (the completion row also serves
  signature help), the shell capabilities row, and the completion implementation
  note.
- [x] `docs/requirements.md`: traceability — signature help completes the typing
  support the v0.3 milestone describes.
- [x] `docs/dev/changelog.md`: the shipped entry.
- [x] `docs/log.md`: the reconciliation.
- [x] `cargo build --all-targets`, `cargo fmt --all -- --check`, `cargo test`
  green (AC1–AC5).
