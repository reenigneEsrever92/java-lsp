---
type: ChangeRequest
kind: feature
title: Create-symbol quick fixes for unresolved symbols
description: Offer create-stub quick fixes for symbols absent from the index — a class/interface/enum/record, a method or field (signature inferred from the usage), or a local variable — including on a workspace receiver's type.
state: done
priority: medium
tags: [dev, diagnostics, lsp]
owner: felix
verified:
  by: cargo test --all-targets (229 passed - 198 lib, 6 bench bin, 24 harness, 1 stdio)
  at: 2026-09-24T20:08:50Z
---

# Problem

`unresolved-symbol-diagnostics` shipped create-stub fixes, but they are
minimal and incomplete:

- An unknown **type** offers only **"Create class `X`"** — the diagnostic
  hardcodes `"kind": "class"` (`src/analysis.rs`, `check_type`), and neither
  `interface`, `enum`, nor `record` is ever offered, even though
  `stub_type_source` already knows the `interface` keyword.
- An unknown **bare identifier** offers `public void name() {}` for a call and
  `private Object name;` for a value — the stub ignores the call's arguments
  and the expected type, so the created method never matches its usages.
- An unresolved **member** on a known receiver (`widget.newMethod()`) offers only
  a did-you-mean rename — there is no way to create the missing method.
- There is no way to create a **local variable**.

So for the very common "I typed `widget.getName()` and `getName` doesn't exist
yet" or "I used `Widget` before writing it", the only useful action is missing.

# Proposal

Turn the create fixes into a full set of create-symbol actions:

- **Type** (unknown type): four actions — **Create class / interface / enum /
  record `X`** — each writing a stub file under the source root of the file's
  own package (gated on the client supporting `CreateFile`).
- **Member** (unresolved member on a receiver, or an unknown identifier used as
  a call): **Create method `m` in `T`**, with the signature inferred from the
  usage — parameter types from the call's argument types, the return type from
  the context (assigned-to type, `return`, argument slot).
- **Field / local**: for an unknown identifier used as a value, **Create local
  variable `v`** (preferred) and **Create field `v`**, with the type inferred
  from an initializer or assignment when one is present.

Library receivers (jar/JDK declarations) offer nothing — their source cannot be
edited. Creating a member on a workspace type, or a type file, may touch a file
that is not open; those edits are unversioned (matching the rename caveat).

# Decisions

- **D1 — Separate actions per type kind.** A single unknown-type diagnostic
  carries four actions (class, interface, enum, record) and the client shows a
  picker, mirroring the per-candidate add-import choice. Reason: the language
  gives no signal for which kind is intended, and a class is not always right.

- **D2 — Signatures are inferred from the usage, with placeholders as fallback.**
  A created method's parameter types come from the call's argument types
  (`receiver_type` on each argument, already available) and its return type from
  the context — the declared type of the variable an initializer assigns to, the
  enclosing method's return type for a `return`, or an argument's expected type.
  An unresolvable type falls back to `Object` and an unresolvable return type to
  `void`. Reason: a stub that matches its call sites is the difference between a
  useful action and a chore; the type layer already infers the pieces.

- **D3 — Parameter names come from the arguments, else positional.** When an
  argument is a bare identifier its name is reused; otherwise the parameter is
  `arg1`, `arg2`, … Reason: named parameters read better and this is what IDEs
  do, with a deterministic fallback.

- **D4 — A value use offers local variable and field, local preferred.** An
  unknown identifier in a value position offers **Create local variable** (marked
  preferred) when it sits inside a method/constructor body, and **Create field**
  always; outside a method body only the field action. The local declaration is
  inserted at the start of the innermost enclosing block; the field/method into
  the enclosing type. Reason: the user asked for both, with the local preferred;
  the block start is the only position guaranteed to precede every use in the
  block.

- **D5 — Create-on-receiver writes into a workspace type's file.** For an
  unresolved member whose receiver's type is a workspace source, the "Create
  method/field in `T`" action reads `T`'s file from disk (as `rename` already
  does), locates its type body, and inserts the stub before the closing brace as
  a plain `changes` edit — unversioned, because the shell tracks versions only
  for open documents. Reason: the receiver's file is often closed, and the
  existing rename path already accepts on-disk, unversioned edits.

- **D6 — Library declarations are excluded.** A receiver whose type is a jar or
  JDK declaration (`dependency`, including `library_source`) offers no create
  action, and no create-type action targets a library package. Reason: those
  files are not the user's to edit.

- **D7 — The create-type `CreateFile` gate stays.** The four type actions are
  withheld unless the client advertises `workspace.workspaceEdit.resourceOperations`
  with `CreateFile`. Reason: a new type is a new file, and LSP has no other way
  to create one. (If the action is absent in a given editor, this capability is
  why; there is no fallback.)

- **D8 — The diagnostics carry the symbol; the handler derives the rest.** A
  create diagnostic's `data` keeps the fix marker, the name, and (for a value
  use) `local`/`field`; the code-action handler re-reads the node and its
  context at the diagnostic range to infer the signature, so the diagnostics
  pass stays cheap and the fix always matches the current buffer. Reason: the
  inference needs the tree and scope, which the handler already has.

- **D9 — Enum and record stubs are minimal but valid.** `public enum X {}` and
  `public record X() {}`; no extra members. Reason: enough to compile and be
  filled in; inventing components or constants would be guessing.

- **D10 — Constructors and enum constants are out of scope.** Only the types,
  method, field, and local variable above are created. Reason: constructors need
  a target type with fields to initialise, and enum constants need an enum
  context; both are speculative here.

# Acceptance criteria

- `Widget w;` with nothing matching in the index offers **four** actions — Create
  class / interface / enum / record `Widget` — each producing a file whose
  content is `package <pkg>;` plus `public class|interface|enum|record Widget
  {…}`; all four are absent when the client does not advertise `CreateFile`.
- `foo(a, b)` where `foo` resolves nowhere offers "Create method `foo`" producing
  `public void foo(<type of a> a, <type of b> b) {}` (parameters named from bare
  identifiers, `arg1`/`arg2` otherwise; unknown types `Object`), inserted into
  the enclosing type.
- `int n = bar();` where `bar` resolves nowhere offers a method whose return type
  is `int` (from the assignment context), not `void`.
- `Object x = value;` where `value` resolves nowhere offers "Create local
  variable `value`" (preferred) inserting `Object value;` at the start of the
  enclosing block, and "Create field `value`" inserting into the enclosing type.
- `widget.newMethod(5)` where `widget`'s type is a workspace source offers
  "Create method `newMethod` in `Widget`" inserting `public void newMethod(int
  arg1) {}` into `Widget`'s file (an unversioned `changes` edit); when
  `widget`'s type is a jar/JDK type, no create action is offered.
- Every created stub is syntactically valid Java and, once applied, clears the
  diagnostic on the next change.
- Tests: unit tests in `src/analysis.rs` for each action and the signature
  inference (argument types, return type, local-vs-field, workspace-vs-library
  receiver); a harness test driving `textDocument/codeAction` for one
  create-on-receiver case and the four type actions.
- `initialize` continues to advertise `codeActionProvider` (kind `quickfix`);
  the create-type actions remain gated on `CreateFile`.

# Docs to update

- `docs/architecture.md` — extend the code-action paragraph: the create fixes
  and their signature inference, the workspace-only receiver rule, and the
  unversioned edit into another file.
- `docs/requirements.md` — widen R11 to name the create actions and their
  inferred signatures.
- `README.md` — the Diagnostics bullet's quick-fix list.
- `docs/dev/backlog/index.md` — this request's row.
- `docs/dev/changelog.md` — an entry at implementation.

# Implementation plan

## Approach

All changes are in `src/analysis.rs`. The diagnostic `data` grows a `fixes`
array (`{ "name": …, "fixes": [ { "fix": …, … }, … ] }`) so one diagnostic can
carry several actions — a member can offer both a did-you-mean rename and a
create-on-receiver. The code-action handler iterates the fixes and dispatches:

- **`create-type`** → four actions (class / interface / enum / record) writing a
  stub file under the file's own package, gated on `CreateFile`.
- **`create-symbol`** → for an unqualified call, "Create method" with parameters
  inferred from the call's arguments (`receiver_type` per argument; names from
  bare identifiers, else `argN`) and the return type from the context (the
  assignment/declaration type, the enclosing method's type for a `return`, else
  `void` for a discarded call and `Object` otherwise); for a value, "Create
  local variable" (preferred, inserted at the enclosing block's start) and
  "Create field" (into the enclosing type), their type from the
  initializer/assignment, else `Object`.
- **`create-receiver-member`** → "Create method/field in `T`" for a workspace
  receiver type: the owner's file is read (the open buffer when it is the
  current document, else disk), its type body located, and the stub inserted
  before the closing brace as an unversioned `changes` edit. Library types
  never carry this fix.

New helpers: `node_at` (the node under a diagnostic range), `enclosing_block` /
`enclosing_method`, `type_body_by_name`, and the signature-inference functions
(`parameter_list`, `return_type`, `value_type`, `display_type`). A fresh
`java_parser()` parses the owner file so the shared parser lock is never taken
while the documents lock is held, avoiding a lock-order deadlock with
`store_tree`.

## Steps

- [x] `src/analysis.rs`: change the diagnostic `data` to `{ name, fixes }` and
      update `check_type`, `check_identifier`, and `check_member` to emit the
      right fixes (create-type; create-symbol; rename + create-receiver-member),
      adding `workspace_owner` for the receiver case.
- [x] `src/analysis.rs`: rewrite `code_actions` to iterate `data.fixes`, keeping
      add-import and rename, expanding create-type to four actions, and adding
      create-symbol and create-receiver-member.
- [x] `src/analysis.rs`: add the signature-inference and location helpers
      (`parameter_list`, `return_type`, `value_type`, `display_type`,
      `enclosing_block`, `enclosing_method`, `node_at`, `type_body_by_name`).
- [x] `src/analysis.rs`: unit tests for the four type actions, method parameter
      and return-type inference, local-vs-field, and create-on-receiver; updated
      the tests that inspect the old `data` shape and the create-action count.
      Also fixed `collect_declared_names`, which was treating a call's callee
      (a `method_invocation`'s `name` field) as a binding and so suppressed the
      create-symbol diagnostic for any unqualified call.
- [x] `tests/harness.rs`: `textDocument/codeAction` coverage for the four create
      type actions (client advertising `CreateFile`).
- [x] `docs/architecture.md`: extended the code-action paragraph with the create
      fixes, signature inference, the workspace-only receiver rule, and the
      unversioned edit into another file.
- [x] `docs/requirements.md`: widened R11 to name the create actions.
- [x] `README.md`: the Diagnostics bullet's quick-fix list.
- [x] `docs/dev/changelog.md`: the entry with the verified test count.
