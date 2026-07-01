---
name: plan-approval
description: Enforces explicit user approval before implementing any plan. Use when presenting a plan, receiving plan feedback, or about to delegate implementation.
user-invocable: false
---

## Plan-to-Implementation Gate

When the user asks you to plan something, follow this strict protocol:

### Phase 1: Plan
- Research the codebase as needed.
- Present the plan as a decision surface: the contract (the abstraction-level interface diff) plus numbered fork points, each with a recommendation and a one-line tradeoff.
- End with: "Say **take off** when ready to implement."

### Phase 2: Feedback Loop
- Fork-point replies are plan feedback, not approval. The user resolves forks by number (e.g. "1 default, 2 both"); record each resolution inside its fork block. Unmentioned forks accept the recommendation.
- Other feedback works the same way — refine the plan and wait:
  - "no validator needed" — refining a detail
  - "be careful about X" — adding a constraint
  - "what about Y?" — asking a question
  - "make the output JSON" — changing a contract term
- When the user asks to control something from the Details layer, it graduates into the Contract.
- After incorporating feedback, re-present the updated plan (or the updated parts) and wait.

### Phase 3: Approval
- Proceed to implementation only when the user says **"take off"** (case-insensitive). Silence, corrections, and refinements keep you in Phase 2.
- What "take off" approves is the contract as settled at that moment: fork resolutions applied, promoted details included, unmentioned forks on their recommendations.
- Fork resolutions may arrive in the same message as approval: "1 default, 2 both, take off" resolves the forks first, then approves the resulting contract.
- "take off" is the ONLY approval trigger. "go ahead", "do it", "implement", "proceed", "LGTM", "ship it", "yes" are ambiguous — ask: "Say **take off** to confirm."
- Think of it like flight control: the plane does not move until the tower says "take off".
- **"Take off" is a one-shot token.** Once consumed, it is gone. If the user modifies the plan after saying "take off" (resolves a fork differently, promotes a detail, adds constraints, changes the contract), the previous "take off" is **invalidated**: re-present the updated plan and wait for a new "take off". A "take off" approves the contract as it existed at that moment, not any future version.
