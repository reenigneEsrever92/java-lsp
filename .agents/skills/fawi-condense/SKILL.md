---
name: fawi-condense
description: Condense the backlog by retiring finished change requests older than a window into entries in a single summary document — condensing each request's problem, proposal, decisions, and acceptance criteria, deleting the originals, and repointing inbound links.
---

# Condensing the backlog

The backlog should hold open work. Once a change request is finished and has
been sitting a while, its full document — motivation, feasibility, every
implementation step — is mostly historical. Condensing retires those finished
requests: it replaces them with a compact entry in one summary document and
deletes the originals, so the backlog shrinks while the gist survives. It is a
documentation edit only: it never implements anything.

This removes files other docs link to — `docs/dev/changelog.md` links every
shipped request — so handling those links is part of the job, and the details
are settled with the user before anything is written.

## 1. Select the finished requests

    grep -rn "^state:" docs/dev/backlog

Eligible requests are **finished** — a terminal `state`: `done`, `rejected`, or
`superseded` — and **older than the window**, 10 days by default. Never touch
`proposed`, `planned`, or `in-progress` work, or the reserved `index.md`.

Age is measured from when the request reached its terminal state:

- `done` — `verified.at` in the front matter.
- `rejected` / `superseded` — no stamp, so the file's last commit date:

      git --no-pager log -1 --date=short --format=%ad -- <file>

Include a request only when that date is more than 10 days before today. Show
the resulting list to the user and confirm the window before removing anything.

## 2. Condense each request

Read each selected request whole and keep the irreducible content in compact
form:

- its identity — title, `kind`, final `state`, `priority`, `owner`;
- **Problem** — the gap, in one sentence;
- **Proposal** — the change, in one sentence;
- **Decisions** — each choice with its one-clause reason;
- **Acceptance criteria** — the testable outcomes, or a one-line summary of them.

Drop the rest: restated background, feasibility prose, examples that carry no
decision, implementation minutiae, and filler. Keep concrete facts — crate and
type names, endpoints, flags — but only where they are the point.

## 3. Write the summary document

Keep one accumulating document, `docs/dev/backlog/summary.md`, a concept with
`type: Reference` and `status: stable`. Add one section per condensed request,
newest first, titled with the request's title so links can target it, and state
its final `state` and finish date:

    ## <Title>

    `kind: feature` · `state: done` · finished 2026-08-20

    <condensed problem, proposal, decisions, acceptance criteria>

Create the document if it does not exist and link it from
`docs/dev/backlog/index.md` so nothing dangles; if it already exists, append
rather than rewrite.

## 4. Delete the originals and repoint links

List every reference to the files you are about to remove *before* deleting them.
Search the whole repository, not just `docs/` — references also live in the
bundled skills under `.agents/skills/` and in `README.md`:

    grep -rnE "backlog/[A-Za-z0-9._-]+\.md" . --exclude-dir=.git --exclude-dir=target

Narrow the hits to the removed slugs and repoint each to `backlog/summary.md`
(or its `#<title-anchor>`). `docs/dev/changelog.md` links every shipped request
as `backlog/<slug>.md`, and the skill examples often link a past request too.
Check the archived requests' own cross-links — a superseded request often links
the one that superseded it — and repoint or drop them so no link dangles. Then
delete each replaced `<slug>.md`.

Update `docs/dev/backlog/index.md` and `docs/dev/index.md` to say the backlog
holds open requests and that finished ones are condensed into `summary.md`.

A reference in a doc outside the backlog — `features.md`, `architecture.md` —
is handed to `fawi-docs`, which rewrites the doc to match rather than merely
relinking it.

## 5. Report

List each retired request with its final state and finish date, the files
removed, and every link repointed. Note any request that looked finished but was
left because it was still within the window, and any inbound link you could not
resolve.

## Next steps

Open work stays in the backlog for `fawi-plan` and `fawi-implement`; the summary
is the historical record the changelog entries point into.
