---
type: ChangeRequest
kind: improvement
title: Resolve member navigation through the receiver's declared type
description: Member go-to-definition/declaration/references resolve a name only, so a method declared in both an interface and its implementation (or any overridden method) yields nothing; resolve the member through the receiver's declared type — a field, this, new T(), a static qualifier, or a parameter/local — using the index's type model.
state: done
verified: { by: felix, at: 2026-10-12T19:00:00Z }
priority: medium
tags: [dev, improvement, navigation, types]
owner: felix
---

# Problem

Member navigation (`find_declaration`/`find_references` in
`src/index/navigate.rs`) resolves a member name purely by name and kind against
the flat symbol table, ignoring the receiver. Once a method name is declared in
more than one place — the norm the moment an interface method is implemented, or
any method is overridden — the cross-file ambiguity policy refuses, so
`ti.doSomething()` returns nothing even though `ti`'s declared type
(`TestInterface`) makes the target unambiguous. Interface-heavy code is the
common case (a field or parameter typed by an interface, implemented by a class)
and is effectively unnavigable — exactly what `example/` reproduces:

```java
public class Main {
    TestInterface ti = new TestImpl();   // ti : TestInterface
    public static void main(String[] args) {
        ti.doSomething();                // cannot reach TestInterface.doSomething
    }
}
```

The index already owns the model this needs — `TypeInfo { supertypes, fields,
methods, … }` with `query::lookup` and `query::members` (which walk supertypes,
cycle-guarded) — so the only missing piece is the *receiver scope* step: reading
the cursor's own file to turn the expression before the `.` into a type.

# Proposal

Resolve a member name through the receiver's declared type. When the cursor is
on `recv.name` or `recv.name(...)`, determine `recv`'s type from the cursor's
file together with the index's type model, then pick the declaration of `name`
in that type and its supertypes:

- `this` / an implicit member access → the enclosing type;
- a bare identifier that is a **field of the enclosing type** (`ti`) → the
  field's declared type;
- a **parameter or local of the enclosing method** → its declared type;
- `new T(…).m()` and `Type.staticMember` → `T`;
- the existing "receiver cannot be in question" cases (`Owner.member`,
  `new Owner().member`) fold into the same path.

Apply it to `textDocument/definition`, `textDocument/declaration`, and
`textDocument/references`. The general ambiguity policy stays for names with no
receiver-resolvable relationship: a name declared in two unrelated types still
yields nothing.

# Decisions

- **D1 — Receiver shapes in scope:** `this`/implicit, a field of the enclosing
  type, a parameter or local of the enclosing method, `new T(…).m()`, and
  `Type.staticMember`. **Out of scope** (documented limitations): chained calls
  (`a.b().c()`), generic type-argument substitution, and lambda/anonymous-class
  scopes.
- **D2 — Queries in scope:** definition, declaration, and references.
  Definition and declaration both route to `IndexRequest::FindDeclaration`, so
  both are covered by the target resolution; references additionally requires
  the member-occurrence matcher to accept a receiver whose type resolves.
- **D3 — Approach: extend `navigate.rs`.** Read the receiver from the cursor's
  already-parsed file and resolve its type through the index's existing
  `TypeModel` (`query::lookup`, `query::members`). Do **not** revive the
  deprecated `src/deprecated/types.rs` engine.
- **D4 — Keep the ambiguity policy** for names with no receiver-resolvable
  relationship. No result beats a wrong result; this is the property that must
  not regress.
- **D5 — Type-name navigation and `workspace/symbol` are unchanged.** The
  improvement touches member resolution only.
- **D6 — This is an improvement, not a new capability:** it makes existing
  navigation resolve a receiver where it resolves nothing today, advancing R7
  (type-aware engine) for the navigation features; R5's ambiguity policy still
  governs names that cannot be tied to a receiver.

# Acceptance criteria

1. On `example/` (with `TestInterface`/`TestImpl`), a cursor on `doSomething` in
   `ti.doSomething();` resolves to `TestInterface.doSomething` for both
   definition and declaration, and references on it reports the interface
   method's declaration and its resolved uses.
2. A member call resolves through a receiver that is a parameter/local, a field,
   `this`, `new T()`, or a static qualifier; it resolves to the most-derived
   declaration when the receiver's type and a supertype both declare the name
   (an override, or a concrete-typed receiver such as `TestImpl t; t.doSomething()`)
   and still resolves to the supertype's declaration when the subtype declares the
   name not at all; when several types declare the name and the receiver's type
   does not relate them, no location is returned.
3. A member whose name is unique still resolves as before; type-name navigation,
   `workspace/symbol`, and the existing navigation tests (`navigate.rs`) are
   unchanged.
4. `cargo build`, `cargo fmt --check`, and `cargo test --lib` are clean.

# Docs

- `docs/architecture.md` — the navigation bullets currently say a receiver-typed
  member access "awaits the type layer" and is "deliberately not reported"; they
  must describe receiver-resolved member navigation and the remaining
  limitations (chained calls, generic substitution, lambdas).
- `docs/requirements.md` — record the traceability: R7's type layer now backs
  navigation's receiver resolution (R5 remains the authority for ambiguous
  names).

# Implementation plan

## Approach

The gap is a single step in `src/index/navigate.rs`: turning the expression before
a `.` into a type. Everything else the change needs already exists — the index
owns the declared-type model (`query::lookup`, `query::members`,
`TypeInfo { supertypes, fields, methods, … }`) and the cursor's file is already
parsed to a tree-sitter AST by `read_and_parse`. So the work is a receiver-scope
reader inside `navigate.rs`, threaded through the two resolution paths already
there. Nothing in the shell, the symbol table, or the extraction changes (D3):
`textDocument/definition` and `textDocument/declaration` keep routing to
`FindDeclaration` and `textDocument/references` to `FindReferences`, so
`server.rs` and `messages.rs` are untouched (D2).

**Receiver-scope resolution** — a new helper
`receiver_type_of(index, text, tree, package, name_node) -> Option<Ty>`: find the
member-access name node (`is_member_access_name`), take its parent's `object`,
and classify the receiver into a `Ty`:

- `this`, and an implicit `this` (a bare member name inside the enclosing type)
  → `Ty::reference(enclosing type name)` from the nearest ancestor type
  declaration;
- an `identifier` → resolve it in the enclosing scope, in order: a parameter of
  the enclosing method, a local variable declarator in scope before the cursor,
  then a field of the enclosing type (its `Ty` read from that type's `TypeInfo`
  in the model — D3); if it names a type instead, the type itself
  (`Type.staticMember`);
- a `type_identifier` / `scoped_type_identifier` → the written name;
- an `object_creation_expression` → its `type` node (`new T(…)`);
- anything else — a chained `field_access`/`method_invocation`, `super`, an array
  access, a lambda — → `None` (D1's documented out-of-scope shapes).

A parameter, local, or `new T()` type is turned into a `Ty` with the existing
`analyze::type_from_node`; a field's `Ty` comes straight from the model. A small
`type_closure(index, ty, package)` helper then walks the receiver type and its
`supertypes` (the same cycle-guarded BFS `query::members_with_overloads` uses,
via `query::lookup`) and collects each type's identity — simple name *and*
package, so a same-named type elsewhere is never touched.

**Definition and declaration.** `declaration_candidates` gains the receiver step:
for a member cursor whose receiver resolves, the name-matched candidates are
narrowed to those declared by the receiver type or one of its supertypes
(matching the entry's innermost `container` and its `package`); the existing
`unique_candidates`/`unique_location` policy then applies as before. When the
receiver does not resolve, today's name-only candidate set stands unchanged
(D4). Because definition and declaration share `FindDeclaration`, one change
covers both (D2).

**References.** `find_references` already builds its `Target` from the same
candidates, so narrowing the target to the interface's declaration is automatic;
the missing half is the occurrence matcher. `receiver_is_owner` becomes a
receiver-type check: `collect_member_occurrences` is given the index and the
file's package, and for a member access resolves the receiver's type *in that
occurrence's own file* through the same helper, accepting it when the receiver's
type closure contains the target's declaring type. The bare-name rule (a member
name inside the declaring type's own span — an implicit `this`) is unchanged.

**Tradeoff.** With a receiver whose declared type resolves, a name it does not
declare now yields no candidate rather than falling back to the possibly-unique
name-only set that could land on an unrelated type — the intended direction (D4:
no result beats a wrong result), and a behaviour change for a member access on a
known receiver that shares a name with an unrelated type.

## Steps

- [x] Add the receiver-scope helpers to `src/index/navigate.rs`:
  `receiver_type_of` (classify a `.`-receiver into a `Ty` for `this`/implicit, a
  field, a parameter/local, `new T()`, and `Type.staticMember`, with small
  `enclosing_type_name`/`enclosing_method` walkers and `analyze::type_from_node`)
  and `type_closure` (walk `TypeInfo.supertypes` via `query::lookup`). Return
  `None` for the out-of-scope shapes. (D1, D3; AC2)
- [x] Thread the receiver step into `declaration_candidates`: for a member cursor
  with a resolvable receiver, narrow the candidates to the entries declared by
  the receiver type or a supertype; leave the name-only set when the receiver
  does not resolve, so definition/declaration cover both (D2, D4; AC1, AC2).
- [x] Generalize the occurrence matcher: pass the index and the file's package
  into `collect_member_occurrences`/`receiver_is_owner`, resolve each member
  access's receiver in its own file through `receiver_type_of`, and accept it when
  the receiver's type closure contains the target's declaring type; keep the
  bare-name `owner_span` rule. (D2, D4; AC1)
- [x] Update `docs/architecture.md`: rewrite the navigation prose — the
  definition/declaration passage (the sentence that resolves definition by
  name-only node kind) and the references passage ("a member access only where
  the receiver's type resolves the name back to the same declaring type") — to
  describe resolving a member through the receiver's declared type, and drop the
  "Navigation is the index's own" bullet's documented v1 limitation that a
  receiver-typed member access "awaits the type layer's return"/is not reported,
  stating instead the remaining limitations (chained calls, generic
  substitution, lambda/anonymous-class scopes). (D6; AC1, AC2)
- [x] Update the `src/index/navigate.rs` module doc comment that says a
  receiver-typed member access "awaits the type layer and is deliberately not
  reported" to match the new behaviour and its limits.
- [x] Add tests to `src/index/navigate.rs`'s `tests` module: an interface field
  (`TestInterface ti = new TestImpl(); ti.doSomething();`) resolving to
  `TestInterface.doSomething` for both declaration and references; a
  parameter/local receiver; a `new T()` receiver; a `Type.staticMember` receiver;
  and a negative case where two unrelated types declare the name and the
  receiver's type relates neither, yielding no location. Leave the existing
  navigation tests unchanged. (AC1–AC3)
- [x] Update `docs/requirements.md` to record the traceability: R7's type layer
  now backs navigation's receiver resolution; R5 remains the authority for names
  that cannot be tied to a receiver. (D6)
- [x] Run `cargo build`, `cargo fmt --check`, and `cargo test --lib` clean. (AC4)

## Progress

Re-reviewed after implementation (2026-10-12). A review round found and fixed two
behaviour defects in `src/index/navigate.rs`, each covered by a regression test:

- **Most-derived declaration.** `declaration_candidates` kept *every* candidate
declared by the receiver type or any supertype, so an override or a concrete-typed
receiver spanned two files and `unique_candidates` returned `None`. It now keeps
only the candidates declared by the **nearest type in the receiver's supertype
closure that declares the name** — the closure's BFS order, root-first, mirroring
`query::members_with_overloads`'s first-wins walk — so `TestImpl t; t.doSomething()`
and `new TestImpl().doSomething()` resolve to `TestImpl.doSomething`, an override
resolves to the subtype's declaration, and a supertype-only declaration still
resolves to the supertype. (The narrowing uses a new `declared_by` helper; the
now-unused `in_closure` was removed.)
- **Implicit `this` restricted.** The implicit-`this` fallback narrowed *any* bare
identifier whose text matched an enclosing member — including a method-reference
name (`Strings::trim`), an annotation name, and declaration names — returning a
wrong location. It now fires only for a genuine unqualified member use: an
unqualified `method_invocation` name (`name(...)` with no `object`) of the
enclosing type. Every other bare identifier keeps the name-only ambiguity policy.

A third, smaller defect was fixed in the same pass: an array receiver is now
unresolvable (`resolvable` rejects `Ty::Array`, matching
`query::members_with_overloads`'s array guard), so `String[] xs; xs.charAt(0)`
no longer resolves through the element type.

A fourth gap was closed too: a receiver typed by a generic type **variable**
(`<T> void run(T t) { t.foo(); }`) is no longer resolved through a same-named
class. `identifier_type` now rejects a parameter/local/field type whose name is an
in-scope type parameter (`is_type_parameter`, walking the enclosing method's and
types' `type_parameters`), so the receiver stays unresolved and the ambiguity
policy applies.
