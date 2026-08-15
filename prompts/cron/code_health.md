Code-health cleanup.

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
- `_no_master_wake` — pytest fixture in `tests/test_spawner_finalize_liveness_gate.py`, reached by
  string via `@pytest.mark.usefixtures("_no_master_wake")`; invisible to static dead-code tools.
- `_handle_agent_message`, `_handle_reasoning`, `_handle_command_execution`, `_handle_file_change`,
  `_handle_mcp_tool_call`, `_handle_web_search`, `_handle_todo_list`, `_handle_error` — Codex backend
  item-event handlers in `src/agents/backends/codex.py`, reached by string via the `_ITEM_HANDLERS`
  name list and `getattr(self, handler_name)` dispatch in `_translate_item_event`.
- `openai_compatible_messages` — FastAPI route handler in `src/api/anthropic_proxy.py`
  (`POST /api/anthropic-proxy/openai-compatible/{backend_id}/v1/messages`), reached by string: the
  `cc-openai-compatible` backend registry builds that URL by f-string in
  `src/agents/backends/registry.py`. The Python name has exactly zero whole-repo matches outside
  its own definition, so static dead-code tools (vulture) flag it as an unused function.
- Every FastAPI route handler in `src/api/*.py` (functions under `@router.get/post/patch/put/
  delete/websocket` decorators, e.g. `list_projects`, `get_backlog`, `list_cron_tasks`,
  `get_session_view`, `rate_round`, `get_events_jsonl`) — reached by URL string: `server.py` mounts
  each router with `include_router(prefix=...)` and `web/static/js/` fetches the composed paths
  (e.g. `/api/sessions/projects` from `context-panel.js`, `/rounds/{id}/rate` from
  `chat/ratings-recap.js`). The Python function names have exactly zero whole-repo matches outside
  their definitions, so vulture flags each one as an unused function; they must never be deleted on
  that evidence alone. `openai_compatible_messages` above is the same class, kept as its own entry
  because its URL is built inside the Python registry rather than `web/`.
- `_reset_config_caches` (`tests/test_charliebot_home.py`), `_clear_once_keys`
  (`tests/test_follow_silence_recheck.py`), `_reset_token_usage_single_flight` (`tests/test_pages.py`),
  `_worktree_paths` (`tests/test_reviewer_model_preference.py`) — pytest `autouse=True` fixtures,
  reached by pytest's fixture-name discovery only: zero whole-repo matches outside their
  definitions, so vulture flags them as unused functions. Vulture also flags
  `_clean_ceiling_env` (`tests/test_session_usage.py`) and `pidfd_open_available`
  (`tests/test_trigger_pid_watch.py`, `tests/test_trigger_slurm_watch.py`), but those fixtures are
  named in the parameter lists of the tests that use them, so the Step 3 grep already finds their
  references; no list entries needed.
- `session_websocket`, `voice_websocket` — `@app.websocket` handlers in `server.py`
  (`/ws/sessions/{session_id}`, `/ws/voice/{session_id}`), reached by URL string:
  `web/static/js/websocket.js` dials `/ws/sessions/${SESSION_ID}` and `web/static/js/voice-input.js`
  dials `/ws/voice/${...}`. The Python names have exactly zero whole-repo matches outside their
  definitions, so vulture flags them as unused functions. Same class as the `src/api/*.py` route
  handlers above, kept as its own entry because these live in `server.py` itself.
  (`terminal_websocket` needs no entry: `tests/test_terminal_backend.py` imports it by name, so the
  Step 3 grep finds it.)
- `slack_listener_task`, `slack_backfill_task` — `app.state` task handles assigned in the root
  `server.py` lifespan and read by string: the shutdown loop iterates
  `for attr in ("slack_listener_task", "slack_backfill_task")` and fetches each via
  `getattr(app.state, attr, None)`. Vulture flags the `slack_backfill_task` assignment as an unused
  attribute; the names appear only at the write and inside the string tuple.
- `check_prompt_or_handler_or_loop`, `migrate_and_expand` — pydantic `@model_validator` methods on
  `ScheduledTaskConfig`/`CharlieBotConfig` in `src/core/config.py`, registered with pydantic at
  class-definition time and invoked during model validation. The method names have exactly zero
  whole-repo matches outside their definitions, so vulture flags them as unused methods.
- `seed_default_cron_tasks` (`src/core/init.py`) — production-scope vulture (`src/ server.py`)
  flags it as an unused function because its only production caller is the Python heredoc embedded
  in `scripts/setup.sh` (a shell script, invisible to Python dead-code tools). The absence from the
  server-start path is deliberate: seeding belongs to the explicitly invoked setup command, and
  `tests/test_cron_defaults.py` asserts the name stays out of
  `init_charliebot_home.__code__.co_names`.
- `threshold`, `min_silence_duration`, `min_speech_duration`, `max_speech_duration`, `sample_rate`
  — attribute writes on the sherpa-onnx `VadModelConfig` in `src/agents/transcriber.py`
  (`_load_speech_models`). The vendor C++ binding reads them when the VAD runs; nothing in the
  repo reads them back, so vulture flags the writes as unused attributes. `min_silence_duration`,
  `min_speech_duration`, and `max_speech_duration` each have exactly one whole-repo match (the
  write site), so they sit one grep away from looking phase-1-deletable.
- Pydantic response-model fields read by JSON key from `web/` and nowhere else in Python —
  vulture flags the model declarations as unused variables and the write sites as unused
  attributes:
  `schedule_cron`, `schedule_enabled`, `schedule_next_run`, `schedule_timezone`,
  `schedule_project`, `schedule_allow_failure` (`SessionMetadata`, `src/core/models.py`), written
  in `src/api/sessions.py:list_scheduled_sessions`, read in `web/static/js/sidebar/groups.js`;
  `parent_session_id` (same model), read in `web/static/js/chat/rendering.js`;
  `lines_added` (`WorkerEvent`, `src/core/models.py`), read in `web/static/js/workers.js`;
  `attach_command`, `attach_available` (`ThreadMetadataResponse`, `src/api/threads.py`), read in
  `web/static/js/workers.js`;
  `placeholder` (`SlashCommandParam`, `src/core/slash_commands.py`, served via `model_dump` in
  `src/api/slash.py`), read in `web/static/js/slash-form.js`;
  `fired_at` (`PendingTrigger`, `src/core/models.py`), read by attribute name in the Jinja
  template `web/templates/index.html`.
  (`fire_reason` needs no entry: the trigger tests name it, so the Step 3 grep already finds
  it.)

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
