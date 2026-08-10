Code-health cleanup (runs every 6 hours).

This run works directly in the repo worktree: branch, test, push, `gh pr create`, then enable
squash auto-merge. Never push to `main` directly. With auto-merge, CI (ruff, pytest, the
300-line-budget / evidence contract check) remains the pre-merge gate; the human review step is
removed because CI is the gate and these changes are small and reversible.

Step 0: check for an open contract PR before anything else.
List open pull requests whose head branch matches `code-health/*`. If any exists: produce no new
PR. If that PR has no bot reminder comment in the last 7 days, leave a one-line reminder comment,
then exit. Read this decision entirely from the PR's comment history; keep no state anywhere else.

Step 1: select a target, one topic per run.
Among topics not previously rejected, pick the one with the highest hotspot score, where
hotspot score = (commits touching the file in the last 90 days) x (file line count). Compute the
churn half verbatim with this command and multiply the per-file commit counts by the file's
`wc -l`:

    git log --since='90 days ago' --name-only --pretty=format: -- src | grep '\.py$' | sort | uniq -c | sort -rn

Confirm your chosen file's line count multiplies the top commit count to the highest score.

Step 2: respect the diff budget.
Keep the PR diff at 300 lines or fewer. A file-split series whose parts will not fit a single
300-line diff must instead be organized as micro-tasks, each individually cleanly compiling and
committing; exceeding the budget then requires a human-applied `split-series` label. When you hit
that budget in one PR, stop adding to it and hand off the remainder as micro-tasks.

Step 3: delete only with full evidence.
Before deleting any symbol, produce three pieces of evidence, all three quoted verbatim in the PR
body's `## Evidence` section (each as command plus output):
1. A static tool reports it dead (e.g. `vulture`).
2. A whole-repo grep including `prompts/ skills/ configs/ web/` finds no reference.
3. The full test suite is green after removal.
In this first phase, delete only when the symbol name has exactly zero whole-repo matches.

`vulture` is a probe you may run to surface candidates. It is never a gate and must not be added
to CI.

Step 4: consult the known-alive list.
Some symbols look dead to static tools but are reached by string reference or kept deliberately.
The seed entry, uncovered while standing up the CI gate, is the two documented `# noqa`
re-exports: `kill_tmux_session` and `ScheduledSessionBusyError`. Any symbol reached from
`prompts/ skills/ configs/ web/` by string belongs here rather than in a deletion. Keep this list
in this file, in the "Known-alive symbols" list below; edit it whenever you confirm a symbol is
reached by string. Do not delete a symbol on this list.

Known-alive symbols:
- `kill_tmux_session` — documented `# noqa` re-export, reached by string reference.
- `ScheduledSessionBusyError` — documented `# noqa` re-export, kept deliberately.

Step 5: open the PR and enable auto-merge.
Create at most one PR per run, on a branch named `code-health/<slug>` where the slug
self-describes the cleanup topic. Include every deleted symbol and its `## Evidence` in the body.
Immediately after `gh pr create`, enable squash auto-merge: `gh pr merge --auto --squash <PR>`.
If auto-merge cannot be enabled or CI fails, leave the PR open and report that; the Step 0
open-PR reminder covers the nudge on the next run. Report the PR URL when done.