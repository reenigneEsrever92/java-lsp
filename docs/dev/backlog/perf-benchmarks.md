---
type: ChangeRequest
kind: feature
title: Performance benchmark harness
description: A generated fixture project and a timing harness keeping the open-to-responsive requirement measurable.
state: proposed
priority: medium
tags: [dev, performance, benchmark]
owner: felix
---

# Problem

"Project open → responsive" is a critical, standing requirement (R6), but no
numeric targets were agreed. Without an instrument, regressions would only be
noticed by feel. Any project size must be exercisable, since the requirement is
not calibrated to a fixed size.

# Proposal

Build a generator for a configurable-size fixture Java project and a small
harness that starts the server against it and measures: time from start + file
open to the first successful response per feature, request latency during
index warm-up, and memory. Run the harness per milestone and record the
baselines.

# Decisions

- No fixed numeric targets; instead, measure, document baselines, and keep
  regressions visible — the agreed way to honor a general performance concern.
- The fixture is generated, so any workspace size can be exercised.

# Acceptance criteria

- A single command produces a timing/memory report for a chosen fixture size.
- A baseline report exists for each shipped milestone and is recorded in the
  changelog entry.
- Syntax features demonstrably respond while indexing is still running.
