---
name: fawi-docs
description: Update the docs under docs/ to match the code and the backlog — reconcile documented behaviour (REST endpoints, CLI flags, features, architecture, front matter, UI) with what the code now does, then record the update in the bundle log.
---

# Updating the docs

The docs under `docs/` describe what the code does, but code and the backlog
move faster. This skill reconciles them: read what changed — the change requests
in the backlog and the code itself — and update the docs so they describe
reality. It is a documentation edit only: it never changes code.

`fawi-implement` updates docs as part of landing a change. Use this skill when
the docs have drifted anyway, when a change landed without its docs, or to catch
up a whole area. `fawi-condense` may call on it to repoint docs that link into
the backlog.

## 1. Choose the scope

Two inputs drive the work — the backlog and the code. Pick the scope with the
user:

- **A change request** — most often a `state: done` item, or an `in-progress`
  one. Its `# Problem`, `# Proposal`, and `# Decisions`, plus its `# Docs`
  section or `# Implementation plan`, name the behaviour and the docs it touches.
- **Code changes since a point** — the current work, or everything since the
  last release:

      git --no-optional-locks status
      git --no-optional-locks diff
      git --no-pager log --oneline

Confirm which change requests and which commits are in scope before editing.

## 2. Work out what the code actually does

Read the change request(s) for intent, then read the code for the truth — the
crates under `crates/`, the handlers, flags, and defaults. Document what the
implementation does, checked against it, not what the request promised. Where
they disagree, the code wins and the discrepancy is worth reporting.

## 3. Find the docs to update

Map each changed behaviour to the doc that describes it:

- REST endpoints and query parameters → `docs/api/rest-api.md`; WebSocket →
  `docs/api/websocket.md`.
- CLI flags and defaults → `docs/server/cli.md`.
- Behaviour and capabilities → `docs/features.md`.
- Crate layout and data flow → `docs/architecture.md`.
- Front matter schema → `docs/frontmatter.md`.
- The web UI → `docs/gui/leptos-gui.md`.
- Run instructions → `docs/getting-started.md`.
- Overview and navigation → `docs/index.md` and `docs/overview.md`.

Then fix what the change breaks: cross-references, dangling links, and stale
index entries.

## 4. Update in place

Follow the bundle's conventions, holding the docs to the same bar as the code:

- Update the affected existing doc; add a new one only for a genuinely new area,
  and link it from its section index (`docs/index.md` or a sub-index) so nothing
  dangles.
- Match the doc's structure, heading levels, tone, and front matter; adjust its
  `description`, `tags`, or `status` when the content changes.
- Every concept needs valid front matter (`type` is required); `index.md` and
  `log.md` are reserved and carry none.
- Keep terminology consistent across docs and with the code, and keep related
  docs consistent with each other.

## 5. Verify against the code

Read each updated claim back against the implementation, and where useful run it
— `cargo build -p fawi-server`, `cargo build -p fawi-gui --features ssr`, or the
binary with a `curl` against the endpoint you documented. Never document
behaviour the code does not have.

## 6. Record the update

`docs/log.md` is the bundle's own update log. Prepend a dated entry (newest
first):

    ## YYYY-MM-DD
    * **Update**: Brought <docs> in line with <the change> — <what changed>.

The code-change record is separate: `fawi-implement` appends to
`docs/dev/changelog.md` when a change ships, so do not duplicate it here.

## Next steps

If the drift revealed a behavioural gap rather than a doc gap, capture it with
`fawi-fix`, `fawi-improve`, `fawi-refactor`, or `fawi-propose` instead of
papering over it in prose.
