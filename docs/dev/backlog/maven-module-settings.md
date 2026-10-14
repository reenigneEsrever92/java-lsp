---
type: ChangeRequest
kind: feature
title: "Maven module: Maven settings and local repository"
description: New src/maven/settings.rs honoring ~/.m2/settings.xml (localRepository, mirrors, offline) plus the env-var precedence for the local repository and the download base.
state: done
verified: { by: felix, at: 2026-10-14T12:00:29Z }
priority: medium
tags: [dev, maven, settings]
owner: felix
parent: maven-module
---

# Problem

The deprecated code derives the local repository from `$MAVEN_REPO` /
`~/.m2/repository` and the download base from `$JAVA_LSP_MAVEN_CENTRAL_URL` /
Maven Central, ignoring Maven's own user settings (`~/.m2/settings.xml`).
Decision D4 of the [parent](maven-module.md) requires honoring `localRepository`,
`mirrors`, and `offline` from that file.

# Proposal

New `src/maven/settings.rs`, parsed with roxmltree (already a dependency):
- `Settings` — the user settings that matter: the `localRepository` override, the
  first mirror whose `mirrorOf` matches `central`/`*`/`external:*`, and the
  `offline` flag.
- `local_repository() -> PathBuf` — `$MAVEN_REPO` > settings `localRepository` >
  `~/.m2/repository`.
- `download_base(&Settings) -> String` — `$JAVA_LSP_MAVEN_CENTRAL_URL` > mirror
  > `https://repo1.maven.org/maven2` (trailing slash normalized away).
- `offline() -> bool` — `$JAVA_LSP_OFFLINE` set, or settings `offline=true`.
- `JAVA_LSP_MAVEN_SETTINGS` env override for the settings path (tests and
  unusual setups), defaulting to `~/.m2/settings.xml`; a missing or malformed
  file yields defaults, never an error.

# Decisions

- **S1 — `JAVA_LSP_MAVEN_SETTINGS` env override.** The settings file defaults to
  `~/.m2/settings.xml`; the env var points tests and unusual installs at a
  different file, mirroring how `MAVEN_REPO` already overrides the repository.
- **S2 — Mirrors select the download base only.** Only the first matching mirror
  (ordered as written in the file) is applied, and only to the *source*
  download; `mirrorOf` `central`, `*`, and `external:*` all match. `<servers>`
  credentials and `<proxies>` stay out of scope (D4).
- **S3 — Path expansion.** `~` and `${user.home}` in `localRepository` expand to
  the home directory; a relative path resolves against the home directory,
  matching Maven's own interpretation.

# Acceptance criteria

1. `local_repository()` follows `$MAVEN_REPO` > settings `localRepository` >
   `~/.m2/repository`.
2. `download_base()` follows `$JAVA_LSP_MAVEN_CENTRAL_URL` > first matching
   mirror > Maven Central; a trailing slash is normalized away.
3. `offline()` is true when `$JAVA_LSP_OFFLINE` is set (any non-empty value) or
   settings `offline` is `true`.
4. A missing, empty, or malformed settings.xml yields defaults without error;
   `localRepository`/`mirror`/`offline` are read from the real file when present.
5. Tests run with a temp settings.xml via `JAVA_LSP_MAVEN_SETTINGS`, covering
   each precedence and the `mirrorOf` matcher; no test touches the network.

# Implementation plan

## Approach

Port the precedence logic out of `src/deprecated/resolve.rs::local_repository`
and `src/deprecated/sources.rs::{offline, base_url}` into one settings module,
with the settings file parsing added. The module is pure: no hub types, no
`tracing` beyond a debug/warn line, easily unit-tested. The API contract is the
scaffolded signatures in `src/maven/settings.rs`; keep exactly those public
items so the sibling sub-tasks (resolve/model/sources) compile against them.

## Steps

- [ ] `src/maven/settings.rs`: `Settings { local_repository, mirror, offline }`,
  `Settings::load()` reading `$JAVA_LSP_MAVEN_SETTINGS` (else `~/.m2/settings.xml`);
  parse `<settings><localRepository>`, `<mirrors><mirror>` (`url`, `mirrorOf`),
  and the top-level `<offline>` with roxmltree; `~`/`${user.home}` expansion;
  malformed → defaults.
- [ ] `src/maven/settings.rs`: `local_repository()`, `download_base(&Settings)`,
  `offline()`, `DEFAULT_CENTRAL` const — the precedence chains of AC1–AC3.
- [ ] `src/maven/settings.rs` tests: precedence per AC1–AC3, invalid/missing file
  per AC4, `mirrorOf` matching (`central`, `*`, `external:*`, and non-matching),
  `JAVA_LSP_MAVEN_SETTINGS` redirection, `~` expansion.
- [ ] `cargo test` green for the module; no other file touched.

## Next steps

[resolve](maven-module-resolve.md), [model](maven-module-model.md), and
[sources](maven-module-sources.md) build on this module's API; the
[hub](maven-module-hub.md) sub-task wires `settings` into the spawn.