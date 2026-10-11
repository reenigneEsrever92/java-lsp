---
name: fawi-plan
description: Append an implementation plan to a backlog change request — the technical approach, files to touch, and an ordered step checklist — and move it to state - planned. When a change is too large for one plan, decompose it into standalone sub-task change requests and turn the request into an umbrella that tracks them.
---

# Planning a change

A plan is the "how". Append it to an existing `type: ChangeRequest` in
`docs/dev/backlog/` — the request stays a single document; the plan is a section
inside it. This applies to all four change kinds — `feature`, `bug`, `refactor`,
and `improvement` — regardless of which skill created the request.

A change too large to land as one request is not given one sprawling plan.
Instead it is decomposed into a set of standalone sub-task change requests, and
the request becomes an **umbrella** that tracks them. Which path to take is
decided in step 3, with the user.

## 1. Find the request

    grep -rl "^state: proposed" docs/dev/backlog

Read the request's `# Problem`, `# Proposal`, and `# Decisions` sections.

## 2. Write the approach

Read the relevant crates, tests, and docs under `docs/` (`architecture.md`,
`features.md`, and the API/CLI references). Work out:

- The crate(s), module(s), and file(s) to touch.
- The technology choices and data flow.
- Key decisions and tradeoffs.
- Which docs under `docs/` the change will update or add.

## 3. Decide the size

Judge whether the change lands as one request or must be split. Treat it as
**too large** when the approach spans several independent deliverables — separate
crates or subsystems, or parts that can be planned, built, and reviewed on their
own — or when it would take many sessions to implement as a single unit. A change
that is merely long, but is one coherent deliverable, stays a single plan.

When it is too large, settle the split with the user before writing anything:
which sub-tasks to carve out, what each covers, and whether any depend on
another. Prefer a set of sub-tasks that stand alone and can be implemented in any
order; impose an order only where one genuinely depends on another. Record the
split and its reason in the request's `# Decisions`.

- Fits one request → step 4.
- Too large → step 5.

## 4. Append a single plan

Add an `# Implementation plan` section to the same `<slug>.md`, after the request
sections:

    ## Approach

    <technical details>

    ## Steps

    - [ ] <concrete, ordered step>
    - [ ] ...

Add one step per doc under `docs/` the change affects (architecture, features,
API, CLI, getting-started), naming the document and exactly what will change in
it — e.g. "Document the new `sort`/`filter` query parameters in
`docs/api/rest-api.md`". Place each doc-update step alongside the code step it
describes, so the docs cannot drift from the implementation.

Each step should be verifiable and map to an acceptance criterion.

## 5. Decompose into sub-tasks

The request becomes an umbrella: it stays a single `type: ChangeRequest`, but its
`# Implementation plan` indexes the split instead of listing code steps. Add:

    ## Approach

    <the overall strategy and how the sub-tasks fit together>

    ## Sub-tasks

    - [ ] [<Sub-task title>](<parent-slug>-<sub-slug>.md)
    - [ ] ...

List the sub-tasks unordered when they stand alone; order them, and say why, only
where a dependency exists. The umbrella carries no code steps of its own — the
work lives in the sub-tasks.

Then create one change request per sub-task in `docs/dev/backlog/`, named
`<parent-slug>-<sub-slug>.md` and titled `<Parent title>: <sub-task>`, with this
front matter:

    ---
    type: ChangeRequest
    kind: <the parent's kind>
    title: <Parent title>: <Sub-task>
    description: <one-line summary>
    state: proposed
    priority: <low|medium|high>
    tags: [dev, <parent topic>]
    owner: <actor>
    parent: <parent-slug>
    ---

Keep each sub-task lean and self-contained: give it a `# Problem`, a
`# Proposal`, and `# Acceptance criteria` scoped to just that sub-task, and link
[the parent](<parent-slug>.md) for the shared context and decisions. Add a
`# Decisions` section only when the sub-task has decisions of its own. Each
sub-task stays `state: proposed` — it is planned on its own by a later
`fawi-plan` — so the umbrella is what holds the overall approach.

## 6. Update the front matter

Set the request under plan — the single request, or the umbrella — to
`state: planned`.

## Next steps

`fawi-implement` implements the plan. For an umbrella it works through the
sub-tasks instead, and the umbrella is done once every sub-task is done.
