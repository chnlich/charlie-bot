---
name: plan-approval
description: Enforces explicit user approval at the CharlieBot master-to-user
  boundary. Use only when the CharlieBot master is presenting a plan, receiving plan
  feedback, or preparing to delegate implementation.
user-invocable: false
---

## Scope

- This skill governs only the CharlieBot master-to-user approval boundary; a
  delegated task is already past that boundary.
- A delegated worker, reviewer, or improve-loop worker does not present another plan
  and does not wait for another `take off`; it follows its delegated task contract.
- A delegated agent still stops and reports a blocker, contract conflict, or required
  scope expansion.

## The principle

The user judges before what they cannot judge after. Exactly two things need that
before-judgment: the design, because implementation builds on it and reversing it
later means redoing the work; and any action the user cannot take back afterward —
because something is destroyed, or because it has already reached other people.
Everything else the user can still correct after the fact, so it belongs to
execution — done autonomously, logged, and surfaced in the completion report,
which is where after-the-fact judgment happens.

**approval object = 4.1 Schema + resolved Trade-offs + entries promoted from Other
Details.** These are the exact terms a "take off" approves. The `BLOCK KIT` comment
in `prompts/plan_template.html` is the canonical grammar for the plan surface; do
not restate it here.

## Plan

- Research the codebase first; present the plan as a decision surface following the
  template block kit, with numbered Trade-offs for the choices the user must judge.
- End with: "Say **take off** when ready to implement."

## Feedback

- Replies that touch the plan are feedback, not approval: Trade-off resolutions by
  number, added constraints, questions, changed terms. Fold each into the plan and
  re-present what changed, then wait.
- When the user asks to control an Other Details entry, promote it; it joins the
  approval object. Unmentioned Trade-offs take their recommendation at approval.

## Approval

- "take off" (case-insensitive) is the only trigger. Anything else — "go ahead",
  "LGTM", "ship it" — gets: "Say **take off** to confirm."
- Trade-off resolutions may ride in the approval message ("1 default, 2 both, take
  off"): resolve them first; the approval covers the result.
- One "take off" releases the entire approved scope — every step, gate, and
  delegation inside it, including irreversible actions the plan itself names.
- Approval binds to the approval object as it stands: progress never expires it,
  change does. If a term must change mid-flight, re-present the affected terms and
  wait for a fresh "take off". Amendments to Context, Design Details, or unpromoted
  Other Details need no re-approval.

## Execution

- Default to acting. If you can undo an operation alone at similar cost and its
  effect stays within what you own — reaching neither other people nor systems
  they rely on — do it without asking.
- An action that fails that test and is not named in the approved plan waits for
  explicit approval — for that one action, while the rest of the plan keeps moving
  where it can.
- When reality forces a change that leaves the approval object intact, make it,
  log it as a deviation, and keep going.
- Between "take off" and the report there is nothing to ask: return to the user
  only at a checkpoint the plan assigns to the user, at an unapproved action that
  fails the test, or at a blocking failure. Progress updates are fine; requests
  for permission are not.

## Report

- At completion or at a blocking failure, account for every approval-object term:
  delivered or deviated. Each deviation is a retroactive Trade-off — approved term,
  what landed, one-line reason — accepted by default or reverted on request.
