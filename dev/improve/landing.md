# Improve round: landing protocol

This file is the round protocol for the five improve directions.
Each direction prompt names its own scope, topic sources and evidence.
This file carries the steps that every direction shares.

Read `skills/writing-style/genres/code.md` before writing code.
Its rules govern the diff this round produces.

`main` takes no direct pushes: every change lands through a pull request.
The pull request is the triage record, and CI is the full test run.

Category rule: a pull request whose body names pending measurements is a draft.
Its runner marks it ready for review once its body records the results.
A round adopts only a pull request that is ready for review.
A round merges only a pull request that is ready for review.
A draft is left to its runner, and a round does not count a draft as the started topic.
When a round opens a pull request whose body names pending measurements, the pull request opens as a draft.

Every text the round writes to GitHub — a pull-request title, body, comment, commit message or branch name — goes through `$IMPROVE_KIT_DIR/dev/improve/github-post.sh`, which checks it against the leak rules and the round's credential values before it sends anything.
Every push and every `gh` call of the round passes the same credential check: the clone's pre-push hook checks each pushed commit's message and diff, and the round's `gh` checks every argument and file argument.
A credential value never enters any text the round writes, and no write reaches GitHub while the check holds a hit.
A leak-rule hit is the only hit the round rewrites: the round removes the matched content from the text, rewrites it, and runs the subcommand again.
The script sends nothing on a hit, so the posted text is always text that passed.
A credential hit stops the round. The kind that the check names is the round's stop reason, and the round writes no rewrite of the text.
A merge goes through the posting script's `pr-merge`, which refuses a merge whose head lacks a clean verdict,
refuses a diff that changes a path that origin/main has and the Step 3 list names under another branch,
refuses a web page change whose evidence directory fails the screenshot check, whose pull request body names no
`View:` line, or whose review record shows no read of a screenshot that the body names, and carries the `View:` lines
into the squash commit message. Each `View:` line follows the View contract of the Web UI Screenshots rule in
`prompts/task_base.md`.
Calls that carry no round-written text stay bare `gh` calls through `gh-shim.sh`, which checks their arguments. This includes:

- `gh api -X PUT "repos/{owner}/{repo}/pulls/<PR>/update-branch"`;
- `gh run rerun`;
- the read-only calls.

## The round's state sources

A round runs on the improve host, which has no CharlieBot server.
Its state sources are these:

- the launch directory, `$IMPROVE_LAUNCH_DIR`;
- the direction prompt, `$IMPROVE_KIT_DIR/dev/improve/<direction>.md`;
- the evidence directories under `~/improve/<direction>/evidence/`;
- git, in the direction's clone and at `origin`.

A `charliebot` command that needs the server exits non-zero in the round.
When the round's own scan or measurement cannot run, the round stops and names the missing input.
The round does not reuse an earlier round's result as its current state.

## Rust production scans

A round's scan of Rust production code goes through `$IMPROVE_KIT_DIR/dev/improve/rust-production-scan.mjs`.
The tool yields the production lines of the Rust files or directories that the scan names, each with its file and line,
and it leaves out the test items and the files under `tests/` directories.
It fails with the file and line on input it cannot parse, so an empty result means that no production line matched.
A round's report names the tool's revision for each scan it cites; the tool's stderr summary line carries that revision.

## The round's browser

The runner resolves the round's browser before the round starts: the Chrome of the Playwright build that
`remote-runner.sh` pins (`BROWSER_BUILD`) under the runner's own home. It starts that Chrome once under the write
boundary. A browser that does not start ends the launch with exit 7, and stderr names its path and the error.
The round environment names that browser in three variables:

- `IMPROVE_BROWSER`, for a round-written driver;
- `LIVE_SHOT_CHROME`, which `scripts/live-shot.mjs` reads before its own lookup;
- `POOR_NETWORK_CHROME`, which `dev/poor-network/run.sh` reads before its own lookup.

A round's browser tools start the browser these variables name. A round-written Playwright driver passes
`$IMPROVE_BROWSER` as the `executablePath` launch option. Outside a round, each tool keeps its own lookup.
A round takes no browser from `PATH`, because the system chromium there is a snap wrapper that cannot start under the
write boundary. No round changes `HOME` to find a browser.

## Web page evidence

A round takes the screenshots of a web page change from serve trials of its before and after trees on the shared mock
home, at the widths that the Web UI Screenshots rule in `prompts/task_base.md` sets, each under the input profile that
`dev/improve/ux-input-profile.mjs` gives its width. A web page change runs without a CharlieBot server on the round's
host: each serve trial runs its own server, and the screenshot check and the merge check read none.

The round writes the driver, the side's command that runs under its trial. The driver takes the screenshots and runs
the clicks.

### The shared mock home

The runner supplies the complete shared mock home before the round, at `~/improve/mock-home/<DATA_VERSION>/`.
The round names the DATA_VERSION of that home.
Both sides seed their trial from this home, and the round does not change it, so the before and after views compare.
The home has the production number of sessions, so the pages show representative content and layout.
The home binds scheduled tasks to some of its manager sessions, so a served round shows cron-bound session pages and
their Task & context dialogs.
Its sessions are task-tree nodes with a boot marker, so a serve trial's start logs no node failure.
Each default cron task that the trial seeds is bound to one of the home's sessions before the trial's server starts, so
a served round's task tree holds exactly the home's sessions. The sidebar lists only some roots of that tree.
Each side's server passes `mock-home check` before that side takes any screenshot or measurement. Exit 1 means the
server reads data outside the shared mock home, and it stops the round before any measurement.

### The serve trial

Each side is one serve trial, run through `dev/improve/trial-side.sh`. Its arguments take the form
`--serve --seed-home <the shared mock home> <commit> <server binary> -- <driver command>`, as `scripts/release/trial.sh`
reads them.

The before and after servers use the same release profile on the same host. For a web-only change, one release binary
built from `origin/main` serves both the `origin/main` tree and the branch tree, so the change under test is the only
difference between the sides. For a change that affects server code, the before server represents `origin/main` and the
after server represents the branch. The intended change is the only source difference between the sides.

The trial exports `CHARLIEBOT_SERVE_URL`, `CHARLIEBOT_SERVE_AUTH_HEADER` and `CHARLIEBOT_SERVE_PID` to the driver. The
driver makes every request and process read through that trial's server.

The driver writes each screenshot into the round's evidence directory (Step 5). Its other raw output goes to
`IMPROVE_SIDE_OUTPUT`, and the side record stays in the round's evidence directory.

### The lock of a side

Only an improve-ux side runs beside the bench lock. Its entry runs with `IMPROVE_TRIAL_USE_LOCK=0`, and
`dev/improve/trial-side.sh` refuses that value for every other direction. Screenshots and clicks do not depend on a
quiet host, and the bench's scopes keep CPU priority over the improve scopes, so the side does not take the bench's CPU.
The side's evidence records the bench-lock holder (the process that holds the lock, or `none`) at the side's start and
at its end.

Every other direction's screenshot side holds the bench lock from trial startup through its screenshots, its clicks and
trial shutdown. It keeps the default of `IMPROVE_TRIAL_USE_LOCK`. A side that reports any timing or byte-over-time
measurement holds the lock the same way, because a timing taken beside a bench measures that bench's CPU contention.

A side that holds the bench lock waits at most the lock's wait limit, 15 minutes in a round. If the wait expires, that
side takes no screenshot or measurement. The round reports no result from an incomplete pair, stops without opening a
pull request, and names the held bench lock in its stop reason. The round reports the stop, and a later fire takes the
topic again.

### Input profile, clicks and writes

Every capture and click runs under the input profile of its window size, which `dev/improve/ux-input-profile.mjs` gives:
a desktop size reports a fine pointer that hovers, and the phone size reports touch with no hover. The driver's browser
starts with `INPUT_BROWSER_ARGS` from that module, and the driver checks each size's profile on the loaded page before
its first capture at that size. A page whose hover and pointer answers differ from the profile is not captured.

Every click a driver makes goes through the click helper, `dev/improve/ux-click-target.mjs` (`clickTarget`). The helper
resolves the control's selector at the moment of each click, so a click never lands on a position that an earlier load
or list recorded. The helper refuses a click in these cases:

- no box with a nonzero size appears within the wait that the driver states;
- the center of the box lies outside the viewport;
- another element covers the center of the box.

Each refusal names the selector and the reason, and a covering refusal also names the covering element. A refused click
is a driver result, and the evidence records it as one.

The round records each click's result in the evidence. A result names whether the click gives a visible effect on that
screen (a state change, a message or a navigation), and the non-GET requests and the WebSocket frames the click sends.
A control that the mock home cannot exercise, or that the screen shows as disabled, is recorded as not clicked, with its
reason.

The clicks run only against a serve trial on the shared mock home. Each write a click makes lands in that trial copy,
which has its own home, port and process tree, so the screen shows what it shows after a write there. Two rules decide
which writes start work, and each names what enforces it:

- A WebSocket write is dropped at send time when the driver installs the kit's frame guard
  (`dev/improve/ux-frame-guard.mjs`, `installWriteGuard`) before the page's first navigation. The kit does not launch
  the driver, so the install is the driver's duty. With the guard installed, every frame a page sends is dropped except a
  session socket's `subscribe` frame, so chat input and terminal input do not reach the server. Server frames and the
  page's handlers keep receiving, so the page's reads keep flowing. The guard records each dropped frame on the page
  (`globalThis.__uxWriteGuard.dropped`), and the click result names those frames.
- An HTTP write is one non-GET request. The driver intercepts each one a click issues and records it in the evidence.
  The kit does not check that intercept.

The kit does not keep a terminal socket from opening. A terminal connection starts a tmux session and a login shell in
the trial's own tmux server. The trial stops that server at teardown and fails the side when a process of its tree
remains. A seeded cron task is disabled and bound to a session of the home before the trial's server starts, and read
back as disabled after it. The scheduler binds nothing, so the mock-home check requires the served session list to equal
the manifest's.

A WebSocket is not answered with a stub, because a stub that stops the socket's reads freezes the page under test.

### Screenshots

For each width that the Web UI Screenshots rule names, the driver takes a before and an after screenshot of each state
the change alters, and an after screenshot of each state the change adds. Each pair shows the same view before and after
the change, so the reader can see the user-facing difference. The driver starts the browser that `$IMPROVE_BROWSER`
names (The round's browser, above).

## The round's login and the launch's exit codes

The launch forwards the member's login for one round only.
A subscription member's access token travels to the host with the round, and its refresh token stays on the launching host.
The host keeps the token in the launch's `member-credential` file, mode 600, and the runner removes that file when the round ends.

- A Claude subscription round receives the token as `CLAUDE_CODE_OAUTH_TOKEN`.
- A codex round runs under `CODEX_HOME=/tmp/improve-<direction>/codex-home`, whose `auth.json` holds the access token and an empty `refresh_token`.
  The round uses that per-round login only inside its scope; the refresh token stays on the launching host. The runner
  removes `auth.json` when its round ends or a later start finds no live round. Symlink and ownership checks protect
  the directory and its parent before cleanup or use.
- A gateway member receives its token as `ANTHROPIC_AUTH_TOKEN`.
- A `cc-openai-compatible` member never reaches a round. The server refuses its option at save, at fire and at round start: its model is reached through the server's own proxy route, which a host cannot reach, and its Claude Code token is the server's access key.

The launch exits 8 before it contacts the host when the login's access token has less life left than `IMPROVE_TOKEN_MIN_SECONDS`, and its stderr names the login and the minutes left.
The launch exits 7 when a round dependency is missing, and its stderr names the missing item.
Neither exit starts a round.
An exit 8 moves the round to another pool member when the pool has one; an exit 7 does not, because another member would miss the same item.

## A dropped connection

The connection that carries a round can drop while the round runs.
The round runs on the improve host under its own scope, and a drop leaves that round as it was:

- A drop neither stops the round nor starts a second round of its direction.
- When the host answers again, the launch waits for the round's end, copies its evidence, and returns the round's exit status and final output.
- When the host stays unreachable past `IMPROVE_REATTACH_SECONDS` (1800 seconds by default), the launch exits 3.
- When the round's runner ends with no status, the launch exits 9.

Each failure message names the launch id.
The round's result stays on the host in `~/improve/results/<launch id>/`, as `status` and `output`.

## The member CLI version

A round runs the version of its member CLI that the server host runs, so any model that the server host's CLI serves also works on the improve host.
The launch reads that version from the agent binary's `--version` output: claude prints `X.Y.Z (Claude Code)` and codex prints `codex-cli X.Y.Z`.
The launch exits 7 before it contacts the host when that output names no version.

The runner installs that exact version from the vendor's distribution, and no other CLI on the improve host runs a round:

- Codex: the standalone package `codex-package-<arch>-unknown-linux-musl.tar.gz` of the GitHub release `rust-v<version>`. Its sha256 is the digest that the release's asset list publishes. The whole package is unpacked with its layout: `codex-code-mode-host` and `codex-exec-server` sit beside the executable in `bin/`, because codex starts them for tool calls. A codex install that copied only the executable failed every tool call on 2026-11-20 04:52 UTC.
- Claude Code: the native binary of the version's release, for `linux-arm64` or `linux-x64`. Its sha256 is the digest that the version's `manifest.json` publishes.

The install goes into `~/improve/runner/members/<kind>-<version>/` under the runner's build lock, before the write boundary starts.
The round's PATH puts that directory's `bin` first, so `claude` and `codex` never resolve to a copy in `~/.local/bin`.
The runner then checks the installed CLI's `--version` against the version the launch read.
When the install or the check fails, the runner exits 7 and its stderr names the local version and the host's version (`none` when nothing is installed). No round starts, and no other CLI is tried.

Retention bounds the disk: each kind keeps the version of its last round and every version that a live round names, and removes the rest when a round installs.
The round tools under `~/improve/runner/bin/` keep the current commit and the commits that live rounds name.

`remote-runner.sh --member-cli <kind> <version>` runs the same install alone, prints what the installed CLI reports and starts no round, so an operator can check a version before a round needs it.

These scripts run from the server's checkout at each launch, so a change reaches rounds on the remote test host when the server runs a release that carries it.

## Step 1: set the clone

The round runs on the remote host. Topic work uses the direction's clone,
`~/improve/<direction>/charlie-bot`; Step 6's reviewer uses a separate scratch clone.
This keeps this host's profile home and runtime clone out of reach.
The write boundary allows writes in that clone, in the evidence directory and in two scratch directories.
The scratch directories are `/tmp/improve-<direction>/` and the session TMPDIR, `/tmp/improve-<direction>-tmp/`.
Two rounds of one direction never run together: the runner's scope name, `improve-<direction>.scope`, refuses the second round.

Record the round start in UTC with `date -u +%Y%m%dT%H%MZ`; Step 5 names the evidence directory with it.

The runner makes the round's `TMPDIR` as a new directory under the session TMPDIR, so the round's tool output has its own directory.
A round deletes a path only when the round created that path with `mktemp -d` during the same round.
No round removes either scratch directory by name.
The runner owns `codex-home/` under `/tmp/improve-<direction>/`, and a round leaves it in place.
Each round's `TMPDIR` goes when the next round of its direction starts: before the round's agent starts, the runner removes the `TMPDIR` directories that earlier rounds made.
Each review's scratch directory sits under one of those `TMPDIR` directories.
The kept review record (Step 6) lives in the evidence directory, which this removal never touches.

The kit is the set of `dev/improve/` documents and scripts that this protocol names, and the `dev/` scripts that those scripts source.
The round reads and runs the kit from `$IMPROVE_KIT_DIR`, which holds the `dev/` tree of the revision that the runner names in `IMPROVE_KIT_REVISION`.
That revision is the one that the round's charliebot is built from, and the runner refuses a round whose kit and charliebot differ.
The clone is not the kit's source: the clone holds the round's own edits, so every `dev/improve/` path that the round reads or runs means its copy under `$IMPROVE_KIT_DIR`.
Step 9 names the kit revision.

The skills are the `skills/` tree of that same revision, and `$IMPROVE_SKILLS_DIR` holds them.
Every skill file the round reads comes from `$IMPROVE_SKILLS_DIR`, whatever text names it and whatever path that text gives.
Any other copy of a skill, in a host's skill directory, a checkout or the clone, may differ from this revision.
A pull request does not merge while the round's event log or its review record reads a skill file from any other copy, and pr-merge names each such path.

Before the round's agent starts, the runner sets the clone to `origin/main`, the base of the round's topic: it fetches `origin` and detaches the clone's HEAD there.
Git runs the clone's own `dev/git-hooks`, so the hooks come from that checkout and not from the kit.
The runner keeps the clone's local branches and its uncommitted changes.
A checkout that would overwrite uncommitted work ends the launch with exit 4.

The runner then ends the launch with exit 4 when the clone:

- is missing;
- does not set `core.hooksPath` to `dev/git-hooks`;
- has no executable `dev/git-hooks/pre-push` in its tree;
- has an origin that is not `charlie-bot`.

The hooks check each commit before it can be pushed, so a clone without them is not this protocol's clone.

A dirty clone from a killed round is reset to clean before the round starts:

    git -C ~/improve/<direction>/charlie-bot reset --hard
    git -C ~/improve/<direction>/charlie-bot clean -fd

Otherwise a killed round's uncommitted edits would enter this round's commits.

Then enter the clone, fetch `origin`, and start every branch from `origin/main`:

    git fetch origin main

## Step 2: adopt an open pull request

List the open pull requests whose head branch starts with `improve/<direction>/` and that are ready for review (category rule):

    gh pr list --state open --limit 50 --json number,headRefName,createdAt,isDraft --jq \
      '.[] | select(.headRefName | startswith("improve/<direction>/")) | select(.isDraft | not)'

When more than one is open, take the oldest.
An adopted pull request is the whole round.
Switch the clone to its branch.
When the newest verdict comment (Step 6) names the current head and reports no issues, run Step 7.
Otherwise run Step 6: a head without a verdict gets the freshness step and its review, and a head whose verdict reports findings starts with its fixes.
When its newest comment starts `improve-waiting:`, read Step 8 first.
End the round after it closes, merged or abandoned, and open no new pull request.
This rule finishes a started topic before a new one.

Go to Step 3 only when the list is empty, because an adopted pull request holds the round, and a draft is not adopted.

## Step 3: avoid in-flight files and abandoned topics

Read the in-flight files list in this round's launch directory.
The runner names that directory in `IMPROVE_LAUNCH_DIR`:

    "$IMPROVE_LAUNCH_DIR/inflight-files.txt"

The launcher runs `dev/improve/inflight-files.sh improve/<direction>/` on this host, because the other lines' worktrees exist only there.
The runner also adds the files that the other directions' clones change on the remote host, because those clones are standalone repositories that the launcher cannot see.
The list names live work only, and every source of it — this host's worktrees and the remote clones included — follows one contract: a source claims paths only while live work holds them.
A branch commit is landed when main carries its patch; otherwise it is unmerged.
An open pull request claims its files, whichever checkout holds its branch.
A worktree claims the paths of its unmerged commits since its merge base, and its uncommitted and untracked paths, while a task's record names it and that task is open, or while a process runs in the worktree.
A worktree that no record names keeps its claim while its branch has a commit ahead of origin/main, or while one of its changed, uncommitted or untracked paths changed within the window that `inflight-files.sh` names, because operator work carries no record; otherwise it names no path.
A worktree whose owning task is closed and whose branch has no open pull request names no path, whether its branch landed or not; the worktree stays on disk.
A task owns a worktree when the task store names its path, a run record under `data/runs/` or a task record under `threads/`, and the owning session's event history, not a run's end, decides whether that task is open.
A clone holds one checkout, so its claim is the work of the branch that checkout carries: the paths of the checked-out branch's unmerged commits since its merge base and the clone's uncommitted and untracked paths are claimed while that direction's round runs — the runner's own round state, or a process in the clone, is the live signal — and a branch that the checkout does not carry claims no path; the branch stays in the clone, and its open pull request, when one exists, is already on the list through the pull-request source.
A clone or record that cannot be read stops the list with an error naming the condition.
The runner's own checkout names nothing at all.
Both computations fetch `origin/main` before they compare a branch against it, because a stale ref would make merged work look live.
The fetch goes into a ref the reading side owns, `refs/improve/inflight/<direction>` in each clone and `refs/improve/inflight/<branch prefix>` in this host's main checkout, and never into `refs/remotes/origin/main` or `FETCH_HEAD`: the directions start in a burst, so concurrent launches, rounds and operators fetch those shared refs themselves.
Pick a topic whose files the list does not name on a path that origin/main has.
A file that another branch changes would force that branch to resolve our conflict.
The claim check covers the final diff, not only the topic's planned files.
`pr-create` and `pr-merge` read this same list and refuse a diff that changes a path
that origin/main has and the list names under another branch.
Each refusal names the claimed path and its claimant, and nothing is sent or merged.
The round keeps claimed paths out of its diff rather than sending past a refusal.
The list is the launch's snapshot, so a claim made after the launch is not in it.
A path whose only changes are landed commits is not listed. Every listed path prints as `path<TAB>branch`, naming the branch that carries it, and a claim on a path that origin/main has no object at gets a third field, `absent-on-origin-main`:
- uncommitted work, with the branch of its dirty worktree;
- an unmerged commit's path, with that commit's branch;
- an open pull request's file, with its head branch.

A claim on a path absent from origin/main does not block a topic; the list marks it `absent-on-origin-main`.
The round names each such claim it passes, with its branch, in its report (Step 9).
The pull request body carries no such claim.

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

A change that moves, renames or deletes code or a file updates every reference to the old name or path in the
repository, in the same pull request. Docs, comments, tests and CI path maps count, and a direction's Scope does not
limit them. Recorded data keeps its old text: a fixture, a golden or a captured reply, such as
`dev/poor-network/reply-fixture.txt`. The pull request body names each line of recorded data that keeps an old name,
as `<path>:<line>`.
`pr-create` refuses a diff that leaves a deleted or renamed path named outside recorded data, or a recorded line that
the body does not name, and it names each such line (`dev/improve/moved-references.sh`). That check spells each path in
full, so a bare file name or a renamed symbol is the round's to find.
A reference in a file that another branch claims blocks the pull request, as Step 3 states.

When the topic touches `rust/`, run the checks from the `rust/` directory of the clone, on the remote host where
  the round already runs.
The remote host is the host that the host rule names for agent compiles, so the checks satisfy that rule.
The checks reach no other host, so the round uses neither the remote-run runner (`dev/remote-run/`) nor an ssh
  connection.
The round's profile home, `~/improve/home`, leaves `remote_run.host` unset by design.
Finding it unset is expected, and the round runs its checks directly in the clone.
The checks use the direction's pinned target directory, `~/.cache/improve-target/<direction>`, which the write
  boundary allows.
The round runs `cargo check` for the workspace and `cargo test` for each crate that the change touches.
The round limits its tests to those crates, because CI runs the full suite.
A test that fails inside the write boundary because the boundary refuses a path is reported as not run, with the refused
path named. The pull request's CI result decides for that test; the round reports it neither as passed nor as a round
defect.
The builds run with four build jobs at nice level 19, so the host's CPU stays with the other owners' builds.

## Step 5: write the evidence and open the pull request

The pull request's evidence carries numbers, commands and conclusions.
Keep every screenshot, measurement log and other evidence file in the round's evidence directory, `~/improve/<direction>/evidence/<UTC round start, YYYYMMDDTHHMMZ>/`, one directory per round.
Hourly rounds once overwrote each other's files under a per-day path, so the timestamp separates the rounds.
The clone holds code only.

Before a serve trial side runs, set `CHARLIEBOT_RELEASE_RUN_DIR` to that round directory and export it:

    export CHARLIEBOT_RELEASE_RUN_DIR=~/improve/<direction>/evidence/<UTC round start>

The side record stays in that directory. The trial and its measurement command write their raw output in the side output
location named by the record.
Without the variable the trial writes `~/charliebot-release-logs`, which the write boundary refuses, so the step fails.
The round's runner sets `CHARLIEBOT_TRIAL_PROBE_ANSWER` to the launch's probe answer, so the trial takes its probe model from that file rather than resolving `backends.calls.probe`, which only a server does.

Each serve trial side leaves one side record in the round directory, and the record names that side's unique raw output location.
The round keeps every side record, including the records of sides it discards.
A timing side writes all of its raw output, the trial's own output included, to `IMPROVE_SIDE_OUTPUT`, so no later side overwrites it.
The record fields are listed in the header of `$IMPROVE_KIT_DIR/dev/improve/trial-side.sh`.

The pull request names each screenshot by its file name and width, at the widths and states that the Web UI Screenshots rule in `prompts/task_base.md` sets.
The pull request names no link to an evidence file and no host path; the evidence text stands on its own.
A pull request that cites a timing or memory figure names each side record that produced the figure.
Each cited record has its own line, which starts with `Side record:` and ends with the record's file name.
The sentence that states a figure names the side that produced it.
Each cited record is complete, and its raw output location holds the side's output.
A ready pull request names at least one side record when the round's evidence directory holds side records.
Step 9 carries the evidence files to the user.

Push the branch and open one pull request against `main` through the posting script:

    $IMPROVE_KIT_DIR/dev/improve/github-post.sh push -u origin improve/<direction>/<topic slug>
    $IMPROVE_KIT_DIR/dev/improve/github-post.sh pr-create --title "<topic>" --body-file <body file> --evidence-dir <the round's evidence directory> --base main --head improve/<direction>/<topic slug>

When the body names pending measurements, add `--draft` to the pr-create command (category rule).

The body carries the direction's evidence under a `## Evidence` heading; the CI contract check rejects a pull request without it.
One pull request per round is the unit that CI and the reviewer check.

## Step 6: make the head current, then review the diff

The freshness step is the rebase onto the latest `origin/main` and the checks that follow it.
The round runs the freshness step before every review, so each verdict reads the head that merges.

The freshness step has three parts:

- Rebase the branch onto the latest `origin/main`.
  When the rebase moves the head, push the rebased head through the posting script with `--force-with-lease`.
  The rebase proves that the topic still applies to today's `main`.
  A conflict that the round cannot resolve abandons the pull request, as the abandon rule of Step 7 describes.
- Wait for the checks of the head.
  Checks that a push has not registered yet read as empty, so the round reads them again until they appear.
  A red check takes the fix steps of Step 7, and the fixed head runs the freshness step again.
- With the checks green, confirm that the pull request still carries a diff against `origin/main`.
  An empty diff means that the topic already landed: close the pull request as a duplicate through the posting script,
  with the comment `improve-abandoned: <topic> — duplicate of #<N>`, naming the landed pull request.

The review is an independent check that the topic is complete and correct.
The review runs on the backend that `backends.calls.improve_review` selects in the host's `config.yaml`.
The round runs the review through `charliebot backend run`, and names no model, option or agent CLI for the review.
`option.json` in the review launch directory already holds the resolved backend for `backends.calls.improve_review`; its
`login` variables carry that backend's login.
Pass `--launch "$IMPROVE_LAUNCH_DIR/review"` without `--call` or `--backend`.
The review's task file is a copy of `$IMPROVE_KIT_DIR/dev/improve/code_review_prompt.md` that the round writes in the
review's scratch directory, outside the clone.
The round appends to the copy a `PR:` line that names the pull request by its number, as the copy's last line.
When the pull request names screenshots, the round appends an `EVIDENCE:` line before the `PR:` line, naming the
round's evidence directory, whatever the round's direction.
The repository file stays unchanged, so the `PR:` and `EVIDENCE:` lines exist only in the copy.
The review's credential reaches only the review process.
The review's working directory is a scratch directory outside the clone, and it is not a git checkout.
The review prompt has the reviewer clone the repository into that directory, and the reviewer's git commands run only in that clone.
The review's `gh` calls name the repository, because gh cannot resolve it from the scratch directory.
The round makes the scratch directory with `mktemp -d` under its TMPDIR.
The write boundary allows the scratch directory and the session directory, and denies every other write.
The verdict is the review's final text on stdout.
The raw event stream stays in the review's working directory as `events.ndjson`.
When each review ends, whether it passed or failed, the round copies that `events.ndjson` to `review-events.ndjson` in the round's evidence directory.
A later review of the same round replaces that copy, so the kept copy is the record of the round's last review.
The kept copy stays with the evidence directory after the round, and Step 7 reads it at the merge.
A review that fails names its reason and its resume handle on stderr, and the round quotes that reason verbatim in the final message.
Each review's scratch directory and its `events.ndjson` stay on disk until the next round's start (Step 1), whether the review passed or failed.
A failed review's scratch directory and its `events.ndjson` are named in the round's final message.

The round posts each verdict as one pull-request comment through `$IMPROVE_KIT_DIR/dev/improve/github-post.sh pr-comment`, with `--verdict-head` naming the full head sha that the review read.
The script refuses a head that is not the pull request's full current head sha, and posts nothing on a refusal.
The verdict text ends with one line, `Round start: <the round's start stamp>`, which names the round's evidence directory.
That comment is the verdict comment: it carries the verdict without the event stream, and `pr-merge` reads its head sha.
A verdict is clean when it reports no issues; otherwise it reports findings.
A verdict with findings is acted on: the round fixes every finding on the same branch, pushes the fix, runs the freshness step on the fixed head, and reruns this same review, with the same launch, once on that head.
A round runs at most two reviews, so that rerun is its last review.
When the rerun reports findings, the round leaves the pull request open.
Its verdict comment names the head and the open findings, and Step 2 adopts the pull request in the next round.
A review that cannot run or fails leaves the pull request open, and the round reports the reason in its final message.
One foreground tool call ends at 15 minutes. A review of any length completes: the round waits past that limit and reads back its outcome.
Every wait of a round stays inside the round's run until the verdict is posted and the merge or no-PR outcome is reached.
The round has no wake: no trigger registers for it.
A silent skip breaks the protocol.

## Step 7: land the pull request or abandon it

Merge only when the merge head carries a clean verdict and the pull request is ready for review (category rule).
Every merge head has passed the freshness step and its review, so the checks that the freshness step watched cover it.
A head change after the verdict makes that verdict stale: a review fix, a rebase, an update from main, a CI fix or any other push.
A pure update from main with no conflict is a head change too, and it keeps no earlier verdict.
The new head then goes through Step 6, and the round counts that review against its two reviews.
When the round has no review left for the new head, it leaves the pull request open and ends the round; Step 2 adopts it in the next round.
When the pull request is a draft, leave it open for its runner and end the round after its verdict.

Before the merge, the pull request's title and body describe its final commits.
When the branch gained commits after the description was written, update the title and body through `$IMPROVE_KIT_DIR/dev/improve/github-post.sh pr-edit`.
Keep the body's `## Evidence` section.

Read GitHub's `mergeable` state of the pull request. When GitHub has not computed it yet, read it again.

- When GitHub reports the pull request mergeable, merge the reviewed head without a rebase.
  Mergeable means that the topic still applies to today's `main`, which is the proof that the freshness step's rebase
  gives. GitHub's squash applies the head to the current `main` at merge time.
- When GitHub reports a conflict with `main`, rebase onto the latest `origin/main`, resolve each conflict, and push the
  resolved head through the posting script with `--force-with-lease`.
  The resolved head is a head change: it goes through Step 6, and its review counts against the two reviews.
- When a conflict cannot be resolved, abort the rebase and abandon the pull request: write
  `improve-abandoned: <topic> — conflict with origin/main` into a comment file and close it through the posting script's `pr-close`.
  The abort restores the branch as it was before the rebase, so the clone is left clean for the next round.

Every `improve-abandoned:` and `improve-waiting:` comment goes through the posting script.
The comment records the topic for the next round.

The merge names the evidence directory of the round whose newest verdict names the merge head.
That round is this round, or, for an adopted pull request or a merge after the round ends, the round that the newest verdict comment's `Round start:` line names.
That directory is `~/improve/<direction>/evidence/<Round start>/` on the host that runs the merge.

- The merge names that directory with `--evidence-dir`.
- A pull request whose body names a screenshot file also passes `--review-record <that directory>/review-events.ndjson`.
  The check reads the record for a web page change, and it refuses a web page change whose body names no screenshot.
- A pull request whose body names no screenshot file merges without `--review-record`.

    $IMPROVE_KIT_DIR/dev/improve/github-post.sh pr-merge --evidence-dir <that directory> [--review-record <that directory>/review-events.ndjson] <PR>

On success, pr-merge prints the PR number, the reviewed head, the merge commit and the state that GitHub reports after the merge. Exit 4 means the merge was sent and GitHub does not show it merged at the reviewed head.

When the newest verdict comment has no `Round start:` line, or that directory is missing, or the body names a screenshot file and the record is missing, the merge needs a new review.
The round reviews the head again (Step 6) before the merge, and the merge then names this round's directory.
When a merge outside a round needs that new review, it leaves the pull request open, and Step 2 of the next round adopts it.

The merge checks the verdict on its head, a web page change's screenshots, and the review record's reads of the
screenshots that the body names. Its squash message comes from the pull request's title and body, which the posting
script checks before it sends them.

Read a red check with `gh run view --log-failed <run-id>`.
The failure class selects the fix steps below.

A failure inside this diff earns at most two fixes on the same branch.
Each fix pulls the branch with `git pull --ff-only`, fixes the failure, pushes through the posting script, and watches the checks again.
The fixed head then goes through Step 6 again.
When the check stays red after the second fix, abandon the pull request with `improve-abandoned: <topic> — <reason>` in a comment file, through `pr-close` as above.

Name the topic in the comment, because Step 3 reads it to skip the topic on the next round.

A failure outside this diff takes these steps.
Each step either clears the failure or names the main run that blocks the merge.

1. Update the branch from `main` with `gh api -X PUT "repos/{owner}/{repo}/pulls/<PR>/update-branch"`, and watch the new
   checks. `gh 2.45` has no `pr update-branch` command, so use the REST endpoint. It merges `main` into the pull request
   branch and keeps its existing commits in the review scope.
2. When the endpoint reports that no update is needed, rerun the failed jobs with
   `gh run rerun <run-id> --failed`, and watch again.
3. When the checks pass, the head goes through Step 6 when it has no clean verdict, and to the merge above when it has one.
4. When the check is red again, read main's CI state.
5. When the main run failed, run Step 8.
   When the repair merges, start these steps again at step 1.
6. When the repair returns with main still failed, leave the pull request open with an
   `improve-waiting: main fails <check> at <sha>, run <run-id>` comment file, posted through the posting script, and end the round.
7. When the main run passed, the failure comes from this diff: apply the inside-diff fix steps.

## Step 8: repair main first when main is red

Main's CI state is the newest CI run on `main`:

    gh run list --branch main --workflow ci.yml --limit 1 --json databaseId,status,conclusion,headSha

`--workflow` takes the workflow's file name: `ci.yml` resolves, and the display name `CI` does not.

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
Check out the branch, open its pull request through the posting script with the round's evidence directory, and continue
at repair step 5:

    git fetch origin main-repair/current
    git switch -c main-repair/current origin/main-repair/current
    $IMPROVE_KIT_DIR/dev/improve/github-post.sh pr-create --title "<repair topic>" --body-file <body file> --evidence-dir <the round's evidence directory> --head main-repair/current

A "pull request already exists" refusal means that another direction opened it first: wait as above.

When the branch is absent, this round repairs `main` with the steps below.
These steps return `main` to green and resume the step that needed it.

1. Read the failure with `gh run view --log-failed <run-id>`.
2. When `gh run view <run-id> --json attempt` reads 1, rerun the failed jobs once with `gh run rerun <run-id> --failed`.
   Watch the run after the rerun.
   A green rerun returns to the sending step with main green.
3. When the failure names its fix, create `main-repair/current` from the failing main sha, apply the fix, commit it, and push it without force:

       git switch -c main-repair/current origin/main
       $IMPROVE_KIT_DIR/dev/improve/github-post.sh push origin HEAD:refs/heads/main-repair/current

   A named fix is safe to apply without a design decision.
   A failing test without its expected value in the log names no fix, because a guessed value would hide the regression.
   A rejected push means another direction pushed its repair first: wait as above.
   The two repair commits do not contain each other, so GitHub refuses the second non-force push.
4. Open the pull request through the posting script, with `--head main-repair/current`.
   A red check follows the inside-diff fix steps with the same two-fix budget.
5. Confirm that the repair still carries a diff against a fresh `origin/main`.
   An emptied repair has nothing to merge, so close it through the posting script with `--delete-branch` and a comment file that reads `improve-abandoned: main repair — nothing left to merge`, then return to the sending step.
6. Review the repair pull request with Step 6, act on its findings, and merge it as Step 7 states once it is green and its merge head carries a clean verdict.
   A repair lands on `main` as well, so it gets the same review as a topic.
   Wait for the merge's main run with `gh run watch`, then return to the sending step.
7. When the failure names no fix, or the repair pull request is abandoned,
   return to the sending step with main still failed.

When the newest comment of an adopted pull request starts `improve-waiting:`, read main's CI state first.
When the main run failed, repair main by the steps above.
When the repair returns with main still failed, end the round with no change.
When the main run passed, start at step 1 of the outside-diff steps above.

## Step 9: report once at the end

Before the round ends, write the round's final report to `report.md` in its evidence directory.
The report carries the same content as the final message, so it survives a lost final message.
The evidence directory stays after the round: it holds the round's screenshots, measurement logs and review record.

Write one final message at the end of the round.
It names:

- the topic;
- each dropped worktree and its newest change time from `"$IMPROVE_LAUNCH_DIR/inflight-files-notices.txt"`;
- each claim marked `absent-on-origin-main` that the round passed, with its branch, as Step 3 requires;
- the kit revision, which the round reads from `IMPROVE_KIT_REVISION`;
- the pull-request link;
- the evidence summary;
- for a pull request that names screenshots, each named screenshot and whether the review read it, as its review record shows;
- the absolute path of the round's evidence directory on the remote host.

It names no file-server URL, because the remote host holds no CharlieBot URL and the round constructs none.
The message wakes the task session, which replies to the user.

The task session mints the file-server links for the report from the copy that the launcher places at `~/improve/<direction>/evidence/<round>/` on the task host after the round.
Its reply carries those links, and a reader who opens one reads that copy, because the evidence files themselves stay on the remote host.
No evidence link appears on GitHub.

When the round opened no pull request, it gives the reason.
A round that cannot finish its wait leaves its pull request open and reports that.
Step 2 adopts the pull request in the next round, so the wait resumes without a new topic.
