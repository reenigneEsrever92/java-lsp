---
type: ChangeRequest
kind: bug
title: A type name resolves against what the cursor's file can see
description: Find references (and go-to-definition from a use) return nothing for a class/interface/enum/record whose simple name is also declared in another file, because the type candidate set is not narrowed to what the cursor's file can see before the cross-file ambiguity policy runs.
state: done
verified: { by: felix, at: 2026-10-13T08:16:15Z }
priority: medium
tags: [dev, bug, navigation]
owner: felix
---

# Problem

`find_references` and `find_declaration` resolve the word under the cursor to
the entries the index declares with that name, narrowed by node kind, and then
apply the cross-file ambiguity policy (`unique_candidates`): a name declared in
more than one file is no answer at all. For a **member** that narrowing is
sound — the receiver's declared type pins the declaring type. For a **type** it
is not: `declaration_candidates` (`src/navigate.rs`) keeps every entry of that
name whose kind `is_type()` and applies no package or import narrowing, so a
class/interface/enum/record whose simple name is also declared in another
package (`a.Widget` beside `b.Widget`) makes the whole group ambiguous before
`Target::from` or `Target::visible_in` can pin the target. The search then
returns nothing — even for a file that explicitly `import a.Widget`d it.

This is a regression from the engine-core navigation (`analysis.rs`), whose
`type_target_for` narrowed the type entries to the package the cursor's scope
preferred (the file's own package, or an explicit import) before requiring
uniqueness. The `navigate` extraction kept `Target::visible_in` for the
occurrence files but dropped the candidate-side narrowing, and the ported tests
did not carry the same-named-type case; `docs/architecture.md` still promises
references are "identified by simple name _and_ package so a same-named type
elsewhere is never touched".

The impact is broad: any workspace with two same-named types — `Config`,
`User`, `Builder`, `Main` — sends find-references, and go-to-definition from a
use, silent for both.

# Reproduction

With `a/Widget.java` (`package a`) and `b/Widget.java` (`package b`) both
indexed, and a cursor on the `Widget` of `Widget w;` in `a/Use.java`
(`package a`), `find_references` with `includeDeclaration` returned an empty
list; expected the `a/Use` use and the `a/Widget` declaration, never
`b/Widget`. The same empty result held for a file that imports `a.Widget`.

Regression tests (green after the fix) in `src/navigate.rs`:
`references_ignore_a_same_named_type_in_another_package`,
`declaration_resolves_a_type_use_with_a_same_named_type_elsewhere`, and
`a_type_reference_follows_an_import_when_the_simple_name_is_shared`.

# Proposal

Narrow a **type** name to what the cursor's file can see before the cross-file
ambiguity policy runs: the file's own type, a type in the file's package, or an
imported type (exact, or a package wildcard). What remains must still share one
file. When nothing is visible the candidate set is left whole, so a reference
the predicate does not recognise — a nested type, a qualified name — is never
dropped. The narrowing lives in `declaration_candidates`, so `find_references`
and `find_declaration` gain it from one place.

# Decisions

- **D1 — Narrow type candidates by the cursor file's visibility.** Keep only the
  entries the cursor's file can name (its own, its package's, an exact import, a
  wildcard import) before `unique_candidates`. Chosen because Java scoping pins a
  simple type name, so the group is not genuinely ambiguous; this restores the
  engine-core behaviour and matches the architecture's "by simple name and
  package" promise.
- **D2 — Soft narrowing with a fallback.** If the visible set is empty, keep the
  original candidates, mirroring the engine-core
  `if strict || !in_package.is_empty()`. Chosen so a nested type or a qualified
  reference — which the predicate does not model — is never dropped: the
  previously-working single-candidate case is unchanged.
- **D3 — One narrowing for both requests.** Apply it in `declaration_candidates`,
  shared by `find_references` and `find_declaration`. A type use the file can see
  exactly once is a single candidate, consistent with the "a single candidate is
  an answer" rule; `find_declaration`'s own declaration-cursor resolution
  ([a cursor on a declaration resolves to that declaration](declaration-cursor-self-resolution.md))
  is unaffected, since its pre-check runs before this path.
- **D4 — No textual fully-qualified matching.** Do not add the engine-core
  `text.contains("{package}.{name}")` fallback: it matches comments, strings, and
  longer identifiers, and a false positive can attribute another type's
  occurrences. A cross-file qualified use with no import stays unresolved, as it
  is today — a documented limitation, not a regression.
- **D5 — Keep the three reproduction tests** in `src/navigate.rs` as the
  regression suite.

# Acceptance criteria

1. With `a/W` and `b/W` indexed, references on a use of `W` in a `package a`
   file return the `a` use and (with `includeDeclaration`) the `a` declaration,
   never `b`.
2. An explicit `import a.W` pins the target even with `b/W` present; the import
   line itself counts as a reference.
3. `declaration_of_an_ambiguous_name_is_none` still passes: a default-package
   file that imports neither `a.W` nor `b.W` yields no location for a use.
4. `cargo build`, `cargo fmt --check`, and `cargo test` are clean.

# Docs

- `docs/architecture.md` — the navigation-results bullet's single-candidate rule
  gains the type-visibility narrowing: a type name is first narrowed to what the
  cursor's file can see, so a same-named type elsewhere is never mistaken for
  ambiguity. The References bullet's list of ways a file can see a type drops
  "a fully-qualified use", which `Target::visible_in` never implemented (D4 keeps
  it unsupported).
- `docs/dev/changelog.md` — the shipped entry.

# Implementation plan

## Approach

The defect is a narrowing applied to a member's node but not to a type's.
`declaration_candidates` (`src/navigate.rs`) looks the cursor's word up
(`EntriesByName`), keeps the entries the node-kind constraint admits
(`NameConstraint::Type` → `kind.is_type()`), and, for a member, narrows further
through the receiver's declared type. A type has no such closure, so the full
name group reaches `unique_candidates` and is refused when it spans files.

The fix adds the missing narrowing for the type constraint: a predicate
`type_visible_in_file(entry, uri, text, tree)` is true when the entry is declared
in the cursor's file, in its package, or imported into it (exact, or a wildcard)
— the import path built container-aware by `qualified_type_name`
(`a.Outer.Inner`). `declaration_candidates` keeps the visible candidates, or the
whole set when none is visible (D2), before the member branch. Both callers
(`find_references`, `find_declaration`) gain it, so the cursor file's `uri` is
threaded into the function's signature. `Target::visible_in` already applies the
same visibility to the files searched for occurrences, so the resolved target
and the occurrence search agree.

## Steps

- [x] Add `type_visible_in_file` and `qualified_type_name` to `src/navigate.rs`
  (the cursor file's own type, its package, an exact import, or a wildcard
  import; container-aware dotted name).
- [x] Thread the cursor file's `uri` into `declaration_candidates` and, for
  `NameConstraint::Type`, keep the visible candidates — falling back to the whole
  set when none is visible (D1, D2, D3, D4).
- [x] Add the three regression tests (D5) and confirm
  `declaration_of_an_ambiguous_name_is_none` stays green (D3, D4).
- [x] Update `docs/architecture.md`: the navigation-results bullet's
  single-candidate rule notes the type-visibility narrowing, and the References
  bullet drops the never-implemented "fully-qualified use" visibility (D4).
- [x] Add the `docs/dev/changelog.md` entry for the fix.
- [x] Verify: `cargo fmt --all -- --check`, `cargo build --all-targets`, and
  `cargo test` clean (53 library, 7 bench tests).

## Progress

Implemented in one pass in `src/navigate.rs`: `type_visible_in_file` and
`qualified_type_name`, the `NameConstraint::Type` narrowing in
`declaration_candidates` (with the cursor file's `uri` threaded in), and three
regression tests. `docs/architecture.md` (the type-visibility narrowing, and the
References bullet's never-implemented "fully-qualified use" dropped) and
`docs/dev/changelog.md` updated.
Verified: `cargo fmt --all -- --check` clean; `cargo test` green — 53 library
tests and 7 bench tests, including the three new ones and
`declaration_of_an_ambiguous_name_is_none`. Not committed.
