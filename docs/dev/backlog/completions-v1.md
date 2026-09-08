---
type: ChangeRequest
kind: feature
title: Completions v1
description: Type-free completions from keywords, locals in scope, and workspace index symbols.
state: proposed
priority: high
tags: [dev, completions]
owner: felix
---

# Problem

Typing support is the most-wanted day-to-day feature, and v0.1 can deliver a
useful version without any type resolution (R4, UC2).

# Proposal

Implement completions as a mix of three sources, ranked in that order: language
keywords; locals and parameters in scope, extracted from the open document's
syntax tree; and workspace symbols from the index (types, and members shown
with their enclosing type as a prefix). Results carry correct `CompletionItem`
kinds and insert text.

# Decisions

- No member-access resolution in v1 — completions after `x.` are out of scope
  until a type-aware engine exists (recorded as deferred in
  `requirements.md`); they are the part that genuinely needs javac or a Rust
  type checker.
- Sources are deliberately labeled "type-free" so user expectations match
  reality.

# Acceptance criteria

- Completion inside a method body offers keywords, in-scope locals/parameters,
  and matching workspace symbols.
- Results arrive quickly on the `perf-benchmarks` fixture project and never
  block on index warm-up (empty or partial while warming, per R6).
- No wrong-membership claims: the server never implies a member belongs to the
  type before the dot.
