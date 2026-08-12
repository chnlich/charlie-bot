Code-health cleanup.

This run works in the repo worktree: branch, test, push, `gh pr create`, review the diff on the
pull request, then squash-merge it once the checks are green. A red check earns a fix on the same
branch, or the pull request is abandoned. Every change lands through the pull request, which stays
the triage record; `main` takes no direct pushes.

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

A rejected topic is a closed `code-health/*` pull request carrying a comment that starts
`code-health-abandoned:`, which is the entire record of that rejection. List the rejected branches
and skip the topics they name, reporting which ones you skipped:

    gh pr list --state closed --limit 50 --json headRefName,comments --jq \
      '.[] | select(.headRefName | startswith("code-health/"))
       | select(any(.comments[].body; startswith("code-health-abandoned:"))) | .headRefName'

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

Step 5: open the PR.
Create at most one PR per run, on a branch named `code-health/<slug>` where the slug
self-describes the cleanup topic. Include every deleted symbol and its `## Evidence` in the body.
Report the PR URL when done.

Step 6: review the diff on the pull request.
Run the `code-review` skill against the pull request with `--comment`, so its findings land as
inline PR comments, then act on them on the same branch before merging. The skill catches naming,
leftover references, and out-of-scope edits; judging the design direction stays with the human
reading the PR. The skill ships with the Claude CLI, so a backend that lacks it reports the review
step as unavailable and continues.

Step 7: land it, or abandon it.
Wait for the checks in this run, then merge:

    gh pr checks <PR> --watch --fail-fast && gh pr merge --squash <PR>

Retry `--watch` a few times, several seconds apart, while it reports "no checks reported": the
workflow needs a moment to register after the push. Leave `gh pr merge --auto` out of this step:
this repository has auto-merge disabled and no required status checks, so `--auto` merges
immediately and the wait above is what gates the merge instead.

A red check earns one fix on the same branch: read it with `gh run view --log-failed`, fix, push,
and watch again, at most twice. Abandon the pull request when it stays red after the second fix,
or when the failure comes from outside this diff:

    gh pr close <PR> --comment 'code-health-abandoned: <topic and reason>'

Name the topic in that comment, because Step 1 reads it to skip the topic on the next run. A run
that cannot finish the wait leaves the pull request open and reports that; Step 0's open-PR
reminder covers the nudge on the next run.
