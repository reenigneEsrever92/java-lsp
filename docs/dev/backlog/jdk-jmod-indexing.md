---
type: ChangeRequest
kind: bug
title: Index the JDK from jmods so standard-library members complete
description: Every jmod's ZIP offsets are relative to the byte after its 4-byte `JM` magic, but the archive reader used them as absolute file offsets, so no jmod yielded any entry and the whole standard library was silently absent — no `java.*`/`javax.*` types or their members in completion, hover, or navigation on any JDK 9+ install.
state: done
priority: high
tags: [dev, bug, jdk, completions]
owner: felix
verified:
  by: cargo test --lib jdk:: (5 passed) + cargo test --test harness (28 passed; only the sandbox loopback-socket test fails) + probe against Temurin 21.0.8 — "workspace index warm-up complete ... jdk_classes=73310" (was 0); `String s; s.` returns 110 items incl. String methods
  at: 2026-09-28T00:00:00Z
---

# Problem

On any JDK 9+ install (which ships `jmods`, not `rt.jar`), the standard library
was never indexed: `scan_workspace_core` logged
`workspace index warm-up complete ... jdk_classes=0`. Because the JDK is the
model's non-source base, `java.*`/`javax.*` types were absent, so
`.`-completion on a standard-library receiver (e.g. a `String`) offered
nothing, and hover/definition into the JDK found nothing. Workspace types were
unaffected, which made it look like "only standard-library methods fail to
autocomplete". The absence was silent — `locate_jdk` still found a usable home,
so no warning was printed.

The cause is an offset bug in the ZIP reader. A jmod begins with a 4-byte `JM`
magic followed by a normal ZIP, and **every central-directory offset in that ZIP
is relative to the byte after the magic** (verified: in `java.base.jmod`,
`data[cd_offset..]` is not `PK\x01\x02`; `data[cd_offset + 4..]` is). The reader
(`classfile::for_each_zip_entry`) used those offsets as absolute file offsets, so
its central-directory signature check failed on the first entry and it returned
zero entries. `jdk::class_archive_entries` then saw `classes == 0` and skipped
every archive.

This was masked by the tests: `jdk::tests::jdk_entries_read_jmods_and_filter_internals`
wrote a "jmod" with the plain `test_stored_zip` helper, which omits the `JM`
magic, so the offsets happened to be absolute and the test passed. No test used
a realistic jmod.

# Reproduction

1. Point `JAVA_HOME` (or let discovery find) a JDK 9+ with `jmods`.
2. Open any workspace and wait for warm-up.
3. Observe `workspace index warm-up complete ... jdk_classes=0`.
4. Open a file, type `String s; s.` — no `String` members are offered.

**Expected:** the JDK's `java.*`/`javax.*` types are indexed and their members
complete, hover, and navigate.

# Proposal

Strip the 4-byte `JM` magic before parsing a jmod, so its ZIP offsets index the
sliced buffer directly. Detect the magic generically (`data.starts_with(b"JM")`)
in `jdk::class_archive_entries`; a plain jar/`src.zip` starts with the ZIP
local-file-header signature and is used unchanged. Fix the unit-test fixture to
include the magic so the offset handling is exercised, and make the harness
fake-JDK fixture realistic too.

# Decisions

- **D1 — Fix at the jmod reader, not by special-casing offsets everywhere.** The
  ZIP is intact after its prefix; slicing the magic off makes all offsets
  correct with no change to `for_each_zip_entry`. `src.zip`/`rt.jar`/dependency
  jars are unaffected (they have no prefix).
- **D2 — Detect by magic prefix, not by call site only.** `class_archive_entries`
  is passed `in_jmod`, but the check `starts_with(b"JM")` is the real signal (a
  ZIP cannot start with `JM`), so the reader is robust to how it is called.
- **D3 — Make the tests realistic.** A magic-less "jmod" fixture is exactly what
  hid the bug; the fixture now carries `JM\x01\x00`.
- **D4 — No behavior change beyond the fix.** `java.*`/`javax.*` filtering,
  entry/type shapes, and the "missing JDK is a no-op" path are unchanged.

# Acceptance criteria

- A JDK 9+ with jmods indexes its standard library: warm-up reports a large
  `jdk_classes` (was `0`).
- `.`-completion on a standard-library receiver offers its members.
- `cargo test --all-targets` is green apart from the known sandbox
  loopback-socket failures.
- The jmod unit test uses a fixture with the `JM` magic (it would fail against
  the old reader).

# Docs to update

- `docs/dev/backlog/index.md` — this request's row.
- `docs/dev/changelog.md` — an entry at implementation.

# Implementation plan

## Approach

Strip the jmod prefix in `jdk::class_archive_entries` and make the fixtures
realistic.

## Steps

- [x] `src/jdk.rs`: in `class_archive_entries`, slice off the 4-byte `JM` prefix
      when present before `for_each_zip_entry`. (D1, D2)
- [x] `src/jdk.rs`: prefix the `jdk_entries_read_jmods_and_filter_internals`
      fixture with `JM\x01\x00`. (D3)
- [x] `tests/harness.rs`: make `write_fake_jdk`'s `java.base.jmod` carry the
      magic. (D3)
- [x] Verify against a real JDK (Temurin 21.0.8): `jdk_classes=73310` (was 0). (AC)
- [x] `docs/dev/backlog/index.md` row and `docs/dev/changelog.md` entry. (Docs)
