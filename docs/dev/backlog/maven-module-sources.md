---
type: ChangeRequest
kind: feature
title: "Maven module: dependency sources fetch and extraction"
description: Rework src/deprecated/sources.rs into src/maven/sources.rs — reuse or download each -sources.jar (sha1-verified) and extract its .java files into the cache, returning the extracted root directories — no direct index writes.
state: done
verified: { by: felix, at: 2026-10-14T12:00:29Z }
priority: medium
tags: [dev, maven, sources]
owner: felix
parent: maven-module
---

# Problem

The deprecated `src/deprecated/sources.rs` (1001 lines) fetches sources jars and
indexes them itself (parsing entries, publishing `BaseArtifact`/`RemoveBase`
`DriverMessage`s, progress vocabulary). In the redesigned flow the extracted
sources must instead be published as folders via `FoldersDiscoverred`, so the
index scans them — the fetch/extract work stays, the parse/index/progress work
goes away.

# Proposal

Rework `src/deprecated/sources.rs` into `src/maven/sources.rs`:
- Keep: `sources_jar_path`, `sources_url`, `sha1_hex`, `verify_checksum`,
  `download_sources` (write-then-rename `.jar.part`), the bounded-concurrency
  `fetch_sources` (reuse from the local repository, else download and write it
  back; failures logged and skipped), `cache_dir()`
  (`$JAVA_LSP_SOURCES_CACHE` → `$XDG_CACHE_HOME/java-lsp/sources` →
  `~/.cache/java-lsp/sources`), and the write-if-missing-or-truncated extraction
  optimization.
- Change: `offline()`/`base_url()` delegate to
  [settings](maven-module-settings.md); progress/`DriverMessage` logging becomes
  `tracing`; `index_one`/`index_extracted`/`Timers` are replaced by an
  extract-only pass returning the extracted root directories.
- New: `pub fn fetch_and_extract(artifacts: &[Artifact], repo: &Path, cache:
  &Path) -> Vec<PathBuf>` — the roots (`<cache>/<group>/<id>/<version>`) of the
  artifacts that yielded at least one `.java` file, which the hub spawn publishes
  as the `LibrarySources` batch. `package-info.java` files are skipped, as
  before.

# Decisions

- **SO1 — Extract only; the index does the rest.** No tree-sitter parsing, no
  `SymbolEntry`/`TypeInfo` construction, no hub messages in this module. The
  return value is the list of extracted root directories.
- **SO2 — Keep the offline test pattern.** The deprecated `TestServer` (a
  threaded `TcpListener` over fixed routes) ports over so download/checksum paths
  are exercised against a `http://127.0.0.1` fixture, never the network.

# Acceptance criteria

1. `fetch_and_extract` reuses a `-sources.jar` already in the local repository
  and downloads a missing one (writing it into the repository, `.sha1`-verified
  when published, write-then-rename); a failed download is logged and skipped.
2. Extraction writes each artifact's `.java` files under
  `<cache>/<group>/<artifact>/<version>`, skipping `package-info.java`, and
  returns exactly the roots of artifacts that yielded at least one file.
3. A restart over an already-extracted tree does no write work (length guard).
4. `offline()`/`download_base()` delegates are honored: with `JAVA_LSP_OFFLINE`
  set or settings `offline=true`, nothing is fetched.
5. The download/checksum/extract tests from `src/deprecated/sources.rs` port
  over (adapted to the new return shape), run against the local `TestServer`
  fixture; `cargo test` green for the module; no other file touched.

# Implementation plan

## Approach

Copy `src/deprecated/sources.rs`, strip the index/hub portions, and replace the
per-file parse/index with a per-artifact extraction that returns root
directories. `offline()` and `base_url()` thin out to calls on
`crate::maven::settings`. The scaffold holds the public contract.

## Steps

- [ ] `src/maven/sources.rs`: port `cache_dir`, `sources_jar_path`,
  `sources_url`, `sha1_hex`, `verify_checksum`, `download_sources`,
  `fetch_sources` (bounded semaphore; `tracing::warn` instead of hub logs).
- [ ] Replace `index_one`/`index_extracted`/`Timers` with an extraction pass:
  for each artifact, walk the sources jar's `.java` entries (skipping
  `package-info.java`), `create_dir_all` each package dir once, write the file
  only when missing or wrong length, and record the artifact root when any file
  was written; run on a worker pool (one parser no longer needed — nothing to
  parse).
- [ ] `pub fn fetch_and_extract(artifacts, repo, cache) -> Vec<PathBuf>` combining
  fetch then extract; `offline()` short-circuits; the returned roots are sorted
  and deduplicated.
- [ ] Port the deprecated tests (TempDir, TestServer fixture, stored-zip helper):
  reuse-before-download, download+verify, mismatch failure, missing `.sha1`
  accepted, write-if-missing guard, offline short-circuit, returned roots point
  at the cache with `.java` files present.
- [ ] `cargo test` green for the module; no other file touched.

## Next steps

The [hub](maven-module-hub.md) sub-task calls `fetch_and_extract` and publishes
the returned roots as the `LibrarySources` `FoldersDiscoverred` batch.