# Known-alive symbols (code-health cron appendix)

`prompts/cron/code_health.md` Step 4 points here. These symbols look dead to static tools
but are reached by string reference or kept deliberately. Never delete them on static-tool
evidence alone. Append an entry whenever a code-health run confirms a symbol is reached by
string, and land that edit in the same PR.

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
  the Step 3 grep already protects them and they get no entries.
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
- `art` (`tests/core/test_artifact_check.py`, the lambda in `_patch_height`) — second
  parameter of the stub installed for `artifact_check._measure_page_height(chrome_bin,
  artifact)` via `monkeypatch.setattr`; the replaced signature fixes the arity, so deleting
  the parameter makes the stub raise TypeError when the gate calls it. Vulture flags the
  unused parameter at 100% confidence as an unused variable. (The four earlier sites in
  `tests/core/test_plan_gates.py` were folded into this one helper when the page gates
  merged into the artifact-check entry point.)
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
- `backlog_label` (`src/core/config.py`, `CharlieBotConfig`) — deprecated migration field,
  read by string in the same file's `migrate_and_expand` validator: `values.pop("backlog_label",
  "Backlog")` (its twin `backlog_repo` is reached the same way via `values.get("backlog_repo")`).
  Vulture flags the field as an unused variable because nothing in the repo attribute-reads it.
- `return_value`, `side_effect` attribute writes across `tests/` (e.g.
  `session_mgr.get_session.return_value = ...` in `tests/test_autonamer.py`,
  `resp_mock.json.return_value = ...` in `tests/test_cli_improve.py`) — `unittest.mock`
  configuration attributes the library reads when the configured mock is called
  (`return_value` supplies the call result, `side_effect` overrides it with an iterable,
  callable, or exception). Nothing in the repo reads the names back, so vulture flags the
  writes as unused attributes. The same two names also appear as
  `AsyncMock(return_value=...)`/`patch(..., side_effect=...)` keyword arguments, which vulture
  does not flag.
- `speedup` (`tests/test_ncu_page.py`, attribute of the `_Speedup` stub in
  `test_extract_rules_reads_swig_attribute_objects`) — stand-in for ncu_report's SWIG
  speedup object, read by string: `_object_field(obj, name)` in `src/core/ncu_parsing.py`
  does `getattr(obj, name)` with the literal `"speedup"`
  (line building `entry["speedup_pct"]`). No `.speedup` attribute read exists anywhere in
  the repo, so vulture flags the stub's attribute write as an unused variable; the sibling
  stub attributes (`title`, `message`, `type`) go unflagged only because those names are
  attribute-read elsewhere.
- `handle_starttag`, `handle_startendtag`, `handle_endtag`, `handle_data` (`_TreeBuilder`
  in `src/core/artifact_check.py`) — template-method overrides of stdlib
  `html.parser.HTMLParser`: `feed()` drives the base class's scanner, which invokes these
  on `self` under their contract-fixed names while `_parse_dom` builds the DOM. Nothing in
  the repo calls them, each name has exactly zero whole-repo matches outside its own
  definition, and vulture flags each as an unused method. Same class as the
  `do_GET`/`do_POST`/`log_message` `BaseHTTPRequestHandler` entry above, with base-class
  virtual dispatch in place of stdlib string dispatch.
- `t_mgr` (`tests/test_internal_delegate_takeoff.py`, parameter of the `fake_spawn_worker`
  stub installed for `internal.spawn_worker` via `monkeypatch.setattr`) — the real
  `spawn_worker` (src/core/spawner_lifecycle.py) is called with six positional arguments,
  so the stub's replaced signature fixes the arity and `t_mgr` must stay to receive
  `thread_mgr`; deleting the parameter makes the stub raise TypeError. Vulture flags it
  at 100% confidence as an unused variable. Same class as the `art` stub-parameter entry
  above.
- `identity` (`tests/test_master_restart_transport_unit.py`, parameter of the
  `fake_recovery` stub installed for `server._run_crash_recovery` via
  `monkeypatch.setattr`) — the real `_run_crash_recovery` is called with three positional
  arguments in the root `server.py` lifespan (`_run_crash_recovery(cfg, boot_time,
  identity)`), so the stub's replaced signature fixes the arity and `identity` must stay
  to receive the identity task; deleting the parameter makes the stub raise TypeError.
  Vulture flags it at 100% confidence as an unused variable. Same class as the `art`
  stub-parameter entry above.
