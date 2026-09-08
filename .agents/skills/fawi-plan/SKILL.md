---
name: fawi-plan
description: Append an implementation plan to a backlog change request — the technical approach, files to touch, and an ordered step checklist — and move it to state - planned.
---

# Planning a change

A plan is the "how". Append it to an existing `type: ChangeRequest` in
`docs/dev/backlog/` — the request stays a single document; the plan is a section
inside it. This applies to all four change kinds — `feature`, `bug`, `refactor`,
and `improvement` — regardless of which skill created the request.

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

## 3. Append the plan

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

## 4. Update the front matter

Set `state: planned`.

## Next steps

`fawi-implement` implements the steps and marks the request done.
