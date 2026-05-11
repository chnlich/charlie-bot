---
name: improve-worker
description: methodology for iterative improve loop workers.
version: 1.0.0
---

## Mindset

- **Correctness first, then efficiency** — never sacrifice correctness for speed. Verify correctness after every change.
- **Review existing code critically** — before writing any new code, audit the current implementation with a strict eye. Question every abstraction, every redundant operation, every boundary. Understanding what's suboptimal comes before proposing changes.
- **Zero progress is acceptable** — most optimization ideas will not pan out. An iteration that concludes "no viable improvement found" after rigorous analysis is a valid outcome. Do not force changes just to show progress. Record what was tried and the metrics result. No need to guess why it failed — the next iteration will read the record and think for itself.

## Research

When out of ideas for the next optimization, **search the web** for techniques used in similar workloads.

Rules:
- **Use established, trusted references** — prefer well-known, maintained libraries and official documentation as pattern references. Do NOT introduce unvetted third-party dependencies without explicit approval from the domain skill.
- **Learn the technique, implement it yourself** — the purpose of web research is to discover *approaches*, not to copy code.
- **Cite what you found** — in the iteration report, note what you searched for and any useful references discovered, so future iterations have context.

## Optimization Workflow: Profile First

Before writing any optimization code, follow this sequence:

1. **Collect a baseline measurement** — run a profiler or benchmark appropriate to the domain. Get a representative profile of the current state.
2. **Read the data** — identify where time is spent: which operations, which phases.
3. **Compute efficiency** — how close is current performance to the theoretical limit? This tells you how much headroom exists.
4. **Identify the bottleneck** — is the system compute-bound, memory-bound, I/O-bound, or communication-bound?
5. **Target the bottleneck** — only then choose what to optimize. Optimizing a non-bottleneck wastes an iteration.

Do NOT guess what to optimize based on code reading alone. Measurement is the source of truth.

## Iteration Discipline

- **One change each time** — make only one logical change. Multiple unrelated changes make it impossible to attribute regressions.
- **Never loosen tests or tolerances** — tests exist to catch regressions. Weakening them is not an optimization. Fix the implementation, not the tests.
- **Never modify ground truth** — if a reference implementation exists, it is read-only. Do not alter it to make your implementation match.
- **Verify on real infrastructure** — complete the verification step on the target infrastructure before concluding the iteration. An iteration without verified results is a wasted iteration; the next worker will re-do the same work.
- **No measurable improvement → revert.** Improve loops keep only validated wins. Cleanup-only iters (dead code, readability) are valid output; iter may end with no commit.
- **Cleanup tasks must not increase LOC.**
- **Scope changes need user approval, not worker judgement** — if measured throughput projects a runtime significantly above the task's stated budget, or if you encounter any obstacle that would require narrowing the deliverable (skipping slices, subsampling, dropping horizons, adding a default CLI flag that reduces work, etc.), do NOT silently trim scope. These are scope changes — they need the user to decide, not the worker. Surface the situation with concrete numbers (e.g. `observed 180 samples/s × 2.3M rows → projected 3h, task budget said 5-20 min`) and wait for the user's decision before proceeding. "Budget overshoot" is a signal to escalate to the user, not to redefine the deliverable.

## Anti-Patterns

| What went wrong | Why |
|----------------|-----|
| Flip-flopped between approaches across iterations | Pick one approach and commit; if it doesn't work, record that and move on — don't revisit abandoned ones |
| Single-run benchmarks as evidence | Too noisy; use repeated measurements with sufficient warmup |
| Multiple changes per iteration | Hard to attribute regressions; keep diffs small |
| Added a uncommon third-party library instead of implementing the technique | Violates dependency rules; learn the approach, write it yourself |
| Optimized a component already near peak efficiency while ignoring one far from it | Profile-guided analysis should drive target selection, not intuition |
| Ended iteration without verification because infrastructure was slow | Wasted iteration — next worker re-does the same work. Wait for results. |
| Loosened test tolerances to make tests pass | Hides real errors; fix the implementation, not the tests |
| Created new tests with generous tolerances to bypass "never loosen" rule | Same as loosening, just indirect |
| Deleted tests for still-live functions, ran without them, reported "all pass" | Fake pass — tests for live functions must actually execute |
| Silently reduced task scope (subsampled, added `--max_samples` default, dropped slices) to fit runtime budget | Scope is the user's decision — escalate with numbers, don't trim silently |
