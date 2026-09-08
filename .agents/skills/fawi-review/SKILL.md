---
name: fawi-review
description: Review the current work — every change since the last commit — for completeness, correctness, tests, and doc coverage; settle the findings with the user, then write one type - ChangeRequest under docs/dev/backlog capturing the agreed follow-up.
---

# Reviewing the current work

A review looks at the work in progress before it ships: everything that has
changed since the last commit — tracked modifications, staged changes, and
untracked files — and decides whether it is complete and shippable. It never
changes code. Like the other capture skills, reviewing is an interactive
conversation: inspect the work, verify it against its intent and the docs,
surface the findings, agree on them with the user, and only then write exactly
one `type: ChangeRequest` in `docs/dev/backlog/` that captures what the review
calls for. A clean review produces no change request — report that the work is
ready and stop.

## 1. Scope the current work

    git --no-optional-locks status
    git --no-optional-locks diff --stat
    git --no-optional-locks diff

Identify every file that changed since the last commit and read them all,
including untracked files. Work out what the change is meant to do and name the
crates, modules, and files it touches. If the work implements a backlog change
request — one is `state: in-progress`, or a recent one matches the change — read
it and its `# Implementation plan` first, so you review against intent rather
than guessing it. Do not invent files, crates, or commands.

## 2. Verify the change

Read the changed code and its tests against what the change promises:

- **Completeness** — does the change deliver everything the backlog request's
  acceptance criteria (or the change's own intent) require? Is anything left
  half-done?
- **Correctness** — edge cases, error paths, silent failures, `unwrap`/panic
  paths, and behaviour that contradicts the surrounding code or the docs.
- **Tests** — did behaviour change without a test being added or updated? Do the
  tests assert the new behaviour and would they fail without the change?
- **Docs** — does the change alter documented behaviour under `docs/`
  (`features.md`, `architecture.md`, `api/`, `gui/leptos-gui.md`,
  `server/cli.md`)? If so, is the doc updated to match the new endpoints,
  flags, defaults, or behaviour?
- **Hygiene** — debug leftovers (`dbg!`, `println!`, `todo!`), accidental or
  unrelated edits, dead code, formatting drift.

Run the verification commands from `docs/contributing.md` — the subset covering
the crates the change touches is enough:

    cargo build -p fawi-server
    cargo build -p fawi-gui --features ssr
    cargo test -p fawi-core -p fawi-storage

A failing build or test is a finding. State in the review what you actually ran.

## 3. Compile the findings

Turn what you found into a short list. For each finding, record what is wrong,
where (file and, where useful, line or function), the evidence, its severity —
must-fix (defect or doc that is now wrong), should-fix (quality), or optional —
and what would resolve it. Classify the change each finding calls for: a defect
(`bug`), an enhancement of existing behaviour (`improvement`), a redesign
(`refactor`), or a missing capability (`feature`).

If there are no findings, report that the current work is complete and shippable
and stop — do not write a change request.

## 4. Discuss open points

Surface every point that needs the user's decision before the request can be
written. Present the findings, state what the code shows for each, then ask.
Typical points:

- which findings the review's change request covers — the review writes exactly
  one request, so findings left out are deliberately deferred, either to a
  targeted `fawi-fix`, `fawi-improve`, or `fawi-refactor` session or to a later
  review;
- the `kind` for the request — the dominant nature of the findings it covers
  (`bug`, `improvement`, `refactor`, or `feature`), agreed rather than assumed;
- scope — what the follow-up change is in and what it deliberately leaves out;
- tradeoffs and defaults (behaviour, naming, performance, dependency choices);
- acceptance criteria that are ambiguous for a finding.

Ask one question at a time, or group them when they are tightly related. Repeat
until every point is resolved. Record each decision as it is made — a short
sentence with the choice and the reason.

## 5. Write the change request

Only when all open points are resolved and at least one finding is in scope,
create `docs/dev/backlog/<slug>.md` with this front matter:

    ---
    type: ChangeRequest
    kind: <bug|improvement|refactor|feature>
    title: <Title>
    description: <one-line summary>
    state: proposed
    priority: <low|medium|high>
    tags: [dev, review, <topic>]
    owner: <actor>
    ---

A change request uses a single `state` field (no `status`) to capture its whole
lifecycle: `proposed` → `planned` → `in-progress` → `done`. `fawi-check` may
move it to `rejected` or `superseded`. The `kind` field marks the dominant
change the review calls for; the `review` tag records that the request came out
of a review of the current work.

## 6. Fill in the sections

- `# Problem` — what was reviewed (the uncommitted work since the last commit,
  and the backlog request it implements, if any) and each finding the request
  covers, numbered, with its location and impact.
- `# Proposal` — the change that resolves the findings, in one or two
  paragraphs.
- `# Decisions` — the key decisions agreed in step 4, each with its reason,
  including the agreed `kind` and which findings were deliberately left out.
  There is no separate feasibility section; feasibility findings fold into the
  decisions.
- `# Acceptance criteria` — testable, concrete outcomes, numbered to the
  findings they resolve.

If the change alters documented behaviour — features, architecture, the REST
API, or the CLI — note which docs under `docs/` it will touch and what in each
will change, so the implementation plan can turn that into concrete doc-update
steps.

## Next steps

`fawi-plan` appends the implementation plan to this request. For a whole-project
audit — defects, drift, and debt across every crate and doc — use
`fawi-review-all` instead.
