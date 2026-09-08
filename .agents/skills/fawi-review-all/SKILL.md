---
name: fawi-review-all
description: Review the whole project — every crate, its docs, and the backlog — for defects, drift, and debt; settle the prioritized findings with the user, then write one type - ChangeRequest under docs/dev/backlog capturing the agreed follow-up.
---

# Reviewing the whole project

A whole-project review audits everything — the code under `crates/`, the docs
under `docs/`, and the state of the backlog — for defects, drift, and debt,
rather than looking only at the current work (`fawi-review`). It never changes
code. Like the other capture skills, reviewing is an interactive conversation:
audit the project, consolidate the findings into a prioritized list, agree with
the user which of them the review will act on, and only then write exactly one
`type: ChangeRequest` in `docs/dev/backlog/`. A review with nothing worth
acting on produces no change request — report that the project is healthy and
stop.

Because the review writes exactly one request, it cannot capture every finding
of a large audit at once: the request covers the findings you agree to act on
now — usually the top-priority cluster — and the rest are recorded as deferred,
so they are not lost and can be picked up later.

## 1. Orient

- List the crates under `crates/` and read `docs/architecture.md` for the
  intended crate layout, responsibilities, and data flow.
- Read `docs/features.md` and the API/GUI/CLI references (`docs/api/`,
  `docs/gui/leptos-gui.md`, `docs/server/cli.md`) to know the documented
  behaviour.
- Survey the backlog and changelog — `grep -rn "^state:" docs/dev/backlog` and
  `docs/dev/changelog.md` — for what is open and what has shipped recently.
- Read any crates, modules, or files you have not seen before rather than
  assuming their shape. Do not invent files, crates, or commands.

## 2. Audit the project

Walk the whole tree and check each dimension. Ground every finding in evidence
— a file, a function, a command's output — and skip anything vague.

- **Build and tests** — run the verification commands from
  `docs/contributing.md`:

      cargo build -p fawi-server
      cargo build -p fawi-gui --features ssr
      cargo test -p fawi-core -p fawi-storage

  Note failures, and tests missing for behaviour that looks critical.
- **Docs drift** — do the docs match the code? Real endpoints, query
  parameters, CLI flags and defaults, and behaviour in `docs/api/rest-api.md`,
  `docs/server/cli.md`, `docs/features.md`, and `docs/gui/leptos-gui.md`; the
  crate layout and data flow in `docs/architecture.md`; the front matter schema
  in `docs/frontmatter.md` against what the server reads. Broken or dangling
  links between docs.
- **Code health** — panic paths and `unwrap`s, `todo!`/`unimplemented!`,
  debug leftovers, dead code, duplicated logic across crates, weak error
  handling, and code that contradicts `docs/architecture.md`.
- **Backlog health** — open change requests (`proposed`, `planned`,
  `in-progress`) whose problem has already been solved, duplicates, and
  requests whose plan contradicts the current code. Do not re-validate them
  individually — that is `fawi-check`'s job — just note the candidates.
- **Consistency** — naming, style, and structural drift between crates, and
  front matter usage that disagrees with `docs/frontmatter.md` or the
  conventions in `docs/dev/index.md`.

## 3. Consolidate and prioritize

Merge the findings into a numbered, prioritized list. For each finding record
what is wrong, where, the evidence, its severity, and the change it calls for —
a defect (`bug`), an enhancement of existing behaviour (`improvement`), a
redesign (`refactor`), or a missing capability (`feature`). Group findings that
belong to the same area or root cause into clusters.

If nothing is worth acting on, report that the project is healthy and stop — do
not write a change request for the sake of one.

## 4. Discuss open points

Surface every point that needs the user's decision before the request can be
written. Present the prioritized findings, state what the audit shows for each,
then ask. Typical points:

- false positives to drop, and findings whose severity or impact you have
  misjudged;
- which findings the review's single change request covers — usually the
  top-priority cluster you agree to act on next — and which are deferred;
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
of a project review.

## 6. Fill in the sections

- `# Problem` — what was reviewed (the whole project, or the areas the audit
  concentrated on) and each finding the request covers, numbered, with its
  location and impact.
- `# Proposal` — the change that resolves the findings, in one or two
  paragraphs.
- `# Decisions` — the key decisions agreed in step 4, each with its reason,
  including the agreed `kind`, and the deferred findings listed by number so
  nothing from the audit is lost.
- `# Acceptance criteria` — testable, concrete outcomes, numbered to the
  findings they resolve.

If the change alters documented behaviour — features, architecture, the REST
API, or the CLI — note which docs under `docs/` it will touch and what in each
will change, so the implementation plan can turn that into concrete doc-update
steps.

## Next steps

`fawi-plan` appends the implementation plan to this request. For a review of
just the current work — everything since the last commit — use `fawi-review`
instead.
