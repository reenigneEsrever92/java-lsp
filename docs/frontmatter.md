---
type: Reference
title: Front matter schema
description: The front matter fields used across the docs bundle.
tags: [reference, meta]
status: draft
---

Concept documents (everything except the reserved `index.md`/`log.md` files)
start with YAML front matter.

## Common fields

| Field | Required | Meaning |
|-------|----------|---------|
| `type` | yes | The document kind: `Overview`, `Requirements`, `Reference`, `Changelog`, `ChangeRequest`. |
| `title` | no | Human-readable title. |
| `description` | no | One-line summary. |
| `tags` | no | Topic tags. |
| `status` | no | Lifecycle of the concept, e.g. `draft`. |

## Change-request extensions

Change requests replace `status` with a single `state` field and add workflow
fields:

| Field | Required | Meaning |
|-------|----------|---------|
| `state` | yes | `proposed` → `planned` → `in-progress` → `done`; `fawi-check` may move a request to `rejected` or `superseded`. |
| `kind` | yes | `feature`, `bug`, `improvement`, or `refactor`. |
| `priority` | yes | `low`, `medium`, or `high` — same scale as the requirement list in `requirements.md`. |
| `owner` | yes | The actor responsible for driving the request (currently `felix`). |
