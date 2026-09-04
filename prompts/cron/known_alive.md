# Known-alive symbols (code-health cron appendix)

`prompts/cron/code_health.md` Step 4 points here. These symbols look dead to static tools
but are reached by string reference or kept deliberately. Never delete them on static-tool
evidence alone. Append an entry whenever a code-health run confirms a symbol is reached by
string, and land that edit in the same PR. Entries anchor each symbol by name and file only;
code_health.md Step 1 bans coordinate citations, so no line numbers appear here.

Known-alive symbols:
- `kill_tmux_session` — documented `# noqa` re-export, reached by string reference.
- `ScheduledSessionBusyError` — documented re-export (src/api/cron.py imports it from
  src/core/sessions), kept deliberately. Used in-file by `_elone_scheduled_successor`'s
  raise, so the import line carries no `# noqa`.
- `_no_master_wake` — pytest fixture in `tests/test_spawner_finalize_liveness_gate.py`, reached by
  string via `@pytest.mark.usefixtures("_no_master_wake")`; invisible to static dead-code tools.
- `_clean_ceiling_env` — pytest fixture in `tests/test_session_usage.py`, reached by string via
  `@pytest.mark.usefixtures("_clean_ceiling_env")`; invisible to static dead-code tools.
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
- `_fresh_unknown_limit_shape_registry` (`tests/test_ext_usage.py`),
  `_reset_config_caches` (`tests/test_charliebot_home.py`), `_clear_once_keys`
  (`tests/test_follow_silence_recheck.py`), `_reset_token_usage_single_flight` (`tests/test_pages.py`),
  `_worktree_paths` (`tests/test_reviewer_model_preference.py`),
  `_fresh_search_read_failure_registry` (`tests/test_session_search_content.py`),
  `_fresh_unhandled_part_type_registry` (`tests/test_opencode_backend.py`),
  `_clean_sidebar_state` (`tests/test_sidebar_state_snapshot.py`),
  `_clear_events_cache` (`tests/test_thread_worker_events.py`),
  `_clear_tolerant_read_memo` (`tests/test_plans_tolerant_memo.py`),
  `_clear_store_memo` (`tests/test_memory_store_memo.py`),
  `_clear_aggregate_memo` (`tests/test_token_tally.py`),
  `_clear_jsonl_memo` (`tests/test_tui_backend.py`),
  `clear_next_run_memo` (`tests/test_cron_next_run_memo.py`) — pytest `autouse=True` fixtures,
  reached by pytest's fixture-name discovery only: zero whole-repo matches outside their
  definitions, so vulture flags them as unused functions. Vulture also flags
  `pidfd_open_available` (`tests/conftest.py`, shared skip gate for the pid/slurm watch
  tests), but it is named in the parameter lists of the tests that use it, so the Step 3
  grep already finds its references; no list entry needed.
- `_reset_declared_window_warnings` (`tests/test_session_usage.py`) — pytest `autouse=True`
  fixture, reached by fixture-name discovery like the block above. A substring grep for the
  name finds matches, but all of them are `_reset_declared_window_warnings_for_tests`, the
  live reset helper the fixture calls; a word-match grep finds only the definition.
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
- `chrome`, `art` (`tests/core/test_artifact_check.py`, the lambda in `_patch_height`) — the
  two parameters of the stub installed for `artifact_check._measure_page_height(chrome_bin,
  artifact)` via `monkeypatch.setattr`; the replaced signature fixes the arity, so deleting
  either parameter makes the stub raise TypeError when the gate calls it. Vulture flags the
  unused parameter at 100% confidence as an unused variable.
- The `if False: yield {}` lines in `tests/test_chat_cancel.py`, `tests/test_master_cc_consumer.py`,
  `tests/test_master_cc_voice.py`, and `tests/test_worker_diagnostics.py` are flagged as
  100%-confidence unsatisfiable `if` conditions; the unreachable branch is what keeps each fake
  backend's `run()` an async generator (the consumer's `async for` would TypeError a plain
  coroutine), as each site's inline comment states. The condition is the point; nothing to
  delete.
- `model_config` (twelve pydantic `BaseModel` classes: `ScheduledTaskConfig` and
  `CharlieBotConfig` in `src/core/config.py`, and `DelegateInvocationMetadata`, `ImproveRequest`,
  `ScheduleTriggerRequest`, `SessionMessageRequest`, `SlackReplyRequest`, `SlackAckRequest`,
  `PlanPresentRequest`, `PlanAmendRequest`, `PlanApproveRequest`, `PlanCloseRequest` in
  `src/core/models.py`) — the pydantic v2
  `ConfigDict` class attribute, which `ModelMetaclass` consumes by attribute name at
  class-definition time; `extra='forbid'` is what turns an unknown config or request key into a
  validation error. Nothing in the repo reads the name (whole-repo grep finds only the twelve
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
- `dir_path` (ten `create_provider(provider, label, dir_path)` stubs in
  `tests/test_ext_usage.py`, installed for `ext_usage_mod._create_provider` via
  `monkeypatch.setattr`) — the real `_create_provider` (src/api/ext_usage.py) is called
  with three positional arguments, so the stubs' replaced
  signature fixes the arity and `dir_path` must stay to receive it; deleting the parameter
  makes each stub raise TypeError when the poll loop calls it. Vulture flags it at 100%
  confidence as an unused variable at all ten sites in `tests/test_ext_usage.py`. Same class as the `chrome`/`art` stub-parameter entry above.
- `rollout_paths` (`tests/test_ext_usage.py`, parameter of the `_broken_compute`
  stub installed for `CodexUsageProvider._compute_spend` via `monkeypatch.setattr`) —
  the real `_compute_spend` (src/api/ext_usage.py) is called with one positional
  argument (through `asyncio.to_thread`), so the stub's
  replaced signature fixes the arity and `rollout_paths` must stay to receive it;
  deleting the parameter makes the stub raise TypeError when `fetch()` calls it.
  Vulture flags it at 100% confidence as an unused variable. Same class as the
  `dir_path` arity-fixed entry above.
- `verify_report`, `on_spawned` (`tests/conftest.py`, parameters of the
  `fake_notify_completion` and `CapturingWorker.__init__` stubs), `entry_id`
  (`tests/core/test_artifact_check.py`, the `get_backend_option` lambda), `host_boot`
  (`tests/test_master_restart_transport_unit.py`, the `is_run_alive` lambda),
  `scheduled`, `include_running_status`, `include_pending_trigger_status`
  (`tests/test_pages.py`, the two `list_sessions` overrides), and
  `exclude_thread_id` (`tests/test_reviewer_model_preference.py`, the `fake_spawn_review`
  parameter) — stub parameters whose keyword name or arity is fixed by the production call
  each stub replaces. Finalize passes `verify_report=` by keyword
  (`_run_finalize_effects` in src/core/spawner_finalize.py). The production `Worker`
  construction passes `on_spawned=` by keyword (src/core/spawner_launch.py).
  `iter_light_backends` passes one positional argument to `cfg.get_backend_option`
  (src/core/autonamer.py), so the lambda must take exactly one. The `host_boot` lambda
  receives `runs.is_run_alive`'s four positional arguments. The pages routes pass
  `scheduled=`/`include_running_status=`/`include_pending_trigger_status=` by keyword into
  `list_sessions` (src/api/pages.py). Both `spawn_review_worker` call sites in
  src/core/review.py pass `exclude_thread_id=` by keyword. Vulture flags each at 100%
  confidence as an unused variable. Same class as the `art`/`t_mgr`/`dir_path`
  stub-parameter entries above.
- `check` (`tests/conftest.py`, keyword parameter of the `fake_run_tmux` stub) and `format`
  (`tests/test_cli_restart_contract.py`, the `log_message` override's second parameter)
  — signature-mirror parameters kept deliberately, not fixed by any call: no caller passes
  `check=` to `pty_common._run_tmux`, and the stdlib invokes `log_message(format, *args)`
  positionally into the override's trailing `*args`, so deleting either parameter stays
  green; both keep the stub a faithful mirror of the signature it replaces (the
  `fake_run_tmux` factory docstring states that drop-in contract, and `format` mirrors the
  stdlib `BaseHTTPRequestHandler.log_message(self, format, *args)` signature). Vulture flags
  each at 100% confidence as an unused variable.
- `bundle` (`tests/test_transcriber_sampling.py`, first parameter of the two
  `fake_decode` stubs installed for `transcriber._decode_samples` via `monkeypatch.setattr`)
  — the real `_decode_samples` (src/agents/transcriber.py) is called with two positional
  arguments from `_drain_closed_segments` and `_decode_live_segment_if_due`
  (the two decode call sites in the same file), so `bundle` must stay to receive `self._bundle`;
  deleting the parameter makes the stub raise TypeError on the first decode. Vulture flags
  it at 100% confidence as an unused variable at both sites. Same class as the `art`/`t_mgr`
  stub-parameter entries above.
- `interrupt_reason` (`tests/test_worktree_quarantine.py`, keyword parameter of the
  `fake_resume_worker` stub installed for `spawner.resume_worker` via `monkeypatch.setattr`)
  — all three production call sites in `src/core/init_worker_recovery.py`
  pass `interrupt_reason=` by keyword, and the stalled-run test asserts the fake ran
  (`resume_calls == [True]`), so deleting the parameter makes the stub raise TypeError on
  the unexpected keyword. Vulture flags it at 100% confidence as an unused variable. Same
  class as the `verify_report` keyword-fixed stub-parameter entry above.
- `cls` (`src/core/config.py`, first parameter of `migrate_and_expand`, the
  `@model_validator(mode="before")` `@classmethod` on `CharlieBotConfig`) — the pydantic
  classmethod-validator protocol passes the class as the first positional argument, so the
  arity is framework-fixed even though the body reads only `values`; deleting `cls` turns
  every `CharlieBotConfig` construction into a TypeError. Vulture flags it at 100%
  confidence as an unused variable. Same framework-fixed class as the `model_config` entry
  above.
- `sig` (`tests/test_worktree_quarantine.py`, second parameter of the
  three identical `lambda pid, sig: killed.append(pid)` stubs installed for
  `worker_recovery_module.kill_process_group` via `monkeypatch.setattr`) — signature-mirror
  parameter kept deliberately: all three tests assert the recorded list stays empty (no
  tested recovery path reaches `kill_process_group`), so deleting `sig` stays green, but it
  keeps the lambda a drop-in mirror of `kill_process_group(pid, sig=signal.SIGTERM)`
  (src/core/process.py), which `src/core/init_worker_recovery.py` already calls with
  two positional arguments. Vulture flags each site at 100% confidence as an unused
  variable. Same class as the `check`/`format` signature-mirror entry above.
- `check` (`tests/test_terminal_backend.py`, keyword parameter of the inline
  `fake_run_tmux` stub installed for `terminal._run_tmux` via `monkeypatch.setattr`) —
  second site of the signature-mirror class: the stub mirrors
  `pty_common._run_tmux(*args, check: bool = False)` (src/agents/backends/pty_common.py,
  imported in src/agents/backends/terminal.py), the reuse test's only stub call is
  `("has-session", "-t", "charliebot-terminal")` with no `check=`, so deleting the parameter
  stays green; the mirror keeps the stub a faithful drop-in. Vulture flags it at 100%
  confidence as an unused variable.
- `feishu_app_id`, `feishu_app_secret`, `feishu_refresh_token`, `feishu_user_access_token`,
  `gemini_api_key`, `gemini_model`, `google_client_id`, `google_client_secret`,
  `google_docs_client_id`, `google_docs_client_secret`, `google_docs_default_folder_id`,
  `google_docs_refresh_token`, `google_refresh_token`, `linear_api_key`, `slack_user_token`,
  `twitter_api_key`, `twitter_api_secret`, `twitter_access_token`,
  `twitter_access_token_secret`, `public_base_url` (`src/core/config.py`,
  `CharlieBotConfig` fields) — yaml keys hosts carry in `config.yaml` / `config.d/*.yaml`,
  kept deliberately: consumers read the raw yaml outside this repo. Ten of the twenty are
  quoted by name in the skill files that read them (`skills/feishu/SKILL.md`,
  `skills/gmail/SKILL.md`, `skills/google-sheets/SKILL.md`, `skills/google-docs/SKILL.md`,
  `skills/linear/SKILL.md`, `skills/slack/SKILL.md`); the other ten — `gemini_api_key`,
  `gemini_model`, `google_docs_client_id`, `google_docs_client_secret`, the four
  `twitter_*` keys, and `public_base_url` — have no in-repo script consumer
  (`gemini_api_key` surfaces only in README/setup/template prose). The four `twitter_*`
  keys are read by the host-only `x-posting` skill, which is not mirrored into `skills/`,
  so no in-repo grep can reach it; they were deleted on zero-match evidence in PR #455 and
  that broke startup for every command going through `load_config()`. For this whole
  block, an absent in-repo consumer is not evidence: the consumer is out of repo by
  construction, so the zero-match bar of `code_health.md` Step 3 can never clear it. The
  fields exist so `extra='forbid'` keeps those host files loadable — the
  block comment directly above the fields in `config.py` states this for the whole set.
  `test_declared_integration_keys_round_trip` (`tests/test_config_fragments.py`) pins the
  full set by name. Nothing in the repo attribute-reads the values, so vulture flags
  every field as an unused variable. Same kept-deliberately class as the `backlog_label`
  entry above.
- `panel-summary`, `panel-details`, `panel-roofline`, `panel-source`, `panel-session`,
  `panel-raw` (`web/templates/ncu.html`, the six tab-panel element ids) — reached by
  string construction: the inline tab switcher activates panels with
  ``p.classList.toggle('active', p.id === `panel-${name}`)``, where `name` is each tab
  button's `data-tab` attribute. A whole-repo grep for any full id finds only its
  definition, so a dead-id scan flags each as unused markup; the constructed
  `panel-${name}` match is what makes them live.
