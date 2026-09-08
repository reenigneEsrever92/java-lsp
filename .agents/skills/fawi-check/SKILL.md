---
name: fawi-check
description: Re-check a backlog change request against the current codebase to confirm it is still up to date and its implementation plan is still viable, updating state to rejected or superseded when it is not.
---

# Checking a change request

Re-check a change request to confirm it still reflects the codebase and that its
implementation plan would still work.

## 1. Select requests to check

    grep -rn "^state:" docs/dev/backlog

Review requests whose plan may have drifted from the code, or that have been
sitting for a while. Start with `state: planned` and `state: proposed` items.

## 2. Re-validate against the codebase

For each request:

- Is the problem still real? Has the code changed to make it obsolete?
- Is the `# Decisions` still accurate?
- Does the `# Implementation plan` still match the current crate layout, APIs,
  and dependencies? Would the steps still work?
- Do the docs under `docs/` that the plan touches still match the code, or have
  they drifted since the request was written?

## 3. Record the outcome

- Still viable and current → add `checked: { by, at }` to the front matter and
  note the result in the body. No state change.
- No longer needed → `state: superseded`, and explain why.
- Not viable as planned → `state: rejected`, and either correct the plan or
  hand it back to `fawi-plan`.

Do not implement anything — only confirm or invalidate.
