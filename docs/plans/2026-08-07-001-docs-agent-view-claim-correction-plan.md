---
title: "docs: Correct the Claude Code agent-view comparison in ARCHITECTURE.md"
type: docs
date: 2026-08-07
---

# docs: Correct the Claude Code agent-view comparison in ARCHITECTURE.md

## Summary

`ARCHITECTURE.md`'s M4 section claims Claude Code's agent view "needs a
supervisor process because its sessions live in memory." That premise is wrong:
current Claude Code docs state session state persists on disk, and the
supervisor exists for worker lifecycle, IPC, prewarming, attach/reply,
notifications, and reconnection. Bullpen's daemonless architecture remains a
real differentiator — the claim just needs to rest on the accurate distinction.
This plan corrects the wording and aligns the M4 Stage 2 checklist with the
full Agent View feature list from the same docs.

## Requirements

- R1. `ARCHITECTURE.md` no longer asserts that Claude Code sessions live only
  in memory. The comparison states the accurate distinction: bullpen runs
  detached durable processes with SQLite as the coordination/state plane and no
  supervisor at all; Claude Code persists sessions to disk but routes worker
  lifecycle, IPC, attach/reply, notifications, and reconnection through a
  supervisor process.
- R2. The M4 Stage 2 list reflects the full Agent View checklist — attach/detach,
  peek replies, durable queued input, needs-input state, notifications,
  stop/delete, process restart, directory dispatch, summaries, PR status —
  distinguishing committed Stage 2 items from candidates, and noting which
  capabilities Stage 1 already covers (peek, dispatch from the dashboard).
- R3. No other repo text carries the inaccurate claim. Verified during
  planning: the only occurrence is `ARCHITECTURE.md` (M4 section);
  `crates/cli/src/bg.rs` describes only bullpen's own design and stays
  accurate as written.

## Key Technical Decisions

- **The differentiator is supervisorless coordination, not durability.** Both
  systems persist session state. What bullpen uniquely lacks is a supervisor:
  a background session is a detached `bullpen run` process, the SQLite WAL
  store is the only coordination plane, and the dashboard reads that store
  (and can dispatch) without supervising anything — there is no central
  supervisor whose death stops or strands detached sessions. Each session
  still depends on its own process; the WAL store is what lets it resume.
  Write the comparison on that axis.
- **Checklist items enter as labeled candidates, not commitments.** The doc's
  roadmap style is deliberately minimal ("second provider adapter when
  genuinely needed, not before"). Items beyond the already-committed Stage 2
  set (attach, needs-input, notifications) — durable queued input, stop/delete,
  process restart, directory dispatch, summaries, PR status — are listed as
  Stage 2+ candidates so the checklist is complete without inflating
  commitments.
- **Commit `5e9c7d4`'s message stays as-is.** It repeats the inaccurate claim,
  but history is immutable and the doc is the living source of truth. No
  rebase, no amend.

## Assumptions

- Folding the missing checklist items into M4 as candidates matches intent;
  the alternative reading (wording fix only) loses the checklist value the
  review surfaced.
- The Claude docs findings are taken from the review as given (persist-on-disk
  sessions; supervisor for lifecycle/IPC/attach/notify/reconnect). The
  reviewed page's URL was not captured; if it should be cited in the doc, add
  it during implementation.

## Implementation Units

### U1. Correct the comparison wording

- **Goal:** Replace the "sessions live in memory" premise with the accurate
  distinction from R1.
- **Requirements:** R1, R3
- **Files:** `ARCHITECTURE.md` (M4 bullet, the "Unlike Claude Code's agent
  view…" sentence)
- **Approach:** Keep the bullet's shape and length discipline. The corrected
  sentence contrasts coordination models: Claude Code persists sessions but
  needs a supervisor for worker lifecycle, IPC, and attach; bullpen has no
  supervisor — detached runs coordinate through the WAL store, so the
  dashboard is a pure read view and closing it stops nothing.
- Test expectation: none — documentation-only change.
- **Verification:** A search for "live in memory" / "sessions live in" in the
  repo's markdown returns nothing inaccurate; the M4 bullet reads correctly
  against the R1 distinction.

### U2. Align the Stage 2 checklist

- **Goal:** Expand the M4 "Stage 2:" sentence into the full Agent View
  checklist per R2.
- **Requirements:** R2
- **Dependencies:** U1 (same bullet; land the corrected premise first so the
  checklist hangs off accurate framing)
- **Files:** `ARCHITECTURE.md` (M4 bullet, "Stage 2:" sentence)
- **Approach:** Keep the three committed items with their existing
  prerequisite notes (attach needs the per-session control socket, needs-input
  needs approvals, notifications). Append the candidate set — durable queued
  input, stop/delete, process restart, directory dispatch, summaries, PR
  status — labeled as candidates. Note in passing which capabilities Stage 1
  already covers (peek latest output, dispatch from the dashboard input line)
  so the checklist doesn't imply they're missing.
- Test expectation: none — documentation-only change.
- **Verification:** Every checklist item from R2 appears exactly once in the
  M4 bullet, attributed to Stage 1 (shipped), Stage 2 (committed), or
  candidate.

## Sources

- `ARCHITECTURE.md` M4 bullet — the current inaccurate claim and Stage 2 list.
- Commit `5e9c7d4` ("Agent view (Stage 1)") — origin of the comparison framing.
- Review of the current Claude Code Agent View docs (supplied findings:
  sessions persist on disk; supervisor scope; full feature checklist).
