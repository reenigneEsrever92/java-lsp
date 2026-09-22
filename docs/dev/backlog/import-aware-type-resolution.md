---
type: ChangeRequest
kind: bug
title: Import-aware type-name resolution
description: Resolve a simple type name through the file's imports so an imported name shared across packages (e.g. List) reaches the right type.
state: done
priority: high
tags: [dev, types, engine]
owner: felix
verified:
  by: cargo test (136 passed - 111 lib, 6 bench bin, 18 harness, 1 stdio)
  at: 2026-09-22T19:37:31Z
---

# Problem

Reported against inlay hints: `var x = List.of(3);` produced no hint. The root
cause is broader than hints.

`resolve_name` (`src/types.rs`) resolved a simple name through locals, fields,
the enclosing type's members, type parameters, and finally
`TypeModel::find_unique(simple_name, context_package)`. `find_unique` returns
`None` when the name is ambiguous and no candidate sits in the context package:

    let mut preferred = candidates.iter().filter(|info| info.package.as_deref() == package);
    let first = preferred.next()?;

A real JDK indexes both `java.util.List` and `java.awt.List`, so in any file whose
package is neither, `List` resolved to nothing. Hover, `.`-completions, and inlay
hints therefore failed for the many names shared across packages (`List`, `Date`,
`Timer`, `Point`, …) — `scope.imports` was never consulted, even though the
semantic-diagnostics path already special-cases imports. A second, related gap:
once a name *did* resolve, a qualified reference such as
`Ty::reference("java.util.List")` lost its package, because `lookup`/`members`
matched on the simple name against the *caller's* package only.

# Proposal

Resolve a simple type name through the compilation unit's imports, and honour a
qualified reference's own package during member lookup.

# Decisions

- **Java's resolution order, conservative at the end.** A simple name resolves
  to: an exact single-type import; then the file's own package; then a wildcard
  import's package; then a unique model match anywhere. Two wildcard imports that
  could both supply the name leave it unresolved — no result beats a wrong one.
- **Resolved type references carry their package.** `resolve_name` now returns a
  package-qualified `Ty` (e.g. `java.util.List`) so a later member lookup can
  disambiguate; `Ty::display` still renders the simple name, so nothing visible
  changes for unambiguous code.
- **A receiver's own name is qualified too.** `receiver_type` resolves a local's
  or field's declared type name through the same rules, so `List<String> x;
  x.size()` resolves even though the declaration is written with the simple name.
- **No behaviour change when a name is already unambiguous.** The context-package
  and unique-match fallbacks are preserved, so same-package and `java.lang` names
  resolve exactly as before.

# Acceptance criteria

- `var x = List.of(3);` with `import java.util.List;` produces a type hint, and
  `List` resolves to `java.util.List` even when the model also holds
  `java.awt.List`.
- A wildcard import (`import java.util.*;`) resolves the name; two wildcard
  imports that both could supply it leave it unresolved.
- A qualified reference (`Ty::reference("java.util.List")`) finds its members
  without relying on the caller's package.
- An ambiguous name with no import is still left unresolved (negative test).
- Verified by unit tests in `src/types.rs` and `tests/harness.rs`: one test over a
  workspace with two `List` packages, and one over a fake `src.zip` JDK whose
  `java.util.List` declares several `of` overloads (`var list = List.of(5);`
  yields `: List<E>`).

# Implementation plan

## Approach

All changes are in `src/types.rs`; no new dependency and no shell change.

- **`Import::package()`** — the package an import brings names from (a wildcard's
  path minus `.*`, otherwise everything before the last segment).
- **`resolve_type_info`** — one helper implementing the resolution order, used by
  both `resolve_name` and the receiver qualification.
- **`Ty::qualified_package`** and honouring it in `TypeLookup::lookup`,
  `TypeLookup::members`, and `member_owner`, so a qualified reference is resolved
  in its own package.
- **`qualify`** wrapping `receiver_type`, so a receiver whose declared type name
  is a simple imported name is resolved before its members are looked up.

## Steps

- [x] Add `Import::package()` and the `qualified_package` helper.
- [x] Make `lookup`/`members`/`member_owner` honour a reference's own package.
- [x] Rewrite `resolve_name`'s model lookup to go through imports and return a
      package-qualified reference.
- [x] Qualify `receiver_type` results so a local's imported declared type resolves.
- [x] Unit tests in `src/types.rs` (exact import, wildcard import, unresolved
      ambiguity, qualified member lookup, imported local receiver).
- [x] Harness test driving `textDocument/inlayHint` over two `List` packages.
- [x] Update `docs/architecture.md`'s resolution description and the changelog.
