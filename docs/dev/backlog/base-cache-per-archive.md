---
type: ChangeRequest
kind: improvement
title: Cache each archive's parse in its own file, read on demand
description: The class-file base cache is one JSON file per kind, loaded whole into memory and re-written whole each run (jars.json reached 3.7 GB); store one file per archive and read it only when that archive is needed.
state: done
priority: high
tags: [dev, improvement, performance, memory, cache]
owner: felix
verified:
  by: "cargo build/cargo fmt --check clean; the `base_cache`, `bus`, and `sources` tests pass. The jar and JDK indexers now `get`/`insert` per archive with no whole-cache load or save; a second JDK warm-up under a replaced key is served from the store (the updated `a_second_jdk_warmup_is_served_from_the_cache`)."
  at: 2026-10-01T00:00:00Z
---

# Problem

`base_cache::BaseCache<T>` was a `HashMap` in one JSON file per kind
(`base/jars.json`, `base/jdk.json`). `jars.json` had grown to **3.7 GB**, and both
indexers loaded the whole file and re-wrote the whole file every run:

- `BaseCache::load` read the file into a `String` and parsed it into the map — a
  multi-gigabyte transient spike on top of the index, then held for the pass.
- `index_jars` built a `fresh` copy (a second full map) and `save`d it, so it also
  re-wrote the whole 3.7 GB each start.

This is the "RAM goes up too quick" spike: loading the cache is, by itself, several
gigabytes.

# Proposal

A per-archive store: `base_cache::ArchiveStore`, one small JSON file per archive
identity under `<cache>/base/<kind>/<hash>.json`, each guarded by a schema version
and the identity. The indexers `get` an archive's parse only when they need it and
`insert` it only when they (re)parse, so nothing is loaded or written wholesale.

# Decisions

- **D1 — One file per archive, keyed by a hash of the identity.** Reason: read
  only what is needed, and write only what changed — no whole-file load, no
  whole-file rewrite, no `fresh` second copy.
- **D2 — The file carries the version and the identity.** A schema bump or a hash
  collision is then a miss, not a wrong parse. Reason: correctness over cleverness.
- **D3 — `insert` only on a miss.** The jar indexer no longer re-inserts every
  archive on every run. Reason: a warm run writes nothing.
- **D4 — The JDK keeps one key for the whole JDK** (43 MB, one small file).
  Reason: it is small and is indexed as a unit.
- **D5 — Drop the legacy `<kind>.json` on first use.** Reason: it could be 3.7 GB
  and is no longer read.

# Acceptance criteria

1. No code path loads or re-writes a whole-kind cache; a warm run writes nothing.
2. A cache hit is guarded by version + identity; a miss reparses (unchanged
   answers).
3. `cargo build`/`cargo fmt --check` clean and the `base_cache`/`bus`/`sources`
   tests pass.

# Docs to update

- `docs/dev/changelog.md` — an entry at implementation.
- `docs/architecture.md` — the class-file-base cache description (per archive, on
  demand).

# Note

Deleting the old `~/.cache/java-lsp/sources/base/jars.json` (3.7 GB) is folded into
the store's construction; the first run after this reparses once.
