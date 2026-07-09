---
name: plan-approval
description: Enforces explicit user approval before implementing any plan. Use when presenting a plan, receiving plan feedback, or about to delegate implementation.
user-invocable: false
---

## Plan-to-Implementation Gate

When the user asks you to plan something, follow this strict protocol:

The `BLOCK KIT` comment in `prompts/plan_template.html` is the canonical source for plan section semantics. Do not restate its grammar here.

**approval object = 4.1 Schema + resolved Trade-offs + entries promoted from Other Details.** "take off" approves the approval object as settled at that moment.

### Phase 1: Plan
- Research the codebase as needed.
- Present the plan as a decision surface following the template block kit, including numbered Trade-offs for choices the user must judge.
- End with: "Say **take off** when ready to implement."

### Phase 2: Feedback Loop
- Trade-off replies are plan feedback, not approval. The user replies by number to override a recommendation or resolution (e.g. "1 default, 2 both"); record each resolution inside its numbered block. Unmentioned items accept the recommendation when the plan is approved.
- Other feedback works the same way — refine the plan and wait:
  - "no validator needed" — refining a detail
  - "be careful about X" — adding a constraint
  - "what about Y?" — asking a question
  - "make the output JSON" — changing a settled term
- When the user asks to control an Other Details entry, promote it; it then joins the approval object.
- After incorporating feedback, re-present the updated plan (or the updated parts) and wait.

### Phase 3: Approval
- Proceed to implementation only when the user says **"take off"** (case-insensitive). Silence, corrections, and refinements keep you in Phase 2.
- Apply Trade-off resolutions before consuming approval; unmentioned items take their recommendations.
- Trade-off resolutions may arrive in the same message as approval: "1 default, 2 both, take off" resolves the Trade-offs first, then approves the resulting approval object.
- "take off" is the ONLY approval trigger. "go ahead", "do it", "implement", "proceed", "LGTM", "ship it", "yes" are ambiguous — ask: "Say **take off** to confirm."
- Think of it like flight control: the plane does not move until the tower says "take off".
- **"Take off" is a one-shot token.** Once consumed, it is gone. Any later change touching the approval object **invalidates** the previous "take off": re-present the affected terms and wait for a new "take off". A "take off" approves the approval object as it existed at that moment, not any future version.
- Corrections to Context, Design Details, or unpromoted Other Details entries are amendments and do not require re-approval.
