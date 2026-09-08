---
type: ChangeRequest
kind: feature
title: Performance benchmark harness
description: A generated fixture project and a timing harness keeping the open-to-responsive requirement measurable.
state: done
priority: medium
tags: [dev, performance, benchmark]
owner: felix
verified:
  by: bench run (cargo run --release --bin java-lsp-bench -- --files 500
    --methods-per-class 10, plus --files 5 and --json spot checks) + cargo test
    (58 passed)
  at: 2026-09-08T20:18:55Z
---

# Problem

"Project open → responsive" is a critical, standing requirement (R6), but no
numeric targets were agreed. Without an instrument, regressions would only be
noticed by feel. Any project size must be exercisable, since the requirement is
not calibrated to a fixed size.

# Proposal

Build a generator for a configurable-size fixture Java project and a small
harness that starts the server against it and measures: time from start + file
open to the first successful response per feature, request latency during
index warm-up, and memory. Run the harness per milestone and record the
baselines.

# Decisions

- No fixed numeric targets; instead, measure, document baselines, and keep
  regressions visible — the agreed way to honor a general performance concern.
- The fixture is generated, so any workspace size can be exercised.

# Acceptance criteria

- A single command produces a timing/memory report for a chosen fixture size.
- A baseline report exists for each shipped milestone and is recorded in the
  changelog entry.
- Syntax features demonstrably respond while indexing is still running.

# Implementation plan

## Approach

A second binary target, `src/bin/java-lsp-bench.rs`, in the existing single
crate. It generates a fixture workspace in a temp dir, spawns the **real**
`java-lsp` binary and drives it over raw stdio JSON-RPC (the exact transport an
editor uses, same framing pattern as `tests/stdio_smoke.rs`), measures, prints
one report, and tears everything down. Plain cargo; **std only** (no tokio in
the bench — the client side is naturally sequential, so one main driver thread
plus one stderr-reader thread is enough); no new dependencies.

Key decisions and tradeoffs:

- **Separate bin, not an example or `[[bench]]` target.** A `[[bench]]` target
  would get `CARGO_BIN_EXE_java-lsp` for free, but only under
  `cargo test --bench`, which conflicts with the agreed single-command shape
  (`cargo run --release --bin java-lsp-bench -- --files 500`). Examples have
  the same resolution problem. The bench resolves the server binary itself:
  `--server <path>` flag, then `JAVA_LSP_BIN` env, then
  `<manifest>/target/{release,debug}/java-lsp`; if none exists and it is
  running under cargo (`CARGO` env set), it spawns `cargo build
  [--release] --bin java-lsp` first so the single command stays true, and
  otherwise exits with build instructions.
- **Real stdio binary, not in-process `LspService`.** In-process driving
  (like `tests/harness.rs`) would exclude process startup, transport framing,
  and tower-lsp dispatch — precisely the latency an editor experiences for
  "project open → responsive". End-to-end numbers are the point (R6).
- **Warm-up detection via the server's own readiness log line.**
  `scan_workspace` (`src/index.rs`) already emits
  `tracing::info!(... "workspace index warm-up complete", files, elapsed_ms)`
  on stderr at info level, which is the default `RUST_LOG=java_lsp=info`
  filter — the bench spawns the server with stderr piped and a reader thread
  watches for that line. Alternatives rejected: polling `workspace/symbol`
  (not implemented — no workspace-symbol capability yet); a completion probe
  as the *primary* signal (a completion only proves one symbol got indexed,
  not that the scan finished — nondeterministic for large fixtures). The log
  line is emitted by the scan itself, deterministic, and additionally yields
  the scan's own `files`/`elapsed_ms` stats for the report. A completion
  probe is still run *after* warm-up as a sanity check that index data
  actually reaches a feature.
- **Latency during warm-up**: right after `initialized` (scan starts) the
  bench `didOpen`s one fixture file, times the first response per feature
  (documentSymbol, foldingRange, semanticTokens/full, completion, hover),
  then keeps issuing a lightweight request (`hover` on the open file) on a
  fixed tick (default 25 ms) until the readiness line arrives, recording every
  RTT. Reporting, not asserting thresholds — no numeric targets were agreed —
  but the harness asserts every request got a successful response, which is
  exactly AC3 (syntax features respond while indexing runs). Large `--files`
  values make the warm-up window long enough to accumulate samples.
- **Memory via `/proc/<pid>/status` `VmHWM`** of the server child, read with
  plain `std::fs` after warm-up and again before shutdown. `VmHWM` is the
  kernel-maintained peak RSS — the same metric `/usr/bin/time -v` reports,
  but without an external tool, process wrapping, or parsing its output, and
  it stays available for the whole child lifetime (no final-read race).
  Linux-specific: on other targets the report says `memory: n/a` rather than
  failing. `/usr/bin/time -v` is kept as a documented manual fallback.
- **No criterion.** The harness measures end-to-end wall-clock medians/max of
  a handful of requests per run; a statistical benchmarking framework adds a
  heavy dependency and produces noise-free microbenchmarks that don't model the
  requirement (one cold start per real open, not hot loops). Rejected.
- **Self-contained JSON-RPC client.** The minimal framing client is
  duplicated from `tests/stdio_smoke.rs` into the bench (tests can't import a
  bin target; factoring a shared client module into the lib would touch the
  existing test for no behavioral gain). Revisit if a third driver appears.
- **Fixture shape**: `--files N` (default 200), `--methods-per-class M`
  (default 5), `--fields-per-class F` (default 3). Each file holds one
  uniquely named class (`BenchClass00042`) with M multi-line methods and F
  fields, so document symbols, folding ranges, semantic tokens, and index
  entries all have material. Written to
  `std::env::temp_dir()/java-lsp-bench-<pid>-<millis>/`; deleted at the end
  unless `--keep`.
- **Report**: human-readable table on stdout (per-feature first response from
  `didOpen` and from process start; warm-up duration, sample count, max/mean
  RTT during warm-up; post-warm-up RTT; peak RSS; scan stats from the log
  line; fixture shape), plus `--json` for machine-readable capture when
  recording baselines.

## Steps

- [x] Add `src/bin/java-lsp-bench.rs` with CLI parsing (`--files`,
      `--methods-per-class`, `--fields-per-class`, `--server`, `--json`,
      `--keep`) and the fixture generator (unique class names, multi-line
      methods/fields, temp-dir root, cleanup unless `--keep`), plus unit tests
      for generator output shape (file count, names) in the same file.
      (AC1 — configurable fixture size from a single command.)
- [x] Implement server-binary resolution (`--server` > `JAVA_LSP_BIN` >
      `target/{release,debug}/java-lsp`, with auto `cargo build --bin
      java-lsp` under cargo and a clear error otherwise) and the stdio JSON-RPC
      client (framing, request/response correlation, notification skipping —
      same pattern as `tests/stdio_smoke.rs`, including no-`params` for
      `shutdown`/`exit`), spawning the server with `RUST_LOG=java_lsp=info`
      and piped stderr read by a watcher thread that captures the
      "workspace index warm-up complete" line (and its `files`/`elapsed_ms`).
      (AC1 — the harness starts the real server.)
- [x] Implement the measurement run: `initialize` with the fixture dir as
      `rootUri` → `initialized` → `didOpen` of one fixture file (drain its
      `publishDiagnostics`) → time the first successful response for
      `textDocument/documentSymbol`, `textDocument/foldingRange`,
      `textDocument/semanticTokens/full`, `textDocument/completion`, and
      `textDocument/hover`, from both the `didOpen` send and process start →
      then sample `hover` RTT on a fixed tick until the readiness line,
      recording every sample and asserting all responses succeeded →
      after warm-up, one completion probe whose prefix matches a
      fixture-only class name and assert the symbol is offered → read
      `VmHWM`/`VmRSS` from `/proc/<server pid>/status` → `shutdown`/`exit`
      and reap. (AC3 — all requests issued before readiness must have
      returned successfully; AC1 — timing and memory collected.)
- [x] Implement the report printer (table on stdout, `--json` variant) with:
      fixture shape, per-feature first-response times, warm-up duration and
      during-warm-up RTT count/max/mean, post-warm-up RTT, peak RSS, and the
      scan's own `files`/`elapsed_ms` from the log line. Verify end-to-end
      with `cargo run --release --bin java-lsp-bench -- --files 500
      --methods-per-class 10` and spot-check a small run (`--files 5`) and a
      `--json` run; `cargo test` still passes. (AC1 — a single command
      produces the timing/memory report for a chosen fixture size.)
- [x] Document the harness in `docs/architecture.md` under **"Verifying
      without an editor"**: a third bullet for `src/bin/java-lsp-bench.rs`
      naming the single command, the flags, what the report contains, and
      that warm-up detection uses the server's readiness log line. (Doc step
      for AC1.)
- [x] Record the first baseline in `docs/dev/changelog.md`: run the bench at
      the agreed default shape (`--files 500 --methods-per-class 10`) on this
      machine, and add the report numbers (per-feature first response,
      warm-up samples, peak RSS) plus a one-line machine context to the
      changelog entry for this CR, as the v0.1 baseline future milestones
      compare against. (AC2 — baseline recorded in the changelog entry.)
