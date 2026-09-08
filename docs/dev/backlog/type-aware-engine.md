---
type: ChangeRequest
kind: feature
title: Type-aware semantic engine
description: Decide and implement full type-aware semantics — javac daemon via GraalVM Native Image vs. a pure-Rust type checker.
state: proposed
priority: medium
tags: [dev, engine, architecture]
owner: felix
---

# Problem

Syntax and index features have a ceiling: type-aware hover, accurate
diagnostics, member completions after `x.`, and rename all require real type
resolution (R7). The init conversation deliberately postponed choosing the
approach so that v0.1 ships pure Rust first.

# Proposal

Evaluate the two candidate engines behind the `SemanticEngine` trait and
implement the winner:

1. **javac-based analysis daemon** — a thin Java wrapper around javac's
   supported Compiler Tree API (`com.sun.source.*`), compiled with GraalVM
   Native Image so it ships as a native binary with no JVM at runtime. Known
   risk: Lombok and other annotation processors that mutate javac internals.
2. **pure-Rust type checker** — full front-end in Rust; maximal control, years
   of effort for generics/overload resolution.

Keep the request state `proposed` until the evaluation settles the choice.

# Decisions

- No JVM at runtime — the standing constraint from project init.
- The decision is postponed on purpose; nothing here blocks v0.1.
- A second native binary (daemon) is a possible deployment shape, decided with
  this request.

# Acceptance criteria

- A written evaluation comparing both options on accuracy, effort, memory, and
  the Lombok/annotation-processor risk.
- The chosen engine implemented behind `SemanticEngine` with no changes to the
  LSP layer.
- Type-aware hover and diagnostics work on the `perf-benchmarks` fixture
  project.
