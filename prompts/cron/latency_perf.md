This run is one iteration of the hourly latency-perf loop: measure the standing metrics of the
running instance, then land one measurable performance improvement that can be finished and
verified within this run, as one pull request. One iteration per run keeps every claim fresh and
every diff reviewable. The loop fixes the contract, not the path: wherever the evidence points is
where the improvement may come from, inside the metrics or outside them, and healthy ranges start
the hunt without bounding it. When the numbers say the last merge regressed a metric, fixing that
regression outranks new work. The loop keeps its state in the repository: the baseline file holds
the metrics and their history, merged pull requests hold what landed, and the rejection ledger
holds what was abandoned. Work in the repo worktree: branch, fix, push, `gh pr create`, then
squash-merge once the checks are green. `main` takes no direct pushes; the pull request stays the
record of what was measured and why it changed. Before writing code, read
`skills/writing-style/genres/code.md` and follow it: comments carry constraints only; provenance
lives in blame.

Isolation rule: any validation that runs charliebot components uses a scratch `CHARLIEBOT_HOME`
copy of the state directory, never the live home, and the live instance gets read-only observation
only. The run never edits `~/.charliebot`, never restarts the server, and never touches CI.

Measure before anything else. Run the standing collectors listed in `docs/perf_baseline.md`
exactly as that file lists them, so every round's numbers compare with the history; each takes
seconds, and together they are the regression watch. The collectors observe the live instance
read-only, which is the only contact this run has with it. A collector that fails or prints
nothing is itself a finding: the summary reports it, and the round treats that metric as
unmeasured. That file is the single home for metric definitions, collector commands, healthy
ranges, and sampling history, and nothing it owns is duplicated here. A topic whose evidence
metric has no baseline row gets its before numbers self-measured under the same conditions the
fix will run in, and its definition and history rows are added in the same pull request. A range
that needs recalibration instead of a code fix travels as the docs-only calibration pull request
the baseline file describes.

Then pick one target. A degraded healthy range is a candidate, and so is any hot spot found by
reading the code; the scan reads the repository as it stands today, not as earlier rounds left
it. Pick the measurable optimization with the largest measured effect — any measurable performance
optimization qualifies, however small — and stay inside what one run can finish and verify. A
topic with no baseline row is a valid target all the same: the measurement rule above gives it
before numbers, and its row lands with the fix. Skip every topic a merged latency-perf pull
request landed in the last 1 day. Host-state findings and LLM-provider findings go to the run
summary and never produce a pull request. An open latency-perf pull request bounds the run: it
leaves a one-line reminder comment naming this cron when the request has none from the last 7
days, and exits, reading that decision entirely from the request's comment history and keeping no
state anywhere else. With no open request, the run exits without a pull request only when no
measurable optimization survives the scan, and the summary names what was scanned.

Implement the fix as one pull request per run, on branch `latency-perf/<slug>` with a slug that
self-describes the topic. Keep the diff at 300 lines or fewer; when the budget runs out, stop and
leave the remainder to a later run. Reads stay targeted, because the backend has a hard 18k-token
output cap: open only the files the fix needs and read only the relevant ranges. Keep the test
suite green. Verified means the metric that motivated the fix moves in the after numbers, taken by
the same collector commands; re-measure the affected metrics, then open the pull request. The body
states what changed and why, and carries an `## Evidence` section quoting the before and after
numbers, each command verbatim, and the load state at measurement time.

Land it in this run. Wait for the checks:

```bash
gh pr checks <PR> --watch --fail-fast
```

Retry the watch a few times, several seconds apart, while it reports no checks reported; the
workflow needs a moment to register the checks after the push.
Before merging, run the PR review per `prompts/cron/code_review_prompt.md` with
charlie-code (`--json`, from a scratch directory outside the worktree; model, api_base,
and context_window from the `charlie-code-kimi-k3` backend entry; task file is the
prompt verbatim plus a final `PR: <number> <url>` line; the verdict is the final
`result` event's `final_output`), post the verdict as one PR comment, act on its
findings on the same branch, and report a skipped review, with its reason, in the
summary. With the checks green, confirm the
request still carries net content against a fresh `main`; a rival run that already merged the same
fix is invisible to the open-request scan, so this guard lives at merge time, and a non-empty diff
merges:

```bash
git fetch origin main && git diff origin/main...HEAD --stat
gh pr merge --squash <PR>
```

An empty diff means the fix already lives on `main`, and the request closes with the abandonment
comment below. A run that cannot finish the wait leaves the request open and reports that in the
summary; the next run's open-request scan picks it up. After a merge, fast-forward the repo's
local main checkout (`git merge --ff-only origin/main`) so sibling cron runs keep a fresh base for
the same guard.

A red check earns one fix on the same branch, at most twice: read the failure with
`gh run view --log-failed`, fix, push, and watch again. When the request stays red after the
second fix, or the failure comes from outside this diff, close it with a comment starting
`latency-perf-abandoned:` that names the topic; that comment is the rejection ledger's whole
entry, and the topic name is what the next run skips by. The next run reads the ledger with the
windowless query, because a limit once hid rejections, and skips every topic it returns:

```bash
gh search prs --repo "$(gh repo view --json nameWithOwner -q .nameWithOwner)" --state closed "latency-perf-abandoned"
```

End every run that reaches measurement with a summary of five fixed sections; a run with no pull
request still reports all five:

- the standing metrics as measured, with the serve-count bias the baseline file documents;
- the comparison of every metric against its healthy range in `docs/perf_baseline.md`;
- what was fixed and why, or why nothing was;
- host-state and LLM-provider findings;
- a recommendation for the next run.
