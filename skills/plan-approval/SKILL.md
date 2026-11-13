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
  approval interaction approves the settled plan terms; the runtime gate judges each
  later `/delegate` or `/improve` request.
- A delegated worker, reviewer, or improve-loop worker does not present another plan;
  it follows its delegated task contract.
- A delegated agent still stops and reports a blocker, contract conflict, or required
  scope expansion.
- A child whose approver is its parent node sends its divergences, when its page waits, and its plan links
  to its parent (Between Agents, `prompts/task_manager.md`).
  Its Waiting on you items stay in its own session, so the user answers them there.

## The principle

Action Boundaries in `prompts/master.md` and the host's resident memory entries in `$CHARLIEBOT_HOME/memory` set the
approval boundary of each step.
The user takes off each high-consequence step before it runs.
The agent tree carries out each reversible step and design choice directly, and states what changed and how to undo
it in the session reply or report.
The line's approver answers the reading on each understanding page that waits (Understanding section)
before work builds on it.
The approver is the user, or the parent node for a child under an e2e handover (`prompts/master.md`, `prompts/task_manager.md`).
The user also gives each explicit user approval that a rule requires, and reviews the delivered product.

**approval object = sections 1 and 3 + 4.1 Schema + resolved Trade-offs + entries
promoted from Other Details.** Plan approval covers these exact terms. The `BLOCK KIT` comment
in `prompts/plan_template.html` is the canonical grammar for the plan surface; do
not restate it here.

## Understanding

An understanding page lists the divergences of a request that admits several readings
(the trigger and its exemptions live in the master prompt).

- The page is `artifacts/understanding_<slug>_v<n>.html`, reusing the head and style of
  `prompts/plan_template.html`. Six blocks: goal in one sentence carrying the why one level up; the
  deliverable through one concrete worked example and one figure showing the main reading or outcome;
  acceptance criteria, each a trigger condition and an observable result in the page's natural language; non-goals; numbered
  divergences rendered as `div.fork` blocks, each carrying its options and their consequences outside folds
  and the fork grammar's folded explainer (block kit section 5, `prompts/plan_template.html`);
  Linear.
- The Linear block appears only for a repo marked Linear-tracked on the memory entry
  `charliebot/workspace-repos`; a repo not tracked carries no block, and an absent marker list gets one line
  saying the whitelist is not configured. Use the `linear` skill to list candidates or prepare a ready-to-file
  draft. For a root session, the user decides which and the `linear` skill's confirmation rule applies.
  For a sub session, follow the master prompt's External System Writes rule. Fill the draft from the Linear defaults
  in memory.
  After the ticket exists, carry its identifier as a header meta chip on the understanding page and the plan that
  follows.
- Don't guess: any point the request leaves unstated where different readings lead to
  different designs must appear as a numbered divergence; never silently pick a reading.
- Map each outcome that the request names to a deliverable or a divergence.
  State the share of the problem that the deliverable covers.
- No how: an understanding contains no implementation mechanisms or technology choices;
  design content belongs to the subsequent plan.
- Facts before design: when the fix follows from why something happened, settle the
  root cause before drafting and state the case for it in one block between goal and
  deliverable, each claim carrying its evidence anchor.
- Share the page with the line's approver.
  The approver then corrects a misread request before it costs a build.
- An understanding page waits for its approver only when the estimated work exceeds about one worker-day
  (8 hours of worker time) or its result reaches other people (the reading threshold).
  This applies to root lines and child lines alike.
- A page whose approver is the user waits only while no e2e handover covers its reading (`prompts/master.md`).
- Below the reading threshold, the line proceeds on the recommended reading, and its delivery lists each divergence
  with the reply that selects the other reading.
- For a departure from the user's stated method, follow the Design section of `prompts/manager_workflows.md`.
- While the page waits, continue read-only research.
  The plan that follows then starts from read evidence.
- When the approver is the user and the page waits, list the page as one Waiting on you item.
- When the approver is the parent node and the page waits, send the page's divergences to the parent (Scope section).
  The parent then answers them from the reading that the user approved for the parent's line.
- The approver answers the divergences on the page.
  An unmentioned divergence keeps its recommendation.
- After the approver's reply on the page, in any wording, continue the line on the answered reading.
  One reply then releases the line, with no special approval word.
- The plan registry, the plan verify round, and plan approval start with the plan
  that follows the page.
- External system writes follow the master prompt's External System Writes rule.
- Before sharing, the page passes the mechanical assertions with
  `--genre understanding`. Its cold read runs after sharing, with the originating request as the
  trigger, per the Cold-Read Gate of the file-server skill. The page author or the worker that built it
  verifies each cited `path:line` and derived number before the page goes out.
- The plan carries the understanding page's file path as a header meta chip;
  section 1 states the goal in its own words.
- Page layers follow the canonical first-draft contract in `prompts/master.md` under
  "Explaining to the user" → "Length"; each divergence keeps its question, options, and consequences visible, with
  its full rationale in the folded explainer.

## Plan

- Research the codebase first; present the plan as a decision surface following the
  template block kit, with numbered Trade-offs for the choices the user must judge.
- Contract text (rule sentences the plan lands verbatim in prompt docs, skills, or task
  specs) is drafted to the standing-reality form before `charliebot plan present`, and each
  block passes the three prompt-admission checks in `prompts/master.md` ("Text a model reads").
- Render the artifact per the USAGE note atop the BLOCK KIT comment in `prompts/plan_template.html`.
- Run the Cold-Read Gate of the file-server skill with `--genre plan`.
- Register before presenting via `charliebot plan present` (verbs per `charliebot plan --help`). The artifact's status chip is a presentation-time snapshot; the plan registry is the live truth. Record the code baseline when the plan pins one.
- An improve-loop plan follows this same contract; its approval object covers
  repo, goal, iterations, work branch, and merge-back — loop parameters with reasonable
  alternatives (iteration count, merge-back) make natural Trade-offs.
- Draft the plan to the layering contract in `prompts/master.md` under "Explaining to the user" → "Length"; keep the
  decision, necessary scope, and acceptance visible.
- Put operational history in `$CHARLIEBOT_HOME/LESSONS.md`; put supporting detail behind links or folds.
- A chat reply about a plan answers the question in at most five bullets and gives the page link.
  Detail stays on the page.

## Feedback

- Replies that touch the plan are feedback, not approval: Trade-off resolutions by
  number, added constraints, questions, changed terms.
  Fold each into the plan, and register the revision with `charliebot plan amend`.
  Continue the line after following the revision rule in Verify.
  This lets that rule determine when the revision ends.
  A revision registers with `charliebot plan amend`, which requires `--note`: the one
  line saying why this version differs from its predecessor. The note rides on the version record; the body stays the complete
  current design and carries no revision marks, and the reader gets the differences
  from `charliebot plan diff`.
- A style finding on one passage stands for its pattern: sweep the whole artifact for that
  pattern and fold every fix into the same revision.
- Only the user changes the goal. Versions answer feedback within the standing goal; a changed
  goal is a new plan — `charliebot plan present` opens the new lineage and the old one closes
  as superseded.
- When the user asks to control an Other Details entry, promote it; it joins the
  approval object. At approval, each unmentioned Trade-off that the approval covers takes its
  recommendation. Each decision that only the user makes then stays with the user.

## Approval

- Carry a line's work to delivery on its reading, and pause at high-consequence steps that wait for a take off.
  The reading is the answered understanding page, or the recommended reading when the task starts directly.
- For a reversible design choice, take the recommended option and name each alternative in the report.
- A user message in a running line is feedback.
- A request to stop, in any wording or language, or a cancel stops the line.
- The parent node approves a child line's plan. A root line approves its own plan.
- The parent's approval covers the plan terms other than decisions that only the user makes.
  Each such decision then waits for the user's answer in the child's session.
- Before each high-consequence step, ask the user for a take off that names the step.
- A pre take off names lines and steps ahead of time. It holds until those lines deliver.
- Before a take off, state the goal, blast radius, and undo plan.
- For a deletion, list each path and its size. Send each rule or code change that other nodes follow to their managers.
- Recheck every cited number and attach its result so the user can approve a step that already works.
- Give the user a diagram or worked example and list the alternatives with their consequences in one read.
- When work under a Linear ticket lands, draft one English comment for that ticket.
  The comment states the landed design, the resolved choices and the commit.
  A root session asks for its take off in the completion report. A sub session follows the master prompt's External
  System Writes rule. Delegation then waits for no ticket write.
- Post one comment per plan lineage or direct change. Work without a ticket posts nothing;
  the plan stage never creates a ticket. Linear then records the design that landed.

## Verify

- Draft and assert the plan, register it with `charliebot plan present`, complete one Improve round, then verify and
  cold-read it in the same turn. Finish the revision under the rules below, then approve it with
  `charliebot plan approve` and send the user its link.
- Verify is a read-only repo-less delegation on a registered plan. It checks fidelity — claims against evidence —
  and adequacy: whether the design, assumed accurate and implemented, entails what section 1 claims. Adequacy reads
  section 1 as the goal statement only; scope boundaries, thresholds, and exemptions stated in Context, section 3,
  4.1, or Trade-offs bind adequacy equally. Quote the understanding page's path in the spec as the adequacy
  reference when one exists, and otherwise the originating request.
- Verify also checks each wait, test and user question against the requirement it protects
  (prompts/manager_workflows.md, Design section), and checks each rule change against the three prompt-admission
  checks in `prompts/master.md` ("Text a model reads"). A step that names no requirement, or a rule change that
  fails a check, is a finding.
- The initial plan is revision one; versions between triggers share a revision. User feedback or a design change outside
  a findings fold starts a new revision; keep verify and cold-read folds in the current revision.
- Run verify round 1 on each revision, as full for a lineage's first revision. For a later revision declare delta or
  full, and declare full when the structure changes; a delta spec quotes its terms from `charliebot plan diff`.
  Verify an undeclared scope as full.
- Run at most two verify rounds on a revision of a plan for the user: round 1 as declared, then round 2 as delta over
  the round-1 fold only, quoting those changes from `charliebot plan diff`. A cold read started with verify counts in
  that round. When round 1 findings change nothing, finalize after round 1. When the round 1 revision only applies
  the specific items that round 1 verify and the cold read named, keeps the plan's structure, and adds no causal
  claim and no derived number, finalize after that revision and run no round 2. Otherwise run round 2, fold its
  findings, and finalize, with no third round.
- Every round measures with the same ruler: the spec's Goal quotes the registered version's section 1 by path, and
  cites fidelity and adequacy as defined here. A round that wants a wider question puts that question to the user.
- Adequacy findings are advisory. `gap-design:` says the mechanism looks short of the goal — weigh it and revise the
  design if it holds. `gap-goal:` says the goal itself may be misread — put that question to the user.
- A revision answers findings by correcting the plan against the goal it was registered with; section 1 keeps saying
  what the first version said. Report the adequacy findings when you send the user the plan link.

## Runtime delegation authorization

The runtime gate admits a delegation when the calling manager and its ancestors are open
and the chain holds a real user message or a thread platform summon. Verify and cold-read
always pass.
The gate judges the chain alone; its definitions live in rust/crates/runtime/store/src/task_tree.rs (`is_read_only_exempt` and the delegation refusals).

## Execution

- Classify each step by Action Boundaries in `prompts/master.md` and the host's resident memory entries before it runs.
- When a high-consequence step waits for a take off, list it as one Waiting on you item, and continue independent
  work in the same turn.
- Carry out reversible steps and design choices directly, and state how to undo each external or repository change
  in the session reply or report.
- List each action that only the user can perform (a browser login, an approval in another system, or information
  that only the user holds) as one Waiting on you item.
- Hold work that builds on a waiting understanding page until the approver answers the page.
- When reality forces a change that leaves the approval object intact, make it, log it as a deviation, and keep going.
- Between plan approval and the report, return to the user only at a checkpoint the plan assigns to the user, at an
  unapproved high-consequence step, or at a blocking failure.

## Report

- At completion or at a blocking failure, account for every approval-object term as delivered or deviated.
  Write each deviation as a retroactive Trade-off (approved term, what landed, and one-line reason), accepted by
  default or reverted on a new delegation when requested.
- For each reading below the reading threshold and each design choice made on a recommendation, name the alternative and the reply that switches to it.
- State how to undo each delivered change.
