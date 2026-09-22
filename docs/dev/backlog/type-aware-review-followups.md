---
type: ChangeRequest
kind: bug
title: Type-aware review follow-ups
description: Fix the defects, test gaps, and hygiene issues found reviewing the uncommitted type-aware work (references/rename, diagnostics, hints, class-file model).
state: done
priority: high
tags: [dev, review, types, navigation]
owner: felix
verified:
  by: cargo test --all-targets (157 passed - 132 lib, 6 bench bin, 18 harness,
    1 stdio) + java-lsp-bench --files 5 with the machine JDK indexed (Temurin 25,
    warm-up 6.2 s, peak RSS 197 MB, warm-up hover RTT <= 1.1 ms, post-warm-up
    hover RTT 1.2 ms)
  at: 2026-09-22T20:19:48Z
---

# Problem

This reviews the uncommitted work that ships seven change requests —
`type-aware-engine`, `jvm-member-descriptors`, `references-and-rename`,
`type-hints`, `import-aware-type-resolution`, `same-name-completions`, and
`generic-type-argument-inference` — all currently flipped to `state: done` in
the working tree. `cargo test --all-targets` passes (141 tests: 116 lib, 6 bench
bin, 18 harness, 1 stdio), but the review found defects the tests do not catch,
plus test, documentation, and fixture-hygiene problems.

Each finding below is in scope; nothing is deferred.

1. **Debug leftover.** `tests/harness.rs:1100` prints
   `println!("SRCZIP HINT LABELS: {labels:?}")` on every run. Hygiene.
2. **References/rename conflate same-named members across packages.**
   `member_access_matches` (`src/engine/syntax.rs:1418`) accepts an occurrence
   when `types::member_owner(...)` equals `target.owner`, but `member_owner`
   returns only the *simple* name (`src/types.rs:1527`) and `member_target`
   stores that same simple name (`src/engine/syntax.rs:400`); `target.package`
   is never consulted. With two unrelated `Widget` classes in different
   packages that each declare `run`, renaming `a.Widget.run` also rewrites
   `b.Widget.run` — a wrong, code-breaking edit, against the request's
   "same *declaring type*" rule and its AC3. `declared_in_workspace(&owner, …)`
   (`src/engine/syntax.rs:375`) has the same simple-name weakness.
3. **Duplicate semantic diagnostics.** `checked_type_nodes` expands a `type_list`
   under `super_interfaces`/`extends_interfaces` into its leaf types
   (`src/engine/syntax.rs:2006`), and `visit_type_positions` then recurses into
   the same `type_list` child and expands it again
   (`src/engine/syntax.rs:1987`). `class A implements Missing, Also {}` reports
   each unresolved interface twice.
4. **`include_declaration = false` still returns the declaration.** Members are
   collected by an identifier match inside the declaring type's span
   (`src/engine/syntax.rs:1386`) and locals by their own declarator name
   (`src/engine/syntax.rs:1436`), so the declaration is always among the
   occurrences regardless of the flag, contradicting the trait contract
   (`src/engine/mod.rs:63`).
5. **Cursor on a non-terminal `import` segment resolves to the type.**
   `resolve_target` (`src/engine/syntax.rs:209`) walks to any `import_declaration`
   ancestor and unconditionally takes the last dotted segment, so references or
   rename invoked on `a` or `b` in `import a.b.Widget;` silently targets
   `Widget`.
6. **Nested library types neither resolve nor render.** `class_type_info` keys a
   nested class by its innermost `$` segment (`src/classfile.rs:65`, e.g.
   `demo/Outer$Inner` → name `Inner`), while `parse_descriptor` keeps
   `Outer$Inner` in the reference name (`src/types.rs:222`); `simple_name`
   and `display` split only on `.` (`src/types.rs:105`, `src/types.rs:154`), so
   a descriptor-typed `java.util.Map$Entry` renders `Map$Entry` and its members
   never resolve. The same keying also lets two nested types sharing an
   innermost name in one package overwrite each other in the model
   (`src/types.rs:404`).
7. **`rename` silently drops occurrences it cannot read.**
   `collect_occurrences` `continue`s on a read error, a parse failure, or a
   poisoned parser lock (`src/engine/syntax.rs:426`, `:433`, `:442`), so a
   `WorkspaceEdit` that omits a genuine reference in an unreadable file is
   returned as if it were complete.
8. **Type visibility misses fully-qualified uses.** `type_visible_in`
   (`src/engine/syntax.rs:1287`) admits only the declaring file, its package, or
   files that import the type; a file that writes `a.Widget x;` with no import
   is skipped, so references miss it and rename leaves it stale.
9. **Inherited members lost for supertypes with a shared simple name.**
   `members`/`member_owner` walk supertypes by `simple_name()` plus the
   *subtype's* package (`src/types.rs:551`, `src/types.rs:1531`), discarding the
   supertype's own qualifier. With a real JDK indexing both `java.util.List` and
   `java.awt.List`, a subtype outside `java.util` inherits nothing from `List`;
   a same-named type in the context package would instead be inherited wrongly.
10. **Near-vacuous hint assertion.** `tests/harness.rs:1101` only checks that
    some label `starts_with(": ")`, so it cannot catch a wrongly resolved type
    and does not verify the changelog's specific `: List<Integer>` claim. The
    test named `inlay_hints_render_types_parameters_and_chains`
    (`tests/harness.rs:1292`) still passes with the chain-hint family removed,
    because `: Widget` also comes from the `var` variable hint.
11. **Coverage gaps.** No test exercises: the member cross-package conflation
    (2), `include_declaration = false` (4), duplicate diagnostics (3), the
    import-segment cursor (5), the library-declaration rename refusal that the
    changelog claims (`docs/dev/changelog.md`, references bullet) and the
    `declared_in_workspace` guard, the chain-hint family end to end,
    `jdk.rs`'s new `TypeInfo` output (all three `jdk.rs` tests assert only the
    first tuple element), and the warm-up `index.type_model()` wiring
    (`src/index.rs:406`+, untested by `scan_workspace`'s test).
12. **Example fixture hygiene.** `example/greeting-app/src/main/java/com/example/app/Main.java`
    holds scratch variables (`var test = "dwad";`, `Data bla = new Data(5);`
    with `Data` never imported), unused imports (`java.util.Spliterator`,
    `java.awt.*`), and an invalid `var array = {3};` (an array initializer is
    not allowed with `var`). `example/greeting-lib/.../Data.java` was cut to a
    single component, and the tracked `example/*/target/*.jar` and
    `surefire-reports/*` were regenerated, adding binary and build-log noise —
    these artifacts are tracked despite `**/target/` in `.gitignore`.
13. **`parameter_hints` can mislabel an argument.**
    `member_for_call` falls back to the name-only `member_of` when no overload's
    arity matches (`src/types.rs:1375`), so a call whose argument count matches
    no overload still gets the first parameter's name.
14. **Inlay hints can fall outside the requested range.** A matched node that
    straddles the boundary emits hints at its own sub-positions
    (`src/engine/syntax.rs:1739`), so a fine-grained request can receive hints
    outside `[start, end]`.
15. **`declaration_target` treats a variable initializer as a declaration
    name.** `src/engine/syntax.rs:1155` matches any node whose parent is a
    `variable_declarator`/`enhanced_for_statement`, including the `value` and
    the iterable, and can pick the wrong symbol under shadowing.
16. **Diagnostics are narrower than advertised.** Only bare `type_identifier`s in
    a subset of positions are checked (`src/engine/syntax.rs:1994`); `throws`,
    `catch`, `instanceof`, array element types, generic arguments, and qualified
    names are unchecked, and `model.contains(name)` (`src/engine/syntax.rs:2051`)
    is workspace-wide by simple name, so a same-named type in any package
    suppresses the warning everywhere.
17. **`is_valid_identifier` accepts non-Java input.**
    `src/engine/syntax.rs:1496` admits some code points Java rejects and does not
    reject restricted identifiers (`var`, `record`, `yield`).
18. **Unreachable code.** The `None if strict` arm of `type_target_for`
    (`src/engine/syntax.rs:349`) cannot be reached; the only `strict = true`
    caller passes `Some(package)`.
19. **Test-harness hygiene and flakiness.** Scan-driven harness tests call
    `std::env::set_var("JAVA_LSP_JDK", …)` without restoring the previous value
    and do not all take `jdk::env_lock()`, and temp workspaces are not always
    removed. One intermittent failure was observed in ~65 parallel runs of
    `cargo test --test harness`.
20. **Changelog notes.** The "Standard library indexing" bullet is filed under
    the `## 2026-09-09` heading while it is part of this uncommitted change set,
    and the "Verified by N tests" figures are cumulative suite totals, which
    reads as coverage for the named feature.

# Proposal

Fix the two correctness defects first — the cross-package member identity in
references/rename (2) and the duplicate diagnostics (3) — by comparing a
package-qualified owner identity and by de-duplicating the `type_list`
expansion. Then close the remaining behavioural gaps: gate the search-collected
declarations on `include_declaration` (4), only take the type branch for the
import's terminal segment (5), make nested-type naming agree between the
class-file keying and descriptor rendering (6), refuse rename when a candidate
file cannot be read or parsed (7), treat a fully-qualified use as visibility
(8), and resolve a supertype through its own qualifier (9). Finish with the
tests the changes need (10, 11, and a negative per fixed finding), the
conservative-scope hardening (13–18), the harness/env hygiene (19), the fixture
cleanup (12), and the changelog corrections (20).

Throughout, keep the server's standing rule — no result beats a wrong result —
so every fix must refuse rather than guess, and the LSP shape stays as it is
(plain `changes` `WorkspaceEdit`, no versioning).

# Decisions

- **Kind: `bug`.** The dominant nature is correcting defects in shipped
  behaviour, even though the request also carries test, doc, and hygiene work.
- **Scope: everything, nothing deferred.** All twenty findings are in this one
  request (the alternatives — splitting a targeted `fawi-fix` per defect, or
  pushing the optional items to a later review — were considered and dropped).
- **Refusal stays the failure mode.** Fixing 7 must make `rename` return `None`
  when the search cannot complete, rather than emitting a partial edit; this
  matches the existing decision that a destructive operation refuses when it
  cannot be shown safe.
- **One nested-type naming convention.** Fix 6 picks one rule for how a nested
  type is keyed and rendered and applies it to both the model and descriptor
  parsing, rather than patching one side.
- **Noisy artifacts leave the change.** Regenerated `example/*/target/`
  artifacts and surefire reports are build output; they should not be part of a
  source change (12).
- **No new dependencies.** Every fix lands in the existing modules.

Documentation impact: `docs/architecture.md` needs its references/rename
resolution and refusal rules, its type-resolution description (nested-type
naming, qualified supertypes), and its diagnostics-scope statement updated to
the corrected behaviour, and `docs/dev/changelog.md` records the fixes.
`docs/requirements.md`'s R7 approximation notes may need the same corrections.

# Acceptance criteria

1. `cargo test` prints no stray output; the `SRCZIP` `println!` is gone (1).
2. Renaming a member of `a.Widget` leaves a use of `b.Widget`'s same-named
   member untouched, with a negative test over two same-named types (2).
3. `class A implements Missing, Also {}` yields exactly one diagnostic per
   unresolved name, with a test (3).
4. `references`/`rename` with `include_declaration = false` omit the
   declaration and include it when `true`, with a test for each (4).
5. A cursor on a non-terminal segment of an import path yields no target
   (references empty, rename `None`), with a test (5).
6. A receiver typed as a nested library class resolves its members and renders a
   source-form name (not `Map$Entry`); two nested types sharing an innermost
   name in one package do not overwrite each other, with tests (6).
7. `rename` returns `None` when any candidate file cannot be read or parsed,
   with a test (7).
8. A file referencing a type only by its fully-qualified name is included in
   references and rename, with a test (8).
9. A subtype inheriting a member from a supertype whose simple name is shared
   across packages resolves the inherited member, with a test (9).
10. The JDK-file hint test asserts the exact `: List<Integer>` label, and the
    chain-hint test fails when that family is removed, with position-asserted
    assertions (10).
11. Tests exist for the library-declaration rename refusal, `jdk.rs`'s
    `TypeInfo` output, and the warm-up `index.type_model()` model (11).
12. `example/greeting-app/.../Main.java` is valid Java with no scratch
    variables and no unused imports, and the regenerated `example/*/target/`
    artifacts and surefire reports are not part of the change (12).
13. A call whose argument count matches no overload produces no parameter hint,
    with a test (13).
14. No inlay hint is emitted outside the requested range, with a test (14).
15. A variable initializer and an enhanced-for iterable are not treated as
    declaration names, with a test (15).
16. The diagnostic scope and its simple-name suppression are either broadened
    or documented as limitations in `docs/architecture.md` (16).
17. `is_valid_identifier` rejects restricted identifiers and non-Java
    identifier characters, with tests (17).
18. The unreachable `type_target_for` arm is removed (18).
19. Scan-driven harness tests take `env_lock()`, restore every environment
    variable they set, and clean their temp directories; the suite runs with no
    intermittent failure (19).
20. The changelog's standard-library bullet is dated correctly and the
    "Verified by N tests" wording notes the counts are cumulative suite totals
    (20).

# Implementation plan

## Approach

No new dependencies and no LSP-shape changes. Work lands in four source files,
the harness, the example, and the docs.

- **`src/types.rs` — identity and resolution.** `member_owner`
  (`src/types.rs:1502`) returns a package-qualified identity (name plus the
  declaring type's package) instead of a bare simple name, and
  `members`/`member_owner` resolve each supertype through its own qualifier with
  `lookup(supertype, context)` rather than `simple_name()` + the subtype's
  package (`src/types.rs:551`, `src/types.rs:1531`). One nested-type naming rule
  is chosen — the `$`-joined binary simple path (`Map$Entry`) — and
  `Ty::simple_name`/`qualified_package`/`display` (`src/types.rs:103`) are
  taught to split on `$` so descriptor parsing (`src/types.rs:222`) and model
  keying agree. A held method/field's `type_params` binding is unchanged.
- **`src/classfile.rs` — model keying.** `class_type_info` (`src/classfile.rs:58`)
  keys a nested class consistently with the chosen rule, so distinct nested
  types no longer overwrite each other and jar types match descriptors.
- **`src/engine/syntax.rs` — the query fixes.** `member_access_matches`
  (`src/engine/syntax.rs:1402`) and `member_target` (`src/engine/syntax.rs:365`)
  compare the qualified owner identity (and `declared_in_workspace` takes the
  package); `checked_type_nodes` (`src/engine/syntax.rs:1994`) expands a
  `type_list` exactly once, so diagnostics de-duplicate;
  `collect_member_nodes`/`collect_local_occurrences` exclude declaration name
  nodes and `add_declarations` is the only path that adds them, so
  `include_declaration` is honoured; `resolve_target`
  (`src/engine/syntax.rs:209`) takes the import's type branch only when the
  cursor is on its terminal segment; `type_visible_in`
  (`src/engine/syntax.rs:1287`) also admits a file that spells the type
  fully-qualified; `collect_occurrences` (`src/engine/syntax.rs:409`) reports
  whether the search completed, and `rename` (`src/engine/syntax.rs:700`)
  returns `None` when it did not. The conservative-scope fixes: `parameter_hints`
  emits nothing without an exact arity match, `collect_inlay_hints` requires the
  emitted position to lie inside the requested range, `declaration_target`
  (`src/engine/syntax.rs:1155`) requires the construct's `name` field,
  `is_valid_identifier` (`src/engine/syntax.rs:1496`) applies Java's
  identifier and restricted-identifier rules, and the unreachable
  `type_target_for` arm (`src/engine/syntax.rs:349`) is deleted.
- **Tests.** `tests/harness.rs` gets a negative test per fixed defect and the
  end-to-end coverage the changelog claims (library rename refusal, exact
  `: List<Integer>`, chain hints by position, `include_declaration`); unit tests
  cover the nested-type rule, qualified supertypes, import-segment cursors,
  identifier validation, and the strict parameter hint. Scan-driven harness
  tests take `jdk::env_lock()` and save/restore every environment variable they
  set, and remove their temp directories.
- **`src/index.rs`/`src/jdk.rs`.** Add assertions that the warm-up model
  (`index.type_model()`) holds source types and that `jdk.rs` returns typed
  `TypeInfo`s; the code itself is unchanged unless a test exposes a gap.
- **Example and hygiene.** `example/greeting-app/.../Main.java` becomes valid
  Java with no scratch variables and no unused imports, and the regenerated
  `example/*/target/` artifacts and surefire reports are restored to the
  committed bytes so they leave the source change.

## Steps

- [ ] Fix the member identity (2): return a package-qualified owner from
      `member_owner`, compare it in `member_access_matches`/`member_target`, and
      pass the package into `declared_in_workspace`; add the two-package
      negative test. (AC2.)
- [ ] De-duplicate diagnostics (3): expand the `type_list` once in
      `checked_type_nodes`/`visit_type_positions`; add the
      `implements Missing, Also` test. (AC3.)
- [ ] Honour `include_declaration` (4): exclude declaration name nodes from
      `collect_member_nodes`, `collect_local_occurrences`, and
      `collect_type_occurrences`; add tests for both flag values. (AC4.)
- [ ] Fix the import cursor (5): take the type branch only on the terminal
      segment in `resolve_target`; add the test. (AC5.)
- [ ] Fix nested-type naming (6): apply the `$`-split rule in `Ty` and
      `class_type_info`; add tests for rendering, member lookup, and two nested
      types sharing an innermost name. (AC6.)
- [ ] Make rename refuse an incomplete search (7): have `collect_occurrences`
      report completeness and `rename` return `None` when a candidate file
      cannot be read or parsed; add the test. (AC7.)
- [ ] Fix type visibility (8): admit fully-qualified uses in `type_visible_in`;
      add the test. (AC8.)
- [ ] Resolve supertypes through their qualifier (9): use `lookup` in
      `members`/`member_owner`; add the shared-simple-name inheritance test.
      (AC9.)
- [ ] Strengthen the hint tests (10): assert the exact `: List<Integer>` label
      and isolate each hint family by position so the chain test fails without
      the chain family. (AC10.)
- [ ] Close the remaining test gaps (11): library-declaration rename refusal,
      `jdk.rs` `TypeInfo` assertions, and an `index.type_model()` warm-up
      assertion in `src/jdk.rs`/`src/index.rs`. (AC11.)
- [ ] Apply the conservative-scope fixes (13–15, 17–18): strict arity in
      `parameter_hints`, in-range positions in `collect_inlay_hints`, `name`
      field in `declaration_target`, Java identifier rules in
      `is_valid_identifier`, and remove the dead `type_target_for` arm, each
      with a test. (AC13–AC15, AC17–AC18.)
- [ ] Harden the harness (19): take `env_lock()`, save/restore environment
      variables, and clean temp directories in every scan-driven test. (AC19.)
- [ ] Clean the example (12): make `example/greeting-app/.../Main.java` valid
      Java with no scratch code or unused imports, and restore
      `example/*/target/` and the surefire reports to their committed bytes so
      build artifacts leave the change. (AC12.)
- [ ] Document the corrected behaviour in `docs/architecture.md`: the
      package-qualified member identity and supertype-qualifier resolution in
      the references/rename and type-layer bullets, the single-expansion
      diagnostic rule, the `include_declaration` behaviour, the nested-type
      naming rule, and the diagnostics scope and simple-name suppression
      (finding 16). (AC6, AC8, AC9, AC16.)
- [ ] Correct `docs/dev/changelog.md` (20): fix the heading/date of the
      standard-library bullet, note that the "Verified by N" figures are
      cumulative suite totals, and record these fixes under today's date.
      (AC20.)
- [ ] Review `docs/requirements.md`'s R7 approximation notes against the
      corrected resolution and diagnostics behaviour and update any that are
      now inaccurate. (AC16.)
- [ ] Run `cargo test --all-targets` and confirm the full suite passes with no
      stray output and no intermittent failure. (all ACs.)
