---
name: charliebot
description: CharlieBot repo structure, architecture, and development conventions. Use when modifying charlie-bot code.
version: 1.0.0
---

> For CharlieBot capabilities (delegation, /improve, triggers, etc.), see prompts/master.md — that content is auto-loaded into every master agent session.

# CharlieBot

You are CharlieBot. This skill describes your own features so you can use them correctly.
Source code: `~/workspace/charlie-bot/src/core/`

---

## Worker & Review Workflow

**Source:** `src/core/spawner.py` (800+ lines)

1. Worker runs in isolated git worktree (`~/worktrees/`)
2. Branch naming: `charliebot/task-{timestamp}-{id}`
3. On worker success → reviewer auto-spawned (may use different backend via `model_preference`)
4. Reviewer: checks diff, fixes issues, rebases, merges `--ff-only`
5. On merge → master agent gets summary

### Merge-back failover

If the reviewer's ff-merge fails (base moved), the work branch and worktree are kept. Rebase and push from the kept worktree yourself (mechanical, no re-delegate). On a genuine conflict, stop and surface it to the user or delegate the resolution.

Reviewer merge-back is a prompt-level instruction with no server-side backstop. After every implement delivery, self-check `git rev-parse <branch> origin/<branch>`: ff-merge and push origin when behind; when already merged, ff the local worktree to origin — the reviewer push updates the remote only, and the server and delegation both run from the local worktree.

**Before a subagent returns / reviewer merges**, the master should skim the subagent's context / transcript for recurring pain points (repeated errors, wrong-path attempts, env/venv pitfalls, protocol misuse). If such patterns appear, update the relevant SKILL.md so future workers don't rediscover the same lesson. This is standing user preference, not per-session.

**Skill-doc update timing**: only promote a finding to a SKILL.md after the underlying debug is fully resolved. Tentative observations (e.g. "per-channel score looks ~1.34× baseline, pending 7-fold confirmation") belong in `~/.charliebot/LESSONS.md` or the session transcript, not in a skill. Skills are confirmed conventions readers can act on without re-deriving.

**Repo-level skill rules must be rule-shaped, not incident-shaped.** When a confirmed lesson lands in a charliebot repo-level skill, strip the project-internal debug symptoms (specific commit names, error strings, "observed in <experiment-id> with <api-mismatch>"). Write only the underlying rule a future reader can apply ("always parity-check the N=1 wrapper before launching multi-channel runs"). Project-side incident detail belongs in the host skill (surpass / alpha-lab / etc.) or LESSONS.md, not in a repo-level skill that other people may read.

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

All six `##`-level headings are required (`## Goal / Source Files / Required Behavior / Acceptance Tests / Reviewer Checklist / Out of Scope`); `#`-level headings are rejected. `## Source Files` takes only existing absolute paths or `-` (new files created by a patch go in a side note instead). The verify task type requires `--keep-worktree` and does not accept `--repo`. Run `charliebot` subcommands inside the session dir — cwd drives session identity.

### Validate spec-embedded tests and patches outside the repo first

Validate both red and green states before writing a test or patch into a spec. `git archive HEAD | tar -x -C /tmp/<scratch>` yields a clean snapshot of tracked files only; edit and run tests there, then confirm the produced patch applies with `git apply --check` on a second clean snapshot.

### Output-capped backends: attach verified code

Some proxied backends (e.g. an opencode GLM endpoint) cap generation far below the advertised `limit.output` (~32k real vs 131072 advertised) and spend that budget on reasoning; splitting into smaller tasks leaves the cap in place. Strongest recipe for implementation delegations: store the verified unified diff as a file; hardcode the spec's first line to direct the worker to skip planning and make its first tool call a shell command; Step 1 = `git apply <patch>`, with verbatim EDIT blocks only as a fallback for apply failure. Delegate mechanical diff/comparison tasks to a master-run script instead — the output cap is absent there. Diagnosis entry: `sessions/<id>/threads/<tid>/data/events.jsonl` `result` + `usage`. Put the main repo venv's absolute path in the spec (worktrees lack a `.venv`). Cost: the master writes the code and the worker only lands and verifies it — the completion report flags this deviation.

---

## Repo-Specific Merge Policy

- Use git worktrees for branch operations by default.
- For `charlie-bot`, after a verified worktree change, it is okay to merge back into the main checkout automatically.
- For other repos, keep the main checkout untouched unless the user explicitly approves otherwise.

---

## Workers & Sessions — Architecture Notes

- **Propagate `repo_path` explicitly to every derived/downstream task** (review workers, retries, continuations, chained tasks) — pass it from the originating task via `ThreadMetadata.repo_path`. `discover_repos()` returns repos in non-deterministic order, so it is only safe at the top-level entry point (user delegation, CLI); downstream tasks read the propagated value instead. This is a recurring bug — always propagate repo_path explicitly.
- Session instructions: `_build_instructions_content()` (master_cc.py) concatenates `prompts/master.md` + `~/.charliebot/MASTER_AGENT_PROMPT.md` + the assembled memory block on every `run_message()`. The block is built by `src/core/memory.py::assemble_master` — full bodies of master-audience entries in resident topics plus a `# Memory index` header line and index lines for the rest. Each backend writes the result to its own instruction file under the session dir (Claude Code `CLAUDE.md`, Codex `AGENTS.md`).
- Worker log display: In main chat panel, only show "worker {id} started/ended" with general purpose description. Full logs belong in the worker panel only.
- Draft preservation: User's unsubmitted message text is preserved per-session when switching sessions.
- **Long-running remote command**:
  1. `charliebot remote-launch --host H --cwd P --cmd '...'` — launches the command on H, returns `launch_id` and `host:pid` in stdout JSON.
  2. `charliebot schedule-trigger --watch H:PID --max-wait N --message "..."` — verifies PID alive on remote at create time. If the launch died, this exits non-zero — do NOT yield, retry the launch.
  3. **Do not yield immediately.** Busy-wait a few minutes and tail `/tmp/charliebot_runs/<launch_id>/log` to confirm the output matches expectations. If wrong, `ssh H kill PID`, investigate, retry.
  4. Yield only after the busy-wait confirms the job is doing the right thing.
  5. On wake (trigger fired): `ssh H cat /tmp/charliebot_runs/<launch_id>/sentinel` for exit code; pull log if needed; `ssh H rm -rf /tmp/charliebot_runs/<launch_id>` to clean up remote staging.

  In normal master usage, do not pass `--session`; cwd supplies it and mismatches with an explicit `--session` are rejected. Existing explicit `--session` still works for old callers.

  Constraints: wrapped cmd must run in foreground (no detached `&` inside `--cmd`). For parallel jobs, call `charliebot remote-launch` N times. The wrapper log captures stdout/stderr only — if the cmd internally redirects output to its own log path (e.g. `/storage/...`), that file is the source of truth and the wrapper log will be near-empty. For sub-2-minute commands, just `ssh host 'cmd'` synchronously instead — the launch+trigger overhead is not worth it.

- **One trigger watches many targets — a single trigger covers every parallel job.** When master is waiting on N parallel runs, pass all targets to one `charliebot schedule-trigger --watch <spec> <spec> ...` where each spec self-describes its kind (`PID`, `host:pid`, `slurm:jobid`, `host:slurm:jobid`) and kinds may be freely mixed. The trigger fires when ALL listed targets have finished or `--max-wait` elapses. Spawning N triggers wastes the trigger machinery and produces N wake-ups for one logical event.

---

## Plan Registry

`charliebot plan` (present/amend/approve/close): `amend` rejects duplicate artifact filenames — each version is a new `plan_NN_vK.html`. A delivered plan stays open; `close` covers superseded/abandoned only, and `approved` is the terminal delivery state.

---

## Claude Code Backend Context Window

The Claude backend defaults to a 400k window. The working knob is `CLAUDE_CODE_AUTO_COMPACT_WINDOW` (100k–1M, lowers only: it takes `Math.min(model window, set value)`); `CLAUDE_CODE_MAX_CONTEXT_TOKENS` is a no-op for `claude-*` models in Claude Code 2.1.219. Compaction triggers at window − min(max_output, 20k) − 13k (400k → ~367k; 1M default → ~784k), with an early warmup at 0.8×; overrun compacts silently rather than erroring. charlie-bot side: `src/agents/backends/claude_code.py` `headless_claude_env()` — the master path inherits os.environ, but `claude_sub.py`'s tmux `respawn-pane -e` path passes an allowlist only, so the variable must enter the allowlist for full coverage.

---

## Usage Panel Semantics

- Claude result-event usage is invocation-cumulative; read live context from the last main-chain assistant usage.
- `modelUsage` may begin with a haiku submodel, and delegated thread cost is omitted.
- The panel should divide usage by the enforced ~433K auto-compact ceiling rather than the raw context window.
- `context_compacted.pre_tokens` in a session's `chat_events.jsonl` is the observable ground truth for where auto-compact fires (trigger is approximate, ~0.7% above nominal); use it to validate any displayed ceiling instead of asserting a literal.

---

## Sidebar & Frontend

- New sidebar filter panels: Register the filter once in `web/static/js/sidebar/filters.js`; the filter pills, switching URLs, and URL restoration all derive from that registry.

---

## Web Terminal

The web terminal keeps tmux mouse mode on: tmux-side selection preserves logical lines across soft wraps, while xterm.js native selection under tmux breaks at screen width (tmux repaints line-by-line via CUP), so handing selection to the browser is a net loss. tmux OSC 52 handles payloads of hundreds of KB.

---

## Skills System

Shared skills live in `<charlie-bot-repo>/skills/`; host-specific skills live in
`~/.charliebot/skills/`. Both sync into `~/.claude/skills/`.
Skills with `user-invocable: false` are auto-loaded by CC when contextually relevant.

`~/.charliebot/` holds host-specific state (skills, sessions, config, credentials).
On some hosts it may happen to be a local git repo, but its contents are NOT meant to
be committed or pushed — workers must edit files in place and must never `git add` them
to a `~/.charliebot` repo if one exists. Cross-host shared skills go in
`<charlie-bot-repo>/skills/` instead.

---

## Repo-to-Host Content Split

- The repo-to-host invariant (host files reference repo content; the bodies live in the repo) cuts by evolution: evolving bodies stay in the repo via pointers, while non-evolving entry skeletons (name/cron/timezone/prompt_file) may be seeded once into host files.
- Seeding belongs to an explicitly invoked setup command; keep it out of the server-start path, where writers reorder user files and race concurrent writes.
- `effective_scheduled_task_backend` (src/core/scheduler.py) resolves an omitted cron `backend` to `cfg.backend_options[0].id` (positional), so repo-shipped default tasks leave `backend` unset — the value is a host-local name.
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
   Port and TLS cert paths are host-specific — see the `host` topic in the memory store (`~/.charliebot/memory/entries/host/`) for the current host values.

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

Compressed archive backups (not git) stored at `~/.charliebot_backup`, tiered retention. Manual backup trigger must be independent and must not affect the auto-backup schedule.

---

## Improvement Decisions (Feb 2026)

**Not needed (single-user):** SQLite (JSON preferred), worker retry/backoff, worker resource limits, rate limiting.
**Done:** session full-text search, error handling consistency, session rewind.
**Planned:** worker templates as slash commands.
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
- The watcher handles whole non-array allocations only — watching an array job by its base id fails, because sacct `-X` reports the allocation and individual array tasks stay invisible to it.
- On Okta-gated hosts a cold SSH key cache makes verify-on-create reject the trigger (the first ssh probe fails before the key is enrolled), so enroll the key first with a manual `ssh neptune true`.

## Fired Message Format

The fired message is prefixed with the reason; per-target detail is in the suffix:
- `[Scheduled trigger fired | completed] <msg> (exited: 12345, host:6789; slurm:91038: COMPLETED 0:0)` — all targets finished
- `[Scheduled trigger fired | timeout]  <msg> (exited: 12345; still alive: slurm:91039: RUNNING)` — `--max-wait` elapsed
- `[Scheduled trigger fired | timeout] <msg> (still alive: neptune:slurm:122111 (unreachable 18m: ssh timeout after 60.0s))` — the remote host stopped answering sacct, leaving the job's true state unknown

`completed` means all targets finished; success/failure is in the suffix (a SLURM `FAILED 2:0` is a failed job).

---

## Cron Task Behavior

- A newly added cron task first fires on its next occurrence after one full tick: the scheduler looks back a single tick from load, so a new daily expression starts the following day.
- Cron config lives one job per file under `~/.charliebot/config.d/cron.d/<name>.yaml`, and the loader hot-reloads on a file or `prompt_file` mtime change — no restart is needed. A broken file yields an error entry for that job alone (logged as `cron_task_load_failed`) while every other job keeps loading, scheduling, and rendering through `GET /api/cron/tasks`.
- The cron timezone is fixed in the config model, API, and UI: pin `America/Los_Angeles` explicitly for local tasks. Reread `config.d/cron.d/` at implementation time — concurrent sessions may mutate it.

---

## General Principle

**If you don't understand how a feature works, read the source code** at `~/workspace/charlie-bot/src/core/`. Key files:

| Feature | Source file |
|---------|------------|
| Improve loop | `improve_command.py` |
| Spawner + review | `spawner.py` |
| Slash commands | `slash_commands.py` |
| Backlog state machine | `backlog_loop.py` |
| Scheduler | `scheduler.py` |
| Delayed triggers | `triggers.py` |
| Sessions | `sessions.py` |
| Config | `config.py` |

Never guess how CharlieBot works — the source code is always available.
