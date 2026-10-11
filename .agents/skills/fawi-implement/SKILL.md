---
name: fawi-implement
description: Implement a planned change request from the backlog — follow its implementation plan, run the build and tests, mark it done, and append a short entry to docs/dev/changelog.md.
---

# Implementing a change

## 1. Find work

    grep -rl "^state: planned" docs/dev/backlog

Pick the highest `priority`. Read the request and its `# Implementation plan`
before writing any code.

Some `planned` requests are **umbrellas**: their `# Implementation plan` has a
`## Sub-tasks` list and their sub-tasks carry `parent: <slug>`. Do not implement
an umbrella directly — work through its sub-tasks instead. A sub-task is ready
once it is itself `state: planned` (plan it first with `fawi-plan` if it is still
`proposed`); pick them up in any order, honouring an order only where the
umbrella states a dependency. The umbrella closes itself once every sub-task is
done.

## 2. Implement

Follow `docs/architecture.md` for the crate layout and consult the relevant docs
under `docs/` (`features.md`, and the API/CLI references) so the change stays
consistent with what is documented. Make the smallest change that satisfies the
acceptance criteria, and add or update tests.

## 3. Verify

    cargo build -p fawi-server
    cargo build -p fawi-gui --features ssr
    cargo test -p fawi-core -p fawi-storage

## 4. Close the request

- Set `state: done`.
- Add `verified: { by: <actor>, at: <ISO-8601 timestamp> }`.
- Link the commit in the body.

When the request carries `parent: <slug>`, tick its box in the umbrella's
`## Sub-tasks` list. Once every sub-task of an umbrella is `done`, close the
umbrella too — set its `state: done` with the same `verified` stamp — so the two
never drift apart.

## 5. Update the docs

If the change alters documented behaviour — features, architecture, the REST API,
the CLI, or run instructions — update the affected docs under `docs/`
(`features.md`, `architecture.md`, `api/`, `server/cli.md`,
`getting-started.md`) so they stay accurate. Keep these updates at the same
quality as the code:

- Update the affected existing doc in place; only add a new one when the change
  opens a genuinely new area, and then link it from its section index so nothing
  dangles.
- State what the code actually does — real endpoints, query parameters, CLI
  flags and defaults, and behaviour — checked against the implementation rather
  than paraphrased from the change request.
- Match the doc's existing structure, heading levels, tone, and front matter;
  adjust its `description`, `tags`, or `status` when the content changes.
- Keep related docs consistent with each other and fix any cross-references the
  change breaks.

## 6. Update the changelog

Append one short bullet to `docs/dev/changelog.md` under today's date heading
(create it if missing), newest first: a bold title, one precise sentence naming
the concrete change (the endpoint, flag, behaviour, or file), and a link to the
backlog request. Record what changed, not why — leave out motivation, impact,
and filler:

    - **Sort and filter by front matter fields** — `/api/dirs` now accepts
      `sort=<field>` and `filter=<field>=<value>`. See
      [Sort and filter by front matter fields](backlog/summary.md#sort-and-filter-by-front-matter-fields).
