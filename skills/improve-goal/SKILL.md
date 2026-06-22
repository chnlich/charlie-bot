---
name: improve-goal
description: How to write effective improve loop goal prompts — for the master CC agent that writes the --goal-file content.
version: 1.0.0
---

# Improve Loop — Goal Prompt Construction

This skill is for the **master CC** (orchestrator). It guides construction of the goal prompt written to the file passed via `--goal-file` to `charliebot improve` (the file content becomes the live, per-iteration goal). Workers do NOT need this skill — they read `improve-worker` instead.

## Goal Prompt Rules

When writing the `--goal-file` content for an improve loop:

1. **Start with skills to read** — first line should be `Read skills: improve-worker, <domain-skill>, <infra-skill>, ...`. List all relevant skills explicitly. Always include `improve-worker`. Workers won't know which skills exist unless told.
2. **Order goals by importance** — list goals from most important to least important, numbered (1), (2), (3)... Workers may not reach later goals if earlier ones consume all iterations.
3. **State goals, not methods** — say "close the performance gap" not "try fusing kernel X with Y". Workers decide their own approach.
4. **Include reference numbers** — target and rough gap size, so workers can judge whether a change matters. Don't paste a precise measured baseline: the A/B protocol re-measures it every iteration, so a hard-coded number just goes stale.
5. **"Zero progress is acceptable"** — always include this. Prevents workers from shipping bad changes just to report something.
6. **Point at existing working scripts** — if a benchmark/training script
   already exists and works, say "base your work on `<path>`, do not write
   from scratch". Otherwise workers reinvent the wrapper and re-hit known
   pitfalls.
7. **Keep it lean** — don't re-list prior attempts; the loop already feeds each iteration's work report back, so past tries are visible there. Cut anything the worker re-derives. A shorter goal re-reads better every iteration.

## Master Discipline During Loop Execution

Once the loop is running, do not repeatedly propose stop when iterations look unproductive. The user authorized the iteration count at take-off and will say "stop" if they want to stop. If a failure pattern emerges, diagnose it once and continue reporting iter-by-iter without re-proposing stop.

## Planner / Executor Separation

For large "elephant" improvements, split planning from execution:

- **Planner**: Optional pre-loop delegation orchestrated by the master. Use a separate `script-run` task to profile the repo, identify prioritized levers by impact/headroom, and write acceptance criteria for each lever. The deliverable is `plan.md`, passed to the loop with `--plan-file`. The planner may use a different backend/model through `delegate --backend <id>`.
- **Executor**: Improve loop iterations read `goal.md`, optional `plan.md`, and previous iteration summaries. Each worker decides how to implement the current highest-priority incomplete lever, not what to tackle. Ordering comes from `plan.md`.
- **Elephants first**: Order levers by impact/headroom, not ease. Multi-iteration levers are expected and normal.
- **Plan is optional**: Small tasks can skip the planner and omit `--plan-file`, preserving the original thin-goal behavior.
- **Re-steering**: Both `goal.md` and `plan.md` are re-read each iteration. The user can edit either mid-loop to steer subsequent workers.

## What Makes a Good Goal Prompt

1. **Context** — gives current state and target so workers understand where they stand
2. **Prioritized goals** — numbered (1)-(N) with most important first
3. **Delegation of method** — says *what* to achieve, not *how*
4. **Analysis directive** — pushes workers to measure and analyze before guessing
5. **Zero-progress escape hatch** — lets workers report "tried X, didn't work" without feeling forced to ship bad changes
6. **Skill reference** — methodology lives in skills, not repeated in the prompt; keeps the goal clean

## Example Structure

```
Read skills: improve-worker, <domain-skill-1>, <domain-skill-2>, <infra-skill>.

<Context: current metrics, target, gap size.>

Focus on:
(1) <highest priority goal — what to achieve, not how>
(2) <second priority goal>
(3) <lowest priority — cleanup, readability, etc.>

Follow the methodology in improve-worker and <domain-skill> strictly.
Zero progress is acceptable — record what was tried and metrics.
```

For a concrete CUDA optimization example, see `cuda-block-improve/example_improve_prompt.md`.
