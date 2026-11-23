---
name: charliebot
description: CharlieBot repo structure, architecture, and development conventions. Use when modifying charlie-bot code. Also use when operating CharlieBot, for delegation and running-worker amendments, run records and usage audits, the Claude account pool, and CLC (charlie-code) runs.
version: 1.0.0
---

> For CharlieBot capabilities (delegation, /improve, triggers, etc.), see prompts/master.md — that content is auto-loaded into every master agent session.

# CharlieBot

You are CharlieBot. This skill describes your own features so you can use them correctly.
Source code: the server and the `charliebot` CLI are `rust/crates/`.

---

## Worker & Review Workflow

**Source:** `rust/crates/runtime/tasks/src/execution.rs`, `rust/crates/runtime/tasks/src/spawner_prompt.rs`, `rust/crates/runtime/tasks/src/spawner_backends.rs`

1. Worker runs in isolated git worktree (`~/worktrees/`)
2. Branch naming: `charliebot/task-{timestamp}-{id}`
3. On worker success → review uses its configured target or the worker route fallback. It prefers a selectable backend
   outside the work run and earlier work-run backends, then tries the writer first and another previously used member.
   A reviewer that already failed this work run stays excluded from its retries.
4. A reviewer lands the task with `charliebot land`. On a gated landing, a single-commit branch supplies the PR title
   from its subject and the PR body from its body. A branch of several commits lands with an explicit `--title` and
   either `--body` or `--body-file`, both written by `~/.claude/skills/writing-style/genres/commits-and-prs.md`.
   `--body` and `--body-file` are exclusive. An existing open PR is synchronized to the landed title and body, and a
   new draft PR uses them. A blank title or unreadable or blank body file fails before landing. An ungated landing
   accepts these values and leaves them unused, except that a diff that changes a web page checks its body as below. When the base is the default branch and declares a
   `pull_request` workflow, the command moves the base only when its CI gate passes on the landed commit. When that
   gate does not apply, the landing does not wait for CI and reports `landed-no-ci`. `landed` and `landed-no-ci` exit
   0 and leave the task branch's PR merged or closed. Every other state leaves the change unlanded and names what
   holds it. Before any push, a diff that changes a web page (a file that a CharlieBot page serves, or a page template
   directly under `prompts/`) is refused unless its PR body names the approved demo on a `Demo:` line, the changed view's URL path on
   a `View:` line, and a screenshot at each CSS window size `charliebot land --help` lists by absolute path. Each
   `View:` line is written into the landed commit as a trailer, which the release switch reads back to capture the live
   page.
5. On landing → master agent gets summary

### Merge-back failover

`charliebot land` binds to the calling run's session: the server writes `CHARLIEBOT_SESSION_ID` into every run's
environment, and the command refuses a `--task` that names a different session, so only the task's own run — its
reviewer — lands it. A blocked landing after a moved base, failed or unstarted checks, or a rebase conflict keeps the
work branch, PR, and worktree. A manager recovers a blocked landing by a new implement delegation that carries the kept
branch's commits onto the current base; the new task's reviewer lands it, resolving any rebase conflict inside that
task. Never hand-push.

A landing also refuses before any push when a file its branch changes holds a hardcoded backend choice: the refusal
names each `file:line` and the matched value. The call takes its backend from `backends.calls.<class>` through
`charliebot backend resolve`.

When the CI gate applies, delivery blocks a base-branch commit without a `landing.json` record in state `landed`.

Keep the runtime checkout (the clone the server runs from) at its current commit. The live server and its agents then use the files of the live release.
The live server and its agents read these files from the checkout:
- prompt documents and skills;
- page templates under `web/templates`;
- `web/static` assets;
- the `charliebot` CLI of the live release.

Only a release switch or the rollback in README.md (Quick start) moves the checkout.

Base each new delegation on `origin/<branch>`, because the local branch can lag origin. To read another revision, use `git show <rev>:<path>` or a worktree of your own.

Before relying on local ancestry to verify a landed commit, check whether the clone is shallow.
Before deploying through the runtime checkout, pause scheduled jobs that read repo content through it.

**Skill-doc update timing**: only promote a finding to a SKILL.md after the underlying debug is fully resolved. Tentative observations (e.g. "per-channel score looks ~1.34× baseline, pending 7-fold confirmation") belong in `$CHARLIEBOT_HOME/LESSONS.md` or the session transcript, not in a skill. Skills are confirmed conventions readers can act on without re-deriving.

**Repo-level skill rules must be rule-shaped, not incident-shaped.** When a confirmed lesson lands in a charliebot repo-level skill, strip the project-internal debug symptoms (specific commit names, error strings, "observed in <experiment-id> with <api-mismatch>"). Write only the underlying rule a future reader can apply ("always parity-check the N=1 wrapper before launching multi-channel runs"). Project-side incident detail belongs in the host skill or LESSONS.md, not in a repo-level skill that other people may read.

### Codex backend transcript-recording bug

Long Codex-backed worker/reviewer sessions occasionally hit `failed to record rollout items: thread <id> not found` at the very end of execution. Symptoms:
- Result message in chat is truncated mid-summary
- Worker process actually completed successfully and pushed work
- Reviewer (if any) likewise pushed the merge

Master should NOT treat the truncation as failure. Verify final state directly:
- `git log origin/<base_branch>` — confirm the expected commit landed
- `git ls-remote origin <branch_name>` — confirm push happened

If both confirm, work is done; the truncated chat output is harmless. If commit is missing, follow the usual SIGTERM-stranded-commit recovery (cherry-pick from local task branch, etc.).

### Surface file-op failures, don't silently work around

When a worker hits a failure on a command that touches real data — `cp -r`, `mv`, `rm`, `rsync`, dir rename — STOP and ask the user before retrying, switching strategy, or falling back to a workaround. "It probably means X, let me try Y" is exactly when state gets corrupted. The T0 deletion rule (in the memory store's `rulings` topic) is the floor; this generalizes it: any non-trivial file op on persisted data (model_report dirs, checkpoints, eval npz, datasets) gets the same treatment.

### Don't push worker-side rules for failures master CC should fix itself

When you find a failure mode that master CC can recover from after-the-fact (e.g. SIGTERM-stranded local commit on an unpushed branch — master can cherry-pick from the local task branch), do NOT add a prescriptive "always do X immediately" rule to the delegation prompt boilerplate. The delegation prompt is for task content, not for offloading reliability work that belongs in the master loop. Fix the master-side recovery path; leave delegation prompts focused on the deliverable.

### Include verified code in delegation prompts

Include code in a task spec only after verifying it. Otherwise describe the check or point to existing prior art.

### Don't include improve-loop revert semantics in delegate prompts

Never include revert/keep-only-report decision rules in delegate prompts — those are improve-loop semantics. The delegate worker's code change IS the artifact regardless of run outcome; failed attempts must still commit. Be specific in the task spec (file paths, function names, acceptance criteria); one task per delegation.

### Delegation spec hard format

Delegation specs require `## Goal`, `## Source Files`, `## Required Behavior`, `## Acceptance Tests`,
`## Reviewer Checklist`, and `## Out of Scope`. List existing absolute paths or `- (none)` under `Source Files`.
Put patch-created files in a side note.

Specs must not forbid test edits: tests asserting removed behavior get updated or deleted with it.

Spec precision follows the task kind: an exploratory or judgment-heavy task (a cleanup whose extent is a judgment call, a review, a comment or doc rewrite) states the goal, the reason in the requester's words, the judging standard, the invariants, and the scope, and stops there - an enumerated change list caps the result at what the master already found. A fixed mechanical contract keeps its executable contract and acceptance assertions: there the precision is the deliverable.

A spec for one of the kinds of work listed under Requester Words in `charliebot delegate --help` carries that section; the help gives the rule and the line format.

### Task-Spec Amendments

- `charliebot delegate` stores each task spec as JSON in `<child-session>/data/runs/<run>/task_spec.md`.
- The `goal` field contains the Markdown task text.
- Workers receive that text in their launch prompt.
- Edit a running worker's run copy with a JSON parser because the worker reads only that file.
- A finished run stores its log as `agent.raw.ndjson.zst`; the decompressed content is unchanged.
- Later work runs and review runs read the spec from the worker session task record in `metadata.json`.
- Editing one run copy changes that run only.
- Delegate again to amend a worker that resumes after a trigger.
- A `charliebot session send` message stays pending until the current run ends.
- A pending message can start one extra work run after review.
- The backend that `delegate` resolves runs every work run of the worker session, including follow-up runs that pending input starts. The worker session task record keeps that backend. See `charliebot delegate --help`, Backend selection.

### Validate spec-embedded tests and patches outside the repo first

Validate the test's red and green states and the patch's applicability in an isolated copy outside the repository
before adding either to a task spec.

### Output-capped backends: attach verified code

Some backends cap output below their advertised limit, so splitting a task does not remove the cap. For implementation
delegations to those backends, attach the verified patch file so the worker can apply code directly.

---

## Repo-Specific Merge Policy

- Use git worktrees for branch operations by default.
- For `charlie-bot`, land a verified worktree change by a push to origin. Keep the runtime checkout as the merge-back paragraph above states.
- For other repos, keep the main checkout untouched unless the user explicitly approves otherwise.
- Before a delegated write, read the repo manifest for test commands and run `git var GIT_AUTHOR_IDENT` for the active identity.

### Worktree Retention and Recovery

- Cancelled and failed delegation worktrees keep their environments.
- At server start, failed worktrees move to `worktrees/.trash` after seven days.
- Run `charliebot gc-trash --yes` to purge that directory.
- `refs/stash` is shared by worktrees of one repository. Restore pre-fix code with `git show <base>:<path>` or a scratch worktree.
- Recover displaced work with its stash hash. Do not use a bare `git stash pop`.

---

## Workers & Sessions — Architecture Notes

- **Propagate `repo_path` explicitly to every derived/downstream task** (review workers, retries, continuations, chained tasks) — pass it from the originating task via `ThreadMetadata.repo_path`. `discover_repos()` returns repos in non-deterministic order, so it is only safe at the top-level entry point (user delegation, CLI); downstream tasks read the propagated value instead. This is a recurring bug — always propagate repo_path explicitly.
- **Package-owned metadata keys**: A package registers its top-level `metadata.json` keys from its `register()` as a `MetadataSlot` (`rust/crates/infra/core/src/metadata_slot.rs`), and the store's metadata module (`rust/crates/runtime/store/src/metadata.rs`) serializes and writes them. A package reads and writes its fields through its own `metadata.rs` accessors. `SessionMetadata` and `ThreadMetadata` keep a key that no package registers and write it back unchanged. The cc-claude package owns `claude_account` and `claude_session_id`.
- Session instructions: the prompts task in `rust/crates/runtime/tasks/src/prompts/` assembles the ordered instruction blocks for each Run, and pins them in the Run's `prompt_snapshot.json`. A manager's blocks hold `prompts/task_base.md`, `prompts/master.md`, the workflow rules file, `prompts/task_manager.md`, and the host supplement (`$CHARLIEBOT_HOME/MASTER_AGENT_PROMPT.md`, named by `cfg.claude_md_file`). The memory package adds its segments through the turn-contribution registry: `select_master_memory` and `select_worker_memory` in `rust/crates/features/memory/src/memory.rs`, reached through `rust/crates/features/memory/src/turn_contribution.rs`, serve a manager and every worker kind. Each selection carries the full bodies of the applicable entries in resident or repo topics. After them come the `# Memory index` header line and one line per remaining entry. Each backend delivers the joined text on every launch, a resumed run and a pool relay included. A manager turn writes it to the instruction file in the session cwd (Claude Code `CLAUDE.md`, other backends `AGENTS.md`), and a worker or review run writes it to `harness_instructions.md` in its Run directory.
- Worker log display: In main chat panel, only show "worker {id} started/ended" with general purpose description. Full logs belong in the worker panel only.
- Draft preservation: User's unsubmitted message text is preserved per-session when switching sessions.
- Process-session recovery:
  - Start each CLI run in its own process session so it survives a server restart.
  - On the next boot, reattach live runs and drain dead runs.
  - Set a drained run's `ended_at` to its raw log's last write.
  - A reattached or drained run ends the way its first launch would end: for worker runs, its end record, then the same pool failover or parent report.
  - Exclude backends registered with `restart_reattach=False` and improve-loop controllers from restart reattachment.
- For CLC run end conditions, resuming an errored session, context reset, landing a CLC change, or running its reviewer, read the charlie-code `README.md`.
- **Long-running remote command**:
  `charliebot session trigger --cmd '...' --host HOST --cwd DIR --max-wait N --message "..."` starts a command and
  registers a wake.
  `--host` selects its host; `--cwd` names its working directory, and both require `--cmd`. The `--cmd` and `--watch`
  forms are mutually exclusive. `--max-wait` bounds the wait; it takes at most 14400 seconds, and a longer value
  exits 2 before anything starts.
  SSH start failure exits 3 without registration; registration rejection exits 2 and kills the started command.
  Registration prints one JSON object with `trigger_id`, `fire_at`, and the launch record (`launch_id`, `host`, `pid`,
  `log`, `exit_file`); the wake carries the exit status and log path, and a non-zero exit wakes as `watch failed`.
  Normal master calls use the server's session; explicit mismatches in `--session` are rejected.

- **One trigger watches many targets — a single trigger covers every parallel job.** When master is waiting on N parallel runs, pass all targets to one `charliebot session trigger --watch <spec> <spec> ...` where each spec self-describes its kind (`PID`, `host:pid`, `slurm:jobid`, `host:slurm:jobid`) and kinds may be freely mixed. The trigger fires when ALL listed targets have finished or `--max-wait` elapses. Spawning N triggers wastes the trigger machinery and produces N wake-ups for one logical event. The per-session pending-trigger cap and the sixth registration's exit-2 behavior are contract in `prompts/master.md`, section Delayed Triggers.

---

## Task Reparent

`charliebot session reparent <task id> --parent <new parent id>` patches `/api/sessions/{task id}` with only
`task_parent_id`.

- A run-token caller may move a task below itself to itself or a descendant manager. A root session bound by the current
  cron configuration may also move under the caller's task or a descendant manager; other targets return 403.
- A run-token PATCH may change only `task_parent_id`; other fields require operator credentials.
- Profiles, task records, and rules stay with the operator.
- Structural refusals for operator reparents also apply to agent reparents and return 409.
- Reparenting preserves each moved session's effective prompt word for word. It carries a dropped ancestor subtree rule
  only when the moved task has no own rule; it refuses added rules and dropped rules it cannot carry.
- The user alone sets the rules that a session reads.
- A carried rule changes the prompt hash, so each moved manager's next turn starts a fresh native conversation.
- Every reparent appends a `task_reparented` fact to the moved task's chat events with the actor (`agent` or `user`) and
  calling session.

---

## Plan Registry

`charliebot plan` (present/amend/approve/close): `amend` rejects duplicate artifact filenames — each version is a new `plan_NN_vK.html`. A delivered plan stays open; `close` covers superseded/abandoned/completed, and `approved` is the terminal delivery state.

---

## Claude Code Backend Context Window

The declared window is 433000 (compaction point 400000) for every run kind. The working knob is `CLAUDE_CODE_AUTO_COMPACT_WINDOW` (100k–1M, lowers only: it takes `Math.min(model window, set value)`), and a host export of it beats the per-kind value; `CLAUDE_CODE_MAX_CONTEXT_TOKENS` is a no-op for `claude-*` models in Claude Code 2.1.219. Compaction triggers at window − min(max_output, 20k) − 13k, with an early warmup at 0.8×; overrun compacts silently rather than erroring. charlie-bot side: `headless_claude_env(run_kind)` in `rust/crates/backends/claude_code/src/claude_launch.rs`, whose per-kind table the session page's compaction line reads too.

---

## OpenCode Backend

- For DeepSeek models on OpenCode Go, set the workspace Privacy region to Global.
- When every request in an OpenCode session times out, first check whether the host has exhausted `fs.inotify.max_user_instances`. `@parcel/watcher` permanently blocks the main thread when it cannot obtain an instance.

---

## Claude Account Pool

One Claude login carries a five-hour and a weekly window, so a long master turn or a delegated worker that crosses the line stops mid-work. The account pool keeps that work running on another login of the same subscription tier (`rust/crates/backends/claude_code/src/claude_accounts.rs` owns account state and selection; `rust/crates/backends/claude_code/src/claude_relay.rs` owns the relay mechanics both the master turn and workers use).

- **Config**: `accounts.claude:` in `$CHARLIEBOT_HOME/config.yaml` lists `label` + `config_dir` pairs, each directory one `CLAUDE_CONFIG_DIR` login. `accounts.claude_pools:` maps each pool name to the account labels it holds. Each `cc-claude` entry names its pool in `account_pool` when pools are defined; selection and relay stay inside that pool. One entry per model and pool.
- The server reloads changed account-pool config at the next usage poll.
- **Selection**: headroom is one minus the higher of the five-hour and seven-day utilization read from `rate_limit_event` and the usage-panel poll, and a rejection holds it at zero until its `resetsAt`. Every placement of a run on a login (a launch, a relay, or a reattach after restart) reserves a fixed share of that login's five-hour window until the run ends or 30 minutes pass, so runs placed together see one another before the readings rise. Unreserved headroom is the five-hour headroom minus the login's reservations, capped by headroom. Among healthy logins with headroom above zero, those whose unreserved headroom keeps them under the 90 percent line (more than 10 percent unreserved headroom) contend first, then those with positive unreserved headroom, then the rest. A reservation never refuses a run. An idle login contends ahead of a busy one. Login selection follows the scoring that the account code implements, on unreserved headroom. Logins whose scores are close to the best form the tie group; inside it the most unreserved headroom wins, then the least recently read `rate_limit_event`, then pool order. A master turn that resumes its conversation keeps its account while the prompt cache is warm (last request under 60 minutes ago) and the reading sits under 90 percent. Every other turn and every worker without a held transcript picks. The warning relay moves a run only to a login with more than 10 percent unreserved headroom. When every login of a subscription member that has headroom has no unreserved headroom, a priority pool runs its pay-as-you-go member instead.
- Identify the account that served a request from its `rate_limit_event` `resetsAt` pair.
- **Relay** (unkeyed runs only): a run moves to another login of its pool when its login stops it:
  - the login is out: the run moves once its process has ended;
  - the login reaches its warning line: the run moves at its next tool result, unless the code judges the login's reset near; then the run finishes on that login;
  - the login is lost: the run moves once its process has ended, and the login cools.
  A run that ended without error (its last result is not an error) does not move, whatever its exit code.
  Assistant text alone never marks login loss. Other API errors end the run through the backend's API-error path, without cooling the login or triggering a login relay.
  A move never overwrites a destination copy that is strictly newer: the turn adopts that copy and continues, and never fails. The conversation keeps its id and continues from the newest transcript copy (the one with the latest modification time).
  A turn's moves are capped in code. A pool with no login left ends the turn with the earliest reset time; a worker reports that as quota exhaustion, review and cold-read children retry on the next member of their pool, and verify children do not retry.
  Redundant transcript copies are retired after a sound round, keeping the newest; failed rounds delete nothing.
- **Keyed rounds** (a task that names an `improve_host`): a keyed Claude round keeps its login past the warning line until it ends. A quota rejection moves the round to the pool's next member. A lost login or a refused forwarded token is a token shortage: the round moves to the pool's next member, and no login or member is recorded out. A round that ended without error does not move.
- **Headless calls**: pass the expanded command body and its arguments to `claude -p`; a bare `/command` can reach the model literally.
- `claude -p --resume <session-id> --output-format json` reports session-cumulative `modelUsage`.
- Subtract the latest pre-run `cost-state` row to isolate that resumed run's models.
- **Sonnet compaction** (`rust/crates/backends/claude_code/src/claude_compaction.rs`) runs only when the cache is already cold: before a relay (Fable, 100K tokens or more) and at turn start after more than 60 minutes idle (Fable, 50K or more), never at turn end. It runs `claude -p --resume <uuid>` on the model of the option that `backends.calls.compaction` selects, with `/compact`; success means a new `compact_boundary` row in the transcript and a Sonnet-only `modelUsage`. Chat shows "Context compacted (manual, by Sonnet)".
- **Login loss**: the usage panel row shows `re-login needed: <config_dir>` while the account is unhealthy or its credential file is empty; chat gets one account-free notice. Re-login on the host with `CLAUDE_CONFIG_DIR=<config_dir> claude login`.
- **Where the account shows**: session metadata `claude_account` only (not in backend options, chat, or the panel badge); server log events `master_cc_account_chosen`, `master_cc_account_relay`, `worker_account_relay`, `claude_account_login_required`, `claude_login_reserved`; `master_done.account_relays` counts a turn's relays.

---

## Usage Panel Semantics

- Claude result-event usage is invocation-cumulative; read live context from the last main-chain assistant usage.
- `modelUsage` may begin with a haiku submodel, and delegated thread cost is omitted.
- The panel should divide usage by the enforced ~433K auto-compact ceiling rather than the raw context window.
- `compact_metadata.pre_tokens` on a `context_compacted` event in a session's `chat_events.jsonl` is the observable ground truth for where auto-compact fires (trigger is approximate, ~0.7% above nominal); use it to validate any displayed ceiling instead of asserting a literal. Events persisted before the counts moved into the payload carry a top-level `pre_tokens` instead.

### Usage Accounting

- Update usage accounting in the same change that adds or renames a backend.
- Resolve historical backend ids from session records and config.
- Read Codex rollouts from the configured Codex homes.
- OpenCode's local usage database can include counts that CharlieBot events omit.
- When a record moves or cleanup deletes it, the tally cache removes its usage from the page.
- A Claude `result` event's `usage` covers its own run.
- A Claude `result` event's `total_cost_usd` accumulates across resumes.
- Use `result.usage` for output tokens because per-message `usage.output_tokens` can be partial.
- Codex `turn.completed.usage` is cumulative across a worker thread's runs.
- Subtract the previous run's final `input_tokens` from the current run's final value to get its input.
- Compare model cost over time with output tokens from the same model. Record serving throughput separately.
- Count Claude API calls once by message id because copied transcripts repeat calls.
- The `api-request` and `api-request-blob` entries record sent request content.
- Claude Code carries earlier thinking blocks into later inputs. Transcripts store their text as empty.
- Read thinking tokens from `usage.output_tokens_details.thinking_tokens` and take the maximum entry for each message id.

### Run Timing

- Run metadata at `<session>/data/runs/<run>/metadata.json` records each run's kind, backend, and times.
- A relayed run keeps earlier process logs as numbered `agent.raw.ndjson` files; `.1` is oldest.
- A run whose end is 24 hours or more old stores each log file compressed, as `<name>.zst`, in place of the plain file; when the plain name is missing, read the `.zst` decompressed, which is the same content.
- Read every rotated log.
- Claude Code transcripts timestamp each message and may have copies in multiple account directories. Find a delegated run's native session id in its `init` event.
- A gap ending at assistant content measures model time.
- A gap ending at `tool_result` measures tool time.
- `duration_api_ms` includes earlier resumes of the session.
- A CLI-agent transcript marks each step with `## assistant · step N · <UTC time>`.
- A command log's modification time marks that command's end.
- A background command can keep writing its log; cap its tool time at the next step's arrival.
- Chat event logs can repeat `tool_use` ids across turns.
- Pair each id with its first same-id `tool_result` inside that turn's time range.
- Per-run `events.jsonl` ids are unique, and `events.jsonl.zst` holds the same ids.

### Feature-Use Audits

- Session metadata and chat events provide the complete audit record for backend, profile, origin, creation time, and manager shell commands.
- Worker shell commands appear only in their runs' `events.jsonl`, compressed or not.
- Attribute manager shell commands to the `manager_turn` run whose time span contains them.
- Claude nests `tool_use` in `assistant.message.content`.
- CLI backends emit top-level `tool_use` events. Codex wraps each command as `/bin/bash -lc '<command>'`.

### Server Logs and Reloads

- Server stdout logs are the only source for HTTP route-use events.
- The server reloads prompt documents on each master run (`read_source_file` in `rust/crates/runtime/tasks/src/prompts/sources.rs` reads each file fresh). Static assets under `web/static` load on the next page load.
- Each release is one commit on main. `scripts/release.sh` installs the server and CLI binaries that CI built for that commit when their sha256 checks pass. It also installs the commit's slice units. It keeps the newest installs and the live release. It runs no trial and no rehearsal: the checks live on the switch.
- A change is live when the commit of the live release contains it. This holds for server code and for files in the runtime checkout.
  Use `scripts/switch-server.sh rust <commit>` to switch the live Rust release.
  The script owns its preflight and checks.

## Backend Choice

- For a closed-set backend choice, request `response_format: json_schema` and put a short `why` field before the enum.
- Give each candidate a one-line condition for when to choose it.
- Match a request that names a model by deterministic model-name matching.
- When Codex rejects a newly released model, run `codex update`; the CLI version determines model availability.
- A verify run's latency varies by backend.
- Name the backend when a plan cites a verification duration.

---

## Sidebar & Frontend

- New sidebar filter panels: Register the filter once in `web/static/js/sidebar/filters.js`; the filter pills, switching URLs, and URL restoration all derive from that registry.
- New utilities in `web/static/js/**` or templates: check the exact class name against committed `web/static/css/tailwind.css` first — `text-emerald-400` and `text-emerald-400/50` are different selectors and only the suffixed form may exist. Tailwind's content scan token-extracts from comments too, so never write an unused class name even inside a code comment.

---

## Web Terminal

The web terminal keeps tmux mouse mode on: tmux-side selection preserves logical lines across soft wraps, while xterm.js native selection under tmux breaks at screen width (tmux repaints line-by-line via CUP), so handing selection to the browser is a net loss. tmux OSC 52 handles payloads of hundreds of KB.

---

## Repo-to-Host Content Split

- The ownership cut comes first. A repository whose only user is the user (charlie-bot) keeps its agent machinery in that repository. A repository with other users keeps no agent machinery there: the machinery lives in the profile home, and host files point into the repository (the category rule in `prompts/master.md`).
- In a repository whose only user is the user, the repo-to-host invariant (host files reference repo content; the bodies live in the repo) cuts by evolution: evolving bodies stay in the repo via pointers, while non-evolving entry skeletons (name/cron/timezone/prompt_file) may be seeded once into host files.
- Seeding runs on each server start: it writes each default cron entry whose host file is missing and leaves every existing host file unchanged.
- A cron task file names no task-level backend. The bound session's backend runs the session's own turns, each round that the session starts with `charliebot delegate --bound-task`, and each `steps` step that names no `backend` of its own. A session that the scheduler creates for a task takes its backend by the ordinary session-creation rule (`rust/crates/runtime/store/src/task_tree.rs`). A fire of a `prompt_file` or `steps` task whose bound session's backend names no current option or pool id records `failed`, posts the error to that session, and queues no input. A `handler` task uses no backend, so its fire runs whatever the bound session's backend names.
- Both steps of a `distinct_backend_from` pair carry their own `backend`, and the loader refuses a pair step without one. A pair whose two steps resolve to the same model stops its firing before the second step. The server seeds a missing host file from `configs/cron.default.yaml`; seeding writes that file only when the host config holds every backend id it names.

---

## Code Server (VS Code Web)

A self-hosted VS Code instance running as a web service for browsing code in the browser with full IDE features (syntax highlighting, file tree, search, go-to-definition).

**Setup:**
1. Install (one-time, no root needed):
   ```bash
   curl -fsSL https://code-server.dev/install.sh | sh -s -- --method standalone
   ```
   Binary installs to `~/.local/bin/code-server`.

2. Config at `~/.config/code-server/config.yaml`:
   ```yaml
   bind-addr: 0.0.0.0:<PORT>
   auth: none
   cert: <path-to-tls-cert>
   cert-key: <path-to-tls-key>
   ```
   Port and TLS cert paths are host-specific — see the `host` topic in the memory store (`$CHARLIEBOT_HOME/memory/entries/host/`) for the current host values.

3. Start:
   ```bash
   ~/.local/bin/code-server --disable-telemetry --disable-update-check <default-folder>
   ```

**Usage:**
- Open any folder via URL: `https://<host>:<port>/?folder=/path/to/dir`
- Can browse any filesystem path — the startup directory is only the default view
- Runs with open access when behind Tailscale
- Shares the same TLS cert as CharlieBot

**Features:** File tree, syntax highlighting, Ctrl+P (quick open), Ctrl+Shift+F (project search), Ctrl+Click (go-to-definition with language extensions), minimap, git diff view.

**Note:** Not auto-started. Run the start command manually. Process is not managed by CharlieBot.

---

## Backup

Compressed archive backups (not git) stored at `${CHARLIEBOT_HOME}_backup`, tiered retention. Manual backup trigger must be independent and must not affect the auto-backup schedule.

---

## Diff Comment Batches

A message beginning `[Diff comments · <repo> · <base>..<head> @ <sha>]` is a batch of line-anchored review comments on that diff. Numbered entries cite `file:line` on the stated side at the stated head SHA, and `[suggestion]` entries contain literal replacement code.

Treat the head branch as the working branch and turn the batch into one delegation by default; `quick-edit` is a good fit when suggestions dominate. After the changes land, respond to every numbered item as done, deviated with a reason, or a question back, and direct the user to refresh the same `/diff` link to review again.

## SLURM Submit + Watch

Submit with `sbatch --parsable` (prints only the job id), then watch it. The remote form is the primary one — submit over ssh on a cluster login host and watch with `HOST:slurm:`; the trigger server does not need slurm installed for the remote form, since `sacct` runs on the cluster over ssh:

Remote watch specs use `HOST:slurm:JOBID`; local specs use `slurm:JOBID`.

Caveats:
- Give every `sbatch` command absolute output and error paths with the home prefix expanded.
- Filter credentials from printed environments and keep job-log directories at mode `700`.
- If a batch script calls `srun`, use `sbatch --wrap "bash script.sh"`. If it runs programs directly, use
  `sbatch --wrap "srun bash script.sh"` so the outer `srun` binds the GPUs.
- `sacct` formats times in the local time zone of the process that runs it. Before you align `sacct` times with other logs, check that zone.
- The watcher handles whole non-array allocations only — watching an array job by its base id fails, because sacct `-X` reports the allocation and individual array tasks stay invisible to it.
- verify-on-create rejects a trigger whose ssh probe fails. The probe runs with `BatchMode=yes`. When the probe fails
  because an action only the user can take remains, end the run with a blocker report per `prompts/task_base.md`; register
  the trigger after the probe succeeds.

Keep `--message` a short label (capped at `MAX_TRIGGER_MESSAGE_CHARS`): the wake lands back in the same session with full history, so the label only names which watch fired.

## Fired Message Format

The fired message is prefixed with the reason; per-target detail is in the suffix:
- `[Scheduled trigger fired | completed] <msg> (exited: 12345, host:6789; slurm:91038: COMPLETED 0:0)` — all targets finished
- `[Scheduled trigger fired | watch failed] <msg> (watch failed: 12345: exit 2, log /path)` — a `--cmd` command exited non-zero, or its exit code could not be read
- `[Scheduled trigger fired | timeout]  <msg> (exited: 12345; still alive: slurm:91039: RUNNING)` — `--max-wait` elapsed
- `[Scheduled trigger fired | timeout] <msg> (still alive: neptune:slurm:122111 (unreachable 18m: ssh timeout after 60.0s))` — the remote host stopped answering sacct, leaving the job's true state unknown

`completed` means all targets finished and every `--cmd` exited 0; success/failure of a SLURM job is in the suffix (a SLURM `FAILED 2:0` is a failed job).
`watch failed` means the watch itself did not succeed: the wake does not report its job as completed.

---

## Cron Task Behavior

- A newly added cron task first fires on its next occurrence after one full tick: the scheduler looks back a single tick from load, so a new daily expression starts the following day.
- Cron config lives one job per file under `$CHARLIEBOT_HOME/config.d/cron.d/<name>.yaml`, and the loader hot-reloads on a file or `prompt_file` mtime change — no restart is needed. A broken file yields an error entry for that job alone (logged as `cron_task_load_failed`) while every other job keeps loading, scheduling, and rendering through `GET /api/cron/tasks`.
- The cron timezone defaults to `America/Los_Angeles` in the config model and the API: pin it explicitly for local tasks. Reread `config.d/cron.d/` at implementation time — concurrent sessions may mutate it.
- At each due fire of a `prompt_file` or `steps` task, the server first checks the bound session's backend. When that backend names no current option or pool id, the fire records `failed`, posts the error to that session, and queues no input. A `handler` task uses no backend, so its fire runs whatever the bound session's backend names. Every fire that the backend check does not fail follows the skip rules and the source rules below.
- Before every fire and every manual run, the server runs the backend-literal check (`charliebot backend check`) over the profile's `improve`, `prompts`, `scripts` and `skills` directories and over the task's own `prompt_file` and step prompt files. Each site it finds records the fire `failed`, posts one `file:line` line per site to the bound session, and queues no input; a check that cannot run does the same with its error. At server start the same sites go to the bound session of each enabled task, and the server starts anyway.
- The server handles each cron fire that the backend check does not fail and that it does not skip by the task's source:
  - `prompt_file`: the fire goes to the bound session as a `[Scheduled trigger]` input. The input of a `mode: master` task is its prompt. The input of any other `prompt_file` task names the task on its first line and carries the category template: round.md for a task with a repo, repo_less.md for one without.
  - `steps`: the server runs the steps in order; each step starts after the previous step exits 0. The bound session receives one report when the steps finish or stop.
  - `handler`: the server runs the handler and posts its result to the bound session.
- The server skips a fire that the backend check does not fail and records the skip with its reason. It skips a `prompt_file` fire while an input, or a worker that an earlier fire or a `--bound-task` delegation started, is queued or running. It skips a `steps` fire while a round of the task is in flight. It skips a non-master `prompt_file` fire while the task's host awaits login approval, and any fire while the bound session is archived. A skip wakes no session.
- When a `prompt_file` task's template cannot be read, the server posts an error to the bound session and records a skip.
- `charliebot delegate --bound-task <name>` starts a round of a `prompt_file` task without `mode: master`. `--note <text>` is optional and needs a task with a repo. With `--bound-task`, the CLI refuses `--task-spec-file`, `--keep-worktree`, `--repo`, `--base-branch`, `--backend`, `--task-type` and `--request-id`, and `--note` without `--bound-task`. The route answers 400 to `backend`, `task_type`, `keep_worktree`, `repo_path`, `base_branch`, `request_id`, `delegate_invocation` and `cold_read`, to a `mode: master`, `steps` or `handler` task, and to a note on a task without a repo. It answers 404 for an unknown task, 403 when the task's session is not the caller's, and 409 while a round of the same task is in flight (the detail names its worker) or while the host awaits login approval. A 409 starts no worker and writes no child session.
- A 409 means a round of the task is in flight, and the server starts nothing. To act later, the session registers a delay with `charliebot session trigger --max-wait <seconds> --message <label>` (at most 14400 seconds); `--watch <target>` adds a PID or Slurm job that the round waits for.
- A non-bound `charliebot delegate` whose request id starts with `cron:` is refused with 400.
- Run a scheduled task once now with `POST /api/cron/tasks/{name}/run`. An unknown task name answers 404. A `steps` task whose round is still in flight answers 409 and starts nothing. A run whose bound session is archived answers 500 and starts nothing. Both refusals come before the backend check. For a run of a `prompt_file` or `steps` task, the server then checks the bound session's backend. When that backend names no current option or pool id, the run records `failed`, posts the error to that session, queues no input, and answers 202 with the session id and no thread id. Otherwise a run of a `prompt_file` task queues its input behind the current turn and has no busy check, and a run of a `steps` task writes no chat event. A `handler` task uses no backend, so a run of it starts the handler whatever the bound session's backend names.

## Improve Loop Control

`charliebot improve stop` marks the session's running improve loop stopped; the session comes from `CHARLIEBOT_SESSION_ID`. A successful stop prints `stopped` and exits 0. With no running loop the command prints a message and exits 1. The current iteration finishes, and the next `charliebot improve start` in the same session starts a new loop.

---

## General Principle

**If you don't understand how a feature works, read the source code** in `rust/crates/`. Key server files:

**A one-shot call dispatches to the per-backend CLI-native overrides.** `one_shot_text` resolves to the claude, codex, and opencode overrides. These overrides run tool-less, and the claude override also disallows Read.
In a one-shot that needs file or tool access, bind the base `AgentBackend.one_shot_text` explicitly for every backend. This keeps the tool access that the overrides lack. `rust/crates/features/explain/src/explain.rs` binds it through `agent_one_shot_text`.

Never guess how CharlieBot works — the source code is always available.
