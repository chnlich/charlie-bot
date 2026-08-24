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

Step 1: select the mode and the target, one per run.
Selection reads the working tree, never the commit history: a rank derived from commit history
counts this cron's own merged pull requests and feeds the selection back into itself.

List the candidate files by line count, descending; both modes rank from this one listing:

    find src -name '*.py' | xargs wc -l | sort -rn | grep -v ' total$'

Measure a file's largest top-level symbol — its longest `def`, `async def`, or `class` span — with
ast:

    python3 -c 'import ast, sys
    print(max((n.end_lineno - n.lineno + 1, n.name) for n in ast.parse(open(sys.argv[1]).read()).body
    if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef))))' <file>

Split mode fires when a `.py` file under `src/` satisfies all of:
- more than 1000 lines;
- largest top-level symbol at most 1000 lines;
- no open pull request whose head branch is `code-health/split-<stem>` for the file's stem;
- no `code-health-abandoned:` record naming it (the rejected-topic listing below).

Pick the largest qualifying file; Step 5 defines the split contract. A file whose largest
top-level symbol exceeds 1000 lines is exempt — a pure move cannot split one oversized symbol —
and is skipped silently: recompute the exemption from the tree each run and keep no record of it.

When no file qualifies, the run falls back to deletion mode: the target is the largest file whose
topic was not previously rejected.
When no symbol in the target meets the Step 3 evidence bar, the run may instead land a
behavior-preserving cleanup in the target file (deduplication, stale comment or annotation
hygiene) within the same 300-line budget, labeled refactor or style rather than deletion.
Open no PR only when the run finds neither.

A rejected topic is a closed `code-health/*` pull request carrying a comment that starts
`code-health-abandoned:`, which is the entire record of that rejection. List the rejected branches
and skip the topics they name, reporting which ones you skipped:

    gh pr list --state closed --limit 50 --json headRefName,comments --jq \
      '.[] | select(.headRefName | startswith("code-health/"))
       | select(any(.comments[].body; startswith("code-health-abandoned:"))) | .headRefName'

Step 2: respect the diff budget.
Keep a deletion-mode PR diff at 300 lines or fewer. When you hit that budget in one PR, stop
adding to it and leave the remainder to a later run. Split pull requests are exempt from the
budget: a pure move touches every moved line by definition, and the whole split lands in one pull
request.

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
The seed entry, uncovered while standing up the CI gate, is the two documented
re-exports: `kill_tmux_session` and `ScheduledSessionBusyError`. Any symbol reached from
`prompts/ skills/ configs/ web/` by string belongs here rather than in a deletion. Keep this list
in this file, in the "Known-alive symbols" list below; edit it whenever you confirm a symbol is
reached by string. Do not delete a symbol on this list.

Known-alive symbols:
- `kill_tmux_session` — documented `# noqa` re-export, reached by string reference.
- `ScheduledSessionBusyError` — documented re-export (src/api/cron.py imports it from
  src/core/sessions), kept deliberately. Used in-file by `_elone_scheduled_successor`'s
  raise, so the import line carries no `# noqa`.
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
- `seed_default_cron_tasks` (`src/core/init_seed.py`) — production-scope vulture (`src/ server.py`)
  flags it as an unused function because its only production caller is the Python heredoc embedded
  in `scripts/setup.sh` (a shell script, invisible to Python dead-code tools). The absence from the
  server-start path is deliberate: seeding belongs to the explicitly invoked setup command, and
  `tests/test_cron_defaults.py` asserts the name stays out of
  `init_charliebot_home.__code__.co_names`.
- `threshold`, `min_silence_duration`, `min_speech_duration`, `max_speech_duration` (on
  `vad_config.silero_vad`) and `sample_rate` (on `vad_config`) — attribute writes on the
  sherpa-onnx `VadModelConfig` in `src/agents/transcriber.py` (`_get_model_bundle`). The vendor
  C++ binding reads them when the VAD runs; nothing in the repo reads them back, so vulture
  flags the writes as unused attributes. `min_silence_duration`, `min_speech_duration`, and
  `max_speech_duration` each have exactly one whole-repo match (the write site), so they sit
  one grep away from looking phase-1-deletable. Vulture also flags pydantic response-model
  fields served to `web/` (`schedule_cron`/`schedule_enabled`/`schedule_next_run`/
  `schedule_timezone`/`schedule_project`/`schedule_allow_failure` on `SessionMetadata`,
  `parent_session_id` likewise, `lines_added` on `WorkerEvent`, `attach_command`/
  `attach_available` on `ThreadMetadataResponse`, `placeholder` on `SlashCommandParam`,
  `fired_at` on `PendingTrigger`) as unused variables/attributes, but every one of those names
  is grep-findable in repo (`_TRANSIENT_METADATA_FIELDS`, tests, web JS, Jinja templates), so
  the Step 3 grep already protects them and they get no entries. Production-scope vulture
  likewise flags the `cli_command` attribute write in `src/core/spawner_launch.py`
  (`_construct_worker`; the tests-inclusive scope clears it): the name resolves
  to the `ThreadMetadata.cli_command` field in `src/core/models.py` plus two assertions in
  `tests/test_spawner_backend_propagation.py`, so the Step 3 grep protects it too; no entry.
  `cli_command` rides the same implicit whole-model dump as the response-model fields above —
  `get_session_view` serves every `ThreadMetadata` field verbatim in its `threads` payload —
  and nothing web-side or production-side reads it back (whole-repo grep finds only the field,
  the write site, and the two test assertions); the write + field + assertions form one
  deletion candidate for a later phase that permits name matches.
- `pytestmark` (`tests/test_voice_sherpa_streaming.py`) — module-level
  `pytest.mark.local_only` assignment that pytest's collection reads by attribute name (the
  marker is registered in `pyproject.toml`). Exactly one whole-repo match (the assignment
  itself), so vulture flags it as an unused variable.
- `do_GET`, `do_POST`, `log_message` (`tests/test_cli_restart_contract.py`) —
  `http.server.BaseHTTPRequestHandler` overrides: the stdlib handler dispatches to them by
  string (`'do_' + self.command` through `getattr`, `log_message` by name). Each name has
  exactly one whole-repo match (its definition), so vulture flags them as unused methods.
- `_content` (two writes in `tests/test_cli_restart_contract.py`) — attribute writes on stdlib
  `requests.Response` stand-ins; `Response.json()` reads `self._content` when the fake
  response is consumed. Nothing in the repo reads the name back, so vulture flags the writes
  as unused attributes.
- `art` (four sites in `tests/core/test_plan_gates.py`) — second parameter of the
  lambda/`measure` stubs installed for `plans._measure_page_height(chrome_bin, artifact)` via
  `monkeypatch.setattr`; the replaced signature fixes the arity, so deleting the parameter
  makes the stub raise TypeError when the gate calls it. Vulture flags each unused parameter
  at 100% confidence as an unused variable.
- The `if False: yield {}` lines in `tests/test_chat_cancel.py`, `tests/test_master_cc_consumer.py`,
  `tests/test_master_cc_voice.py`, and `tests/test_worker_diagnostics.py` are flagged as
  100%-confidence unsatisfiable `if` conditions; the unreachable branch is what keeps each fake
  backend's `run()` an async generator (the consumer's `async for` would TypeError a plain
  coroutine), as each site's inline comment states. The condition is the point; nothing to
  delete.
- `model_config` (nine pydantic `BaseModel` classes: `ScheduledTaskConfig` in
  `src/core/config.py`, and `DelegateInvocationMetadata`, `ImproveRequest`,
  `ScheduleTriggerRequest`, `SessionMessageRequest`, `PlanPresentRequest`, `PlanAmendRequest`,
  `PlanApproveRequest`, `PlanCloseRequest` in `src/core/models.py`) — the pydantic v2
  `ConfigDict` class attribute, which `ModelMetaclass` consumes by attribute name at
  class-definition time; `extra='forbid'` is what turns an unknown config or request key into a
  validation error. Nothing in the repo reads the name (whole-repo grep finds only the nine
  assignments — the `model_config` substring in `src/agents/backends/codex.py` is the unrelated
  `_model_config_args` method), so vulture flags each assignment as an unused variable.
- `return_value`, `side_effect` attribute writes across `tests/` (e.g.
  `session_mgr.get_session.return_value = ...` in `tests/test_autonamer.py`,
  `resp_mock.json.return_value = ...` in `tests/test_cli_improve.py`) — `unittest.mock`
  configuration attributes the library reads when the configured mock is called
  (`return_value` supplies the call result, `side_effect` overrides it with an iterable,
  callable, or exception). Nothing in the repo reads the names back, so vulture flags the
  writes as unused attributes. The same two names also appear as
  `AsyncMock(return_value=...)`/`patch(..., side_effect=...)` keyword arguments, which vulture
  does not flag.

Step 5: open the PR.
Create at most one PR per run and report the PR URL when done.

In deletion mode, the branch is `code-health/<slug>`, where the slug self-describes the cleanup
topic. Include every deleted symbol and its `## Evidence` in the body.

In split mode, the branch is `code-health/split-<stem>` for the chosen file `<stem>.py`. One file
per pull request, and the whole split lands in that one pull request, under this contract:
- Pure move: top-level symbols move verbatim into new sibling modules named `<stem>_<content>.py`,
  each under 1000 lines, with zero behavior change and zero signature change anywhere in `src/`.
- The original file stays at its path as a facade that re-exports every pre-split top-level symbol
  through an explicit import list — no star import — so grep and vulture keep behaving and every
  string-reachable path from Step 4 stays alive.
- Parts import from parts; a part never imports the facade. The facade imports every part, so a
  part importing it back closes an import cycle.
- Tests may receive monkeypatch-target and import updates pointing at a symbol's new home, with
  zero assertion changes.
- A moved-out part that still exceeds 1000 lines needs no handling here: the Step 1 predicate
  fires on it in a later run.

The split PR body's `## Evidence` section quotes two lists side by side, each as command plus
output, and they must be equal; add the full test suite green. The pre-split top-level symbol
list, read from `main` rather than the worktree:

    git show origin/main:<file> | python3 -c 'import ast, sys
    print(sorted(n.name for n in ast.parse(sys.stdin.read()).body
    if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef))))'

and the facade's exported list, read from its import list:

    python3 -c 'import ast, sys
    print(sorted(a.asname or a.name for n in ast.parse(open(sys.argv[1]).read()).body
    if isinstance(n, ast.ImportFrom) for a in n.names))' <file>

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
