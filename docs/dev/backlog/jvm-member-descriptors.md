---
type: ChangeRequest
kind: feature
title: JVM member descriptors for library type signatures
description: Parse class-file member descriptors and supertypes so indexed jar/JDK types carry real signatures and inherited members.
state: done
priority: medium
tags: [dev, types, classfile, index]
owner: felix
verified:
  by: cargo test (113 passed - 92 lib, 6 bench bin, 14 harness, 1 stdio) +
    java-lsp-bench --files 5 (hover probes resolve; peak RSS 192 MB, warm-up
    6.2 s with the machine JDK indexed)
  at: 2026-09-22T17:47:42Z
---

# Problem

`type-aware-engine` gave the server a declared-type model, but library types
(jars and the JDK) entered it **name-only**: `classfile.rs` is deliberately
"declaration-only by design — descriptors and attributes are skipped", so a
`TypeInfo` synthesized from index entries has `Ty::Unknown` for every member and
no supertypes at all. Two observable consequences: hovering a library member
shows a bare name with no type (`getName()` rather than `String getName()`), and
member completion on a receiver of a library type lists only the members
declared directly on that class, never inherited ones — while the class file has
carried everything needed all along.

# Proposal

Parse the two pieces `classfile.rs` currently skips:

1. **Member descriptors** — each field/method entry's descriptor index names a
   JVM descriptor (`I`, `Ljava/lang/String;`, `(ILjava/lang/String;)Z`). Parse
   it into the existing `Ty` (which already has `from_descriptor` and
   `method_from_descriptor`, currently unused in production) and record it as a
   typed `Member` with the static flag from the member's access flags.
2. **Supertypes** — the `super_class` entry and the `interfaces` table (today
   read and discarded) become the type's `supertypes`, so `TypeModel::members`
   walks into inherited members as it already does for source types.

The JDK's source-archive path (`lib/src.zip`) already parses each file with
tree-sitter, so it can feed the same model through `collect_type_infos` instead
of only index entries.

# Decisions

- **A follow-up to `type-aware-engine`, per that request's stated scope.** The
  engine and its model are unchanged in shape; only how library types are
  populated changes.
- **`java.lang.Object` is not recorded as a supertype of every class**, matching
  source-extracted types (which never record an implicit `Object`). Without
  this, `toString`/`equals`/`hashCode`/`wait`/`notify` would appear on every
  completion list in the workspace — correct Java, but noise nobody asked for.
  An explicit `extends`/`implements` edge is always kept.
- **The public class surface is preserved.** `ClassInfo.methods`/`fields` (name
  lists) stay, now derived from the parsed members, so `class_entries` and the
  existing tests are unaffected; `entries_from_jar` stays as a thin wrapper over
  a new combined reader so no caller has to parse a jar twice.
- **Private/synthetic members stay filtered** exactly as today (`ACC_PRIVATE`,
  `<init>`, `<clinit>`, `$`-containing names), so a library type never offers
  members a caller cannot use.
- **No change to the LSP shell or the trait.**
- **Cost:** the model now holds a signature per library member, so peak RSS
  rises against the `type-aware-engine` baseline. Measured and recorded.

# Acceptance criteria

- A jar/JDK type's members carry their declared types: hover on a library member
  renders `String getName()` (or `int size`) rather than a bare name.
- Member completion on a receiver of a library type includes members inherited
  from its superclass and interfaces, exactly as for workspace types.
- `java.lang.Object` is not injected as a supertype; an explicit
  `extends`/`implements` edge is.
- Private, synthetic, `<init>`, and `<clinit>` members remain absent.
- Existing behaviour is preserved: the current index entries, navigation
  filtering, and tests are unchanged.
- Documented in `docs/architecture.md` and the changelog, with the measured
  memory cost.

# Implementation plan

## Approach

Three files, no new dependencies. `classfile.rs` grows a typed member parse and
supertypes and emits `TypeInfo`s alongside its `SymbolEntry`s; `jdk.rs` does the
same for its archives (and routes source archives through
`types::collect_type_infos`); `index.rs` feeds those `TypeInfo`s into the
warm-up model instead of synthesizing name-only ones from entries.

Key decisions and tradeoffs:

- **One reader, both outputs.** `jar_outputs(path) -> (Vec<SymbolEntry>,
  Vec<TypeInfo>)` reads a jar once and produces both, so the warm-up neither
  parses twice nor holds two copies of the bytes.
- **Descriptor parsing lives in `types.rs`** (already written and unit-tested);
  `classfile.rs` only splits the descriptor into params/return via
  `Ty::method_from_descriptor` and maps the access flags.
- **A malformed or absent descriptor degrades to `Ty::Unknown`**, never an
  error: the hand-built test class files write descriptor index 0, and a real
  class file that confuses the parser must still contribute its member names.
- **`ClassInfo` gains `members: Vec<Member>` and `supertypes: Vec<Ty>`**; the
  `methods`/`fields` name lists are derived from `members` in `parse_class`, so
  every existing assertion and `class_entries` keeps working unchanged.
- **`ty.rs`'s `TypeInfo::new` becomes public** so `classfile.rs` can build the
  same struct the source extractor builds.

## Steps

- [x] Make `types::TypeInfo::new` public. (Groundwork: one constructor for both
      extraction paths.)
- [x] Extend `classfile.rs`: `ClassInfo` gains typed `members` and `supertypes`;
      `read_members` parses each member's descriptor into a `Ty` (field type, or
      method params/return) and its static flag; `parse_class` records the
      superclass (skipping `java/lang/Object`) and the interfaces table; add
      `class_type_info(&ClassInfo) -> Option<TypeInfo>` and
      `jar_outputs(path) -> Option<(Vec<SymbolEntry>, Vec<TypeInfo>)>` with
      `entries_from_jar` delegating to it; unit tests for descriptor parsing,
      supertypes/interfaces, the `Object` skip, and degradation on a missing
      descriptor. (AC1, AC2, AC3, AC4, AC5.)
- [x] Extend `jdk.rs`: `class_archive_entries` and `src_zip_entries` also build
      `TypeInfo`s (the latter via `types::collect_type_infos`), and
      `jdk_entries` returns them alongside the entries; update its tests to the
      new tuple. (AC1, AC2 — library types get signatures whichever JDK layout
      is found.)
- [x] Wire the warm-up in `index.rs`: feed the jars' and JDK's `TypeInfo`s into
      the model instead of `TypeModel::add_entries`, keeping the background task
      and the request path untouched (R6). (AC1, AC2, AC5.)
- [x] Update `docs/architecture.md`: correct the `classfile.rs` description
      (descriptors and supertypes are now parsed), and the type-layer bullet's
      claim that library types are name-only with no inherited members. (AC6.)
- [x] Record the memory delta and the change in the changelog. (AC6.)
