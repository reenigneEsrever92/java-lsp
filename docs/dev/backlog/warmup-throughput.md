---
type: ChangeRequest
kind: improvement
title: Cache the class-file base across warm-ups
description: The dependency-jar and JDK class-file base is re-read and re-parsed from scratch on every start; persist it under the existing cache dir, keyed by a schema version and each archive's identity, and re-index only what changed.
state: done
priority: high
tags: [dev, improvement, performance, warmup]
owner: felix
verified:
  by: cargo test --all-targets — lib 287 passed / 4 failed, harness 28 passed / 1 failed, stdio 1 passed, bins 7 passed; the 5 failures are the known sandbox loopback binds (the `sources` `TestServer` and the harness `SourceServer`), not regressions
  at: 2026-09-29T00:00:00Z
---

# Problem

On a large workspace the non-source base — the resolved dependency jars and the
installed JDK — is the repeated cost: it is re-read and re-parsed from scratch
on **every** server start, so warm-up duration scales with the total class-file
count and is paid in full on every open, even when nothing changed since the
last run. Everything else in the warm-up (scheduling, progress reporting) is
already addressed by `driver-message-bus`; this request is only the cross-run
cache.

# Proposal

Persist the parse output of the non-source base so a restart re-parses only what
changed:

- **Cache the base, version-stamped and best-effort.** Persist the base's
  `Vec<SymbolEntry>` + `Vec<TypeInfo>` for the resolved jars and the JDK under
  the existing cache dir (`$JAVA_LSP_SOURCES_CACHE`, else the XDG/temp default,
  `sources.rs`), keyed by a schema/parser version plus each archive's identity
  (path + size + mtime) and the JDK home; load it on start and re-index only
  archives whose identity changed. A missing, short, or corrupt file, or a
  version change, invalidates the cache and triggers a full parse — the cache is
  an optimization, never a source of a wrong or partial index.

# Decisions

- **D1 — Cache only the non-source base (jars + JDK), not the workspace sources.**
  The class-file archives are the repeated, expensive, and stable part; workspace
  sources change and are parsed cheaply enough.
- **D2 — The cached content is exactly what the parse produced,** so answers are
  identical; the cache never changes the index, only how fast it is built.
- **D3 — Serialization uses `serde`/`serde_json`** (already in the dependency
  tree via `tower-lsp`): `SymbolEntry`/`TypeInfo` gain derives, and their
  `Arc`-shared fields (`large-project-memory` D5) need serde's `rc` feature or an
  owned copy at the cache boundary.
- **D4 — Best-effort and safe by default.** A key mismatch, a corrupt or partial
  file, or a version change falls back to a full parse; the cache is written
  beside its target and renamed, so a partial write is never read.

# Acceptance criteria

- A second warm-up over an unchanged workspace re-parses no class files (the jar
  and JDK phases are served from the cache) and produces an index identical to
  the uncached one.
- A changed archive (or a schema-version change) is re-parsed; the cache is never
  a source of a stale or partial base.
- A missing, corrupt, or partial cache file degrades to a full parse without an
  error.
- `cargo test --all-targets` is green apart from the known sandbox loopback
  failures; warm-up wall time on a large fixture drops versus the uncached path.

# Notes

- The parallelization and progress-reporting halves of the former
  `warmup-throughput` request were folded into
  [driver-message-bus](driver-message-bus.md): the drivers run concurrently and
  each reports its own stage, so the "stuck on one phase message" symptom is
  gone. What remains is the cache, above.

# Implementation plan

## Approach

A best-effort, version-stamped JSON cache of the class-file parse output.
`src/base_cache.rs` holds a generic `BaseCache<T>` (load, save, and a schema
version check), the `ArchiveOutput`/`JdkArchive` value types, and an `identity`
helper (path + size + mtime). The two producers in `src/index.rs` consult it:
`index_jars` per jar, and `index_jdk` keyed by the JDK home plus the identity of
every archive it reads (so a JDK upgrade re-parses it in full). The base types
(`SymbolEntry`, `IndexKind`, `TypeInfo`, `Member`, `Param`, `Ty`, `Prim`) gain
`serde` derives; `serde`'s `rc` feature carries the `Arc`-shared fields. Writes
go to a temp file that is renamed into place, and every failure — missing,
corrupt, partial, version mismatch, or an archive with no identity — degrades to
a full parse. The cached content is exactly what the parse produced, so answers
are identical with or without it.

## Steps

- [x] Add `serde` (`derive`, `rc`) and derive `Serialize`/`Deserialize` on the
      base types (`SymbolEntry`, `IndexKind`, `TypeInfo`, `Member`, `Param`,
      `Ty`, `Prim`).
- [x] Add `src/base_cache.rs` (`BaseCache<T>`, `ArchiveOutput`, `JdkArchive`,
      `identity`, and the `base/jars.json` + `base/jdk.json` paths under the
      sources cache dir); register the module in `lib.rs`.
- [x] Wire `index_jars` (per jar) and `index_jdk` (whole JDK, keyed by home +
      every archive identity) to the cache; add `jdk::jdk_archive_paths` for the
      JDK key.
- [x] Tests: a cache round-trip plus version invalidation, a missing/corrupt
      load, an identity change, and a JDK warm-up served from the cache.
- [x] Update `docs/architecture.md` (layout and the base/warm-up bullet),
      the changelog, and this request's state.
