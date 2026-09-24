---
type: ChangeRequest
kind: feature
title: Unresolved-symbol diagnostics and quick fixes
description: Report unresolved types, members, identifiers, and imports as errors and offer code-action fixes — add import, did-you-mean, and create-stub.
state: done
priority: high
tags: [dev, diagnostics, lsp, engine]
owner: felix
verified:
  by: cargo test --all-targets (225 passed - 194 lib, 6 bench bin, 24 harness, 1 stdio)
  at: 2026-09-24T19:36:59Z
---

# Problem

java-lsp already publishes diagnostics, but the semantic half is deliberately
narrow. `TreeSitterEngine::diagnostics` (`src/analysis.rs`) yields parse errors
from tree-sitter (`ERROR`/missing nodes, severity `ERROR`) and, only when the file
parses cleanly, a single semantic family: bare, unqualified **type names** in a
declaration, `new`, a cast, `extends`, or `implements` that resolve nowhere —
reported as a **`WARNING`** whose message is `type X cannot be resolved`.

That leaves the everyday "this symbol doesn't resolve" complaints unanswered:

- `List.of(3)` with no `import java.util.List;` is **silent**, even though the
  fix is a known import the index already holds (auto-import completions add it).
- A call or field access that isn't a member of the receiver's type
  (`list.sixe()`) is silent.
- A bare identifier that resolves nowhere (`total` where no local, field, type,
  or import supplies it) is silent.
- A typo'd or unresolvable `import com.example.Nope;` is silent.

The current type diagnostic is so conservative that it also treats *any* name the
model knows in *any* package as resolved (`if model.contains(name) { return; }`),
so it can never say "you forgot the import". A developer reading Java expects the
red squiggles javac and every Java IDE give.

# Proposal

Widen semantic diagnostics from "unresolved type-name hints" to a full
unresolved-symbol check across four families, all at **`ERROR`** severity (red,
the same as syntax errors):

1. **type references** — the existing check, widened, and now distinguishing
   "unknown everywhere" from "known but not imported";
2. **member accesses** — `receiver.name` where the receiver's type is known and
   `name` is not a field or method of it;
3. **bare identifiers** — an unqualified name that resolves to no local, field,
   type, or import;
4. **import declarations** — `import a.b.C;` (and best-effort `.*` and `static`)
   whose target the index cannot supply.

Each firing diagnostic carries a `code` and `data` so the shell can offer a
quick fix: **add import** (one action per candidate when ambiguous),
**did-you-mean** for a near member name, or **create a class/interface/method
stub**. Delivering fixes adds a code-action surface to the shell
(`codeActionProvider` + `textDocument/codeAction`); the add-import edit reuses the
existing `import_edit`/`import_target` helpers, and the create-file fix uses an
LSP resource operation (`CreateFile`).

The check keeps the existing trust gate — a clean parse and a model that vouches
for `java.lang` — and is switchable off with `JAVA_LSP_SEMANTIC_DIAGNOSTICS`,
on by default.

# Decisions

- **D1 — All four families, all `ERROR`.** Types, members, bare identifiers, and
  imports are all reported at `ERROR` severity, replacing today's `WARNING` for
  type names. Reason: the user asked for red squiggles like an IDE, and a symbol
  that does not resolve is as fatal to compiling as a syntax error. This
  deliberately inverts the project's old "no result beats a wrong result"
  caution for diagnostics; the opt-out (D6) is the escape hatch.

- **D2 — Four resolution outcomes drive the message and the fix.** For a symbol
  under a value/type position the layer answers one of: **unknown** (nothing in
  the model matches) → red, no import fix, offer a create-stub; **known but not
  visible** (the model has it, but no import, same package, or `java.lang` makes
  it visible here) → red on the *usage*, offer "Add import"; **unresolved
  member** (the receiver's type is known, the name is not a member) → red,
  offer did-you-mean; **unresolvable import** → red on the *import declaration*.
  Reason: the fix must follow the cause, and this mirrors how hover/definition
  already resolve.

- **D3 — The trust gate carries over unchanged.** Semantic diagnostics only run
  when the file parses cleanly (a broken tree makes positions and scopes
  unreliable) and the model vouches for `java.lang` (`Object` and `String` are
  indexed). Reason: a missing JDK or a broken file must never become a wall of
  false positives, and the existing check already proved this gate.

- **D4 — Unresolved dependencies are flagged, not skipped.** When a dependency's
  jar or sources were never indexed (offline, missing from the local repository,
  `JAVA_LSP_OFFLINE`), its imports and usages are **red**. Reason: the user wants
  an honest report of what does not resolve; D6 is the way out when that is noisy
  on a machine with unresolved artifacts. Consequence to accept knowingly: an
  unindexed dependency lights up its types across the project.

- **D5 — The import declaration is the root cause; usages of it are not
  double-flagged.** When a simple name is covered by an import that itself failed
  to resolve, flag the import only and suppress the per-usage diagnostics for
  that name. Reason: one import error should produce one squiggle, not one per
  use.

- **D6 — Opt-out through the environment: `JAVA_LSP_SEMANTIC_DIAGNOSTICS`.**
  Unset (or any value but `0`/`false`) keeps the check on; `0`/`false` disables
  all semantic diagnostics, leaving syntax errors. Reason: the project configures
  entirely through environment variables (there is no `didChangeConfiguration`
  surface), and every existing knob — `JAVA_LSP_OFFLINE`, `JAVA_LSP_JDK` — works
  this way, so a new one is consistent and needs no new protocol surface.

- **D7 — Quick fixes ride a new code-action surface.** The shell advertises
  `codeActionProvider` (kind `quickfix`) and serves `textDocument/codeAction`; a
  new `Command::CodeActions` variant carries the request like every other query.
  Diagnostics carry `code` (`unresolved-type`, `unresolved-member`,
  `unresolved-symbol`, `unresolved-import`) and `data` (the symbol name, its
  kind, and the candidate fully-qualified names), so the handler builds edits
  from the diagnostic rather than re-deriving the analysis. Reason: keeps the
  fix aligned with what was reported and avoids a second resolution pass.

- **D8 — Add-import is offered per candidate; ambiguity is the client's choice.**
  When the index holds one importable candidate, one "Add import `a.b.C`"
  action; when several (e.g. `java.util.List` and `java.awt.List`), one action
  per candidate so the client shows a picker, and no action when the name is
  already visible. Edits come from the existing `import_edit` (after the last
  import, else after the `package`, else at the top; never-worsen rules intact).
  Reason: reuses the working auto-import path and never guesses a package.

- **D9 — Create-stub creates a type file or a member, gated on client support.**
  "Create class/interface `X`" writes a new file under the source root of the
  file's own package (or the file's directory when the workspace has no Maven
  model), with the package declaration and an empty type body — so the current
  file needs no import — delivered as a `CreateFile` resource operation plus an
  edit, **withheld unless the client advertises
  `workspace.workspaceEdit.resourceOperations` including `CreateFile`**.
  "Create method `m(...)` in `T`" (and field) inserts a stub into the enclosing
  type in the open file as a plain edit. Reason: a new type must be a new file,
  which needs a capability the server must check rather than assume.

- **D10 — Did-you-mean picks the nearest member.** For `unresolved-member`, offer
  "Change to `m`" for the closest member name by edit distance among the
  receiver's members (inherited included), when one is close enough; nothing when
  none is. Reason: mirrors what IDEs offer and stays quiet when there is no
  plausible match.

- **D11 — Imports are checked best-effort.** A single-type `import a.b.C;` is red
  when `C` is absent from the index of package `a.b`; a wildcard `import a.b.*;`
  is red when package `a.b` has no indexed type; a `static` import is red when
  its type or named member is absent. All are suppressed when the trust gate
  (D3) is off. Reason: the user asked for unresolved imports to be red, including
  the wildcard and static forms.

- **D12 — Diagnostics stay off the request path.** The check grows heavier than
  today's, and it runs per edit; it must not delay text sync or request handling
  (R6). The implementation measures with `java-lsp-bench` and, if per-edit cost
  regresses first-response after `didOpen`, moves computation to a spawned task
  that publishes only for the current document version. Reason: R6 is a standing
  constraint and the dispatcher computes diagnostics inline today.

- **D13 — Message wording is part of the contract.** e.g.
  "cannot resolve type `X`", "cannot resolve member `m` on `T`",
  "cannot resolve symbol `x`", "cannot resolve import `a.b.C`", all with
  `source: "java-lsp"` and the codes from D7. Reason: tests and users both key
  on the text, so it is fixed here.

# Acceptance criteria

- In a clean-parsing file in a JDK-indexed workspace, `List<String> xs;` with no
  import and `java.util.List` indexed yields one `ERROR` on `List` whose message
  names the symbol; `textDocument/codeAction` over it returns "Add import
  `java.util.List`"; applying it inserts `import java.util.List;\n` at the right
  position (the `import_edit` rules) and the diagnostic clears on the next
  `didChange`.
- With both `java.util.List` and `java.awt.List` indexed and no import, the code
  action returns one add-import action **per candidate**.
- `Widget w;` with nothing matching in the index yields an `ERROR` with
  `code: unresolved-type` and a "Create class `Widget`" action; applying it
  (client supporting `CreateFile`) creates the file in the source-root package
  path with the `package` declaration and an empty class body; the action is
  absent when the client does not advertise `CreateFile`.
- `list.sixe()` where `list` is `java.util.List` yields an `ERROR` on `sixe` with
  `code: unresolved-member` and a "Change to `size`" action; applying it renames
  the member and clears the diagnostic.
- `import com.example.Nope;` with `Nope` absent yields an `ERROR` on the import
  declaration with `code: unresolved-import`; when the same name is also used in
  the file, those usages are **not** flagged again (D5).
- `import a.b.*;` with no indexed type in `a.b`, and
  `import static java.util.Collections.sort;` with the member absent, each yield
  an `ERROR` on the import declaration.
- Gating: a file containing a parse error yields only syntax diagnostics; a
  workspace with no JDK indexed yields no semantic diagnostics;
  `JAVA_LSP_SEMANTIC_DIAGNOSTICS=0` yields no semantic diagnostics.
- Fixing a symbol, a member, or an import clears its diagnostic on the next
  change (diagnostics replace, never accumulate).
- `java-lsp-bench` shows no regression in per-feature first-response after
  `didOpen` or in hover RTT during warm-up (R6).
- Tests: unit tests in `src/analysis.rs` for each family and each fix; harness
  tests in `tests/harness.rs` driving `publishDiagnostics` and
  `textDocument/codeAction` end to end (draining the client socket); the existing
  diagnostic tests updated for `ERROR` severity and the new known-but-not-imported
  behavior.
- `initialize` advertises `codeActionProvider` with kind `quickfix`.

# Docs to update

- `docs/architecture.md` — rewrite the "Semantic diagnostics" clause (it currently
  promises a narrow `WARNING` check); add the code-action surface and the
  `CreateFile` capability check to the shell description; add
  `JAVA_LSP_SEMANTIC_DIAGNOSTICS` where configuration is described.
- `docs/requirements.md` — widen UC4 from syntax diagnostics to unresolved-symbol
  diagnostics with quick fixes, adjust R3's wording, add a requirement (R11) for
  the semantic diagnostics and their fixes, and add the milestone.
- `README.md` — the "Type-aware diagnostics" feature bullet, and a
  `JAVA_LSP_SEMANTIC_DIAGNOSTICS` row in the Configuration table.
- `docs/dev/backlog/index.md` — this request's row.
- `docs/dev/changelog.md` — an entry at implementation.

# Implementation plan

## Approach

All analysis changes live in `src/analysis.rs` plus a few helpers in
`src/types.rs` and `src/index.rs`; the code-action surface crosses
`src/analysis.rs` → `src/engine.rs` → `src/server.rs`. No new dependency.

**Resolution classification (the core).** The diagnostics need to answer, for a
symbol reference, one of: *visible* (resolves), *missing import* (the index has
candidates but none is visible here), or *unknown*. `scope_at` already returns
the visible imports, package, locals, fields, and type parameters; the index
supplies candidates. A single helper classifies a name:

- *visible* when a local/field/type-parameter matches, when an exact single-type
  import's simple name matches, when the file's own package declares it, when a
  wildcard import's package declares it, or when it is a `java.lang` type;
- *missing import* when it is not visible but `index.query_name(name)` yields
  importable candidates (types for a type position; methods/fields for a member);
- *unknown* when neither holds.

`import_edit` already encodes the visible/conflict half of this (it returns an
empty vec when the name resolves or would conflict) and is reused verbatim for
the fix, so the diagnostic and the offered edit can never disagree.

**The four families, one tree walk.** `semantic_diagnostics` walks the tree once
(as `visit_type_positions` does today) and dispatches on node kind, guarding
against declaration names (a declaration is not a reference) exactly as
`resolve_target` does:

- `type_identifier` in a type position (the existing `checked_type_nodes` set,
  widened) → type check;
- a `method_invocation`/`field_access` name whose receiver type resolves (not
  `Unknown`) → member check via `member_owner`/`member_for_arguments`;
- a bare `identifier` in an expression position that resolves to no local, field,
  type, or import → symbol check, choosing the candidate kind from its context
  (type position, call name, or value);
- an `import_declaration` → import check against the index (single-type by
  package+name, wildcard by package existence, static by type and member).

Every firing diagnostic carries `code` and `data`; `data` holds the fix kind,
the symbol name, and the candidate fully-qualified names, so the fix handler
never re-derives the analysis.

**Code actions.** `textDocument/codeAction` becomes a new engine query: a
`Command::CodeActions` variant and an `EngineHandle::code_actions`, dispatched
through the existing `read` (spawned) path. The handler builds `CodeAction`s from
the diagnostics' `data`: "Add import `fqcn`" one per candidate (edits from
`import_edit`), "Change to `m`" for a near member (a `TextEdit` over the range),
and "Create class/interface `X`" / "Create method `m`" stubs. Create-file uses
`ResourceOp::Create` plus a `TextDocumentEdit`, positioned under the source root
that contains the file (`WorkspaceIndex::source_roots`) in the file's package;
the shell forwards the client's `workspace.workspaceEdit.resourceOperations`
capability to the engine so those actions are withheld when `CreateFile` is not
advertised.

**Performance (D12).** The pass grows heavier and runs per edit; `java-lsp-bench`
is the check. If per-edit cost regresses first-response after `didOpen`, move the
computation to a spawned task that publishes only for the current document
version.

## Steps

- [x] `src/index.rs`: add a cheap package query (`has_package`), maintained
      alongside the name index; tested via the import checks below.
- [x] `src/analysis.rs`: add the `semantic_diagnostics` flag read from
      `JAVA_LSP_SEMANTIC_DIAGNOSTICS` in `TreeSitterEngine::new` and honoured by
      `diagnostics`; plus a setter and a pure mapping test (avoiding process-wide
      env mutation).
- [x] `src/analysis.rs`: rewrite the type check to `ERROR` severity and the
      visibility classification (unknown vs missing-import), emitting `code` and
      `data` (AC: `List` red + add-import; ambiguous → one action per candidate).
      Update the existing diagnostic tests for the new severity and behaviour.
- [x] `src/analysis.rs`: add `check_member` for `field_access`/`method_invocation`
      names on a known receiver (AC: `list.sixe()` red + did-you-mean).
- [x] `src/analysis.rs`: add `check_identifier` for bare identifiers that resolve
      nowhere, classifying the candidate kind from context (AC: unknown symbol
      red + create-stub).
- [x] `src/analysis.rs`: add `check_import` for single-type, wildcard, and static
      imports (AC: `import com.example.Nope;` red; wildcard/static red on
      absence); usages bound by an import are not reported twice (D5).
- [x] `src/analysis.rs`: wire the new families into a single `SemanticCheck` walk
      with declaration-name guards, and add unit tests per family and fix.
- [x] `src/analysis.rs`: add `TreeSitterEngine::code_actions` building the
      add-import, rename, and create-stub `CodeAction`s from diagnostic `data`,
      with the create-file location from `source_roots` (AC: applying the fix
      clears the diagnostic).
- [x] `src/engine.rs`: add `Command::CodeActions`, `EngineHandle::code_actions`,
      the `read` dispatch arm, and `Command::SetClientCapabilities` carrying
      `resource_operations`.
- [x] `src/server.rs`: advertise `codeActionProvider` (kind `quickfix`), read
      `workspace.workspaceEdit.resourceOperations` in `initialize`, forward it to
      the engine, and implement `textDocument/codeAction` (AC: capability
      advertised; create-file withheld without support).
- [x] `tests/harness.rs`: an end-to-end test driving `publishDiagnostics` and
      `textDocument/codeAction` (the add-import fix), plus a capability
      assertion; the create-file withheld case and the `JAVA_LSP_SEMANTIC_DIAGNOSTICS`
      mapping are covered by unit tests.
- [x] Ran `java-lsp-bench` (200 files): first-response 0.8–1.0 ms and warm-up
      hover RTT max 1.1 ms, so the added per-edit work shows no regression and
      diagnostics stay inline (D12 holds).
- [x] `docs/architecture.md`: rewrote the "Semantic diagnostics" clause (ERROR,
      four families, the trust gate), documented the code-action surface and the
      `CreateFile` capability check, and added `JAVA_LSP_SEMANTIC_DIAGNOSTICS`.
- [x] `docs/requirements.md`: widened UC4 (renamed "See diagnostics"), added R11,
      and added the v0.6 milestone.
- [x] `README.md`: updated the "Diagnostics" bullet and added a
      `JAVA_LSP_SEMANTIC_DIAGNOSTICS` row to the Configuration table.
- [x] `docs/dev/changelog.md`: added the entry with the verified test count.
