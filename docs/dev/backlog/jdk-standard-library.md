---
type: ChangeRequest
kind: feature
title: Standard library (JDK) indexing
description: java.* and javax.* declarations from the installed JDK's jmods (rt.jar fallback) feed completions with auto-import.
state: planned
priority: high
tags: [dev, jdk, index, completions]
owner: felix
---

# Problem

The standard library is the biggest remaining gap in typing support: `String`,
`List`, `Map`, `ArrayList` and friends are invisible to completions, and the
navigation CR explicitly recorded "JDK/library imports are not indexed" as a
limitation. A Java developer expects the standard library the moment the
server starts — without configuring anything.

# Proposal

Index the installed JDK's standard-library declarations during warm-up, using
the same pipeline as dependency jars:

- **Source**: the class files inside `$JAVA_HOME/jmods/*.jmod` (JDK 9+; jmods
  are ZIP archives with a short magic prefix — the tail-scanning ZIP reader
  already handles that), falling back to `rt.jar` (JDK 8). Class entries under
  `classes/` (jmod) or the archive root (rt.jar) are parsed by the existing
  minimal class-file reader. No JVM is run, nothing is invoked — this is
  static reading of the local JDK installation.
- **Scope**: only `java.*` and `javax.*` packages, skipping `jdk.*`,
  `com.sun.*`, `sun.*` internals and `module-info`/`package-info` entries —
  the standard library a developer expects, keeping index size and completion
  noise bounded.
- **Discovery**: `$JAVA_HOME` first, then a heuristic over common locations
  (`/usr/lib/jvm/*`, `/Library/Java/JavaVirtualMachines/*/Contents/Home`),
  overridable via `$JAVA_LSP_JDK`. No JDK found → graceful no-op: features
  degrade exactly like missing dependency jars, never blocking (R6).
- **Index flow**: entries are flagged `dependency: true` — offered in
  completions with auto-import edits, filtered out of definition and
  `workspace/symbol` (jar/module locations are not openable). `java.lang.*`
  is implicitly imported in Java, so those symbols never get an import edit.

# Decisions

- **Class files from jmods (rt.jar fallback), not src.zip sources** — the
  class-file reader already exists, it is faster than reparsing ~8k source
  files with tree-sitter, and declaration-only fidelity is exactly how
  dependency jars are handled; sources would add nothing the index consumes.
- **`java.*`/`javax.*` scope** — excludes internal and tool packages from
  both the index and the completion list; the filter is on the class FQCN.
- **$JAVA_HOME with heuristic + env override** — zero configuration for the
  common case, deterministic override for unusual setups, and a documented
  no-op when no JDK is present.
- **java.lang needs no import** — implicitly imported in Java; importing it
  would be redundant noise (never-worsen extension).
- **Measured, not guessed** — the full `java.*`/`javax.*` surface grows the
  index by hundreds of thousands of small entries; the bench harness records
  the memory/warm-up impact so the regression stays visible.

# Acceptance criteria

- With a JDK present, standard-library types (`String`, `List`, …) and their
  members appear in completions, with an `import java.util.List;`-style edit
  attached for non-`java.lang` symbols and none for `java.lang` ones.
- Internal packages (`jdk.*`, `com.sun.*`, `sun.*`) and module/package-info
  entries do not appear anywhere in the index.
- Without a JDK (or with an unusable one), the server behaves exactly as
  today: warm-up completes, no JDK entries, no errors surfaced to the client.
- Definition and `workspace/symbol` still return nothing for JDK symbols.
- The bench records the warm-up time and peak-RSS impact of JDK indexing;
  request latency stays non-blocking during it (R6).
- `docs/architecture.md` documents discovery, scope, and the java.lang rule.

# Implementation plan

## Approach

One new module (`src/jdk.rs`), one small refactor (`classfile.rs` selective
entry reading), wiring in the warm-up, and a one-rule extension of the
auto-import pipeline. Two env vars: `JAVA_LSP_JDK` (explicit override) and
`JAVA_HOME` (standard).

- **Discovery** (`jdk::locate_jdk`): `JAVA_LSP_JDK` → `JAVA_HOME` → heuristic
  over `/usr/lib/jvm/*` and `/Library/Java/JavaVirtualMachines/*/Contents/Home`
  (sorted, first home with `jmods/` wins, else first with an `rt.jar`).
  Nothing found → `None`, and the warm-up simply skips the step.
- **Extraction** (`jdk::jdk_entries(home) -> Vec<(Url, Vec<SymbolEntry>)>`):
  with `jmods/`, every `*.jmod` (sorted) is read with the ZIP tail-scan
  reader (jmods are ZIPs with a short magic prefix); with only `rt.jar`, the
  same reader handles it. Only entries named `classes/**.class` (jmod) or
  `**.class` (rt.jar) are decompressed and parsed — the refactor in
  `classfile.rs` adds a filtered variant so multi-megabyte native libraries
  inside jmods are never touched. Internal names are turned into dotted
  FQCNs; `module-info`/`package-info` and anything outside `java.*`/`javax.*`
  is skipped. Entries reuse `class_entries` (kinds, containers, packages,
  `dependency: true`) and are attributed to the jmod/rt.jar file's URI.
- **Warm-up wiring** (`index.rs`): after dependency jars, locate the JDK and
  upsert one file entry per module archive; the readiness log line gains a
  `jdk=<classes>` field. All of it stays on the background task (R6).
- **java.lang rule** (`engine/syntax.rs`): `import_edit` suppresses the edit
  for `java.lang.*` symbols — implicitly imported in Java. Everything else
  (offer, filter-from-navigation, conflict rules) comes free from the
  existing dependency path.
- **Testing without a JDK**: unit tests build a fake JDK home — a STORED-zip
  jmod containing `classes/java/lang/String.class`, a filtered
  `jdk/internal/X.class`, a `classes/module-info.class`, and a native entry
  that must not be decompressed — plus an `rt.jar` fallback fixture and a
  `JAVA_HOME`-based discovery test. The Maven integration test gains a
  sibling: a fake-JDK workspace asserting `List` is offered with an
  `import java.util.List;` edit and `String` without one.

**Docs touched**: `docs/architecture.md` — a JDK bullet (discovery, scope,
java.lang rule) in the components section, `jdk.rs` in the layout tree, and
the navigation limitation sentence "JDK/library imports are not indexed"
retired.

## Steps

- [ ] Refactor `classfile.rs`: `read_zip_entries_filtered(data, predicate)`
      with the existing `read_zip_entries` delegating to it, so jmods' native
      libraries are skipped without decompression; unit test that a filtered
      entry is never read. (Groundwork: jmods are multi-hundred-MB archives)
- [ ] Implement `src/jdk.rs`: `locate_jdk` (override → JAVA_HOME → heuristic
      → None) and `jdk_entries` (jmods first, rt.jar fallback, package and
      module-info filtering, reuse of `class_entries`); unit tests with a
      generated fake JDK covering jmod reading, package filtering, the
      rt.jar fallback, and discovery. (AC: JDK types indexed; internals and
      module-info excluded; graceful no-op)
- [ ] Wire the warm-up (`index.rs`): after dependency jars, locate the JDK
      and upsert its entries; extend the readiness log line with `jdk=`.
      (AC: standard library appears in the index off the request path)
- [ ] Add the java.lang rule to `import_edit` (`engine/syntax.rs`) with a
      unit test: `java.lang.*` symbols are offered but carry no import edit.
      (AC: no redundant java.lang imports)
- [ ] Integration test in `tests/harness.rs`: fake-JDK workspace, completion
      offers `List` with `import java.util.List;` and `String` without an
      edit; definition/`workspace/symbol` still return nothing for JDK
      symbols. (AC: end-to-end behaviour)
- [ ] Run the bench harness and record the new warm-up/memory baseline in
      the changelog entry. (AC: impact measured, R6 intact)
- [ ] Update `docs/architecture.md` per the doc note above. (Doc step)
