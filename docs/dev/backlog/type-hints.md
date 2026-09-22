---
type: ChangeRequest
kind: feature
title: Type and parameter inlay hints
description: Render inferred and declared types, parameter names, and chained-call return types as LSP inlay hints.
state: done
priority: medium
tags: [dev, types, lsp, ux]
owner: felix
verified:
  by: cargo test (128 passed - 105 lib, 6 bench bin, 16 harness, 1 stdio)
  at: 2026-09-22T18:10:45Z
---

# Problem

The server is type-aware (R7) but only surfaces types on demand, through hover
and `.`-completions. Editors now render **inlay hints** — annotations drawn
inline with the code — and Java hides a lot of type information behind
inference and sugar: `var` locals, the diamond operator, and method chains all
obscure the type a reader has to reconstruct mentally. The type layer already
computes exactly this information, but nothing exposes it: `ServerCapabilities`
(`src/server.rs:94`) advertises no inlay-hint provider, and `SemanticEngine`
(`src/engine/mod.rs:16`) has no hint method, so the feature is unavailable in
every client.

# Proposal

Add LSP inlay hints in three families, served from the open document's
tree-sitter tree plus the existing type layer:

1. **Variable type hints** — the resolved type rendered after a local variable
   or field name (e.g. `var total = compute();` shows `total: int`).
2. **Parameter name hints** — the parameter name rendered before an argument at
   a call site (e.g. `f(count: 5)`).
3. **Chained-call hints** — the return type of each intermediate link in a
   method/field access chain.

The feature lands as a new `SemanticEngine::inlay_hints` method (with a default,
so the `SyntaxOnlyEngine` stub stays a conforming reference), an
`inlayHintProvider` capability advertised by the shell, and the computation
itself in `src/engine/syntax.rs` on top of `src/types.rs`. Hints are computed
on demand from the open document, scoped to the range the client requests.

# Decisions

- **Interpretation and scope: inlay hints, all three families.** "Type hints"
  is read as `textDocument/inlayHint`; variable types, parameter names, and
  chained-call return types are all in scope for this request.
- **`var` and diamond inference is pulled forward.** Resolving the type of
  `var x = expr;` (and `new Foo<>()`) was an explicit non-goal of
  `type-aware-engine`, but it is precisely what makes a variable type hint worth
  showing, so this request implements it by reusing the existing
  `receiver_type` on the initializer. This also corrects a latent behaviour:
  `collect_locals` (`src/types.rs:944`) reads the declaration's `type` node, so
  today `var x = ...` yields `Ty::reference("var")`, which hover and completions
  would surface as a type literally named `var`.
- **Variable-hint coverage: locals and fields.** Every local variable
  declaration (whether written with an explicit type, `var`, or the diamond) and
  every field gets a type hint. Parameters do not — their type is already
  written beside them at the declaration site.
- **Conservative, as everywhere in this server.** An initializer, receiver, or
  callee that the type layer cannot pin to a single answer yields **no** hint
  rather than a guess. Hints do not require an indexed JDK or dependency to
  fire; they render whatever the model can prove.
- **Parameter-name hints are limited by a real data gap.** `Member.params` is
  `Vec<Ty>` — parameter *types* only (`src/types.rs:221`) — and JVM descriptors
  carry no parameter names at all, so a jar/JDK-from-classfile method can never
  supply them. The type model is therefore extended to retain parameter names
  for source-derived types (workspace sources, and `src.zip`-backed JDK types),
  which also lets hover render fuller signatures. A call to a library method
  declared only as a class file produces no parameter-name hint.
- **Chained-call hints follow the intermediate-link rule.** An invocation earns
  a hint only when its result is immediately dereferenced — the receiver of a
  further `.field` or `.method()` — showing that call's return type. The
  outermost call of a chain and standalone statements get no hint, which keeps
  the annotation from restating every call.
- **Range-scoped, on demand, never blocking (R6).** The inlay-hint request
  always carries a `range` — the spec calls it "the visible document range for
  which inlay hints should be computed" — so the tree walk and inference are
  bounded to that range and cost scales with what is on screen, not with file
  size. Hints are computed per request from the open document with whatever
  model exists, so they are partial during warm-up and fill in afterwards,
  exactly like completions.
- **No lazy resolve, no cache.** `InlayHintOptions` has no `range` flag (the
  range is always in the request), and the expensive part of a hint is the
  inference that produces its label — not the optional `tooltip`/`text_edits`
  that `inlayHint/resolve` exists to defer — so `resolve_provider` stays off.
  A per-version hint cache is likewise deferred: nothing else in this codebase
  caches, and it would add invalidation complexity for an unproven gain.
- **The LSP layer changes here, unlike R7.** A new `SemanticEngine::inlay_hints`
  method gets a default (returning no hints) so the stub conforms unchanged, and
  the shell advertises `inlayHintProvider` and routes `textDocument/inlayHint`.
- **Labels are plain.** A type hint renders `: Type`, a parameter hint renders
  `name:`, with the hint's `kind` set to `Type`/`Parameter` respectively. Hints
  carry no `text_edits` and no tooltips.
- **No server-push refresh.** Hints computed before the model is ready are
  simply partial; they appear on the client's next re-request (an edit or a
  scroll). `workspace/inlayHint/refresh` is deliberately left out of this slice.

# Acceptance criteria

- Variable type hints: a `var` or diamond local shows its initializer's inferred
  type; an explicitly typed local and a field show their declared type; an
  initializer the layer cannot resolve yields no hint.
- Parameter name hints: arguments at a call to a source-declared method (a
  workspace type, or a JDK type indexed from `lib/src.zip`) are annotated with
  the parameter names; a call to a jar/classfile-declared method yields no
  parameter-name hint (negative test included).
- Chained-call hints: each intermediate link of a chain is annotated with its
  return type; the outermost invocation and a standalone call are not (negative
  test included).
- Every hint family returns nothing for an unresolved or ambiguous target, and
  no hint family requires an indexed JDK or dependency.
- Range scoping: only nodes intersecting the requested range produce hints, and
  a request with a small range over a large document never infers the whole
  tree.
- The shell advertises `inlayHintProvider` and routes `textDocument/inlayHint`
  to the engine; the `SyntaxOnlyEngine` stub still conforms (returns no hints).
- Verified by unit tests per family (including the negative cases) plus a
  `tests/harness.rs` test driving `textDocument/inlayHint` end to end against a
  fixture workspace.

# Documentation impact

- `docs/architecture.md` — add `inlay_hints` to the `SemanticEngine` method
  list, add `inlayHintProvider` to the shell-capabilities sentence, and add a
  component bullet describing hint computation: the three families, range
  scoping, `var`/diamond inference, and the parameter-name limitation for
  library methods.
- `docs/requirements.md` — add a numbered functional requirement for inlay
  hints, add a milestone entry for it, and record that `var` inference has moved
  out of "deferred".
- `docs/dev/changelog.md` — recorded when the change ships (`fawi-implement`).

# Implementation plan

## Approach

Hints are computed on demand in `TreeSitterEngine` from the open document's
parsed tree plus the same `TypeQuery` (workspace model over the open buffer's
local model) that hover and member completions already use. The tree walk is
pruned to the request's byte range, so only nodes intersecting the visible
region are visited. No new dependencies.

- **Trait seam.** `SemanticEngine::inlay_hints(&self, uri, range) -> Vec<InlayHint>`
  with a default returning an empty list, so the `SyntaxOnlyEngine` stub stays a
  conforming reference. A range in the request is always present in the protocol
  (`InlayHintParams.range`), so the engine takes it directly.
- **Parameter names in the type model.** `Member.params` is `Vec<Ty>` today and
  cannot carry names. Replace it with `Vec<Param>`, where
  `Param { name: Option<String>, ty: Ty }`: source extraction
  (`collect_members`) fills the name from each `formal_parameter`/`spread_parameter`'s
  `name` field, while class-file members (`classfile.rs`) and name-only synthesized
  types leave it `None`. `Member::display`/`signature` render the name when
  present, which also enriches hover.
- **`var`/diamond inference.** A local whose declared type node is the `var`
  keyword resolves its type by feeding the declarator's initializer to the
  existing `receiver_type`; an uninferrable initializer yields no hint. This also
  stops `collect_locals`-style reads from treating `var` as a type named `var`
  for hint purposes.
- **Variable hints.** `local_variable_declaration`, `field_declaration`,
  `constant_declaration`, and `enhanced_for_statement` nodes each contribute a
  `Type` hint after every declared name; the type is the declared type, or the
  initializer's inferred type for `var`.
- **Parameter hints.** For a `method_invocation`, resolve the callee through the
  receiver type (or the enclosing type for an unqualified call) with the existing
  `member_of`, then annotate each argument with the matching parameter's name as
  a `Parameter` hint. Callees whose parameters carry no names (jar/JDK class
  files) contribute nothing.
- **Chained-call hints.** A `method_invocation` that is the `object` child of a
  parent `field_access` or `method_invocation` is an intermediate link: its
  `receiver_type` return type becomes a `Type` hint after its closing paren. The
  outermost call and standalone calls are skipped.
- **Conservative.** Any `Unknown`/ambiguous type or unresolved callee produces no
  hint, matching the rest of the server.
- **Shell.** Advertise `inlayHintProvider` and route `textDocument/inlayHint`
  through a new handler that forwards `text_document.uri` and `range` to the
  engine.

## Steps

- [x] Replace `Member.params: Vec<Ty>` with `Vec<Param>` in `src/types.rs`
      (new `Param` struct with an optional name), update `collect_members` to
      capture parameter names and `Member::display`/`signature` to render them,
      update `src/classfile.rs` to build name-less `Param`s, and adjust the
      affected unit tests. (AC: parameter hints; enriches hover.)
- [x] Add `SemanticEngine::inlay_hints` (default `Vec::new()`) in
      `src/engine/mod.rs`, import the LSP types, and give `SyntaxOnlyEngine` an
      explicit empty override in `src/engine/stub.rs`. (AC: stub conforms.)
- [x] Implement hint computation in `src/engine/syntax.rs`: a range-pruned tree
      walk plus the three families (variable/`var`, parameter, chained-call),
      reusing `scope_at`/`receiver_type`/`member_of` and `lsp_position`; unit
      tests per family including the negative cases (unknown initializer,
      library callee, outermost call) and a range-scoping test. (AC1–AC5.)
- [x] Advertise `inlayHintProvider` and implement `textDocument/inlayHint` in
      `src/server.rs`, forwarding the document URI and range to the engine.
      (AC6.)
- [x] Add a `tests/harness.rs` test driving `textDocument/inlayHint` end to end
      against a fixture workspace. (AC7.)
- [x] Update `docs/architecture.md`: the new trait method and capability, plus a
      component bullet for hint computation (families, range scoping,
      `var`/diamond inference, the parameter-name limitation). (AC: docs.)
- [x] Update `docs/requirements.md`: a numbered requirement for inlay hints, a
      milestone entry, and `var` inference moved out of "deferred". (AC: docs.)
- [x] Record the change in `docs/dev/changelog.md` under today's date.
