---
name: fawi-propose
description: Turn a new feature or change request into a backlog item — inspect the codebase, settle open questions with the user, then write a type - ChangeRequest with kind - feature under docs/dev/backlog.
---

# Proposing a change

A change request is the entry point of the change-driven workflow. Before any
code is written, capture the request as a `type: ChangeRequest` in the backlog.
Proposing is an interactive conversation: inspect the codebase, surface the
points that need clarification, agree on them with the user, and only then
write the request.

## 1. Understand the request

Restate what is being asked, why, and who wants it. Read the codebase to ground
the request in reality — the crates under `crates/`, the relevant docs under
`docs/` (start with `architecture.md`, `features.md`, and the API/CLI
references), and the existing tests. Do not invent files, crates, or commands.

## 2. Check feasibility

Read the code to work out whether the change can land and how:

- Where would the change land? Name the crate(s), module(s), and file(s).
- Is there an existing mechanism it can extend, or does it need something new?
- What are the risks, tradeoffs, and out-of-scope concerns?
- Is it blocked by a missing dependency, an external service, or a hard
  constraint?

If the change is clearly infeasible as asked, say so now and stop. Do not write
a request for something that cannot work.

## 3. Discuss open points

Surface every point that needs the user's decision before the request can be
written. For each one, state what the code shows, then ask. Typical points:

- scope — what is in and what is deliberately left out;
- approach — extend an existing mechanism versus build something new;
- tradeoffs and defaults (behaviour, naming, performance, dependency choices);
- acceptance criteria that are ambiguous in the request.

Ask one question at a time, or group them when they are tightly related. Repeat
until every point is resolved. Record each decision as it is made — a short
sentence with the choice and the reason.

## 4. Write the change request

Only when all open points are resolved, create
`docs/dev/backlog/<slug>.md` with this front matter:

    ---
    type: ChangeRequest
    kind: feature
    title: <Title>
    description: <one-line summary>
    state: proposed
    priority: <low|medium|high>
    tags: [dev, <topic>]
    owner: <actor>
    ---

A change request uses a single `state` field (no `status`) to capture its whole
lifecycle: `proposed` → `planned` → `in-progress` → `done`. `fawi-check` may
move it to `rejected` or `superseded`. The `kind` field marks the change type as
`feature`, distinguishing it from `bug` (`fawi-fix`), `improvement`
(`fawi-improve`), and `refactor` (`fawi-refactor`).

## 5. Fill in the sections

- `# Problem` — the motivation or gap.
- `# Proposal` — the change in one or two paragraphs.
- `# Decisions` — the key decisions agreed in step 3, each with its reason.
  There is no separate feasibility section; feasibility findings fold into the
  decisions.
- `# Acceptance criteria` — testable, concrete outcomes.

If the change alters documented behaviour — features, architecture, the REST
API, or the CLI — note which docs under `docs/` it will touch and what in each
will change, so the implementation plan can turn that into concrete doc-update
steps.

## Next steps

`fawi-plan` appends the implementation plan to this request.
