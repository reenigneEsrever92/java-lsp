---
type: ChangeRequest
kind: bug
title: Deep analysis recursion overflows the engine runtime's default thread stack on large workspaces
description: On large Maven workspaces a deep, finite recursive analysis chain exceeds tokio's default 2 MiB worker stack and aborts the server; build the runtime with an explicit larger thread stack so no `RUST_MIN_STACK` is needed.
state: done
priority: high
tags: [dev, bug, runtime, robustness]
owner: felix
verified:
  by: cargo build (10.21s) + cargo test --all-targets --no-fail-fast (283 lib,
    7 bench, 28 harness, 1 stdio passed; the only 5 failures are the sandbox
    loopback-socket binds already documented in driver-message-bus — 4 in
    src/sources.rs, 1 in tests/harness.rs) + cargo clippy --lib (14 warnings,
    baseline)
  at: 2026-09-29T09:29:54Z
---

# Problem

On a large Maven workspace the server aborts after running for a while with:

```
thread 'tokio-rt-worker' has overflowed its stack
fatal runtime error: stack overflow, aborting
```

`main` builds the default multi-thread runtime with `tokio::runtime::Runtime::new()`
(`src/main.rs:16`). That runtime sets no thread stack size, so its worker
threads — and its blocking pool — use the standard library default of 2 MiB, a
quarter of the 8 MiB the process's main thread gets. Any deep-enough call chain
therefore overflows on a runtime thread even where the same work would fit on
the main thread, and the process aborts: the stack guard is fatal, not a
catchable panic, so nothing in the server can recover.

This is confirmed to be **deep but finite recursion**, not an unbounded loop or
a message cycle. Exporting `RUST_MIN_STACK=268435456` — which raises the
standard library default and therefore the size of tokio's threads, because the
runtime never sets one explicitly — makes the abort stop. The subsystem bus
cannot be the cause: it is a DAG (no driver reacts to a message it, or a
downstream driver, emits), and a message cycle would spin CPU or grow memory,
never the call stack.

The trigger is not simple source nesting. The workspace's own 9,202 sources top
out at an abstract-syntax-tree depth of 117, far too shallow to exhaust 2 MiB,
and there is no recursion over a shrinking collection and no unguarded
type-hierarchy walk (`members_with_overloads`/`is_subtype_of`/`lookup` are all
cycle-guarded). The exact recursive frame was not captured from the abort, so
the walk that reaches this depth is not yet identified.

# Reproduction

1. Open the `oemportal-service` Maven workspace (9,202 source files plus
   ~267,000 dependency-source files) in Zed with this `java-lsp` build.
2. Let warm-up run, then open or edit files.

- **Observed:** after a while the server aborts with the stack-overflow message
  above. The last log line before the abort is an innocuous `didOpen` of a small
  saga step (`HandleProcedureInitiationResultService.java`) — that file is not
  itself the cause.
- **Expected:** the server keeps running and answering.
- **Confirmed workaround:** with `RUST_MIN_STACK=268435456` exported the abort no
  longer occurs.

The abort is a stack overflow, so it cannot be reproduced inside the test
suite's own process (it is fatal and needs a workspace of this size);
verification is this manual reproduction plus the existing suite staying green.

# Proposal

Build the runtime with an explicit thread stack size in `src/main.rs` rather
than `Runtime::new()`:

```rust
let runtime = tokio::runtime::Builder::new_multi_thread()
    .enable_all()
    .thread_stack_size(N)
    .build()
    .expect("failed to start tokio runtime");
```

`Builder::thread_stack_size` applies to the worker threads **and** the blocking
pool: tokio copies the configured size into each blocking thread's builder
(`tokio::runtime::blocking::pool` stores `builder.thread_stack_size` and calls
`.stack_size(..)` when spawning), so one setting covers both the inline
`store_tree` parse/extract on the dispatcher worker (`src/analysis.rs:201`) and
the `spawn_blocking` warm-up scan and diagnostics sweep. This makes the binary
self-sufficient, without relying on `RUST_MIN_STACK` — the editor extension does
not set it, so the workaround only helped the shell that exported it.

# Decisions

- **D1 — Fix with an explicit `thread_stack_size`; do not first chase the
  offending walk.** Agreed with the maintainer. It removes the abort at the
  observed depth with a small, low-risk change verifiable on the reporting
  workspace, and leaves an iterative rewrite of the deep walk as a separate,
  optional follow-up.
- **D2 — One setting must cover both workers and the blocking pool.** Verified
  against the pinned tokio: `BlockingPool` takes `builder.thread_stack_size` and
  passes it to every blocking thread, so a single `thread_stack_size` covers the
  dispatcher worker (which parses, extracts entries, and builds the type model
  inline on `didOpen`) and the `spawn_blocking` warm-up and diagnostics work.
- **D3 — The binary must not depend on `RUST_MIN_STACK`.** Relying on the
  environment variable leaves the 2 MiB default in every editor session that does
  not export it; the fix has to be in-process.
- **D4 — Size from the verified value, then minimise.** Start at the
  verified-good 256 MiB and halve it to the smallest power of two that still
  keeps the reported workspace from aborting, recording the chosen value and why
  in the code comment. The exact depth is unknown, so this bounds the reservation
  (each worker and each live blocking thread reserves the stack) while
  guaranteeing the reported case passes.
- **D5 — Out of scope.** Identifying and making the deep recursion iterative, and
  the separate latent defect that `collect_java_files` (`src/index.rs:948`) walks
  directories with no depth cap and no symlink-cycle guard, unlike its sibling
  `find_poms_recursive` in `src/project.rs` (capped at 32). That walk is a
  different input — the filesystem, not a query — and is tracked separately if
  pursued.

# Acceptance criteria

- `src/main.rs` builds the runtime through `tokio::runtime::Builder` with
  `.thread_stack_size(..)`, and running `java-lsp` with `RUST_MIN_STACK` unset
  still uses the configured size.
- The reported reproduction (open the large Maven workspace, let warm-up run,
  open/edit files) no longer aborts: warm-up logs its summary line and
  hover/completion/diagnostics answer.
- `cargo test --all-targets` stays green.
- The chosen stack size and its rationale are stated in a short comment at the
  runtime construction.

# Docs to update

- `docs/architecture.md` — a line in "Decisions and constraints" that the
  engine's runtime is built with an explicit, larger worker stack, because
  analysis is deeply recursive and the 2 MiB default is not enough on large
  workspaces.
- `docs/dev/backlog/index.md` — this request's row.
- `docs/dev/changelog.md` — an entry at implementation, quoting the reported
  workspace and the chosen stack size.

# Implementation plan

## Approach

Replace `tokio::runtime::Runtime::new()` in `src/main.rs` with an explicit
`Builder::new_multi_thread().enable_all().thread_stack_size(N).build()`, so the
worker threads and the blocking pool both get a stack large enough for the
deepest analysis recursion.

## Steps

- [x] `src/main.rs`: build the runtime with `Builder::thread_stack_size`; add a
      documented `RUNTIME_STACK_SIZE` constant. (D1/D2/D3)
- [x] `cargo build` green, `cargo test --all-targets` green apart from the known
      sandbox loopback failures, `cargo clippy --lib` unchanged. (AC)
- [x] `docs/architecture.md`: the runtime-stack decision bullet. (Docs)
- [x] `docs/dev/backlog/index.md` row → `done`, and a `docs/dev/changelog.md`
      entry. (Docs)

## Implementation notes

- The size is left at the 256 MiB verified to fix the abort on the reporting
  workspace (D4); minimising it needs that workspace, so it stays at the verified
  value, with a comment saying it can be lowered once the deepest walk is
  identified and made iterative.
- The size is a constant, not a knob: the runtime is the only place analysis
  threads are spawned, so one setting covers every path.
- Implementation is **uncommitted**, left for review; no commit or branch was
  created, so no commit is linked in the body.
