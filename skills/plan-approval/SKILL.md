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
- A child whose approver is its parent node sends divergences and plan links to its parent
  (Between Agents, `prompts/task_manager.md`).
  Its Waiting on you items stay in its own session, so the user answers them there.

## The principle

The user judges each consequence step (Execution section) before it happens.
The line's approver judges the reading on each understanding page before any work builds on it.
A misread request then costs one page, not a built version.
The approver is the user, or the parent node for a child that the user handed over end to end (`prompts/task_manager.md`).
The user usually approved that child's reading already on the parent's understanding page.
The user also gives each explicit user approval that a rule requires.
The agent tree judges the rest, design choices included.
The user reviews these judgments on the delivered product.

**approval object = sections 1 and 3 + 4.1 Schema + resolved Trade-offs + entries
promoted from Other Details.** Plan approval covers these exact terms. The `BLOCK KIT` comment
in `prompts/plan_template.html` is the canonical grammar for the plan surface; do
not restate it here.

## Understanding

An understanding page lists the divergences of a request that admits several readings
(the trigger and its exemptions live in the master prompt).

- The page is `artifacts/understanding_<slug>_v<n>.html`, reusing the head and style of
  `prompts/plan_template.html`. Six blocks: goal in one sentence carrying the why one level up; the
  deliverable through two or three concrete examples; acceptance criteria, each a trigger
  condition plus an observable behavior (EARS phrasing, "WHEN <condition> THE SYSTEM
  SHALL <behavior>", is an example shape, not a requirement); non-goals; numbered
  divergences rendered as `div.fork` blocks, each carrying the fork grammar's
  folded explainer (block kit section 5, `prompts/plan_template.html`); Linear.
- The Linear block carries the whitelist gate, the two-path recall, the candidate or
  draft choice, the take off gate, and the identifier readback. It appears only for
  repos marked Linear-tracked on the memory entry `charliebot/workspace-repos`, read
  with `charliebot memory query --topic charliebot` while drafting; a repo not tracked
  carries no Linear block and no explanation. When the marker list is absent altogether
  the block states in one line that the whitelist is not configured, because an absent
  list and an all-untracked list are otherwise indistinguishable and the mechanism
  would fail silently. Two-path recall per the `linear` skill is presented as either a
  candidate list (identifier, title, state, project, each with a one-line relevance
  judgement) or a conclusion that nothing matches plus a ready-to-file draft; never pick
  a match silently, the user decides. The draft's fields are an English title and body
  (goal, deliverables, acceptance), team, project, assignee, and priority, resolved from
  the Linear defaults held in memory with priority defaulting to medium; a new ticket's
  project follows the closest candidate's project and is named in the draft so the user
  can change it. The write waits for take off per the master prompt's External System
  Writes rule, and the authorization may ride in the same message that answers the
  divergences. After the ticket is created, read its identifier back from the API and carry
  it as a header meta chip on the understanding page and on the plan that follows.
- Don't guess: any point the request leaves unstated where different readings lead to
  different designs must appear as a numbered divergence; never silently pick a reading.
- Map each outcome that the request names to a deliverable or a divergence.
  State the share of the problem that the deliverable covers.
- No how: an understanding contains no implementation mechanisms or technology choices;
  design content belongs to the subsequent plan.
- Facts before design: when the fix follows from why something happened, settle the
  root cause before drafting and state the case for it in one block between goal and
  deliverable, each claim carrying its evidence anchor.
- Share the page with the line's approver before any work builds on its divergences.
  The approver then corrects a misread request before it costs a build.
- While the page waits, continue read-only research.
  The plan that follows then starts from read evidence.
- When the approver is the user, list the page as one Waiting on you item that names this rule.
  The cold read then classifies the item as an approval that a rule requires.
- When the approver is the parent node, send the page's divergences to the parent (Scope section).
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
  trigger, per the Cold-Read Gate of the file-server skill. When the page states a root cause,
  it passes the Source Check of the file-server skill before sharing.
- The plan carries the understanding page's file path as a header meta chip;
  section 1 states the goal in its own words.
- The page as it opens obeys the plan template's page budget (the BLOCK KIT comment in
  `prompts/plan_template.html`); the always-visible minimum here is the goal sentence and
  each divergence's question, recommendation, and trade lines.

## Plan

- Research the codebase first; present the plan as a decision surface following the
  template block kit, with numbered Trade-offs for the choices the user must judge.
- Contract text (rule sentences the plan lands verbatim in prompt docs, skills, or task
  specs) is drafted to the standing-reality form and checked block by block before
  `charliebot plan present`: each sentence opens with what stands or works ("one post
  carries the whole reply up to the limit", over "in one post unless it exceeds the
  limit"), a bare prohibition is recast as the working practice it protects ("each rule
  keeps one home; other files reference it", in place of the ban it replaces), the
  exception arrives as a bounded case rather than the sentence's spine, and dashes give
  way to commas, colons, semicolons, or a restructure (code excepted).
- Render the artifact per the USAGE note atop the BLOCK KIT comment in `prompts/plan_template.html`.
- Run the Cold-Read Gate of the file-server skill with `--genre plan`.
- Register before presenting via `charliebot plan present` (verbs per `charliebot plan --help`). The artifact's status chip is a presentation-time snapshot; the plan registry is the live truth. Record the code baseline when the plan pins one.
- An improve-loop plan follows this same contract; its approval object covers
  repo, goal, iterations, work branch, and merge-back — loop parameters with reasonable
  alternatives (iteration count, merge-back) make natural Trade-offs.
- Keep the plan short: retain the decision, the necessary scope and the acceptance.
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

- Carry a line's reversible work to delivery on its reading.
  The reading is the answered understanding page, or the recommended reading when no page exists.
- When a design choice between reversible options comes up, take the recommended option.
  The line then moves on without a wait, and the report names each alternative.
- A user message in a running line is feedback.
- A request to stop, in any wording or language, or a cancel stops the line.
- The parent node approves a child line's plan. A root line approves its own plan.
- The parent's approval covers the plan terms other than decisions that only the user makes.
  Each such decision then waits for the user's answer in the child's session.
- Before each consequence step, ask the user for a take off that names the step.
- A pre take off names lines and steps ahead of time. It holds until those lines deliver.
- Before you ask a take off for a deletion, show its dry-run list: each path and its size.
- Before you ask a take off, try the step on a scratch copy up to its first real use.
- When the step changes rules or code that other nodes follow, send the draft to each such manager node.
- Re-read each number that the ask cites when you write the ask.
- When a draft patches a procedure that an earlier draft patched, first ask whether the procedure should exist.
- Attach each result to the ask.
  The user then approves a step that already works, and approves it once.
- Open the ask with the goal and the method.
- Explain them with a diagram or a worked example, and list the alternatives with their consequences.
  The user then understands the ask at one read.
- When work under a Linear ticket lands, draft one English comment for that ticket.
  The comment states the landed design, the resolved choices and the commit.
  Ask its take off in the completion report. Delegation then waits for no ticket write.
- Post one comment per plan lineage or direct change. Work without a ticket posts nothing;
  the plan stage never creates a ticket. Linear then records the design that landed.

## Verify

- Follow this fixed order:
  1. Draft the plan.
  2. Run the assertions.
  3. Register the plan with `charliebot plan present`.
  4. Run one Improve round: answer the plan page's Improve prompt
     (src/features/artifacts/static/artifact-comments.js) yourself.
  5. Start verify round 1 and the cold read in the same turn, following the revision rule below.
  6. Complete the revision under that rule.
  7. Approve the plan with `charliebot plan approve`, and send the user its link.
  The order puts approval after the revision is complete.
  Verify is a read-only repo-less delegation and runs on
  a registered plan, so the goal it measures against has one home and the plan is listed
  while the rounds run.
- An Improve round narrows the affected system and tests the mechanism.
- Verify checks fidelity — claims against evidence — and adequacy: whether the design,
  assumed accurate and implemented, entails what section 1 claims. Adequacy reads
  section 1 as the goal statement only; scope boundaries, thresholds, and exemptions
  stated in Context, section 3, 4.1, or Trade-offs bind adequacy equally, so section 1 never
  restates them. Quote the understanding page's file path in the spec as the
  adequacy reference when one exists; otherwise quote the originating request. Without
  one of these, adequacy cannot judge section 1 against what was asked.
- Verify also checks each wait, test and user question in the plan.
  Each one names the requirement it protects (prompts/manager_workflows.md, Design section).
  A step that names none is a finding, so a plan carries no step that only adds a wait.
- Treat the initial plan as the first revision.
  This starts the lineage's review sequence.
  Treat the versions between triggers as one revision.
  This keeps versions from one review cycle together.
  When the user gives feedback or the author changes the design outside a findings fold, start a new revision.
  This gives each independent design change its own review cycle.
  Keep verify and cold-read findings folds in the current revision.
  This stops findings folds from resetting the round limit.
  Run verify round 1 on each revision.
  This checks each revision under its declared scope.
  For each revision of a plan for the user, run at most two verify rounds.
  This stops findings folds from starting an endless review loop.
  When a cold read starts in the same turn as verify, count it in that verify round.
  This keeps the cold read within the round limit.
  When a verify round 1 finding holds, fold it into the plan.
  This gives verify round 2 a defined change set to re-check.
  When verify round 1 findings change the plan, run verify round 2 on those changes.
  This checks the fold before finalization.
  For verify round 2, declare delta.
  This confines verify round 2 to the verify round 1 fold.
  For verify round 2, quote only verify round 1 fold changes from `charliebot plan diff` into the spec.
  This keeps verify round 2 scoped to changes from verify round 1.
  Re-check those changes and whether verify round 1 findings are closed.
  This confirms whether the fold closed verify round 1 findings.
  After verify round 2, fold its findings into the plan.
  This records findings from the final round.
  Then finalize the revision.
  This ends verification without a third round.
  When verify round 1 findings do not change the plan, finalize the revision.
  This avoids verify round 2 when round 1 changes nothing.
- Declare the scope in every spec.
  This tells the reviewer which terms to re-check.
  When a spec omits its scope, verify it as full.
  This keeps missing scope from narrowing the review.
  For verify round 1 of a lineage, declare full.
  This gives adequacy the whole plan to refute.
  For verify round 1 of a later revision, declare delta or full.
  This matches the check to the revision's changes.
  When the revision changes the design's structure, declare full for verify round 1.
  This lets adequacy check the whole design after structural changes.
  When verify round 1 of a later revision declares delta, quote its terms from `charliebot plan diff` into the spec.
  This anchors the review in recorded changes.
  Re-check those terms and their dependents.
  This checks the effects of those changes.
  Re-check whether prior findings are closed.
  This confirms earlier findings are resolved.
- Every round of one plan measures with the same ruler: the spec's Goal quotes the registered
  version's section 1 by path, and cites fidelity and adequacy as defined here. A round that
  wants a wider question puts that question to the user.
- Adequacy findings are advisory. `gap-design:` says the mechanism looks short of the
  goal — weigh it and revise the design if it holds. `gap-goal:` says the goal itself may
  be misread — put that question to the user.
- A revision answers findings by correcting the plan against the goal it was registered with;
  section 1 keeps saying what the first version said. When a finding argues the goal itself
  is the problem, that is a `gap-goal:` question for the user.
- Report the adequacy findings when you send the user the plan link, so the user can weigh them too.

## Runtime delegation authorization

The runtime gate admits a delegation when the calling manager and its ancestors are open
and the chain holds a real user message or a thread platform summon. Verify and cold-read
always pass.
The gate judges the chain alone; its definitions live in rust/crates/runtime/store/src/task_tree.rs (`is_read_only_exempt` and the delegation refusals).

## Execution

- Default to acting. This is the reversibility test: if you can undo an operation
  alone at similar cost and its effect stays within what you own (reaching neither
  other people nor systems they rely on), do it without asking.
- Hold work that builds on an unanswered understanding page, reversible work included.
  The approver's reading check then comes before the build.
- These kinds of step always fail the test:
  - deleting data that has no backup;
  - a message in the user's name, or a publication outside CharlieBot;
  - a merge into a main branch that colleagues share;
  - a restart of a running service;
  - a job on a shared cluster;
  - spending money.
- When reality forces a change that leaves the approval object intact, make it,
  log it as a deviation, and keep going.
- Between plan approval and the report there is nothing to ask: return to the user
  only at a checkpoint the plan assigns to the user, at an unapproved action that
  fails the test, or at a blocking failure. Progress updates are fine; requests
  for permission are not.

## Report

- At completion or at a blocking failure, account for every approval-object term:
  delivered or deviated. Each deviation is a retroactive Trade-off — approved term,
  what landed, one-line reason — accepted by default or reverted on request; the revert becomes a new delegation.
- For each choice made on a recommendation, name the alternative and the reply that switches to it.
- For each delivered step, state how to undo it.
