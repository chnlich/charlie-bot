---
name: plan-approval
description: Enforces explicit user approval before implementing any plan. Use when presenting a plan, receiving plan feedback, or about to delegate implementation.
user-invocable: false
---

## Plan-to-Implementation Gate

When the user asks you to plan something, follow this strict protocol:

### Phase 1: Plan
- Research the codebase as needed.
- Present a clear, actionable plan.
- End with: "Say **take off** when ready to implement."

### Phase 2: Feedback Loop
- If the user gives feedback, corrections, or asks questions about the plan, **update the plan and wait again**.
- Plan feedback is NOT implementation approval. Examples of feedback (NOT approval):
  - "no validator needed" — refining a detail
  - "be careful about X" — adding a constraint
  - "what about Y?" — asking a question
  - "change step 3 to ..." — modifying the plan
- After incorporating feedback, re-present the updated plan (or the updated parts) and wait.

### Phase 3: Approval
- Only proceed to implementation when the user says **"take off"** (case-insensitive).
- This is the ONLY approval trigger. No other phrase counts:
  - "go ahead", "do it", "implement", "proceed", "LGTM", "ship it", "yes" — these are **NOT** approval. Treat them as ambiguous and ask: "Say **take off** to confirm."
- Think of it like flight control: the plane does not move until the tower says "take off".
- **"Take off" is a one-shot token.** Once consumed, it is gone. If the user modifies the plan after saying "take off" (adds constraints, changes approach, refines scope), the previous "take off" is **invalidated**. You must re-present the updated plan and wait for a new "take off". A "take off" approves the plan as it existed at that moment, not any future version.

### Never Do
- Never say "Delegating now" after receiving plan feedback.
- Never interpret silence, corrections, or refinements as approval.
- Never auto-escalate from planning to implementation.
- Never carry over a previous "take off" after the plan has been modified. Post-approval modifications reset the approval gate.
