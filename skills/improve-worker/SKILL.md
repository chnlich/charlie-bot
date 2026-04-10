---
name: improve-worker
description: General methodology for iterative improvement loop workers — mindset, research approach, optimization workflow, iteration discipline, and anti-patterns.
version: 1.0.0
---

# Improve Worker — General Methodology

This skill applies to **every** improve loop worker, regardless of domain.
Domain-specific skills provide additional rules on top of this general methodology. Read this skill first, then read the domain skill.

## Mindset

- **Correctness first, then efficiency** — never sacrifice correctness for speed. A faster implementation that produces wrong results is worthless. Verify correctness before and after every change.
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

- **One change per iteration** — make one logical change per iteration. Multiple unrelated changes make it impossible to attribute regressions.
- **Never loosen tests or tolerances** — tests exist to catch regressions. Weakening them is not an optimization. Fix the implementation, not the tests.
- **Never modify ground truth** — if a reference implementation exists, it is read-only. Do not alter it to make your implementation match.
- **Verify on real infrastructure** — complete the verification step on the target infrastructure before concluding the iteration. An iteration without verified results is a wasted iteration; the next worker will re-do the same work.
- **Don't revert everything** — if no optimization panned out, choose a safe, always-valid contribution (cleanup, dead code removal, readability) rather than ending with no commit.

## Anti-Patterns

| What went wrong | Why |
|----------------|-----|
| Flip-flopped between approaches across iterations | Pick one approach and commit; if it doesn't work, record that and move on — don't revisit abandoned ones |
| Single-run benchmarks as evidence | Too noisy; use repeated measurements with sufficient warmup |
| Multiple changes per iteration | Hard to attribute regressions; keep diffs small |
| Reverted everything in final iteration because nothing worked | Pick a safe target (cleanup, dead code) early to ensure at least one useful contribution |
| Added a uncommon third-party library instead of implementing the technique | Violates dependency rules; learn the approach, write it yourself |
| Optimized a component already near peak efficiency while ignoring one far from it | Profile-guided analysis should drive target selection, not intuition |
| Ended iteration without verification because infrastructure was slow | Wasted iteration — next worker re-does the same work. Wait for results. |
| Loosened test tolerances to make tests pass | Hides real errors; fix the implementation, not the tests |
| Created new tests with generous tolerances to bypass "never loosen" rule | Same as loosening, just indirect |
| Deleted tests for still-live functions, ran without them, reported "all pass" | Fake pass — tests for live functions must actually execute |
