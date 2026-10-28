# Improve round: landing protocol

This file is the round protocol for the five improve directions.
Each direction prompt names its own scope, topic sources and evidence.
This file carries the steps that every direction shares.

Read `skills/writing-style/genres/code.md` before writing code.
Its rules govern the diff this round produces.

`main` takes no direct pushes: every change lands through a pull request.
The pull request is the triage record, and CI is the full test run.

## Step 1: set the worktree

When the working directory is a worktree of this repository, work in it.
Run `git rev-parse --show-toplevel` and `git remote get-url origin` in that directory to test the condition.
The run then writes into the tree that the server gave it.

Otherwise create a worktree from `origin/main` and enter it:

    git fetch origin main
    git worktree add --detach ~/worktrees/improve-<direction>-<UTC time> origin/main

The round then has a tree of its own, and a server that creates the worktree later needs no change in this prompt.

## Step 2: adopt an open pull request

List the open pull requests whose head branch starts with `improve/<direction>/`:

    gh pr list --state open --limit 50 --json number,headRefName,createdAt --jq \
      '.[] | select(.headRefName | startswith("improve/<direction>/"))'

When more than one is open, take the oldest.
An adopted pull request is the whole round.
Switch the worktree to its branch.
Run Step 6 unless a review verdict comment already sits on the pull request, then run Step 7.
When its newest comment starts `improve-waiting:`, read Step 8 first.
End the round after it closes, merged or abandoned, and open no new pull request.
This rule finishes a started topic before a new one.

Go to Step 3 only when no such pull request is open, because an adopted pull request holds the round.

## Step 3: avoid in-flight files and abandoned topics

Run `tools/improve/inflight-files.sh improve/<direction>/`.
Pick a topic whose files the output does not list.
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
Complete the topic in the worktree: this round has no line cap and no file cap.
The round lands a whole topic, not a fragment.
For a fresh topic, create the branch with `git switch -c improve/<direction>/<topic slug> origin/main`.
The branch name records the topic for the abandoned-topic check.

When the topic touches `rust/`, run the local checks in the `rust/` directory:

    CARGO_TARGET_DIR=~/.cache/improve-target/<direction> nice -n 19 cargo check
    CARGO_TARGET_DIR=~/.cache/improve-target/<direction> nice -n 19 cargo test -p <crate>

Add `-p <crate>` for every crate that the change touches.
Run no other test locally, because CI runs the full suite.
The pinned target directory and `nice -n 19` keep the live server's CPU free.

## Step 5: write the evidence and open the pull request

Keep every screenshot, measurement log and other evidence file in `~/scripts/<YYYYMMDD>_improve-<direction>/`.
The worktree holds code only.

Link each evidence file in the pull request body with the file-server URL,
`<base_url>/absolute_filepath/<absolute path>`.
`skills/file-server/SKILL.md` words the URL form, and HOST MEMORY holds the CharlieBot URL as `<base_url>`.
The link opens the file for the reviewer and for you at any later time.

Push the branch and open one pull request against `main`, with the direction's evidence under a `## Evidence` heading:

    git push -u origin improve/<direction>/<topic slug>
    gh pr create --base main --head improve/<direction>/<topic slug> --title "<topic>" --body-file <body>

The CI contract check rejects a pull request without that heading.
One pull request per round is the unit that CI and the reviewer check.

## Step 6: review the diff

Write the review task file: `tools/improve/code_review_prompt.md` verbatim, then one final line `PR: <number> <url>`.
Run `charlie-code` from a scratch directory outside this worktree (`mktemp -d`).
Use the model, `api_base` and `context_window` of the `charlie-code-kimi` entry in `backends.options`:

    charlie-code --json \
      --model <model> \
      --api-base <api_base> \
      --context-window <context_window> \
      --task-file <task-file>

The scratch directory keeps the reviewer's own git checkouts away from this worktree's HEAD.
The review is the independent check that the topic is complete and correct.

The reviewer's stdout is an NDJSON event stream.
Take the verdict from the `final_output` field of its final `result` event.

Post the verdict as one pull-request comment.
The comment is the review record on the pull request, and it carries the verdict without the event stream.
Act on every finding on the same branch before the merge.
Report a review that cannot run, with its reason, in the final message.
A silent skip breaks the protocol.

## Step 7: land the pull request or abandon it

Wait for the checks in this round:

    gh pr checks <PR> --watch --fail-fast

Retry the watch a few times, several seconds apart, while it reports no checks.
The workflow needs a moment to register after the push.

Before the merge, rebase onto the latest `origin/main`:

    git fetch origin main
    git rebase origin/main

Resolve each conflict, run `git add` and `git rebase --continue`, then push with `--force-with-lease`:

    git push --force-with-lease

Wait for the checks again.
The rebase proves that the topic still applies to today's `main`.

When a conflict cannot be resolved, run `git rebase --abort` and abandon the pull request:

    gh pr close <PR> --comment 'improve-abandoned: <topic> — conflict with origin/main'

The abort restores the branch as it was before the rebase, so the worktree is left clean for the next round.
The comment records the topic for the next round.

With the checks green, confirm that the pull request still carries a diff against a fresh `main`:

    git diff origin/main...HEAD --stat

An empty diff means that the topic already landed.
Close the pull request with `improve-abandoned: <topic> — duplicate of #<N>`, naming the landed pull request.
A non-empty diff merges:

    gh pr merge --squash <PR>

Read a red check with `gh run view --log-failed <run-id>`.
The failure class selects the fix steps below.

A failure inside this diff earns at most two fixes on the same branch.
Each fix pulls the branch with `git pull --ff-only`, fixes the failure, pushes the fix, and watches the checks again.
When the check stays red after the second fix, abandon the pull request:

    gh pr close <PR> --comment 'improve-abandoned: <topic> — <reason>'

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
6. When the repair returns with main still failed, leave the pull request open with this comment, and end the round:

       gh pr comment <PR> --body 'improve-waiting: main fails <check> at <sha>, run <run-id>'

7. When the main run passed, the failure comes from this diff: apply the inside-diff fix steps.

## Step 8: repair main first when main is red

Main's CI state is the newest CI run on `main`:

    gh run list --branch main --workflow CI --limit 1 --json databaseId,status,conclusion,headSha

When that run is in progress, wait for it with `gh run watch <run-id>`.

When that run failed, repair `main` before this round continues.
A red main turns every direction's checks red, so the repair comes first.
Another direction may hold the repair already:

    gh pr list --state open --json headRefName --jq '.[].headRefName' | grep '^main-repair/'

An open `main-repair/*` pull request ends this round with no change.
All five directions share one `main-repair/*` pull request at a time, so two repairs never race.

Otherwise this round repairs `main`.
These steps return `main` to green and resume the step that needed it.

1. Read the failure with `gh run view --log-failed <run-id>`.
2. When `gh run view <run-id> --json attempt` reads 1, rerun the failed jobs once with `gh run rerun <run-id> --failed`.
   Watch the run after the rerun.
   A green rerun returns to the sending step with main green.
3. When the failure names its fix, create the branch `main-repair/<short-topic>` from the failing main sha.
   Apply the fix.
   A named fix is safe to apply without a design decision.
   A failing test without its expected value in the log names no fix, because a guessed value would hide the regression.
4. Push the branch and open a pull request.
   A red check follows the inside-diff fix steps with the same two-fix budget.
5. Confirm that the repair still carries a diff against a fresh `origin/main`.
   An emptied repair has nothing to merge, so close it and return to the sending step.
6. Merge the repair pull request with `gh pr merge --squash` once green.
   Wait for the merge's main run with `gh run watch`, then return to the sending step.
7. When the failure names no fix, or the repair pull request is abandoned,
   return to the sending step with main still failed.

When the newest comment of an adopted pull request starts `improve-waiting:`, read main's CI state first.
When the main run failed, repair main by the steps above.
When the repair returns with main still failed, end the round with no change.
When the main run passed, start at step 1 of the outside-diff steps above.

## Step 9: report once at the end

Write one message at the end of the round.
It names the topic, the pull-request link, and the evidence summary with its file links.
When the round opened no pull request, it gives the reason.
The message wakes the task session, which replies to the user.

A round that cannot finish its wait leaves its pull request open and reports that.
Step 2 adopts the pull request in the next round, so the wait resumes without a new topic.
