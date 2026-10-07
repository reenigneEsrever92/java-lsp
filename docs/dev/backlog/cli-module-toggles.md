---
type: ChangeRequest
kind: feature
title: Command-line flags to switch engine modules on and off
description: Parse argv with clap so java-lsp can be launched with individual modules (workspace index, dependencies, jars, sources, JDK, diagnostics, quick fixes) switched off, keeping today's all-on behaviour as the default, with a new startup module owning the configuration-aware warm-up.
state: done
verified: { by: felix, at: 2026-10-06T18:40:00Z }
priority: medium
tags: [dev, cli, configuration]
owner: felix
---

# Problem

java-lsp has no command line today: `src/main.rs` installs tracing, builds the
tokio runtime, and calls `engine::start`, ignoring `argv` entirely. The only way
to turn a subsystem off is an environment variable that each module reads for
itself — `JAVA_LSP_OFFLINE` (`sources.rs`, `resolve.rs`) and
`JAVA_LSP_SEMANTIC_DIAGNOSTICS` (`diagnostics.rs`, `analysis.rs`) — or the
`JAVA_LSP_JDK=/definitely/not/a/jdk` trick the README, tests, and bench use to
disable JDK indexing. There is no single place that shows or sets how the server
is composed, and several modules cannot be switched off at all: the workspace
source scan, dependency resolution, jar indexing, the diagnostics subsystem, and
the quick-fix subsystem. A user who wants a lighter server — for example,
syntax-only, with no dependency crawling or JDK indexing — has to know these
scattered knobs, and cannot reach the ones that do not exist. Language servers
are also routinely launched with flags (VS Code's client adds `--stdio`), which
the server currently ignores.

`engine::start(client)` always builds all twelve participants, and the warm-up
coordination is hard-coded inside the `project` driver: it waits for `Sources` +
`Jars` + `Jdk` before sending `Ready`, and for `Downloads` before the summary.
Simply skipping a driver's `spawn` does not switch it off — it stalls `Ready` and
every index-gated feature waits forever. Making that coordination
configuration-aware _in place_, or threading the launch configuration into each
warm-up driver, would spread the launch configuration across the very modules
the coordinator is meant to orchestrate.

# Proposal

Parse `argv` once in `src/main.rs` with a new `clap`-based CLI, yielding a set of
per-module toggles that `engine::start_with` turns into the actual composition.
A short flag per switchable module lets a user drop it from one command line,
`--help` documents them, and the default (no flags) is exactly today's behaviour:
every module on. `engine::start(client)` stays as the zero-config entry point, so
`LspService::new(engine::start)` and the harness tests are unchanged.

| flag                        | turns off                                      | notes                                                   |
| --------------------------- | ---------------------------------------------- | ------------------------------------------------------- |
| `--no-workspace-index`      | `scan` (source scanner, `Sources` stage)       | workspace `.java` files are not indexed                 |
| `--no-dependencies`         | `resolve` plus `jars` and `sources`            | no dependency model, so nothing to index or download    |
| `--no-jars`                 | `jars` (jar indexer, `Jars` stage)             |                                                         |
| `--no-downloads`            | `sources` (download driver, `Downloads` stage) | equivalent to `JAVA_LSP_OFFLINE` in effect              |
| `--no-jdk`                  | `jdk` (JDK indexer, `Jdk` stage)               | replaces the `JAVA_LSP_JDK=/definitely/not/a/jdk` trick |
| `--no-diagnostics`          | `diagnostics` (and `quickfix`)                 | no diagnostics published, no diagnostics service        |
| `--no-semantic-diagnostics` | the unresolved-symbol pass only                | parse errors still report; implies `--no-quickfix`      |
| `--no-quickfix`             | `quickfix`                                     | `codeActionProvider` is not advertised                  |

A new **`startup` module** owns the warm-up sequence and derives its events from
the configuration, so the warm-up drivers stay configuration-free. It labels
itself `startup` and, on `FolderAdded`, emits `Progress(Begin)`, runs
`project::walk_project` on the blocking pool and emits `ProjectModel` +
`SourceInventory` (which is what kicks off `resolve` and `scan`); it then tracks
`StageDone` for the _enabled_ stages only, emits `Ready` once they are done (or
immediately after the walk when none are enabled), emits `Summary` +
`Progress(End)` after `Ready` (waiting on `Downloads` only when downloads are
enabled), and emits the sources-disabled notice when downloads are off and
artifacts resolved. `resolve`, `scan`, `jars`, `sources`, and `jdk` go back to
plain drivers: each does its job if spawned and knows nothing about the toggles;
`engine` decides what to spawn.

The shell, `document`, `index`, `analysis`, and the `startup` coordinator stay
required: the shell renders every editor-facing notification, `document` serves
the text the analysis needs, `index` answers every module's queries, `analysis`
answers every editor query, and `startup` is the root of the warm-up chain.
`--stdio` is accepted and ignored for editor compatibility.

# Decisions

- **D1 — One boolean flag per module, named `--no-<module>`.** Chosen over a
  repeatable `--disable <module>` and over an allowlist: the set is small and
  fixed, the flags discoverable in `--help`, and each reads as a plain on/off.
- **D2 — The switchable set is the eight rows above, all on by default.** The
  infrastructure participants (`server`, `document`, `index`, `analysis`, and the
  `startup` coordinator) are deliberately not switchable: each is a dependency of
  a switchable module or of the shell, so disabling one would leave the server
  unable to answer.
- **D3 — Implications are fixed closure.** `--no-dependencies` implies
  `--no-jars` and `--no-downloads`, because neither has an `Artifacts` model to
  work from. `--no-diagnostics` implies `--no-quickfix`, because the quick-fix
  subsystem reads the diagnostics cache (`DiagnosticsForDocument`) and would only
  ever produce empty fixes. `--no-semantic-diagnostics` also implies
  `--no-quickfix`, because quick fixes are generated _only_ from the semantic
  pass's unresolved-symbol diagnostics — parse errors carry no fix data
  (`collect_errors` sets `data: None`), so with the semantic pass off quickfix can
  act on nothing. `--no-quickfix` (however reached) stops the shell advertising
  `codeActionProvider`, so the client never asks for actions nobody serves. This
  keeps every combination coherent instead of silently returning empty results.
- **D4 — A new `startup` module owns warm-up orchestration, so no warm-up driver
  is configuration-aware.** Today the `project` driver both walks the model and
  coordinates `Ready`/`Summary`; that coordination moves to `startup`, which
  receives the configuration and shapes its event stream from it. The five
  drivers (`resolve`, `scan`, `jars`, `sources`, `jdk`) take no configuration
  parameter — they run because `engine` spawned them, and report the same
  `StageDone`/`Artifacts` events they always did. The sources-disabled notice
  moves from `resolve` to `startup` for the same reason. `project.rs` keeps the
  project model and the walk helpers (`walk_project`, `collect_java_files`) and
  loses its driver, its subscription, and the `project` label; it is no longer a
  hub participant.
- **D5 — The existing environment variables keep working, seeded into the
  configuration.** A `Modules::from_env()` maps `JAVA_LSP_OFFLINE` to downloads
  off (equivalent in effect: `index_sources` early-returns on `offline()` before
  the local-reuse path, so offline reuses no local sources either) and
  `JAVA_LSP_SEMANTIC_DIAGNOSTICS` (`0`/`false`) to semantic diagnostics off; CLI
  flags override the env-derived defaults, and `engine::start(client)` is
  `engine::start_with(client, Modules::from_env())`. This preserves the
  documented behaviour _and_ keeps `tests/harness.rs`, which sets
  `JAVA_LSP_OFFLINE=1` in many tests and calls `engine::start` directly, working
  without a command line. `JAVA_LSP_JDK` remains the JDK-home override, distinct
  from `--no-jdk`.
- **D6 — The runtime semantic-diagnostics toggle flows through the
  configuration into `diagnostics`.** `diagnostics::spawn` takes the boolean
  instead of reading `semantic_diagnostics_enabled()` itself, so the flag and the
  env var resolve to one value. `analysis.rs` keeps its env-based default: its
  `document_diagnostics` path is used only by the core's own tests (the shell
  renders the diagnostics subsystem's output), so it is a test hook, not a
  runtime consumer, and is left unchanged.
- **D7 — Composition is decided in one place.** `engine::start_with(client,
modules)` builds the hub and spawns only the enabled participants, in the
  current shell-first, drivers-last order, so `engine.rs` remains the single
  place the composition is expressed. `engine::start` stays a thin wrapper.
- **D8 — Unknown arguments fail open.** Parsing uses `try_parse`: `--help` and
  `--version` are handled as usual, and any other unrecognised argument logs a
  warning and the server starts with the defaults. An editor that passes a flag
  the server does not know (as they do today) must not be able to prevent it
  from starting.
- **D9 — The configuration surface is deliberately small.** `Modules` is
  consumed whole only by `cli`/`main` (parse), `engine` (composition), and
  `startup` (orchestration). Two behaviour toggles are passed as plain booleans
  to their owning modules — `semantic_diagnostics` to `diagnostics` and
  `quickfix` to `server` (for the advertised capability) — rather than routed
  through `startup`, because they are per-module behaviour, not warm-up.
  `index`, `document`, `project`, `analysis`, `resolve`, `scan`, `jars`,
  `sources`, `jdk`, and `hub` see no configuration at all.
- **D10 — Out of scope.** `src/bin/java-lsp-bench.rs` keeps its own hand-rolled
  `parse_args`; the Zed extension already forwards `arguments` from settings and
  needs no change; no socket/TCP transport or config file is added (stdio only);
  `JAVA_LSP_JDK`'s path semantics and the other value env vars
  (`JAVA_LSP_MAVEN_CENTRAL_URL`, `JAVA_LSP_SOURCES_CACHE`) are unchanged.
- **D11 — The dependency is `clap` with only the `derive` feature.** Version 4
  (4.6.x is available offline in the cargo cache), which needs Rust 1.74; the
  toolchain is 1.97 and the crate is edition 2021. clap's `env` feature is not
  used: the env fallback lives in `Modules::from_env()` (D5), so the same
  defaults apply with or without a command line.

# Acceptance criteria

1. `java-lsp --help` lists the eight `--no-*` flags; `--version` prints the
   crate version.
2. With no flags, the server builds the same twelve participants in the same
   order and behaves exactly as today, except that the warm-up coordinator now
   labels itself `startup` (it was `project`).
3. Each flag removes exactly its module and nothing else: `--no-jdk` runs no JDK
   indexer; `--no-downloads` does no network work; `--no-workspace-index` leaves
   the workspace source scan out; `--no-jars` leaves the jar indexer out;
   `--no-diagnostics` publishes no diagnostics and serves no
   `DiagnosticsForDocument`; `--no-semantic-diagnostics` keeps syntax
   diagnostics but drops the unresolved-symbol ones.
4. `--no-dependencies` runs no dependency resolver, jar indexer, or downloader.
5. `codeActionProvider` is advertised exactly when quickfix runs: it is absent
   under `--no-quickfix`, `--no-diagnostics`, and `--no-semantic-diagnostics`,
   and a `codeAction` request is not served by a module that is off.
6. `Ready` and the summary/progress-`End` always fire for the enabled set — no
   combination hangs warm-up, including the one that disables every core stage.
7. `JAVA_LSP_OFFLINE=1` and `JAVA_LSP_SEMANTIC_DIAGNOSTICS=0` still take effect
   with no flag given, and an explicit flag overrides the env value.
8. `--stdio` is accepted and ignored; an unknown argument warns and the server
   still starts.
9. No warm-up driver takes a configuration parameter: `resolve::spawn`,
   `scan::spawn`, `jars::spawn`, `sources::spawn`, and `jdk::spawn` keep the
   `&HubClient`-only signature, and `project.rs` is no longer a hub participant.
   Only `cli`/`main`, `engine`, `startup`, `diagnostics`, and `server` see the
   toggles.
10. `cargo build --all-targets` is warning-free and `cargo test` passes (library,
    harness, and the `example/` feature tests) with the existing tests unchanged.

# Docs

The launch surface, the engine's composition, and the warm-up ownership story
change, so the implementation plan must update:

- `README.md` — the **Configuration** table (note that `JAVA_LSP_OFFLINE` and
  `JAVA_LSP_SEMANTIC_DIAGNOSTICS` now have `--no-downloads` /
  `--no-semantic-diagnostics` flag equivalents, and that `--no-jdk` replaces the
  unusable-JDK trick) and a short **Command-line flags** subsection with
  `--help`.
- `docs/architecture.md` — the **Layout** list (add `startup.rs`; rewrite the
  `project.rs` one-liner to the model + walk helpers, no driver; correct `main.rs`
  and `engine.rs`), the **components and data flow** diagram (the coordinator is
  `startup`, and the warm-up drivers hang off it), and the **"Every subsystem is
  a module or driver on one hub"** decision (the participant set is a launch-time
  choice; the coordinator owns the configuration-aware warm-up).
- `docs/requirements.md` — the **Technology choices** / deployment paragraph, to
  mention that subsystems can be switched off from the command line.

# Implementation plan

## Approach

A new `src/cli.rs` holds two things: `Cli`, the `clap`-derived argument struct
(one `--no-<module>` bool per switchable module, plus an ignored `--stdio`), and
`Modules`, the resolved on/off decisions the engine actually consumes.
`Modules::from_env()` seeds the defaults from the existing environment variables
(`JAVA_LSP_OFFLINE` → downloads off, `JAVA_LSP_SEMANTIC_DIAGNOSTICS` → semantic
off), and `Cli::into_modules()` starts from those defaults and applies the flags
on top, then closes the implications (`--no-dependencies` ⇒ jars and downloads
off; `--no-diagnostics` or `--no-semantic-diagnostics` ⇒ quickfix off). This is
why the env fallback lives here rather than in clap: `tests/harness.rs` calls
`engine::start` directly and sets `JAVA_LSP_OFFLINE=1`, so the defaults must be
env-aware with or without a command line.

A new `src/startup.rs` becomes the warm-up coordinator: it owns the walk kickoff
and the `Ready`/`Summary`/progress events, tracking only the enabled stages, and
emits the sources-disabled notice. It replaces the driver currently in
`project.rs`, keeping the configuration in one coordinator instead of spreading
it across the five drivers. `project.rs` keeps the project model and the
`walk_project`/`collect_java_files` helpers (crate-visible, called by `startup`)
and stops being a hub participant.

`engine::start_with(client, modules)` builds the hub, constructs the shell with
the quick-fix toggle (so `initialize` can omit `codeActionProvider`), and spawns
only the enabled participants: `index`, `document`, and `analysis` always;
`diagnostics` (with the semantic flag) and `quickfix` conditionally; `startup`
always; and `resolve`, `scan`, `jars`, `sources`, `jdk` conditionally. The five
drivers keep their `&HubClient`-only `spawn`. `engine::start(client)` becomes
`start_with(client, Modules::from_env())`, so the harness and
`LspService::new(engine::start)` keep working unchanged. `main.rs` parses with
`Cli::try_parse()`: help/version print and exit, any other parse error logs a
warning and falls back to the defaults (fail-open, D8).

## Steps

- [x] `Cargo.toml` — add `clap = { version = "4", features = ["derive"] }`.
- [x] `src/cli.rs` (new) + `src/lib.rs` — define `Cli` (the eight `--no-*` bools
      and the ignored `--stdio`, with `version`/`about`) and `Modules` (the eight
      resolved toggles); `Modules::all()`, `Modules::from_env()`, and
      `Cli::into_modules()` applying the flags and the D3 implications. Add
      `pub mod cli;`. Unit-test: no flags enable everything; each flag disables
      its module; the implication closures (dependencies ⇒ jars + downloads;
      diagnostics and semantic-diagnostics ⇒ quickfix); the `JAVA_LSP_OFFLINE` /
      `JAVA_LSP_SEMANTIC_DIAGNOSTICS` defaults (under `crate::jdk::env_lock()`);
      `Cli::try_parse_from` accepts `--no-jdk` and `--stdio` and rejects an
      unknown flag. Covers criteria 3, 4, 5, 7, 8.
- [x] `src/startup.rs` (new) + `src/lib.rs` — the warm-up driver:
      `spawn(hub, modules: &Modules)` labels `startup`, subscribes, and on
      `FolderAdded` emits `Progress(Begin)`, runs `project::walk_project` on the
      blocking pool, and emits `ProjectModel` + `SourceInventory`; tracks
      `StageDone` for the enabled stages, emits `Ready` (immediately after the
      walk when no core stage is enabled), then `Summary` + `Progress(End)`
      (waiting on `Downloads` only when downloads are enabled); emits the
      sources-disabled notice on `Artifacts` when downloads are off and the list
      is non-empty. Add a pure `warm_up_complete(...)` helper with unit tests
      (waits on exactly the enabled core stages; true at once when none are
      enabled) and a notice unit test. Add `pub mod startup;`. Covers criteria
      5, 6.
- [x] `src/project.rs` — remove `spawn` and the `Ready`/`Summary`/`Progress`
      coordination and the `offline_notice`; keep `ProjectModel` (+ pom parsing)
      and `walk_project`/`collect_java_files` as `pub(crate)`; move the
      coordination/notice tests to `startup.rs`. Covers criterion 9.
- [x] `src/resolve.rs` — drop `offline_notice` and its unit test (moved to
      `startup`); `spawn(hub)` keeps emitting `Artifacts` with no configuration.
      Covers criteria 3, 9.
- [x] `src/engine.rs` — add `start_with(client, modules)`: always spawn `index`,
      `document`, `analysis`; conditionally `diagnostics` (semantic flag) and
      `quickfix`; always `startup`; conditionally the five drivers. Rewrite
      `start` as `start_with(client, Modules::from_env())`; update the module doc
      to describe the composition as configuration-driven. Covers criteria 2, 3, 4.
- [x] `src/main.rs` — parse `Cli` (help/version exit; other errors warn and use
      `Cli::default()`), turn it into `Modules`, and build the service with
      `LspService::build(move |client| engine::start_with(client, modules))`.
      Covers criteria 1, 8.
- [x] `src/server.rs` — `JavaLanguageServer::new(client, hub, quickfix: bool)`
      stores the toggle and `initialize` includes `code_action_provider` only
      when it is set; update the constructor doc. Covers criterion 5.
- [x] `src/diagnostics.rs` — `spawn(hub, semantic: bool)` sets the module's
      `semantic` field from the argument instead of
      `semantic_diagnostics_enabled()`; keep the env helpers for `cli.rs`.
      Covers criterion 3.
- [x] `tests/harness.rs` — add `service_with(modules)`; test that `initialize`
      omits `codeActionProvider` when quickfix is off and includes it otherwise.
      Covers criterion 5.
- [x] `tests/stdio_smoke.rs` — let `Server::start` take extra args; add a smoke
      test that launches the binary with `--no-jdk --no-dependencies` and still
      completes initialize → didOpen → hover. Covers criteria 6, 8.
- [x] `README.md` — note the `--no-downloads` / `--no-semantic-diagnostics` /
      `--no-jdk` flag equivalents in the **Configuration** table and add a short
      **Command-line flags** subsection referencing `--help`.
- [x] `docs/architecture.md` — add `startup.rs` to the **Layout** and rewrite the
      `project.rs` one-liner (model + walk helpers, no driver); update the
      components/data-flow diagram (the coordinator is `startup`); update the
      **"Every subsystem is a module or driver on one hub"** decision (the
      participant set is a launch-time choice; the coordinator owns the
      configuration-aware warm-up).
- [x] `docs/requirements.md` — mention in the deployment/technology paragraph
      that subsystems can be switched off from the command line.
- [x] Verify with `cargo build --all-targets` (warning-free), `cargo fmt --all
-- --check`, and `cargo test` (library, harness, `example/` features); run
      the built binary once with `--help`, once with `--no-jdk`, and once with
      every `--no-*` flag to confirm no warm-up hang. Covers criteria 1, 2, 6, 10.

## Progress

- Done. `main.rs` parses the `clap` flags through the new `src/cli.rs` (`Cli`
  and `Modules`, with the env fallback and the implication closures);
  `engine::start_with` spawns only the enabled participants; the new
  `src/startup.rs` owns the configuration-aware warm-up and the sources-disabled
  notice; `project.rs` keeps the model and walk helpers and is no longer a hub
  participant; `resolve.rs`, `scan.rs`, `jars.rs`, `sources.rs`, and `jdk.rs`
  take no configuration; `diagnostics` takes the semantic flag and `server` the
  quick-fix flag. `src/lib.rs` also re-exports `engine::start` as `java_lsp::start`
  (a pre-existing harness reference). Verified with `cargo build --all-targets`,
  `cargo fmt --all -- --check`, and `cargo test` (310 library, 7 bench, 1
  `example/`, 30 harness, 2 stdio).
