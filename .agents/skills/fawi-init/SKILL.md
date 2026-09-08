---
name: fawi-init
description: Initialize a new project — an interactive conversation that discovers the project kind, its potential users and use cases, and the technologies, then records a requirement analysis and an initial set of change requests in a fresh docs/ OKF bundle.
---

# Initializing a project

A project starts as a conversation, not as code. This skill bootstraps a new
project end to end: it asks what the project is, who will use it, what they will
do with it, and how it should be built, then records the resulting requirement
analysis and a first set of change requests that define the work ahead.

The skill produces the project's definition (overview, requirements, and
technology choices) alongside the `docs/` bundle. The initial change requests
then drive the actual code through the rest of the workflow — `fawi-init` does
not implement features; it establishes what there is to build.

## 1. Check for existing work

    ls docs/index.md docs/log.md README.md 2>/dev/null

If any of these exist, the project has already been started. Do not overwrite it
— report what is there and stop, unless the user explicitly asks to start over.

Also read any existing build or manifest files (`Cargo.toml`, `package.json`,
`pyproject.toml`, `go.mod`, …) and any existing source tree so the questions you
ask and the answers you propose are grounded in what is already present. Do not
invent files, crates, packages, or commands.

## 2. Discover the project

This is an interactive conversation. Ask one question at a time, or group them
only when they are tightly related. Repeat until the picture is complete. Record
each answer as it is given — a short sentence with the choice and the reason.

### 2.1 Kind and purpose

Establish what the project is:

- What problem does it solve, and for whom?
- What form does it take — a command-line tool, a web service, a library, a GUI,
  an agent skill, or a combination?
- What is the one-line pitch? What is deliberately out of scope?
- Is this greenfield, or does it extend an existing codebase?

### 2.2 Potential users

Identify who will use the project. For each user, capture:

- A name for the persona (e.g. "developer", "operator", "end user").
- Their goal and their level of technical expertise.
- What they care about — speed, correctness, simplicity, extensibility, trust.
- Which users are primary and which are secondary.

If no users are identified, that is itself a finding: a project with no
envisaged user cannot yield use cases. Say so and resolve it before continuing.

### 2.3 Use cases

For each primary user, work out what they will actually do:

- The concrete task, in "given / when / then" terms where possible.
- The inputs, the expected outcome, and what a successful result looks like.
- Which use cases are essential for the first release and which can wait.
- Edge cases and error conditions worth naming now.

Push until each primary user has at least one concrete use case. A use case is
only complete when you could write an acceptance criterion for it.

### 2.4 Technologies

Agree on how the project will be built:

- Language, runtime, and package/build tooling.
- Key frameworks or libraries, and what each is chosen for.
- Storage, deployment target, and any external services or APIs.
- Hard constraints — licensing, platform, performance, or team familiarity.

For each choice, record the alternative considered and why the chosen one won.
Do not add a dependency unless a use case or requirement actually needs it.

## 3. Analyze requirements

Turn the users and use cases into a list of requirements:

- Number and name each requirement, and tie it back to the user and use case
  that produced it.
- Mark each requirement as functional or non-functional (performance, security,
  reliability, accessibility, maintainability).
- Flag conflicting or ambiguous requirements and resolve them with the user.
- Assign a priority — high, medium, or low — using the same scale the backlog
  uses.

Then decide the **initial milestone**: the smallest coherent slice that
demonstrates the project's value. It should cover the primary users' essential
use cases and no more. Requirements outside this slice stay listed but are
explicitly deferred, so later change requests can pick them up.

## 4. Agree the initial change requests

For each requirement in the initial milestone, agree what the first change
request will be. One requirement may map to one change request, or a use case
may need several. For each:

- The `kind` (almost always `feature` at this stage; a `bug`, `improvement`, or
  `refactor` only if one already applies).
- A title and a one-line description.
- A priority and an owner.
- What is in scope and what is deliberately left out.

Confirm the full set with the user before writing anything. A handful of
well-scoped change requests is better than a large pile of vague ones.

## 5. Create the directories

    mkdir -p docs/dev/backlog

## 6. Write the reserved files

`index.md` and `log.md` are reserved filenames and carry no front matter.

- `docs/index.md` — the bundle entry point and navigation. Introduce the project
  in two or three sentences, then link each top-level area — the overview, the
  requirements, and the development section — with a one-line description of what
  each holds. It is an index: point to the concepts, do not restate them.
- `docs/log.md` — the directory update log. It records how the bundle itself
  evolves. The creation entry from step 9 is its first entry; later updates are
  prepended under their own `## YYYY-MM-DD` heading. Each entry names the action
  (`Creation`, `Update`), what changed, and the files involved.

## 7. Write the concept documents

Every other document is a concept. It must start with YAML front matter whose
only required field is `type`; the common optional fields are `title`,
`description`, `tags`, and `status`. Fill in the full front matter on every new
concept (as in the template below) and keep it consistent with the schema that
`frontmatter.md` documents. Create:

- `docs/overview.md` — `type: Overview`; what the project is. One or two
  paragraphs stating the problem it solves, for whom, in what form, and that it
  is greenfield; then links to the requirements and the development section. Say
  only what was agreed in step 2 — no aspirations or features that were not
  discussed.
- `docs/requirements.md` — `type: Requirements`; the complete requirement
  analysis from step 3, so nothing decided in the conversation is lost:
  - the potential users — each persona's name, goal, expertise, and what it
    cares about, and which are primary;
  - the use cases — for each primary user, concrete enough that an acceptance
    criterion could be written from it;
  - the technology choices — each with the alternative considered and why the
    chosen one won;
  - the requirement list — numbered and traced to its user and use case, marked
    functional or non-functional, with the initial milestone and the deferred
    requirements called out.
- `docs/frontmatter.md` — `type: Reference`; the front matter schema the bundle
  uses. Cover every field that appears anywhere in the bundle — the concept
  fields above and the change-request extensions (`state`, `kind`, `priority`,
  `owner`) — stating which are required and what each means.
- `docs/dev/index.md` — the development section index (no front matter needed;
  index filenames are reserved). Introduce the change-driven workflow in a few
  sentences and link the backlog, the changelog, and the contributing guide.
- `docs/dev/changelog.md` — `type: Changelog`; shipped changes, newest first,
  under dated headings. Leave the body empty until `fawi-implement` records the
  first change.
- `docs/dev/backlog/index.md` — the backlog index (no front matter needed;
  index filenames are reserved). State that change requests live here and what
  `kind` and `state` mean, so readers can navigate the backlog without opening
  every request.

Use this front matter on concept documents, adjusting `type` and fields:

    ---
    type: Overview
    title: <Title>
    description: <one-line summary>
    tags: [<topic>]
    status: draft
    ---

Hold every concept document to the same quality bar as the change requests:

- One job per document. When two documents need the same fact, one states it
  and the others link to it.
- Every statement is grounded in the conversation — no invented users, use
  cases, technologies, or milestones.
- Specific over generic: real persona names, real technology names, numbered
  requirements. No "TBD", "etc.", or placeholder filler.
- Terminology is consistent across the overview, the requirements, the change
  requests, and the indexes.
- Links resolve and point where the reader would look next: the overview
  reaches the requirements and the backlog, and the indexes reach every
  document they list.

## 8. Write the initial change requests

Create `docs/dev/backlog/<slug>.md` for each change request agreed in step 4,
using the change-request front matter:

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

A change request uses a single `state` field (no `status`): `proposed` →
`planned` → `in-progress` → `done`. `fawi-check` may move it to `rejected` or
`superseded`. Give each request the sections the workflow expects:

- `# Problem` — the motivation or gap.
- `# Proposal` — the change in one or two paragraphs.
- `# Decisions` — the key decisions agreed in step 2–4, each with its reason.
- `# Acceptance criteria` — testable, concrete outcomes derived from the use
  case.

If the project already has documented behaviour, note which docs the change will
touch; a greenfield project usually has none yet.

## 9. Record the creation

Append a dated entry to `docs/log.md`:

    ## YYYY-MM-DD
    * **Creation**: Initialized the project as an OKF bundle — discovered the
      project kind, users, use cases, and technologies; recorded the requirement
      analysis in `requirements.md`; and seeded the backlog with the initial
      change requests.

## Next steps

`fawi-plan` appends an implementation plan to each backlog change request, and
`fawi-implement` builds it. `fawi-propose`, `fawi-fix`, `fawi-improve`, and
`fawi-refactor` capture further requirements as the project grows.
