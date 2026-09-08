---
type: ChangeRequest
kind: feature
title: Navigation v1
description: Go-to-definition and workspace symbols backed by the workspace index.
state: proposed
priority: medium
tags: [dev, navigation]
owner: felix
---

# Problem

A Java developer needs to jump to declarations (R5, UC3). Without type
resolution, navigation can still be accurate for declarations, imports, and
unambiguous references.

# Proposal

Implement go-to-definition on top of the workspace index: definitions in the
same file, targets of import statements, and same-name declaration matches
across the workspace. Implement `workspace/symbol` as a query over the index.
Ambiguous references resolve to a best candidate or return no result — never a
silently wrong location.

# Decisions

- Honest limitation: references that require type inference (e.g.
  `x.foo()` on an inferred receiver) may be unresolved in v1; accurate
  type-aware navigation belongs to the postponed engine decision (see
  `type-aware-engine`).
- Ambiguity policy: no result beats a wrong result.

# Acceptance criteria

- Go-to-definition works for class names, import targets, and method/field
  declarations reachable through the index.
- `workspace/symbol` finds types and members by prefix across the workspace.
- Ambiguous cases return no location rather than a wrong one, and the
  limitation is documented.
