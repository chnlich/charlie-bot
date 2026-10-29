# Improve round: landing protocol

This file is the round protocol for the five improve directions.
Each direction prompt names its own scope, topic sources and evidence.
This file carries the steps that every direction shares.

Read `skills/writing-style/genres/code.md` before writing code.
Its rules govern the diff this round produces.

`main` takes no direct pushes: every change lands through a pull request.
The pull request is the triage record, and CI is the full test run.

Every text the round writes to GitHub — a pull-request title, body, comment, commit message or branch name — goes through `tools/improve/github-post.sh`, which checks it against the leak rules before it sends anything.
On a hit, the round removes the matched content from the text, rewrites it, and runs the subcommand again.
The script sends nothing on a hit, so the posted text is always text that passed.
`gh pr merge`, `gh pr update-branch`, `gh run rerun` and the read-only calls stay bare: they carry no text of the round.

## Step 1: set the clone

The whole round runs on the remote host, in the direction's own clone, `~/improve/<direction>/charlie-bot`.
Every command of the round runs there, so this host's profile home and runtime clone stay out of reach.
The write boundary allows writes in that clone, in the evidence directory and in two scratch directories.
The scratch directories are `/tmp/improve-<direction>/` and `/tmp/improve-<direction>-tmp/`.
Two rounds of one direction never run together: the runner's scope name, `improve-<direction>.scope`, refuses the second round.

Record the round start in UTC with `date -u +%Y%m%dT%H%MZ`; Step 5 names the evidence directory with it.

At round start, delete what a killed round left under `/tmp/improve-<direction>/`.
Build that path from the literal prefix `improve-` and the direction name, and refuse an empty direction name.
A killed round's scratch holds large copies, so the next round removes it before it starts.

The runner checks the clone before claude starts, and a missing or foreign clone ends that launch with exit 4.
End the round with a report when the clone lacks `core.hooksPath tools/git-hooks`:

    git -C ~/improve/<direction>/charlie-bot config core.hooksPath

The hooks check each commit before it can be pushed, so a clone without them is not this protocol's clone.

A dirty clone from a killed round is reset to clean before the round starts:

    git -C ~/improve/<direction>/charlie-bot reset --hard
    git -C ~/improve/<direction>/charlie-bot clean -fd

Otherwise a killed round's uncommitted edits would enter this round's commits.

Then enter the clone, fetch `origin`, and start every branch from `origin/main`:

    git fetch origin main

## Step 2: adopt an open pull request

List the open pull requests whose head branch starts with `improve/<direction>/`:

    gh pr list --state open --limit 50 --json number,headRefName,createdAt --jq \
      '.[] | select(.headRefName | startswith("improve/<direction>/"))'

When more than one is open, take the oldest.
An adopted pull request is the whole round.
Switch the clone to its branch.
Run Step 6 unless a review verdict comment already sits on the pull request, then run Step 7.
When its newest comment starts `improve-waiting:`, read Step 8 first.
End the round after it closes, merged or abandoned, and open no new pull request.
This rule finishes a started topic before a new one.

Go to Step 3 only when no such pull request is open, because an adopted pull request holds the round.

## Step 3: avoid in-flight files and abandoned topics

Read the in-flight files list in this round's launch directory.
The runner names that directory in `IMPROVE_LAUNCH_DIR`:

    "$IMPROVE_LAUNCH_DIR/inflight-files.txt"

The launcher runs `tools/improve/inflight-files.sh improve/<direction>/` on this host, because the other lines' worktrees exist only there.
Pick a topic whose files the list does not name.
A file that another branch changes would force that branch to resolve our conflict.

List the closed pull requests of this direction and read each `improve-abandoned:` comment:

    gh pr list --state closed --limit 500 --json headRefName,comments --jq \
      '.[] | select(.headRefName | startswith("improve/<direction>/"))
       | select(any(.comments[].body; startswith("improve-abandoned:"))) | .headRefName'

Read at least 500 closed pull requests: merged rounds fill a short list, and an older abandonment would drop out of it.
Skip every topic that a comment names.
The comment records a topic that already failed, and the round does not retry it.

## Step 4: finish one topic

Take one topic from the direction prompt.
Complete the topic in the clone: this round has no line cap and no file cap.
The round lands a whole topic, not a fragment.
For a fresh topic, create the branch with `git switch -c improve/<direction>/<topic slug> origin/main`.
The branch name records the topic for the abandoned-topic check.

When the topic touches `rust/`, run the checks on the remote host, in the `rust/` directory:

    CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR=~/.cache/improve-target/<direction> nice -n 19 cargo check
    CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR=~/.cache/improve-target/<direction> nice -n 19 cargo test -p <crate>

Add `-p <crate>` for every crate that the change touches.
Run no other test, because CI runs the full suite.
Four build jobs at nice level 19 leave the host's CPU to the other owners' builds.
The pinned target directory stays inside the write boundary.

## Step 5: write the evidence and open the pull request

The pull request's evidence carries numbers, commands and conclusions only.
Keep every screenshot, measurement log and other evidence file in the round's evidence directory, `~/improve/<direction>/evidence/<UTC round start, YYYYMMDDTHHMMZ>/`, one directory per round.
Hourly rounds once overwrote each other's files under a per-day path, so the timestamp separates the rounds.
The clone holds code only.

Before a `trial.sh --serve` run, set `CHARLIEBOT_RELEASE_RUN_DIR` to that round directory and export it:

    export CHARLIEBOT_RELEASE_RUN_DIR=~/improve/<direction>/evidence/<UTC round start>

The trial writes its own evidence under that directory.
Without the variable the trial writes `~/charliebot-release-logs`, which the write boundary refuses, so the step fails.

The pull request names no link to an evidence file and no host path; the evidence text stands on its own.
Links to the evidence files go only into the final message (Step 9), which stays on this host.

Push the branch and open one pull request against `main` through the posting script:

    tools/improve/github-post.sh push -u origin improve/<direction>/<topic slug>
    tools/improve/github-post.sh pr-create --title "<topic>" --body-file <body file> --base main --head improve/<direction>/<topic slug>

The body carries the direction's evidence under a `## Evidence` heading; the CI contract check rejects a pull request without it.
One pull request per round is the unit that CI and the reviewer check.

## Step 6: review the diff

Make the scratch directory with `mkdir -p /tmp/improve-<direction>` and `mktemp -d /tmp/improve-<direction>/XXXXXX`.
Write the review task file there: `tools/improve/code_review_prompt.md` verbatim, then one final line `PR: <number> <url>`.
Run `charlie-code` from that directory, outside the clone.
Use the model, API base and context window of the `claude-code-kimi-k3.1-pro` entry in `backends.options` of `$CHARLIEBOT_HOME/config.yaml`; the gateway route needs the provider prefix on the model and the `/v1` suffix on the API base:

    charlie-code --json \
      --model openai/<model> \
      --api-base <api_base>/v1 \
      --context-window <context_window> \
      --session-dir ~/improve/<direction>/clc-sessions \
      --task-file <task-file>

The round runs on the remote host.
The write boundary there denies every write outside the round's own directories.
The review's sessions therefore go to the direction's own folder.
The scratch directory keeps the reviewer's own git checkouts away from the clone's HEAD.
The review is the independent check that the topic is complete and correct.

The reviewer's stdout is an NDJSON event stream.
Take the verdict from the `final_output` field of its final `result` event.

Post the verdict as one pull-request comment through the posting script:

    tools/improve/github-post.sh pr-comment --body-file <verdict file> <PR>

The comment is the review record on the pull request, and it carries the verdict without the event stream.
Act on every finding on the same branch before the merge.
A round whose review cannot run leaves the pull request open and reports the reason in the final message.
A silent skip breaks the protocol.

## Step 7: land the pull request or abandon it

Merge only when the pull request carries a review verdict comment.
A pull request without one waits: the round that posted no verdict leaves it open, and Step 2 adopts it in the next round to run the review first.

Wait for the checks in this round:

    gh pr checks <PR> --watch --fail-fast

Retry the watch a few times, several seconds apart, while it reports no checks.
The workflow needs a moment to register after the push.

Before the merge, rebase onto the latest `origin/main`:

    git fetch origin main
    git rebase origin/main

Resolve each conflict, run `git add` and `git rebase --continue`, then push through the posting script:

    tools/improve/github-post.sh push --force-with-lease

Wait for the checks again.
The rebase proves that the topic still applies to today's `main`.

When a conflict cannot be resolved, run `git rebase --abort` and abandon the pull request.
Write `improve-abandoned: <topic> — conflict with origin/main` into a comment file and close through the posting script:

    tools/improve/github-post.sh pr-close --comment-file <comment file> <PR>

Every `improve-abandoned:` and `improve-waiting:` comment goes through the posting script.
The abort restores the branch as it was before the rebase, so the clone is left clean for the next round.
The comment records the topic for the next round.

With the checks green, confirm that the pull request still carries a diff against a fresh `main`:

    git diff origin/main...HEAD --stat

An empty diff means that the topic already landed.
Close the pull request through the posting script with the comment `improve-abandoned: <topic> — duplicate of #<N>`, naming the landed pull request, in a comment file:

    tools/improve/github-post.sh pr-close --comment-file <comment file> <PR>

A non-empty diff merges:

    gh pr merge --squash <PR>

The merge stays a bare call: its squash message comes from the commit messages and title that the posting script already checked.

Read a red check with `gh run view --log-failed <run-id>`.
The failure class selects the fix steps below.

A failure inside this diff earns at most two fixes on the same branch.
Each fix pulls the branch with `git pull --ff-only`, fixes the failure, pushes through the posting script, and watches the checks again.
When the check stays red after the second fix, abandon the pull request with `improve-abandoned: <topic> — <reason>` in a comment file, through `pr-close` as above.

Name the topic in the comment, because Step 3 reads it to skip the topic on the next round.

A failure outside this diff takes these steps.
Each step either clears the failure or names the main run that blocks the merge.

1. Update the branch from `main` with `gh pr update-branch <PR>`, and watch the new checks.
2. When `gh pr update-branch` reports no change, rerun the failed jobs with
   `gh run rerun <run-id> --failed`, and watch again.
3. When the checks pass, go to the merge above.
4. When the check is red again, read main's CI state.
5. When the main run failed, run Step 8.
   When the repair merges, start these steps again at step 1.
6. When the repair returns with main still failed, leave the pull request open with an
   `improve-waiting: main fails <check> at <sha>, run <run-id>` comment file, posted through the posting script, and end the round.
7. When the main run passed, the failure comes from this diff: apply the inside-diff fix steps.

## Step 8: repair main first when main is red

Main's CI state is the newest CI run on `main`:

    gh run list --branch main --workflow CI --limit 1 --json databaseId,status,conclusion,headSha

When that run is in progress, wait for it with `gh run watch <run-id>`.

When that run failed, repair `main` before this round continues.
A red main turns every direction's checks red, so the repair comes first.

The repair branch is the fixed name `main-repair/current`.
All five directions share one repair at a time, and the branch is the placeholder.
The merge deletes it, because the repository sets `delete_branch_on_merge`, and an abandoned repair closes with `--delete-branch`; either way the next repair can take the branch.

Before building a fix, read the branch and the open pull requests:

    git ls-remote origin refs/heads/main-repair/current
    gh pr list --state open --json headRefName --jq '.[].headRefName' | grep '^main-repair/'

When the branch exists with an open pull request, another direction holds the repair.
A second repair would race the first for the same branch, so this round waits: write an `improve-waiting: main repair held by another direction` comment file, post it on this round's pull request through the posting script, and end the round.

When the branch exists with no open pull request, the round that pushed it died before it opened one.
Check out the branch, open its pull request through the posting script, and continue at repair step 5:

    git fetch origin main-repair/current
    git switch -c main-repair/current origin/main-repair/current
    tools/improve/github-post.sh pr-create --title "<repair topic>" --body-file <body file> --head main-repair/current

A "pull request already exists" refusal means that another direction opened it first: wait as above.

When the branch is absent, this round repairs `main` with the steps below.
These steps return `main` to green and resume the step that needed it.

1. Read the failure with `gh run view --log-failed <run-id>`.
2. When `gh run view <run-id> --json attempt` reads 1, rerun the failed jobs once with `gh run rerun <run-id> --failed`.
   Watch the run after the rerun.
   A green rerun returns to the sending step with main green.
3. When the failure names its fix, create `main-repair/current` from the failing main sha, apply the fix, commit it, and push it without force:

       git switch -c main-repair/current origin/main
       tools/improve/github-post.sh push origin HEAD:refs/heads/main-repair/current

   A named fix is safe to apply without a design decision.
   A failing test without its expected value in the log names no fix, because a guessed value would hide the regression.
   A rejected push means another direction pushed its repair first: wait as above.
   The two repair commits do not contain each other, so GitHub refuses the second non-force push.
4. Open the pull request through the posting script, with `--head main-repair/current`.
   A red check follows the inside-diff fix steps with the same two-fix budget.
5. Confirm that the repair still carries a diff against a fresh `origin/main`.
   An emptied repair has nothing to merge, so close it through the posting script with `--delete-branch` and a comment file that reads `improve-abandoned: main repair — nothing left to merge`, then return to the sending step.
6. Review the repair pull request with Step 6, act on its findings, and merge it with `gh pr merge --squash` once it is green and carries its verdict comment (Step 7).
   A repair lands on `main` as well, so it gets the same review as a topic.
   Wait for the merge's main run with `gh run watch`, then return to the sending step.
7. When the failure names no fix, or the repair pull request is abandoned,
   return to the sending step with main still failed.

When the newest comment of an adopted pull request starts `improve-waiting:`, read main's CI state first.
When the main run failed, repair main by the steps above.
When the repair returns with main still failed, end the round with no change.
When the main run passed, start at step 1 of the outside-diff steps above.

## Cleanup before the report

Delete each directory that this round's `mktemp` made, through the exact path that `mktemp` returned.
Copies of session data and other large scratch live only there, and deleting by a rebuilt path risks the wrong directory.
The evidence directory stays: it holds the round's screenshots and measurement logs.
A killed round's leftovers under `/tmp/improve-<direction>/` are deleted at the next round's start (Step 1).
The runner empties `/tmp/improve-<direction>-tmp/` when the next round's scope starts, so a killed round's TMPDIR does not grow.

## Step 9: report once at the end

Write one message at the end of the round.
It names the topic, the pull-request link, and the evidence summary with the file-server links of the evidence files.
The links name the copy on this host, under `~/improve/<direction>/evidence/`, which the launcher copies back after the round.
A reader who opens a link reads that copy, because the evidence files themselves stay on the remote host.
When the round opened no pull request, it gives the reason.
The message wakes the task session, which replies to the user.
It stays on this host; the evidence-file links appear nowhere on GitHub.

A round that cannot finish its wait leaves its pull request open and reports that.
Step 2 adopts the pull request in the next round, so the wait resumes without a new topic.
