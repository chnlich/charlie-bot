---
name: plan-approval
description: Enforces explicit user approval at the CharlieBot master-to-user
  boundary. Use only when the CharlieBot master is producing an understanding page,
  presenting a plan, receiving plan feedback, or preparing to delegate implementation.
user-invocable: false
---

## Scope

- This skill governs only the CharlieBot master-to-user approval boundary; a
  delegated task is already past that boundary.
- Plan approval and runtime delegation authorization are separate boundaries. The
  approval interaction approves the settled plan terms; the runtime gate decides
  whether a later `/delegate` or `/improve` request has an active user-message
  authorization window.
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

## Understanding

An understanding page precedes the plan when the master must first align the reading
(the trigger and its exemptions live in the master prompt).

- The page is `artifacts/understanding_<slug>_v<n>.html`, reusing the head and style of
  `prompts/plan_template.html`. Five blocks: goal with the why one level up; the
  deliverable through two or three concrete examples; acceptance criteria, each a trigger
  condition plus an observable behavior (EARS phrasing, "WHEN <condition> THE SYSTEM
  SHALL <behavior>", is an example shape, not a requirement); non-goals; numbered
  divergences rendered as `div.fork` blocks.
- Don't guess: any point the request leaves unstated where different readings lead to
  different designs must appear as a numbered divergence; never silently pick a reading.
- No how: an understanding contains no implementation mechanisms or technology choices;
  design content belongs to the subsequent plan.
- Not registered in the plan registry, no verify worker, and take off does not apply. The
  user confirms by answering the numbered divergences in chat; unmentioned items take
  their recommendation. Confirmation is ordinary feedback and introduces no new approval token.
- The subsequent plan's section 1 references the confirmed understanding's file path.

## Plan

- Research the codebase first; present the plan as a decision surface following the
  template block kit, with numbered Trade-offs for the choices the user must judge.
- Render the artifact per the USAGE note atop the BLOCK KIT comment in `prompts/plan_template.html`.
- Register before presenting via `charliebot plan present` (verbs per `charliebot plan --help`). The artifact's status chip is a presentation-time snapshot; the plan registry is the live truth. Record the code baseline when the plan pins one.
- An improve-loop takeoff plan follows this same contract; its approval object covers
  repo, goal, iterations, work branch, and merge-back — loop parameters with reasonable
  alternatives (iteration count, merge-back) make natural Trade-offs.
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
- Approval binds to the approval object as it stands: progress never expires it,
  change does. If an approval-object term must change mid-flight, surface it to the user;
  nothing forces a re-presentation, the user decides whether to revisit.
- Verify is a required independent step; its results live in the verify thread's own log, never in the
  plan registry. `approve` records a takeoff unconditionally; the runtime delegation gate reads the chat
  log, not the registry, so approve bookkeeping does not gate execution.
- After the user says take off, record it via `charliebot plan approve`.

## Verify

- Launch verify as a read-only repo-less delegation before presenting the plan.
- Verify checks fidelity — claims against evidence — and adequacy: whether the design,
  assumed accurate and implemented, entails what section 1 claims. Quote the confirmed
  understanding's file path in the spec as the adequacy reference when one exists;
  otherwise quote the originating request. Without one of these, adequacy cannot judge
  section 1 against what was asked.
- Adequacy findings are advisory. `gap-design:` says the mechanism looks short of the
  goal — weigh it and revise the design if it holds. `gap-goal:` says the goal itself may
  be misread — put that question to the user.
- Report the adequacy findings when presenting the plan, so the user can weigh them too.

## Runtime delegation authorization

This runtime contract is separate from the plan-approval interaction above. It is
derived statelessly from the existing chat event log and applies at `/api/internal/delegate`
and `/api/internal/improve`:

- `verify` is always allowed without authorization and remains repo-less.
- A real user event is `ET.USER` with string `content`. Scheduled-trigger events
  are `ET.SCHEDULED_TRIGGER`, not `ET.USER`, and are excluded on that basis. Nested
  tool-result events are not real user messages for authorization.
- Match `pre take off` and `take off` independently, case-insensitively, after
  normalizing consecutive whitespace. The `take off` substring inside `pre take off`
  counts, so one message can create both windows.
- A real `pre take off` authorizes every non-`verify` behavior for exactly 12 hours
  from its valid UTC event timestamp, survives later real user messages, and is
  restarted by a new pre token. Missing or unparseable pre timestamps fail closed.
- Ordinary `take off` authorizes every non-`verify` delegation type without a count
  limit when the latest real user message contains it. The next real user message
  ends that window. Scheduled-trigger and nested tool-result events do not mint or cancel it.
- Scheduled workers that call `spawn_worker` directly remain outside this user-facing
  gate. No authorization file, field, event, counter, lease, consumption marker, or
  `TASK_DELEGATED` authorization read is permitted; it remains delegation history/UI data.

## Execution

- Default to acting. If you can undo an operation alone at similar cost and its
  effect stays within what you own — reaching neither other people nor systems
  they rely on — do it without asking.
- When reality forces a change that leaves the approval object intact, make it,
  log it as a deviation, and keep going.
- Between "take off" and the report there is nothing to ask: return to the user
  only at a checkpoint the plan assigns to the user, at an unapproved action that
  fails the test, or at a blocking failure. Progress updates are fine; requests
  for permission are not.

## Report

- At completion or at a blocking failure, account for every approval-object term:
  delivered or deviated. Each deviation is a retroactive Trade-off — approved term,
  what landed, one-line reason — accepted by default or reverted on request; the revert becomes a new delegation.
