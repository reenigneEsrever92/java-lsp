---
name: fawi-improve
description: Turn an intention to enhance existing functionality or quality into a backlog item — inspect the codebase, settle open questions with the user, then write a type - ChangeRequest with kind - improvement under docs/dev/backlog.
---

# Improving a change

An improvement enhances something that already exists — it makes existing
behaviour faster, clearer, more usable, more reliable, or better documented —
without adding a new capability (a `feature`) and without restructuring the
code (a `refactor`). Capture the intention as a `type: ChangeRequest` with
`kind: improvement` in the backlog before any code is written. Improving is an
interactive conversation: inspect the codebase, surface the points that need
clarification, agree on them with the user, and only then write the request.

## 1. Understand the improvement

Restate what is being improved, why, and what "better" means. Read the codebase
to ground the intent in reality — the crates under `crates/`, the relevant docs
under `docs/` (start with `architecture.md`, `features.md`, and the API/CLI
references), and the existing tests. Do not invent files, crates, or commands.

## 2. Assess the current behaviour

Read the code to work out the shape of the change:

- What aspect of existing behaviour or quality is falling short? Name the
  crate(s), module(s), file(s), and the specific friction — performance, UX,
  developer experience, observability, accessibility, documentation, or
  reliability.
- What does "better" look like? Does the improvement extend an existing
  mechanism or tune it?
- What is the blast radius — call sites, tests, docs, or public APIs that must
  change?
- What must not change? An improvement enhances existing behaviour rather than
  adding a capability or redesigning structure.

If the intent is really a brand-new capability, redirect to `fawi-propose`. If
it is a defect (incorrect behaviour), redirect to `fawi-fix`. If it is a
redesign of the code's structure without changing intended behaviour, redirect
to `fawi-refactor`.

## 3. Discuss open points

Surface every point that needs the user's decision before the request can be
written. For each one, state what the code shows, then ask. Typical points:

- scope — what is in and what is deliberately left out;
- the target — which metric or experience improves, and how much is enough;
- approach — tune an existing mechanism versus add a targeted new one;
- tradeoffs and defaults (behaviour, naming, performance, dependency choices);
- acceptance criteria that are ambiguous in the intent.

Ask one question at a time, or group them when they are tightly related. Repeat
until every point is resolved. Record each decision as it is made — a short
sentence with the choice and the reason.

## 4. Write the change request

Only when all open points are resolved, create
`docs/dev/backlog/<slug>.md` with this front matter:

    ---
    type: ChangeRequest
    kind: improvement
    title: <Title>
    description: <one-line summary>
    state: proposed
    priority: <low|medium|high>
    tags: [dev, improvement, <topic>]
    owner: <actor>
    ---

A change request uses a single `state` field (no `status`) to capture its whole
lifecycle: `proposed` → `planned` → `in-progress` → `done`. `fawi-check` may
move it to `rejected` or `superseded`. The `kind` field marks the change type as
`improvement`, distinguishing it from `feature` (`fawi-propose`), `bug`
(`fawi-fix`), and `refactor` (`fawi-refactor`).

## 5. Fill in the sections

- `# Problem` — the quality gap or friction and its cost, in one or two paragraphs.
- `# Proposal` — the improvement and how it makes things better, in one or two
  paragraphs.
- `# Decisions` — the key decisions agreed in step 3, each with its reason,
  including what improves, what stays the same, and how the improvement is
  measured. There is no separate feasibility section; feasibility findings fold
  into the decisions.
- `# Acceptance criteria` — testable, concrete outcomes. For an improvement these
  usually assert that the existing behaviour still works and that the quality has
  measurably improved (e.g. a latency target is met, a screen reader can read the
  control, or a previously unclear message is now clear).

If the improvement changes documented behaviour — features, architecture, the
REST API, or the CLI — note which docs under `docs/` it will touch and what in
each will change, so the implementation plan can turn that into concrete
doc-update steps.

## Next steps

`fawi-plan` appends the implementation plan to this request.
