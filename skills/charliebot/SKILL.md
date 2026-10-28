---
name: charliebot
description: CharlieBot repo structure, architecture, and development conventions. Use when modifying charlie-bot code. Also use when operating CharlieBot, for delegation and running-worker amendments, run records and usage audits, the Claude account pool, and CLC (charlie-code) runs.
version: 1.0.0
---

> For CharlieBot capabilities (delegation, /improve, triggers, etc.), see prompts/master.md — that content is auto-loaded into every master agent session.

# CharlieBot

You are CharlieBot. This skill describes your own features so you can use them correctly.
Source code: the server is `rust/crates/`; the Python `charliebot` CLI is `src/`.

---

## Worker & Review Workflow

**Source:** `rust/crates/runtime/tasks/src/execution.rs`, `rust/crates/runtime/tasks/src/spawner_prompt.rs`, `rust/crates/runtime/tasks/src/spawner_backends.rs`

1. Worker runs in isolated git worktree (`~/worktrees/`)
2. Branch naming: `charliebot/task-{timestamp}-{id}`
3. On worker success → reviewer auto-spawned (may use a different `backends.pool` member)
4. Reviewer: checks diff, fixes issues, rebases onto the remote base, pushes `HEAD:{base_branch}` (git rejects a non-fast-forward push)
5. On push → master agent gets summary

### Merge-back failover

If the reviewer's push fails (base moved), the work branch and worktree are kept. Rebase and push from the kept worktree yourself (mechanical, no re-delegate). On a genuine conflict, stop and surface it to the user or delegate the resolution.

Reviewer merge-back is a prompt-level instruction with no server-side backstop. After every implement delivery, compare `git rev-parse <branch> origin/<branch>`. When origin lacks the delivered commit, push it from the kept worktree.

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

**Before a subagent returns / reviewer merges**, the master should skim the subagent's context / transcript for recurring pain points (repeated errors, wrong-path attempts, env/venv pitfalls, protocol misuse). If such patterns appear, update the relevant SKILL.md so future workers don't rediscover the same lesson. This is standing user preference, not per-session.

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

### Don't include unverified test code snippets in delegation prompts

When writing a delegation that asks the worker to add a unit test or run a verification command, do NOT paste a code snippet you haven't run yourself. Workers copy delegation prompts verbatim, so any typo in the snippet (wrong API name, wrong type assertion, missing import) becomes the worker's debug task. Observed pattern: master spends 30 seconds writing a "helpful" assertion, worker spends 5 minutes figuring out it was wrong.

Better practice — choose one of:
- Verify the snippet locally before pasting (cheapest if the env is convenient)
- Describe the test conceptually ("assert per_channel_chosen[0] equals an independent argsort of channel 0's predictions") and let the worker write the actual code
- Reference an existing test that does something similar and ask the worker to mirror its style

### Don't include improve-loop revert semantics in delegate prompts

Never include revert/keep-only-report decision rules in delegate prompts — those are improve-loop semantics. The delegate worker's code change IS the artifact regardless of run outcome; failed attempts must still commit. Be specific in the task spec (file paths, function names, acceptance criteria); one task per delegation.

### Delegation spec hard format

All six `##`-level headings are required (`## Goal / Source Files / Required Behavior / Acceptance Tests / Reviewer Checklist / Out of Scope`); `#`-level headings are rejected. `## Source Files` takes only existing absolute paths or `-` (new files created by a patch go in a side note instead). The verify task type requires `--keep-worktree` and does not accept `--repo`. Run `charliebot` subcommands inside the session dir, where a relative artifact path resolves and the session-dir check accepts it; session identity itself comes from the server-written environment.

### Task-Spec Amendments

- `charliebot delegate` stores each task spec as JSON in `<child-session>/data/runs/<run>/task_spec.md`.
- The `goal` field contains the Markdown task text.
- Workers receive that text in their launch prompt.
- Edit a running worker's run copy with a JSON parser because the worker reads only that file.
- Count reads with `grep -c task_spec <run-dir>/agent.raw.ndjson` after an edit.
- Stop the worker and delegate again when the count is zero or does not increase.
- Later work runs and review runs read the spec from the worker session task record in `metadata.json`.
- Editing one run copy changes that run only.
- Delegate again to amend a worker that resumes after a trigger.
- A `charliebot session send` message stays pending until the current run ends.
- A pending message can start one extra work run after review.
- The backend that `delegate` resolves runs every work run of the worker session, including follow-up runs that pending input starts. The worker session task record keeps that backend. See `charliebot delegate --help`, Backend selection.

### Validate spec-embedded tests and patches outside the repo first

Validate both red and green states before writing a test or patch into a spec. `git archive HEAD | tar -x -C /tmp/<scratch>` yields a clean snapshot of tracked files only; edit and run tests there, then confirm the produced patch applies with `git apply --check` on a second clean snapshot.

### Output-capped backends: attach verified code

Some proxied backends (e.g. an opencode GLM endpoint) cap generation far below the advertised `limit.output` (~32k real vs 131072 advertised) and spend that budget on reasoning; splitting into smaller tasks leaves the cap in place. Strongest recipe for implementation delegations: store the verified unified diff as a file; hardcode the spec's first line to direct the worker to skip planning and make its first tool call a shell command; Step 1 = `git apply <patch>`, with verbatim EDIT blocks only as a fallback for apply failure. Delegate mechanical diff/comparison tasks to a master-run script instead — the output cap is absent there. Diagnosis entry: the task Run's `sessions/<id>/data/runs/<run>/events.jsonl` `result` + `usage`. Put the main repo venv's absolute path in the spec (worktrees lack a `.venv`). Cost: the master writes the code and the worker only lands and verifies it — the completion report flags this deviation.

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
  - Exclude backends registered with `restart_reattach=False` and improve-loop controllers from restart reattachment.
- For CLC run end conditions, resuming an errored session, context reset, landing a CLC change, or running its reviewer, read the charlie-code `README.md`.
- **Long-running remote command**:
  1. `charliebot schedule-trigger --cmd '...' --host H --cwd P --max-wait N --message "..."` — starts the command on H and registers its wake in one step. Stdout prints one JSON object: `trigger_id`, `fire_at` and the launch record (`launch_id`, `host`, `pid`, `log`, `exit_file`), also written to `<session>/launches/<launch_id>/metadata.json`. Exit 3 means the ssh start failed and nothing was registered; exit 2 means the registration was rejected and the started process was killed.
  2. After the registration prints its result, end the turn.
  3. On wake (trigger fired): the wake message carries the exit code and the log path; pull the log if needed. The remote files sit under `/tmp/charliebot_runs/<launch_id>/` (`log`, `exit`); `ssh H rm -rf /tmp/charliebot_runs/<launch_id>` cleans up remote staging.

  In normal master usage, do not pass `--session`; the server-written environment supplies it and mismatches with an explicit `--session` are rejected. Existing explicit `--session` still works for old callers.

  Constraints: the command inside `--cmd` must not outlive the wrapper. A command that starts jobs with `&` must end with `wait`. Each `--cmd` start registers its own trigger. For parallel jobs under one wake, run them all in one `--cmd` that ends with `wait`; for SLURM jobs, pass every job to one `--watch` (the next bullet). The wrapper log captures stdout/stderr only — if the cmd internally redirects output to its own log path (e.g. `/storage/...`), that file is the source of truth and the wrapper log will be near-empty. For sub-2-minute commands, just `ssh host 'cmd'` synchronously instead — the launch+trigger overhead is not worth it.

- **One trigger watches many targets — a single trigger covers every parallel job.** When master is waiting on N parallel runs, pass all targets to one `charliebot schedule-trigger --watch <spec> <spec> ...` where each spec self-describes its kind (`PID`, `host:pid`, `slurm:jobid`, `host:slurm:jobid`) and kinds may be freely mixed. The trigger fires when ALL listed targets have finished or `--max-wait` elapses. Spawning N triggers wastes the trigger machinery and produces N wake-ups for one logical event. The per-session pending-trigger cap and the sixth registration's exit-2 behavior are contract in `prompts/master.md`, section Delayed Triggers.

---

## Plan Registry

`charliebot plan` (present/amend/approve/close): `amend` rejects duplicate artifact filenames — each version is a new `plan_NN_vK.html`. A delivered plan stays open; `close` covers superseded/abandoned/completed, and `approved` is the terminal delivery state.

---

## Claude Code Backend Context Window

The declared window is 433000 (compaction point 400000) for every run kind. The working knob is `CLAUDE_CODE_AUTO_COMPACT_WINDOW` (100k–1M, lowers only: it takes `Math.min(model window, set value)`), and a host export of it beats the per-kind value; `CLAUDE_CODE_MAX_CONTEXT_TOKENS` is a no-op for `claude-*` models in Claude Code 2.1.219. Compaction triggers at window − min(max_output, 20k) − 13k, with an early warmup at 0.8×; overrun compacts silently rather than erroring. charlie-bot side: `headless_claude_env(run_kind)` in `rust/crates/backends/claude_code/src/claude_launch.rs`, whose per-kind table the session page's compaction line reads too.

---

## Claude Account Pool

One Claude login carries a five-hour and a weekly window, so a long master turn or a delegated worker that crosses the line stops mid-work. The account pool keeps that work running on another login of the same subscription tier (`rust/crates/backends/claude_code/src/claude_accounts.rs` owns account state and selection; `rust/crates/backends/claude_code/src/claude_relay.rs` owns the relay mechanics both the master turn and workers use).

- **Config**: `accounts.claude:` in `$CHARLIEBOT_HOME/config.yaml` lists `label` + `config_dir` pairs, each directory one `CLAUDE_CONFIG_DIR` login. `accounts.claude_pools:` maps each pool name to the account labels it holds. Each `cc-claude` entry names its pool in `account_pool` when pools are defined; selection and relay stay inside that pool. One entry per model and pool.
- The server reloads changed account-pool config at the next usage poll.
- **Selection**: the healthy account with the most headroom, headroom being one minus the higher of the five-hour and seven-day utilization read from `rate_limit_event` and the usage-panel poll; a rejection holds headroom at zero until its `resetsAt`. A master turn keeps its account while the prompt cache is warm (last request under 60 minutes ago) and the reading sits under 90 percent; a worker picks at launch.
- Identify the account that served a request from its `rate_limit_event` `resetsAt` pair.
- **Relay** (master loop in `rust/crates/runtime/tasks/src/master_run.rs`, worker loop in `rust/crates/runtime/tasks/src/worker.rs`): a rejected `rate_limit_event` relays after the process exits; `allowed_warning` at 90 percent or above with the reset more than 45 minutes away terminates the process at the next `tool_result` and relays; an exit naming "Failed to authenticate" marks the account unhealthy for 15 minutes, emits `claude_account_login_required`, and relays. The transcript (`<config_dir>/projects/<slug>/<uuid>.jsonl` plus its sidecar directory) is copied to the new directory — the move refuses to overwrite a strictly newer destination copy (its consumer adopts the newer holder and continues; it never fails the turn), lands through a staged atomic replace, and the account label is persisted to session metadata the moment the move lands — and Claude Code resumes the same id with a fixed continuation prompt. Redundant pool copies retire to the newest two after a sound round (`retire_transcript_copies`; failed rounds delete nothing). Three relays per turn at most; an exhausted pool (the entry's pool) ends the turn with the earliest reset time (a worker reports it as quota exhaustion: an implement task's review and a cold-read child then retry on the next `backends.pool` member; a verify child reports the failure without a retry).
- **Headless calls**: pass the expanded command body and its arguments to `claude -p`; a bare `/command` can reach the model literally.
- `claude -p --resume <session-id> --output-format json` reports session-cumulative `modelUsage`.
- Subtract the latest pre-run `cost-state` row to isolate that resumed run's models.
- **Sonnet compaction** (`rust/crates/backends/claude_code/src/claude_compaction.rs`) runs only when the cache is already cold: before a relay (Fable, 100K tokens or more) and at turn start after more than 60 minutes idle (Fable, 50K or more), never at turn end. It runs `claude -p --resume <uuid> --model claude-sonnet-5` with `/compact`; success means a new `compact_boundary` row in the transcript and a Sonnet-only `modelUsage`. Chat shows "Context compacted (manual, by Sonnet)".
- **Login loss**: the usage panel row shows `re-login needed: <config_dir>` while the account is unhealthy or its credential file is empty; chat gets one account-free notice. Re-login on the host with `CLAUDE_CONFIG_DIR=<config_dir> claude login`.
- **Where the account shows**: session metadata `claude_account` only (not in backend options, chat, or the panel badge); server log events `master_cc_account_chosen`, `master_cc_account_relay`, `worker_account_relay`, `claude_account_login_required`; `master_done.account_relays` counts a turn's relays.

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
- Per-run `events.jsonl` ids are unique.

### Feature-Use Audits

- Session metadata and chat events provide the complete audit record for backend, profile, origin, creation time, and manager shell commands.
- Worker shell commands appear only in their runs' `events.jsonl`.
- Attribute manager shell commands to the `manager_turn` run whose time span contains them.
- Claude nests `tool_use` in `assistant.message.content`.
- CLI backends emit top-level `tool_use` events. Codex wraps each command as `/bin/bash -lc '<command>'`.

### Server Logs and Reloads

- Server stdout logs are the only source for HTTP route-use events.
- The launcher `tee` keeps the server log open on fd 3 after cleanup unlinks its path.
- Read an unlinked log through `/proc/<tee-pid>/fd/3`.
- The server reloads prompt documents on each master run (`read_source_file` in `rust/crates/runtime/tasks/src/prompts/sources.rs` reads each file fresh). Static assets under `web/static` load on the next page load.
- Each release is one commit on main. `scripts/release.sh <commit>` installs the server and CLI that CI built for that commit.
- A change is live when the commit of the live release contains it. This holds for server code and for files in the runtime checkout.
  `scripts/switch-server.sh rust <commit>` switches the live release in these steps:
  - it stops the live server;
  - it moves the runtime checkout to the release commit with `git reset --keep`, forward or back;
  - it points `current` at the new release;
  - it starts the live server from the new release.

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

## Skills System

Shared skills live in `<charlie-bot-repo>/skills/`; host-specific skills live in
`$CHARLIEBOT_HOME/skills/`. Both sync into `~/.claude/skills/`.
Skills with `user-invocable: false` are auto-loaded by CC when contextually relevant.

`$CHARLIEBOT_HOME/` holds host-specific state (skills, sessions, config, credentials).
On some hosts it may happen to be a local git repo, but its contents are NOT meant to
be committed or pushed — workers must edit files in place and must never `git add` them
to a `$CHARLIEBOT_HOME` repo if one exists. Cross-host shared skills go in
`<charlie-bot-repo>/skills/` instead.

---

## Repo-to-Host Content Split

- The repo-to-host invariant (host files reference repo content; the bodies live in the repo) cuts by evolution: evolving bodies stay in the repo via pointers, while non-evolving entry skeletons (name/cron/timezone/prompt_file) may be seeded once into host files.
- Seeding belongs to an explicitly invoked setup command; keep it out of the server-start path, where writers reorder user files and race concurrent writes.
- `effective_backend` (rust/crates/features/cron/src/scheduler.rs) resolves an omitted cron `backend` to `cfg.backends.options[0].id` (positional), so repo-shipped default tasks leave `backend` unset — the value is a host-local name.
- Repo content reaching a host already depends on rerunning setup (`sync-skills.sh` symlinks skills), so "new default cron tasks need setup rerun" matches existing product rules.

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

## Improvement Decisions (Feb 2026)

**Not needed (single-user):** SQLite (JSON preferred), worker retry/backoff, worker resource limits, rate limiting.
**Done:** session full-text search, error handling consistency, session rewind.
**Deferred:** metrics/observability, multi-repo dashboard, semantic search.

---

## Diff Comment Batches

A message beginning `[Diff comments · <repo> · <base>..<head> @ <sha>]` is a batch of line-anchored review comments on that diff. Numbered entries cite `file:line` on the stated side at the stated head SHA, and `[suggestion]` entries contain literal replacement code.

Treat the head branch as the working branch and turn the batch into one delegation by default; `quick-edit` is a good fit when suggestions dominate. After the changes land, respond to every numbered item as done, deviated with a reason, or a question back, and direct the user to refresh the same `/diff` link to review again.

## SLURM Submit + Watch

Submit with `sbatch --parsable` (prints only the job id), then watch it. The remote form is the primary one — submit over ssh on a cluster login host and watch with `HOST:slurm:`; the trigger server does not need slurm installed for the remote form, since `sacct` runs on the cluster over ssh:

```bash
JOBID=$(ssh neptune sbatch --parsable -o ~/slurm_logs/%x-%j.out -e ~/slurm_logs/%x-%j.err train.sbatch)
charliebot schedule-trigger --max-wait 86400 --watch neptune:slurm:"$JOBID" --message "train done"
# Local form (the trigger-server host itself runs slurm):
# JOBID=$(sbatch --parsable -o ~/slurm_logs/%x-%j.out -e ~/slurm_logs/%x-%j.err train.sbatch)
# charliebot schedule-trigger --max-wait 86400 --watch slurm:"$JOBID" --message "train done"
```

Caveats:
- Give every `sbatch` command absolute output and error paths with the home prefix expanded.
- Filter credentials from printed environments and keep job-log directories at mode `700`.
- If a batch script calls `srun`, use `sbatch --wrap "bash script.sh"`. If it runs programs directly, use
  `sbatch --wrap "srun bash script.sh"` so the outer `srun` binds the GPUs.
- `sacct` formats times in the local time zone of the process that runs it. Before you align `sacct` times with other logs, check that zone.
- The watcher handles whole non-array allocations only — watching an array job by its base id fails, because sacct `-X` reports the allocation and individual array tasks stay invisible to it.
- On Okta-gated hosts a cold SSH key cache makes verify-on-create reject the trigger (the first ssh probe fails before the key is enrolled), so enroll the key first with a manual `ssh neptune true`.

Keep `--message` a short label (capped at `MAX_TRIGGER_MESSAGE_CHARS`): the wake lands back in the same session with full history, so the label only names which watch fired.

## Fired Message Format

The fired message is prefixed with the reason; per-target detail is in the suffix:
- `[Scheduled trigger fired | completed] <msg> (exited: 12345, host:6789; slurm:91038: COMPLETED 0:0)` — all targets finished
- `[Scheduled trigger fired | timeout]  <msg> (exited: 12345; still alive: slurm:91039: RUNNING)` — `--max-wait` elapsed
- `[Scheduled trigger fired | timeout] <msg> (still alive: neptune:slurm:122111 (unreachable 18m: ssh timeout after 60.0s))` — the remote host stopped answering sacct, leaving the job's true state unknown

`completed` means all targets finished; success/failure is in the suffix (a SLURM `FAILED 2:0` is a failed job).

---

## Cron Task Behavior

- A newly added cron task first fires on its next occurrence after one full tick: the scheduler looks back a single tick from load, so a new daily expression starts the following day.
- Cron config lives one job per file under `$CHARLIEBOT_HOME/config.d/cron.d/<name>.yaml`, and the loader hot-reloads on a file or `prompt_file` mtime change — no restart is needed. A broken file yields an error entry for that job alone (logged as `cron_task_load_failed`) while every other job keeps loading, scheduling, and rendering through `GET /api/cron/tasks`.
- The cron timezone is fixed in the config model, API, and UI: pin `America/Los_Angeles` explicitly for local tasks. Reread `config.d/cron.d/` at implementation time — concurrent sessions may mutate it.
- Run a scheduled task once now with `POST /api/cron/tasks/{name}/run`. The run takes the scheduled path and writes no chat event. An unknown task name answers 404.

## Improve Loop Control

`charliebot improve-stop` marks the session's running improve loop stopped; the session comes from `CHARLIEBOT_SESSION_ID`. A successful stop prints `stopped` and exits 0. With no running loop the command prints a message and exits 1. The current iteration finishes, and the next `charliebot improve` in the same session starts a new loop.

---

## General Principle

**If you don't understand how a feature works, read the source code** — the server in `rust/crates/`, the Python CLI in `src/`. Key server files:

| Feature | Source file |
|---------|------------|
| Improve loop | `rust/crates/features/improve/src/command.rs` |
| Spawner + review | `rust/crates/runtime/tasks/src/spawner_backends.rs`, `rust/crates/runtime/tasks/src/spawner_prompt.rs` |
| Scheduler | `rust/crates/features/cron/src/scheduler.rs` |
| Delayed triggers | `rust/crates/runtime/triggers/src/triggers.rs` |
| Sessions | `rust/crates/runtime/store/src/session_events.rs` and the other `session_*.rs` blocks in the store crate |
| Config | `rust/crates/infra/core/src/config.rs` |

**A one-shot call dispatches to the per-backend CLI-native overrides.** `one_shot_text` resolves to the claude, codex, and opencode overrides. These overrides run tool-less, and the claude override also disallows Read.
In a one-shot that needs file or tool access, bind the base `AgentBackend.one_shot_text` explicitly for every backend. This keeps the tool access that the overrides lack. `rust/crates/features/explain/src/explain.rs` binds it through `agent_one_shot_text`.

Never guess how CharlieBot works — the source code is always available.
