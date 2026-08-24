This run works in the repo worktree: branch, test, push, `gh pr create`, review the diff on the
pull request, then squash-merge it once the checks are green. A red check earns a fix on the same
branch, or the pull request is abandoned. Every change lands through the pull request, which stays
the triage record; `main` takes no direct pushes. Before writing code, read
`skills/writing-style/genres/code.md` and follow it: comments carry constraints only; provenance
lives in blame.

Step 0: check for an open contract PR before anything else.
List open pull requests whose head branch matches `code-health/*`. If any exists: produce no new
PR. If that PR has no bot reminder comment in the last 7 days, leave a one-line reminder comment,
then exit. Read this decision entirely from the PR's comment history; keep no state anywhere else.

Step 1: pick one worthwhile cleanup.
Scan the repo and choose the single highest-value cleanup you can land within this run: dead code
(Step 3 defines the evidence bar), a behavior-preserving cleanup (deduplication, stale comment or
annotation hygiene), or a stale or duplicated test. Choose from what the working tree shows today,
never from where recent cleanups landed. For a deduplicated literal or cloned fragment, the shared
definition may live outside the file you started from when that module is the natural owner,
provided the started-from copy is the anchor being merged into it and the diff stays focused on
that one unification. Wholesale restructures (moving whole files, split-mode moves) are out of
scope for this cron; they travel as their own programs.

Skip every topic named in the rejected-topic ledger, and report which ones you skipped. A rejected
topic is a closed `code-health/*` pull request carrying a comment that starts
`code-health-abandoned:`, which is the entire record of that rejection:

    gh pr list --state closed --limit 50 --json headRefName,comments --jq \
      '.[] | select(.headRefName | startswith("code-health/"))
       | select(any(.comments[].body; startswith("code-health-abandoned:"))) | .headRefName'

Open no PR when nothing survives the scan; a no-PR run's summary names what you checked.

Step 2: respect the diff budget.
Keep a PR diff at 300 lines or fewer. When you hit that budget in one PR, stop adding to it and
leave the remainder to a later run.

Step 3: delete only with full evidence.
Before deleting any symbol, produce three pieces of evidence, all three quoted verbatim in the PR
body's `## Evidence` section (each as command plus output):
1. A static tool reports it dead (e.g. `vulture`).
2. A whole-repo grep including `prompts/ skills/ configs/ web/` finds no reference.
3. The full test suite is green after removal.
In this first phase, delete only when the symbol name has exactly zero whole-repo matches.

Second phase (user-approved 2026-08-23): a symbol whose name still has matches may be deleted when
every remaining match is itself dead — unused-import leftovers, orphaned fixtures, comment-only
references. Quote every such match in the PR body's `## Evidence` with the reason it is itself
dead. When any match cannot be shown dead, the phase-1 zero-match bar stands.

`vulture` is a probe you may run to surface candidates. It is never a gate and must not be added
to CI.

Step 4: consult the known-alive list.
`prompts/cron/known_alive.md` lists symbols that look dead to static tools but are reached by
string reference or kept deliberately. Read it before deleting anything; a symbol on the list must
never be deleted. Append to that file whenever you confirm a symbol is reached by string, and land
the addition in the same PR.

Step 5: open the PR.
Create at most one PR per run and report the PR URL when done. The branch is `code-health/<slug>`,
with a slug that self-describes the cleanup topic. The body names every deleted symbol and carries
an `## Evidence` section: the Step 3 command-plus-output triple for each deletion, and for a
cleanup-mode PR the vulture/grep probes plus the full-suite green line.

Step 6: review the diff on the pull request.
Run the `code-review` skill against the pull request with `--comment`, so its findings land as
inline PR comments, then act on them on the same branch before merging. The skill catches naming,
leftover references, and out-of-scope edits; judging the design direction stays with the human
reading the PR. The skill ships with the Claude CLI. A skipped review (plugin missing, run cut
short, or any other cause) MUST be reported explicitly with the reason in the run's final
summary; a silent skip is a contract violation.

Step 7: land it, or abandon it.
Wait for the checks in this run:

    gh pr checks <PR> --watch --fail-fast

Retry `--watch` a few times, several seconds apart, while it reports "no checks reported": the
workflow needs a moment to register after the push. Leave `gh pr merge --auto` out of this step:
this repository has auto-merge disabled and no required status checks, so `--auto` merges
immediately and the wait above is what gates the merge instead.

With the checks green, confirm the pull request still carries net content against a fresh `main`;
a rival run that already merged the same cleanup is a merged PR, which Step 0's open-PR check
cannot see, so this guard lives at merge time:

    git fetch origin main && git diff origin/main...HEAD --stat

A non-empty diff merges:

    gh pr merge --squash <PR>

An empty diff means the change already lives on `main`: close the pull request with
`gh pr close <PR> --comment 'code-health-abandoned: duplicate of #<N>'`, naming the landed pull
request when the `main` history identifies it.

A red check earns one fix on the same branch: read it with `gh run view --log-failed`, fix, push,
and watch again, at most twice. Abandon the pull request when it stays red after the second fix,
or when the failure comes from outside this diff:

    gh pr close <PR> --comment 'code-health-abandoned: <topic and reason>'

Name the topic in that comment, because Step 1 reads it to skip the topic on the next run. A run
that cannot finish the wait leaves the pull request open and reports that; Step 0's open-PR
reminder covers the nudge on the next run.
