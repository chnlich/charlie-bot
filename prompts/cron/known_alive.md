# Known-alive symbols (code-health cron appendix)

`prompts/cron/code_health.md` Step 4 points here. These symbols look dead to static tools
but are reached by string reference or kept deliberately. Never delete them on static-tool
evidence alone. Append an entry whenever a code-health run confirms a symbol is reached by
string, and land that edit in the same PR. Entries anchor each symbol by name and file only;
code_health.md Step 1 bans coordinate citations, so no line numbers appear here.

Known-alive symbols:
- `kill_tmux_session` (`src/agents/backends/pty_common.py`; re-exported with `# noqa` by
  `src/agents/backends/tui.py`) — reached by string: `TUI_KILL_TMUX_SESSION_PATCH_TARGET`
  (`tests/conftest.py`) names the `src.agents.backends.tui` path, so the re-export is the
  path the monkeypatch resolves through.
- `ScheduledSessionBusyError` (defined in `src/core/scheduled_sessions.py`) — the import in
  `src/core/sessions.py` is a documented re-export (`src/api/cron.py` and `src/api/sessions.py`
  import it from `src.core.sessions`) and is used in-file by `_elone_scheduled_successor`'s
  raise, so it carries no `# noqa`. Both API-side imports are plain used imports: their in-file
  uses are the `except ScheduledSessionBusyError` clauses in `_ensure_backend_update_session`
  (`src/api/cron.py`) and the elone route (`src/api/sessions.py`).
- `_no_master_wake` — pytest fixture in `tests/test_spawner_finalize_liveness_gate.py`, reached by
  string via `@pytest.mark.usefixtures("_no_master_wake")`; invisible to static dead-code tools.
- `_clean_ceiling_env` — pytest fixture in `tests/test_session_usage.py`, reached by string via
  `@pytest.mark.usefixtures("_clean_ceiling_env")`; invisible to static dead-code tools.
- `_handle_agent_message`, `_handle_reasoning`, `_handle_tool_item`, `_handle_file_change`,
  `_handle_mcp_tool_call`, `_handle_todo_list`, `_handle_error` — Codex backend
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
- `_fresh_credential_read_warning_registry`, `_fresh_unknown_limit_shape_registry`, `_fresh_usage_cache`,
  `_fresh_user_agent_cache` (`tests/test_ext_usage.py`),
  `_fresh_pool_state` (`tests/test_claude_accounts.py`),
  `_reset_config_caches` (`tests/test_charliebot_home.py`), `_clear_once_keys`
  (`tests/test_follow_silence_recheck.py`), `_reset_token_usage_single_flight` (`tests/test_pages.py`),
  `_worktree_paths` (`tests/test_reviewer_model_preference.py`),
  `_fresh_search_read_failure_registry` (`tests/test_session_search_content.py`),
  `_fresh_unhandled_part_type_registry` (`tests/test_opencode_backend.py`),
  `_fresh_cron_body_cache` (`tests/test_cron_tasks_body_cache.py`), `_fresh_switch_memo`
  (`tests/test_switch_payload_gzip.py`),
  `_fresh_renderer_singleton` (`tests/core/test_headless_render.py`),
  `_stub_headless_renderer` (`tests/conftest.py`) — the renderer pair: the first resets the
  warm-renderer singleton around `tests/core/test_headless_render.py`, the second is the
  suite-wide conftest autouse that reshapes `headless_render.render_height` into the
  dump-dom drive seam every artifact/plan-height test relies on,
  `_clean_sidebar_state` (`tests/test_sidebar_state_snapshot.py`),
  `_clear_events_cache` (`tests/test_thread_worker_events.py`),
  `_clear_tolerant_read_memo` (`tests/test_plans_tolerant_memo.py`),
  `_clear_store_memo` (`tests/test_memory_store_memo.py`),
  `_clear_aggregate_memo` (`tests/test_token_tally.py`),
  `_clear_jsonl_memo` (`tests/test_tui_backend.py`),
  `_clean_memo` (`tests/test_thread_meta_scan_memo.py`, `tests/test_trigger_probe_memo.py`),
  `clear_next_run_memo` (`tests/test_cron_next_run_memo.py`),
  `_clean_probe_state` (`tests/test_probe_single_walk.py`) — pytest `autouse=True` fixtures,
  reached by pytest's fixture-name discovery only: zero whole-repo matches outside their
  definitions, so vulture flags them as unused functions. Most are single-line
  `fresh_state_fixture(...)` assignments in their module (built by the conftest factory of the
  same name) rather than `def` fixtures; vulture stays silent on those assignments — its
  underscore-name ignore covers underscore-prefixed variables — so the underscore-prefixed ones
  rely on fixture-name discovery alone, while `def` forms surface as unused functions. Vulture
  also flags
  `pidfd_open_available` (`tests/conftest.py`, shared skip gate for the pid/slurm watch
  tests, requested by name in `tests/test_trigger_pid_watch.py`, `tests/test_trigger_slurm_watch.py`,
  and `tests/test_trigger_succession.py`), but it is named in the parameter lists of the tests
  that use it, so the Step 3 grep already finds its references; no list entry needed.
- `reset_bundle_cache` (`tests/test_voice_engine.py` and `tests/test_voice_qwen3_hf.py`, one
  `fresh_state_fixture(transcriber.reset_bundle_cache_for_tests)` assignment in each file) —
  reached by fixture-name discovery
  like the autouse block above: zero whole-repo matches outside the two assignments, so vulture
  flags each as an unused variable. Each clears the transcriber module-level bundle cache
  around its file's tests.
- `_reset_declared_window_warnings` (`tests/test_session_usage.py`) — pytest `autouse=True`
  fixture, reached by fixture-name discovery like the block above. It resets the registry by
  calling the registry's own `clear()` (the seam `WarnOnceRegistry` documents for tests); a
  word-match grep finds only the definition.
- `session_websocket` — `@app.websocket` handler in `server.py` (`/ws/sessions/{session_id}`),
  reached by URL string: `web/static/js/websocket.js` dials `/ws/sessions/${SESSION_ID}`. The
  Python name has exactly zero whole-repo matches outside its definition, so vulture flags it as
  an unused function. Same class as the `src/api/*.py` route handlers above, kept as its own entry
  because these live in `server.py` itself.
  (`terminal_websocket` needs no entry: `tests/test_terminal_backend.py` imports it by name, so the
  Step 3 grep finds it. Voice input has no websocket handler since the record-then-upload
  migration: `POST /api/voice/...` runs through the `src/api/voice.py` router.)
- `slack_listener_task`, `slack_backfill_task` — `app.state` task handles assigned in the root
  `server.py` lifespan and read by string: the shutdown loop iterates
  `for attr in ("slack_listener_task", "slack_backfill_task")` and fetches each via
  `getattr(app.state, attr, None)`. Vulture flags the `slack_backfill_task` assignment as an unused
  attribute; the names appear only at the write and inside the string tuple.
- `check_type_and_sources` — pydantic `@model_validator` method on `ScheduledTaskConfig`
  in `src/core/config.py`, registered with pydantic at class-definition time and invoked during
  model validation (it enforces the type pm/normal prompt-source rules). The method name has
  exactly zero whole-repo matches outside its definition, so vulture flags it as an unused
  method.
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
  `parent_session_id` likewise, `lines_added` on `WorkerEvent`, `placeholder` on
  `SlashCommandParam`, `fired_at` on `PendingTrigger`) as unused variables/attributes, but every
  one of those names is grep-findable in repo (`_TRANSIENT_METADATA_FIELDS`, tests, web JS,
  Jinja templates), so the Step 3 grep already protects them and they get no entries.
- `pytestmark` (`tests/test_voice_offline_models.py`, `tests/test_voice_qwen3_hf.py`) —
  module-level `pytest.mark.local_only` assignments that pytest's collection reads by
  attribute name (the marker is registered in `pyproject.toml`). The name appears only at
  those assignment sites, so vulture flags each as an unused variable.
- `do_GET`, `do_POST`, `log_message` (`tests/test_cli_restart_contract.py`) —
  `http.server.BaseHTTPRequestHandler` overrides: the stdlib handler dispatches to them by
  string (`'do_' + self.command` through `getattr`, `log_message` by name). Each name has
  exactly one whole-repo match (its definition), so vulture flags them as unused methods.
- `chrome`, `art` (`tests/core/test_artifact_check.py`, the lambda in `_patch_height`) — the
  two parameters of the stub installed for `artifact_check._measure_page_height(chrome_bin,
  artifact)` via `monkeypatch.setattr`; the replaced signature fixes the arity, so deleting
  either parameter makes the stub raise TypeError when the gate calls it. Vulture flags the
  unused parameter at 100% confidence as an unused variable.
- The `if False: yield {}` lines in `tests/test_chat_cancel.py`, `tests/test_master_cc_consumer.py`,
  `tests/conftest.py` (`CapturingBackend`, the shared master-cc round double), and
  `tests/test_worker_diagnostics.py` are flagged as
  100%-confidence unsatisfiable `if` conditions; the unreachable branch is what keeps each fake
  backend's `run()` an async generator (the consumer's `async for` would TypeError a plain
  coroutine), as each site's inline comment states. The condition is the point; nothing to
  delete.
- `model_config` (the pydantic v2 `ConfigDict` class attribute, assigned on the pydantic
  `BaseModel` classes of `src/core/backend_models.py`, `src/core/config.py`, `src/core/models.py`,
  `src/core/project_config.py`, `src/api/diag.py`, and `src/api/cron.py`) — `ModelMetaclass`
  consumes it by attribute name at class-definition time. Every assignment pins
  `extra='forbid'`, which turns an unknown config or request key into a validation error, except
  `TaskCreate` in `src/api/cron.py`, which pins
  `extra='ignore'` (the pydantic default) so the create-request body stays looser than the
  loader's forbid task model, as the comment above the assignment states. The name is read only
  by the schema tests asserting the pin (`tests/test_config_schema.py`,
  `tests/test_backend_option_types.py`); vulture flags each production assignment as an unused
  variable.
- `return_value`, `side_effect` attribute writes across `tests/` (e.g.
  `session_mgr.get_session.return_value = ...` in `tests/test_autonamer.py`,
  `resp_mock.json.return_value = ...` in `tests/test_cli_improve.py`) — `unittest.mock`
  configuration attributes the library reads when the configured mock is called
  (`return_value` supplies the call result, `side_effect` overrides it with an iterable,
  callable, or exception). Nothing in the repo reads the names back, so vulture flags the
  writes as unused attributes. The same two names also appear as
  `AsyncMock(return_value=...)`/`patch(..., side_effect=...)` keyword arguments, which vulture
  does not flag.
- `name`, `section_identifier`, `has_rule_message`, `rule_message`,
  `has_speedup_estimation`, `speedup_estimation`, `rule_results` (the `_FakeRule` and
  `_FakeAction` stub methods of `tests/test_ncu_page.py`) — the rule surface
  `_extract_rules` (src/core/ncu_parsing.py) calls on whatever object the test feeds it:
  `rule_result.name()` through `speedup_estimation()` on each rule result and
  `action.rule_results()` on the action. The call sites type those parameters `Any`, so
  nothing in the repo reads the method names statically and a file-scope vulture run
  flags each stub method as unused (60% confidence). The same function also reaches
  payload fields by string: `_object_field(obj, name)` does `getattr(obj, name)` with the
  literal `"speedup"` when building `entry["speedup_pct"]`.
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
- `dir_path` (the `create_provider(provider, label, dir_path)` stubs in
  `tests/test_ext_usage.py`, installed for `ext_usage_mod._create_provider` via
  `monkeypatch.setattr`) — the real `_create_provider` (src/api/ext_usage.py) is called
  with three positional arguments, so the stubs' replaced
  signature fixes the arity and `dir_path` must stay to receive it; deleting the parameter
  makes each stub raise TypeError when the poll loop calls it. Vulture flags it at 100%
  confidence as an unused variable at every stub site in `tests/test_ext_usage.py`. Same class as the `chrome`/`art` stub-parameter entry above.
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
- `interrupt_reason` (`tests/test_worktree_quarantine.py`, keyword parameter of the
  `fake_resume_worker` stub installed for `spawner.resume_worker` via `monkeypatch.setattr`)
  — every production call site (`src/core/init_worker_recovery.py`)
  passes `interrupt_reason=` by keyword, and the stalled-run test asserts the fake ran
  (`resume_calls == [True]`), so deleting the parameter makes the stub raise TypeError on
  the unexpected keyword. Vulture flags it at 100% confidence as an unused variable. Same
  class as the `verify_report` keyword-fixed stub-parameter entry above.
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
  second site of the signature-mirror class: the stub mirrors the real `_run_tmux`
  signature's keyword flags, `capture` and `check` (src/agents/backends/pty_common.py,
  imported in src/agents/backends/terminal.py), the reuse test's only stub call is
  `("has-session", "-t", "charliebot-terminal")` with no `check=`, so deleting the parameter
  stays green; the mirror keeps the stub a faithful drop-in. Vulture flags it at 100%
  confidence as an unused variable.
- `panel-summary`, `panel-details`, `panel-roofline`, `panel-source`, `panel-session`,
  `panel-raw` (`web/templates/ncu.html`, the six tab-panel element ids) — reached by
  string construction: the inline tab switcher activates panels with
  ``p.classList.toggle('active', p.id === `panel-${name}`)``, where `name` is each tab
  button's `data-tab` attribute. A whole-repo grep for any full id finds only its
  definition, so a dead-id scan flags each as unused markup; the constructed
  `panel-${name}` match is what makes them live.
- `file-chip--failed`, `file-chip--uploaded`, `file-chip--uploading` (`web/static/css/styles.css`)
  — reached by string construction: `file-upload.js` emits
  ``file-chip file-chip--${file.status}``, and `tests/chat_attachments_render.test.js` asserts
  the failed and uploaded variants by name. A dead-selector scan that greps each full class
  name finds no template or JS literal, so it flags them as unused CSS.
- `d2h-wrapper`, `d2h-file-header`, `d2h-del`, `d2h-ins` (`web/static/css/styles.css`, scoped
  under `.diff-modal-body`) — reached by library DOM construction: the diff2html bundle both
  templates load from jsdelivr CDN emits every one of these class names when it renders the
  diff, and the rules restyle that output. A dead-selector scan that greps each full class
  name finds no template or JS literal, so it flags them as unused CSS. (Other `d2h-*`
  selectors in the same block, e.g. `.d2h-code-linenumber`, are reached directly by
  `diff_comments.js` `querySelector` calls.)
- `turn-row-tag-trigger`, `turn-row-tag-turn`, `turn-row-tag-worker`, `turn-row-tag-you`
  (`web/static/css/styles.css`) — reached by string construction: `chat/rendering.js` emits
  ``turn-row-tag turn-row-tag-' + label.toLowerCase()``, with the labels
  `You`/`Turn`/`Worker`/`Trigger` supplying the suffixes; `tests/chat_session_bump.test.js`
  asserts the wrapper class by name. A dead-selector scan grepping each full class name
  finds only its definition, so it flags them as unused CSS.
- `activate` (`web/static/js/terminal_clipboard.js`, the object addon passed to
  `term.loadAddon`) — xterm.js lifecycle dispatch: the library's `_addonManager.loadAddon`
  ends with `t.activate(e)`, invoking the addon's `activate` method by that name
  (verified against the CDN build the template loads, xterm 5.3.0). It is what attaches
  the copy-on-select `mouseup` listener; deleting it silently kills terminal
  copy-on-select. A JS unreferenced-method scan finds only the definition, so it flags
  the method as dead; `dispose` in the same object literal is reached the same way.
- `postCommentMessage` (`web/static/js/comment_post.js`) — cross-file browser global:
  `diff_comments.js` and `artifact-comments.js` call it as a bare identifier resolved
  through the page's script-tag global scope, each page loading the file before the
  widget (`web/templates/diff.html`, and the `_inject_artifact_ui` tags in
  `src/api/files.py` for artifact pages). A per-file dead-function scan finds only the
  definition, so it flags the function as unused.
- `render` (`FastJsonResponse` in `src/api/responses.py`) — template-method override of
  starlette `JSONResponse.render`: the base `Response.__init__` calls `self.render(content)`
  by that name when FastAPI serializes a response. As a Python identifier the name has zero
  matches outside its definition, so a Python-scoped dead-method scan (vulture) flags it as
  an unused method; a whole-repo grep is noisier — `render` is also ordinary web-JS DOM code
  (`plan-panel.js` `function render()`, `backlogPanel.render()`, pdf.js `page.render(...)`)
  that names unrelated methods. Same class as the `do_GET`/`do_POST`/`log_message`
  stdlib-dispatch entry.
- `handle_starttag`, `handle_startendtag`, `handle_endtag`, `handle_data`, `handle_entityref`,
  `handle_charref` (`_Parser` — all six — and `handle_starttag`/`handle_startendtag`/
  `handle_endtag` on `_BoundaryParser`, both in `src/core/plan_diff.py`;
  `handle_starttag`/`handle_startendtag`/`handle_endtag`/`handle_data` also on `_DomParser` in
  `tests/test_plan_diff.py`) — template-method overrides of stdlib `html.parser.HTMLParser`,
  same class as the `_TreeBuilder` entry above; `feed()` drives the base scanner, which
  invokes these under their contract-fixed names while each parser builds its DOM. The
  `_TreeBuilder` entry covers only artifact_check's class; each name matches only the parser
  classes' own definitions, so vulture flags each as an unused method. `_OffsetParser`, the
  shared base of both plan_diff parsers, pins `convert_charrefs=False` because its offset
  math must address raw source spans, so `_Parser`'s `handle_entityref`/`handle_charref` —
  the only overrides of that pair — fire there; parsers without the pair either pin
  `convert_charrefs=True` (`_TreeBuilder`, `_DomParser`), under which the stdlib folds
  references into `handle_data`, or inherit the stdlib no-op defaults (`_BoundaryParser`).
- `isolation_level` (`src/core/storage_cool.py`) — attribute write on a stdlib
  `sqlite3.Connection`; the sqlite3 C module reads it back when executing statements
  (`None` switches the connection to per-statement autocommit transactions, which the
  inline comment pins: one failed DELETE keeps the rest of the batch alive). Nothing in
  the repo reads the name, so vulture flags the write as an unused attribute. Same class
  as the sherpa-onnx `vad_config` attribute-write entry above.
- `base_html`, `new_html` (`tests/test_files_artifact_injection.py`, parameters of the `explode`
  stub installed for `plan_diff.annotate` via `monkeypatch.setattr`) — signature-mirror
  parameters of the stub that guards the annotate memo: the replaced `annotate`
  (src/core/plan_diff.py) is called with two positional arguments by its one production call
  site (src/api/files.py), so the stub keeps both parameters to stay a drop-in mirror, and a
  memo regression that reaches the stub fails with the stub's own assertion message rather
  than a TypeError. A tests-only vulture scan flags both at 100% confidence as unused
  variables (a combined src+tests scan does not: the production `annotate` parameters carry
  the same names, so the names are not zero-match repo-wide — the flags only appear in a
  tests-only scan). Same class as the `check`/`format` signature-mirror entry above. The
  same file's `html_text` (first parameter of the `explode` stub installed for
  `files_api._inject_artifact_ui`) joins this class: the replaced function is called with
  two positional arguments at both production call sites (src/api/files.py), so the stub
  keeps both parameters, and a tests-only vulture scan flags the unused first one.
- `_nonempty`, `_relative`, `_no_explicit_null_supplement` (`src/core/project_config.py`) —
  pydantic `@field_validator` / `@model_validator` methods on `ProjectConfig`, registered with
  pydantic at class-definition time and invoked during model validation. The method names have
  exactly zero whole-repo matches outside their definitions, so vulture flags them as unused
  methods. Same framework-registered class as the `check_type_and_sources` entry above.
- `inline_merge_executor` (`tests/test_perfetto_pages.py`) — pytest fixture (monkeypatches
  `pages._merge_executor` to yield None so the merge runs inline), requested by name in four
  tests' parameter lists; the bodies never reference the parameter, so vulture flags it as an
  unused variable at each request site. Same fixture-name-discovery class as the autouse block
  above.
- `isolated_config` (`tests/test_absolute_filepath_prefix.py`) — pytest fixture (owns the config
  and credentials the file router reads: sessions root under tmp_path, empty access key so the
  gate is a no-op), requested by name in three tests' parameter lists; the bodies never reference
  the parameter, so vulture flags it as an unused variable at each request site. Same
  fixture-name-discovery class as `inline_merge_executor` above.
- `pages_config` (`tests/test_pages.py`) — pytest fixture (monkeypatches the pages routes'
  `get_config` to a tmp home, so the token-usage route's cache path stays off the host profile),
  requested by name in the module's tests' parameter lists; the bodies never reference
  the parameter, so vulture flags it as an unused variable at each request site. Same
  fixture-name-discovery class as `inline_merge_executor` above.
- `cli_katex` (`tests/core/test_artifact_wrap.py`) — pytest fixture (monkeypatches
  `src.cli.common.get_config` so the wrap verb's config home lands under the pytest tmp tree
  instead of the host profile), requested by name in five tests' parameter lists; the bodies
  never reference the parameter, so vulture flags it as an unused variable at each request site.
  Same fixture-name-discovery class as `inline_merge_executor` above.
- `_Node` (`tests/test_plan_diff.py`, imported inside `_anchors_from_full_parse`) — reached by
  string: the helper's `quad` parameter is annotated `"_Node | None"`, so the name appears only
  inside a string literal and vulture flags the import as unused (90% confidence).
- `uri` (`tests/core/test_headless_render.py`, the lambda stubbed for `_WarmRenderer._render_once`)
  — the real `_render_once(self, probe_uri)` (src/core/headless_render.py) is called with one
  positional argument from `render_height`, so the stub's replaced two-parameter signature fixes
  the arity and `uri` must stay; deleting it makes the stub raise TypeError. Vulture flags it at
  100% confidence as an unused variable. Same arity-fixed stub-parameter class as the
  `chrome`/`art` entry above.
- `_isolate_profile` (`tests/conftest.py`) — `@pytest.fixture(autouse=True)` in conftest,
  so pytest applies it to every test in the tree with no in-file reference: it pins
  `CHARLIEBOT_HOME` at a fresh temp profile and resets the config caches around each
  test. Vulture flags it as an unused function. Same autouse class as
  `_stub_headless_renderer` above.
- `_codex_home_under_tmp` (`tests/test_session_usage.py`) — `@pytest.fixture(autouse=True)`
  fixture; pytest invokes it around every test in its module with no in-file reference,
  pinning the codex resolver's default home under tmp_path so the seeded rollout tree
  resolves there. Vulture flags it as an unused function. Same autouse class as
  `_clean_probe_state` above.
- `require_model` (`src/core/backend_models.py`) — pydantic `@model_validator(mode='after')`
  method on `BackendBase`, registered with pydantic at class-definition time and invoked during
  model validation: it rejects a backend config entry whose type requires a `model` but declares
  none. The only exact-name matches outside the definition are a prose comment in
  `tests/test_threads_attach_dispatch.py` and this list, so vulture flags it as an unused
  method. Same framework-registered class as the `check_type_and_sources` entry above.
- `_expand_tilde` (`src/core/config.py`, on `PathsConfig`, `UiConfig`, and `PublishConfig`) —
  pydantic `@model_validator(mode='after')` methods, registered with pydantic at
  class-definition time and invoked during model validation: each expands `~` in its
  section's path settings against the process HOME. The method name has exactly zero
  whole-repo matches outside the three definitions, so vulture flags each as an unused
  method. Same framework-registered class as the `check_type_and_sources` entry
  above.
- `drain`, `wait_closed` (the stdin mocks of `stub_subprocess_spawn` in `tests/conftest.py`)
  — attribute writes on the MagicMock asyncio subprocess the helper installs on a spawn
  patch target: `AgentBackend._write_stdin_prompt` (src/agents/backends/base.py) awaits
  them by attribute read when a backend feeds a prompt over stdin, so nothing in the repo
  reads the names statically. Vulture flags each write as an unused attribute. Same
  dynamic-read class as the `speedup` stub entry above.
- `search_sessions` (the `SessionManager` method in `src/core/sessions.py`) — deliberately
  retained two-tier search API, not an orphan. The `/api/sessions/search` route serves
  `search_sessions_readonly` (the cap before per-row work, shared cache references), so the
  wrapper's owned-copy + sidebar-state-fold form has zero production callers since that
  switch — but the same change added a cross-check test pinning that the wrapper serves the
  same rows, and the search-content, master-cc-consumer, pending-trigger-state, and
  archived-pagination tests plus `docs/perf_baseline.md`'s search benchmark drive the
  wrapper as the semantics reference. A src-only vulture scan flags it as an unused method;
  a whole-repo grep finds only those tests, one docstring cross-reference, the same-named
  route handler in `src/api/sessions.py`, and the perf doc.
- `ClientConnection` (`src/core/slack_listener.py`, the `TYPE_CHECKING`-guarded
  `websockets.asyncio.client` import) — reached by string: `_expect_hello`'s parameter is
  annotated `"ClientConnection"`, and that import is what resolves the forward reference
  for type checkers and IDEs. No type checker runs in CI, so a deletion stays suite-green
  while leaving the annotation unresolved. Vulture flags the import as its only
  production-scope finding (unused import, 90% confidence); never delete it on that
  evidence.
- `__getattr__` (`src/core/artifact_wrap.py`) — the PEP 562 lazy-`requests` hook, a one-line
  delegate to the shared `deferred_module_getattr` (`src/core/deferred.py`), which serves
  `load_requests`. (The former `src/cli/common.py` hook left with the phase-separated
  http.client transport; its conftest patch targets now name the `_request_post`/`_request_get`
  adapters directly.)
  Reached by string: the patch target `src.core.artifact_wrap.requests.get`
  (`tests/core/test_artifact_wrap.py`) resolves the module attribute through the hook.
  Vulture flags it as an unused function at 60% confidence.
- `split_sse_lines` (`src/core/sse.py`) — kept deliberately as the SSE framing oracle. The
  property tests in `tests/test_sse.py` drive it through `_split_chunked` and assert the
  production byte framer (`_ChunkedFramer`, same module) matches its answers on every two-way
  split and on random chunkings; the framer's docstring names it the semantics home. No
  production code calls it, so a production-scope vulture scan flags it as an unused function.
- `__getattr__` (`src/agents/backends/opencode.py`, `src/agents/worker.py`, `src/api/cron.py`,
  `src/core/autonamer.py`, `src/core/recap.py`) — the PEP 562 lazy-import hooks, one-line
  delegates to the shared `deferred_module_getattr` (`src/core/deferred.py`); same class as the
  `src/core/artifact_wrap.py` hook entry above. Each serves one external string patch target:
  `src.agents.backends.opencode.httpx.*` (`tests/test_opencode_backend.py`), the
  `WORKER_BUILD_BACKEND_PATCH_TARGET` spelling `src.agents.worker.build_backend`
  (`tests/conftest.py`), `src.api.cron.croniter` (`tests/test_cron_next_run_memo.py`),
  `src.core.autonamer.build_backend` (`tests/test_autonamer.py`), and
  `src.core.recap.build_backend` (`tests/test_recap.py`). Vulture flags each hook as an unused
  function at 60% confidence.
- `_fresh_detail_memo` (`tests/test_thread_detail_gzip.py`) — a `fresh_state_fixture(...)` assignment
  clearing the thread-detail gzip memo (`src.api.threads._detail_gzip_memo`) around every test
  in its module; pytest applies it with no in-file reference, and its name has exactly zero
  whole-repo matches outside its definition. Vulture stays silent on the assignment form (its
  underscore-name ignore covers underscore-prefixed variables), so fixture-name discovery is
  the only thing reaching it. It is load-bearing: the module's plain-request and attach-mode
  tests assert `len(_detail_gzip_memo) == 0`, which holds only because the autouse reset cleared
  the entries earlier gzip tests stored. Same autouse class as `_clean_probe_state` above.
- `_fresh_search_gzip_memo` (`tests/test_search_gzip_memo.py`) — a `fresh_state_fixture(...)`
  assignment clearing the capped search's body-keyed gzip memo (`src.api.sessions._search_gzip_memo`)
  around every test in its module; pytest applies it with no in-file reference, and its name has
  exactly zero whole-repo matches outside its definition. Vulture stays silent on the assignment
  form (its underscore-name ignore covers underscore-prefixed variables), so fixture-name
  discovery is the only thing reaching it. It is load-bearing: the module's plain-request test
  asserts `len(_search_gzip_memo) == 0`, which holds only because the autouse reset cleared the
  entry the module's earlier gzip tests stored. Same autouse class as `_fresh_detail_memo` above.
- `open_connection`, `post_message`, `add_reaction`, `get_permalink`, `get_thread_replies` (the
  Slack-client double `FakeSlackClient` in `tests/conftest.py`, shared by
  `tests/test_slack_listener.py`, `tests/test_slack_delivery.py`, and
  `tests/core/test_slack_thread_follow.py`) — the summon, reply, follow, and ack paths in
  `src/core/slack_listener.py` dispatch every Slack Web API call on the injected client
  (`client.post_message(...)`, `client.get_permalink(...)`, `client.get_thread_replies(...)`,
  `client.add_reaction(...)`, `client.open_connection()`), so each method is reached
  only through that dynamic dispatch. The double's docstring pins the surface ("implements only
  what the listener paths may call"), so a missing method fails with an AttributeError by
  construction, never silently. Vulture flags each method as unused (60% confidence); the names
  match only the double and the real `SlackClient` in `src/core/slack_listener.py`.
- `raise_for_status`, `aclose`, `aiter_bytes` (the httpx response doubles: `FakeChunkedResponse`
  in `tests/conftest.py`, `_FakeDelayedStreamResponse`/`_StubHttpResponse`/
  `_StubEventStreamResponse` in `tests/test_opencode_backend.py`, `_FakeResponse` in
  `tests/test_ext_usage.py`, `_StubSlackResponse` in `tests/test_slack_delivery.py`,
  `_StubResp` in `tests/test_slack_listener.py`) — production reads each through the duck-typed
  response surface: the SSE consumers iterate `response.aiter_bytes()` (`src/core/sse.py`), the
  fetch and Web-API paths call `response.raise_for_status()`, and the proxy paths await
  `response.aclose()`. Each double name matches only its own definition, so vulture flags the
  methods as unused.
- `receive_text`, `send_json`, `resize` (the WebSocket and attachment doubles: `_ScriptedWebSocket`
  and `_FakeAttachment` in `tests/test_terminal_backend.py`, `FakeWebSocket` in
  `tests/conftest.py`) — `pty_common`'s attachment loop awaits `websocket.receive_text()` and
  sends through `websocket.send_json(...)`, the resize path calls `attachment.resize(cols, rows)`,
  and the server catchup/replay producers send through `FakeWebSocket.send_json`; every call
  dispatches on the injected double. Vulture flags each method as unused.
- `accept_waveform`, `flush`, `empty`, `front`, `pop` (`_FakeVad` in
  `tests/test_transcriber_sampling.py`) — `transcribe_pcm_offline`
  (src/agents/transcriber.py) drives the installed VAD duck-typed: it feeds
  `accept_waveform` in 128 ms steps, calls `flush()`, then drains the segment queue
  through `empty()`/`front`/`pop()`. A vulture scan of `tests/test_transcriber_sampling.py`
  alone flags `accept_waveform`, `flush`, `empty`, and `front` as unused methods/property
  (60% confidence); any scan that also takes in `tests/test_voice_offline_models.py` — the
  real-VAD suite, the whole tests/ tree — stays silent because its call sites use the same
  names.
- `_proc`, `_ws` (backend and warm-renderer doubles in `tests/test_backend_logging.py`,
  `tests/test_opencode_backend.py`, and the fake `_launch` in `tests/core/test_headless_render.py`)
  — the stderr pump reads `self._proc.stderr` (`src/agents/backends/base.py`), the opencode
  stdout pump reads `self._proc.stdout` (`src/agents/backends/opencode.py`), and
  `_WarmRenderer._render_once`/`close` read `self._proc`/`self._ws`
  (`src/core/headless_render.py`); the tests install each by attribute write on the double,
  which vulture flags as an unused attribute.
- `_sleep` (`tests/test_opencode_backend.py`, installed as `backend._sleep = _record_sleep`) —
  the opencode lock-retry loop awaits `self._sleep(_LOCK_RETRY_BACKOFF_SECONDS)`
  (`src/agents/backends/opencode.py`); the write replaces the instance's `asyncio.sleep` seam
  with a recorder, and vulture flags the write as an unused attribute.
- `cgroup_exit_report` (the backend doubles `ScriptedRelayBackend`, `TerminateFlagBackend`,
  `_StoppedMidStreamBackend`, `_OomReportBackend` in `tests/conftest.py` and
  `tests/test_master_cc_relay.py`) — the worker finalize path
  (`self._backend.cgroup_exit_report()`, `src/agents/worker.py`) and the master round's error
  path (`backend.cgroup_exit_report()`, `src/agents/master_cc_run.py`) read the session
  memory-cap attribution off whatever backend the test installed. Most doubles return `None`
  (doubles never run inside a cgroup); `_OomReportBackend` returns its scripted report string.
  Vulture flags the methods as unused.
- `add_done_callback` (`DummyTask` in `tests/conftest.py`'s `capture_create_logged_task`) —
  `create_logged_task` (`src/core/tasks.py`) calls `task.add_done_callback(_task_done_callback)`
  on whatever task-like object the patched stand-in returned; the `DummyTask` override accepts
  the callback and drops it. Vulture flags the method as unused.
- `_cron_snapshot`, `_user_agent_cache`, `_token_usage_task` — production module-global caches
  reset through bare module-attribute writes inside test setup
  (`core_config._cron_snapshot = core_config._CronSnapshot()` in `tests/conftest.py`,
  `ext_usage_mod._user_agent_cache = None` in `tests/test_ext_usage.py`'s probe-arm helper,
  `pages._token_usage_task = None` in `tests/test_pages.py`'s autouse fixture). The reads live
  in `src/core/config.py`, `src/api/ext_usage.py`, and `src/api/pages.py`, so vulture flags
  each write as an unused attribute. Same class as the registry-reset fixtures above, minus the
  named-fixture wrapper.
- `broadcast_only`, `expect_fresh_session` — instance-level writes production reads back.
  `session_mgr.broadcast_only = <recorder>` (`_fake_broadcast` in `tests/test_plan_registry.py`,
  `_capture_broadcast` in `tests/test_internal_plan_endpoints.py`) replaces the real
  `SessionManager` method the plan present path awaits (`self._session_mgr.broadcast_only(...)`,
  `src/core/plans.py`), and `item.expect_fresh_session = True` (`tests/test_master_cc_relay.py`)
  sets the `_WorkItem` field whose read gates the resume-capable path
  (`src/agents/master_cc_run.py`, `src/agents/master_cc_relay.py`). No test reads either name
  back, so vulture flags each write as an unused attribute.
- `_reset_api_round_state` (`tests/test_host_auth.py`) — `@pytest.fixture(autouse=True)`
  fixture; pytest invokes it around every test in its module with no in-file reference,
  resetting `api._round_running` and `api._poller.task` before and after each test.
  Vulture flags it as an unused function (60% confidence). Same autouse class as
  `_codex_home_under_tmp` above.
