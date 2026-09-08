---
name: fawi-fix
description: Turn a bug report into a backlog item — inspect the codebase, reproduce or isolate the fault, settle open questions with the user, then write a type - ChangeRequest with kind - bug under docs/dev/backlog.
---

# Fixing a bug

A bug is a change request too. Capture the defect as a `type: ChangeRequest`
with `kind: bug` in the backlog before any code is written. Fixing is an
interactive conversation: inspect the codebase, reproduce the bug, surface the
points that need clarification, agree on them with the user, and only then
write the request.

## 1. Understand the bug

Restate the defect: what happens, what should happen, and who reported it. Read
the codebase to ground the report in reality — the crates under `crates/`, the
relevant docs under `docs/` (start with `architecture.md` and `features.md`),
and the existing tests. Do not invent files, crates, or commands.

## 2. Reproduce and isolate

Work out whether the report is really a bug and where it lives:

- Capture the steps to reproduce, the observed behaviour, and the expected
  behaviour.
- Find the likely root cause in the code: name the crate(s), module(s), and
  file(s).
- Classify it — a regression, an edge case, or a design-level defect — and note
  whether an existing mechanism should have caught it.

If the report is not actually a bug (it is intended behaviour, a feature request
in disguise, or a duplicate of an existing request), say so now and stop, or
redirect to `fawi-propose` when it is a feature, or to `fawi-improve` when it is
an enhancement of intended behaviour.

## 3. Discuss open points

Surface every point that needs the user's decision before the request can be
written. For each one, state what the code shows, then ask. Typical points:

- scope — what is in and what is deliberately left out;
- the fix approach — patch the symptom versus address the root cause;
- expected-versus-actual behaviour where the report is ambiguous;
- tradeoffs and defaults (behaviour, naming, performance, dependency choices);
- acceptance criteria that are ambiguous in the report.

Ask one question at a time, or group them when they are tightly related. Repeat
until every point is resolved. Record each decision as it is made — a short
sentence with the choice and the reason.

## 4. Write the change request

Only when all open points are resolved, create
`docs/dev/backlog/<slug>.md` with this front matter:

    ---
    type: ChangeRequest
    kind: bug
    title: <Title>
    description: <one-line summary>
    state: proposed
    priority: <low|medium|high>
    tags: [dev, bug, <topic>]
    owner: <actor>
    ---

A change request uses a single `state` field (no `status`) to capture its whole
lifecycle: `proposed` → `planned` → `in-progress` → `done`. `fawi-check` may
move it to `rejected` or `superseded`. The `kind` field marks the change type as
`bug`, distinguishing it from `feature` (`fawi-propose`), `improvement`
(`fawi-improve`), and `refactor` (`fawi-refactor`).

## 5. Fill in the sections

- `# Problem` — the bug and its impact, in one or two paragraphs.
- `# Reproduction` — the steps to reproduce, the observed behaviour, and the
  expected behaviour.
- `# Proposal` — the fix in one or two paragraphs.
- `# Decisions` — the key decisions agreed in step 3, each with its reason.
  There is no separate feasibility section; feasibility findings fold into the
  decisions.
- `# Acceptance criteria` — testable, concrete outcomes that show the bug is
  fixed (the reproduction no longer occurs and the expected behaviour holds).

If the bug stems from or exposes a doc that is now wrong, note which docs under
`docs/` the fix should update and what in each is wrong, so the implementation
plan can include correcting them.

## Next steps

`fawi-plan` appends the implementation plan to this request.
