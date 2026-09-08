---
name: fawi-refactor
description: Turn an intention to redesign or rewrite code into a backlog item — inspect the codebase, settle open questions with the user, then write a type - ChangeRequest with kind - refactor under docs/dev/backlog.
---

# Refactoring a change

A refactor is a change request that improves the design or quality of an
existing piece of code or concept without changing its intended behaviour (or
deliberately changes it as part of a redesign). Capture the intention as a
`type: ChangeRequest` with `kind: refactor` in the backlog before any code is
written. Refactoring is an interactive conversation: inspect the codebase,
surface the points that need clarification, agree on them with the user, and
only then write the request.

## 1. Understand the intent

Restate what is being redesigned, why, and what "better" means. Read the
codebase to ground the intent in reality — the crates under `crates/`, the
relevant docs under `docs/` (start with `architecture.md`, `features.md`, and
the API/CLI references), and the existing tests. Do not invent files, crates,
or commands.

## 2. Assess the current design

Read the code to work out the shape of the change:

- What is wrong or costly about the current design? Name the crate(s), module(s),
  file(s), and the specific coupling, duplication, or complexity.
- What does the target design look like? Does it extend an existing mechanism or
  replace something outright?
- What is the blast radius — call sites, tests, data, or public APIs that must
  change?
- What stays the same? A refactor should preserve intended behaviour (or state
  explicitly where behaviour is allowed to change).

If the intent is really a new feature rather than a redesign, say so now and
redirect to `fawi-propose`; if it is an enhancement of existing behaviour rather
than a structural redesign, redirect to `fawi-improve`.

## 3. Discuss open points

Surface every point that needs the user's decision before the request can be
written. For each one, state what the code shows, then ask. Typical points:

- scope — what is in and what is deliberately left out;
- approach — incremental improvement versus a rewrite;
- the migration path and how behaviour is preserved (or what is allowed to
  change);
- tradeoffs and defaults (naming, public API, performance, dependency choices);
- acceptance criteria that are ambiguous in the intent.

Ask one question at a time, or group them when they are tightly related. Repeat
until every point is resolved. Record each decision as it is made — a short
sentence with the choice and the reason.

## 4. Write the change request

Only when all open points are resolved, create
`docs/dev/backlog/<slug>.md` with this front matter:

    ---
    type: ChangeRequest
    kind: refactor
    title: <Title>
    description: <one-line summary>
    state: proposed
    priority: <low|medium|high>
    tags: [dev, refactor, <topic>]
    owner: <actor>
    ---

A change request uses a single `state` field (no `status`) to capture its whole
lifecycle: `proposed` → `planned` → `in-progress` → `done`. `fawi-check` may
move it to `rejected` or `superseded`. The `kind` field marks the change type as
`refactor`, distinguishing it from `feature` (`fawi-propose`), `improvement`
(`fawi-improve`), and `bug` (`fawi-fix`).

## 5. Fill in the sections

- `# Problem` — the design or quality gap and its cost, in one or two paragraphs.
- `# Proposal` — the target design and how it improves the code, in one or two
  paragraphs.
- `# Decisions` — the key decisions agreed in step 3, each with its reason,
  including what stays the same, what changes, and how the migration is carried
  out. There is no separate feasibility section; feasibility findings fold into
  the decisions.
- `# Acceptance criteria` — testable, concrete outcomes. For a refactor these
  usually assert that intended behaviour is preserved (existing tests still pass)
  and that the structural improvement has landed (e.g. the old path is gone, the
  new module is in place).

If the refactor changes the crate layout, public API, or data flow, note which
docs under `docs/` (usually `architecture.md` and the API/CLI references) it
will touch and what in each must be rewritten, so the implementation plan can
include updating them.

## Next steps

`fawi-plan` appends the implementation plan to this request.
