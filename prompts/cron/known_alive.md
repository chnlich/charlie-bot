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
  `rate_round`, `get_events_jsonl`) — reached by URL string: `server.py` mounts
  each router with `include_router(prefix=...)` and `web/static/js/` fetches the composed paths
  (e.g. `/api/sessions/projects` from `context-panel.js`, `/rounds/{id}/rate` from
  `chat/ratings-recap.js`). The Python function names have exactly zero whole-repo matches outside
  their definitions, so vulture flags each one as an unused function; they must never be deleted on
  that evidence alone. `openai_compatible_messages` above is the same class, kept as its own entry
  because its URL is built inside the Python registry rather than `web/`.
- `_fresh_credential_read_warning_registry`, `_fresh_unknown_limit_shape_registry`, `_fresh_usage_cache`,
  `_fresh_user_agent_cache` (`tests/test_ext_usage.py`),
  `_fresh_pool_state` (`tests/test_claude_accounts.py`),
  `_reset_config_caches` (`tests/test_charliebot_home.py`),
  `_fresh_unhandled_part_type_registry` (`tests/test_opencode_backend.py`),
  `_fresh_renderer_singleton` (`tests/core/test_headless_render.py`),
  `_stub_headless_renderer` (`tests/conftest.py`) — the renderer pair: the first resets the
  warm-renderer singleton around `tests/core/test_headless_render.py`, the second is the
  suite-wide conftest autouse that reshapes `headless_render.render_height` into the
  dump-dom drive seam every artifact/plan-height test relies on,
  `_clear_events_cache` (`tests/test_thread_worker_events.py`),
  `_clear_jsonl_memo` (`tests/test_tui_backend.py`)
  — pytest `autouse=True` fixtures,
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
- `_fresh_single_flight` (`tests/test_token_usage_page.py`) — `@pytest.fixture(autouse=True)`,
  reached by fixture-name discovery like the block above. It resets `pages._token_usage_task`
  (the /token-usage page's module-global single-flight holder) to None around each test. A task
  left installed by one test is bound to that test's event loop, which pytest closes right after;
  the reset keeps a later test's request from awaiting it across loops. Every current test joins
  its request to completion and the request's finally clears the holder, so deleting the fixture
  leaves the suite green. Vulture flags it as an unused function.
- `no_real_usage_capture` (`tests/test_storage_cool.py`) — `@pytest.fixture(autouse=True)`, same
  discovery. It stubs `storage_cool._capture_usage_before_sweep` to `dict`, so every sweep runs
  with the pre-sweep capture as a no-op; the two tests that exercise the capture monkeypatch their
  own fake over the stub, and the fixture is the default every other sweep runs under. Deleting it
  leaves the suite green while the real capture starts running under every sweep test. Vulture
  flags it as an unused function.
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
  Step 3 grep finds it.)
- `voice_preview_websocket` — `@app.websocket` handler in `server.py` (`/ws/voice/{session_id}`),
  reached by URL string: `web/static/js/voice-input.js` dials `/ws/voice/${sessionId}` with the
  transcription backend named in the `?backend=` query, and `tests/voice_input_run.test.js`
  asserts the composed URL. The Python name has exactly zero whole-repo matches outside its
  definition, so vulture flags it as an unused function. Same class as the `session_websocket`
  entry above. The recording and upload endpoints themselves are `src/api/voice.py` route
  handlers, covered by the `src/api/*.py` entry.
- `slack_listener_task`, `slack_backfill_task` — `app.state` task handles assigned in the root
  `server.py` lifespan and read by string: the shutdown loop iterates
  `for attr in ("slack_listener_task", "slack_backfill_task")` and fetches each via
  `getattr(app.state, attr, None)`. Vulture flags the `slack_backfill_task` assignment as an unused
  attribute; the names appear only at the write and inside the string tuple.
- `check_sources_and_mode` — pydantic `@model_validator` method on `ScheduledTaskConfig`
  in `src/core/config.py`, registered with pydantic at class-definition time and invoked during
  model validation (it enforces the prompt-source and mode rules). The method name has
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
  `parent_session_id` likewise, `placeholder` on
  `SlashCommandParam`, `fired_at` on `PendingTrigger`) as unused variables/attributes, but every
  one of those names is grep-findable in repo (`_TRANSIENT_METADATA_FIELDS`, tests, web JS,
  Jinja templates), so the Step 3 grep already protects them and they get no entries.
- `pytestmark` (module-level assignment, e.g. `tests/test_task_prompts.py`) — module-level
  `pytest.mark.asyncio` assignments that pytest's collection reads by attribute name; each name
  appears only at its assignment site, so vulture flags each as an unused variable (60% confidence).
- `pytest_runtest_setup`, `pytest_runtest_call`, `pytest_runtest_teardown` (`tests/conftest.py`)
  — one shared `@pytest.hookimpl(wrapper=True)` wrapper bound under the three stage names;
  pluggy resolves the hook name from the module attribute (pytest requires the `pytest_`
  prefix), so each binding surfaces to vulture as an unused variable. Removing a binding
  silently drops that stage's wall time from the budget accrual `pytest_runtest_makereport`
  enforces.
- `do_POST`, `log_message` (`tests/test_cli_restart_contract.py`) —
  `http.server.BaseHTTPRequestHandler` overrides: the stdlib handler dispatches to them by
  string (`'do_' + self.command` through `getattr`, `log_message` by name). Vulture flags each
  as an unused method; the only other whole-repo matches are the perf doc's embedded collector
  and the handler docstring's own `log_message` mention.
- The `if False: yield {}` lines in `tests/test_master_cc_consumer.py`,
  `tests/conftest.py` (`CapturingBackend`, the shared master-cc round double), and
  `tests/test_worker_diagnostics.py` are flagged as
  100%-confidence unsatisfiable `if` conditions; the unreachable branch is what keeps each fake
  backend's `run()` an async generator (the consumer's `async for` would TypeError a plain
  coroutine), as each site's inline comment states. The condition is the point; nothing to
  delete.
- `model_config` (the pydantic v2 `ConfigDict` class attribute, assigned on the pydantic
  `BaseModel` classes of `src/core/backend_models.py`, `src/core/config.py`, `src/core/models.py`,
  `src/api/diag.py`, and `src/api/cron.py`) — `ModelMetaclass`
  consumes it by attribute name at class-definition time. Every assignment pins
  `extra='forbid'`, which turns an unknown config or request key into a validation error, except
  `TaskCreate` in `src/api/cron.py`, which pins
  `extra='ignore'` (the pydantic default) so the create-request body stays looser than the
  loader's forbid task model, as the comment above the assignment states. Vulture flags each
  production assignment as an unused variable.
- `return_value`, `side_effect` attribute writes across `tests/` (e.g.
  `session_mgr.get_session.return_value = ...` in `tests/test_cli_improve.py`,
  `callbacks.persist_claude_account.side_effect = ...` in `tests/test_claude_accounts.py`) — `unittest.mock`
  configuration attributes the library reads when the configured mock is called
  (`return_value` supplies the call result, `side_effect` overrides it with an iterable,
  callable, or exception). Nothing in the repo reads the names back, so vulture flags such
  writes as unused attributes where it reaches them (the `side_effect` write in
  `tests/test_claude_accounts.py`, 60% confidence). The same two names also appear as
  `AsyncMock(return_value=...)`/`patch(..., side_effect=...)` keyword arguments, which vulture
  does not flag.
- `handle_starttag`, `handle_startendtag`, `handle_endtag`, `handle_data` (`_TreeBuilder`
  in `src/core/artifact_check.py`) — template-method overrides of stdlib
  `html.parser.HTMLParser`: `feed()` drives the base class's scanner, which invokes these
  on `self` under their contract-fixed names while `_parse_dom` builds the DOM. Nothing in
  the repo calls them, each name has exactly zero whole-repo matches outside its own
  definition, and vulture flags each as an unused method. Same class as the
  `do_POST`/`log_message` `BaseHTTPRequestHandler` entry above, with base-class
  virtual dispatch in place of stdlib string dispatch.
- `dir_path` (the `create_provider(provider, label, dir_path)` stubs in
  `tests/test_ext_usage.py`, installed for `ext_usage_mod._create_provider` via
  `monkeypatch.setattr`) — the real `_create_provider` (src/api/ext_usage.py) is called
  with three positional arguments, so the stubs' replaced
  signature fixes the arity and `dir_path` must stay to receive it; deleting the parameter
  makes each stub raise TypeError when the poll loop calls it. Vulture flags it at 100%
  confidence as an unused variable at every stub site in `tests/test_ext_usage.py`.
- `format` (`tests/test_cli_restart_contract.py`, the `log_message` override's second
  parameter) — signature-mirror parameter kept deliberately, not fixed by any call: the
  stdlib invokes `log_message(format, *args)` positionally into the override's trailing
  `*args`, so deleting the parameter stays green; it keeps the override a faithful mirror
  of the stdlib `BaseHTTPRequestHandler.log_message(self, format, *args)` signature.
  Vulture flags it at 100% confidence as an unused variable.
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
  that names unrelated methods. Same class as the `do_POST`/`log_message`
  stdlib-dispatch entry.
- `handle_starttag`, `handle_startendtag`, `handle_endtag`, `handle_data`, `handle_entityref`,
  `handle_charref` (`_Parser` — all six — and `handle_starttag`/`handle_startendtag`/
  `handle_endtag` on `_BoundaryParser`, both in `src/core/plan_diff.py`) — template-method
  overrides of stdlib `html.parser.HTMLParser`,
  same class as the `_TreeBuilder` entry above; `feed()` drives the base scanner, which
  invokes these under their contract-fixed names while each parser builds its DOM. The
  `_TreeBuilder` entry covers only artifact_check's class; each name matches only the parser
  classes' own definitions, so vulture flags each as an unused method. `_OffsetParser`, the
  shared base of both plan_diff parsers, pins `convert_charrefs=False` because its offset
  math must address raw source spans, so `_Parser`'s `handle_entityref`/`handle_charref` —
  the only overrides of that pair — fire there; parsers without the pair either pin
  `convert_charrefs=True` (`_TreeBuilder`), under which the stdlib folds
  references into `handle_data`, or inherit the stdlib no-op defaults (`_BoundaryParser`).
- `isolation_level` (`src/core/storage_cool.py`) — attribute write on a stdlib
  `sqlite3.Connection`; the sqlite3 C module reads it back when executing statements
  (`None` switches the connection to per-statement autocommit transactions, which the
  inline comment pins: one failed DELETE keeps the rest of the batch alive). Nothing in
  the repo reads the name, so vulture flags the write as an unused attribute. Same class
  as the sherpa-onnx `vad_config` attribute-write entry above.
- `isolated_config` (`tests/test_absolute_filepath_prefix.py`) — pytest fixture (owns the config
  and credentials the file router reads: sessions root under tmp_path, empty access key so the
  gate is a no-op), requested by name in one test's parameter list; the body never references
  the parameter, so vulture flags it as an unused variable at that request site. Same
  fixture-name-discovery class as the autouse block above.
- `cli_katex` (`tests/core/test_artifact_wrap.py`) — pytest fixture (monkeypatches
  `src.cli.common.get_config` so the wrap verb's config home lands under the pytest tmp tree
  instead of the host profile), requested by name in three tests' parameter lists; the bodies
  never reference the parameter, so vulture flags it as an unused variable at each request site.
  Same fixture-name-discovery class as `isolated_config` above.
- `uri` (`tests/core/test_headless_render.py`, the lambda stubbed for `_WarmRenderer._render_once`)
  — the real `_render_once(self, probe_uri)` (src/core/headless_render.py) is called with one
  positional argument from `render_height`, so the stub's replaced two-parameter signature fixes
  the arity and `uri` must stay; deleting it makes the stub raise TypeError. Vulture flags it at
  100% confidence as an unused variable. Same arity-fixed stub-parameter class as the
  `dir_path` entry above.
- `_isolate_profile` (`tests/conftest.py`) — `@pytest.fixture(autouse=True)` in conftest,
  so pytest applies it to every test in the tree with no in-file reference: it pins
  `CHARLIEBOT_HOME` at a fresh temp profile and resets the config caches around each
  test. Vulture flags it as an unused function. Same autouse class as
  `_stub_headless_renderer` above.
- `_codex_home_under_tmp` (`tests/test_session_usage.py`) — `@pytest.fixture(autouse=True)`
  fixture; pytest invokes it around every test in its module with no in-file reference,
  pinning the codex resolver's default home under tmp_path so the seeded rollout tree
  resolves there. Vulture flags it as an unused function. Same autouse class as
  `_reset_config_caches` above.
- `require_model` (`src/core/backend_models.py`) — pydantic `@model_validator(mode='after')`
  method on `BackendBase`, registered with pydantic at class-definition time and invoked during
  model validation: it rejects a backend config entry whose type requires a `model` but declares
  none. The only exact-name matches outside the definition are this list and the
  `option_default_model` docstring's reference (`src/core/backend_models.py`), so vulture
  flags it as an unused method. Same framework-registered class as the
  `check_sources_and_mode` entry above.
- `_expand_tilde` (`src/core/config.py`, on `PathsConfig`, `UiConfig`, and `PublishConfig`) —
  pydantic `@model_validator(mode='after')` methods, registered with pydantic at
  class-definition time and invoked during model validation: each expands `~` in its
  section's path settings against the process HOME. The method name has exactly zero
  whole-repo matches outside the three definitions, so vulture flags each as an unused
  method. Same framework-registered class as the `check_sources_and_mode` entry
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
  same rows, and the archived-pagination tests plus `docs/perf_baseline.md`'s search benchmark
  drive the wrapper as the semantics reference. A src-only vulture scan flags it as an unused
  method; a whole-repo grep finds only that test file, one docstring cross-reference, the
  same-named route handler in `src/api/sessions.py`, and the perf doc.
- `ClientConnection` (`src/core/slack_listener.py`, the `TYPE_CHECKING`-guarded
  `websockets.asyncio.client` import) — reached by string: `_expect_hello`'s parameter is
  annotated `"ClientConnection"`, and that import is what resolves the forward reference
  for type checkers and IDEs. No type checker runs in CI, so a deletion stays suite-green
  while leaving the annotation unresolved. Current vulture reads the annotation as a use and
  no longer flags the import; the no-type-checker fact is what keeps a deletion suite-green,
  so the guard stays.
- `__getattr__` (`src/core/artifact_wrap.py`) — the PEP 562 lazy-`requests` hook, a one-line
  delegate to the shared `deferred_module_getattr` (`src/core/deferred.py`), which serves
  `load_requests`. (The former `src/cli/common.py` hook left with the phase-separated
  http.client transport; its conftest patch targets now name the `_request_post`/`_request_get`
  adapters directly.)
  Reached by string: the patch target `src.core.artifact_wrap.requests.get`
  (`tests/core/test_artifact_wrap.py`) resolves the module attribute through the hook.
  Vulture flags it as an unused function at 60% confidence.
- `split_sse_lines` (`src/core/sse.py`) — kept deliberately as the SSE framing oracle. The
  tests in `tests/test_sse.py` drive it directly (the trailing-CR hold, the CRLF straddle, the
  final flush) and pin the production reader's byte mode against its str mode on every two-way
  split; the byte framer's docstring (`_ChunkedFramer`, same module) names it the semantics
  home. No production code calls it, so a production-scope vulture scan flags it as an unused
  function.
- `__getattr__` (`src/agents/backends/opencode.py`, `src/agents/worker.py`, `src/api/chat.py`,
  `src/core/master_trigger.py`)
  — the PEP 562 lazy-import hooks; same class as the `src/core/artifact_wrap.py` hook entry
  above. All but the opencode hook are one-line delegates to the shared `deferred_module_getattr`
  (`src/core/deferred.py`); the opencode hook writes the same match-or-AttributeError shape
  inline (`if name == "httpx": import httpx`) because it binds one import rather than a loader.
  Each serves one external string patch target: `src.agents.backends.opencode.httpx.*`
  (`tests/test_opencode_backend.py`), the `WORKER_BUILD_BACKEND_PATCH_TARGET` spelling
  `src.agents.worker.build_backend` (`tests/conftest.py`), the `CHAT_CANCEL_MASTER_PATCH_TARGET`
  spelling `src.api.chat.cancel_master` (`tests/test_chat_cancel.py`, constant defined in
  `tests/conftest.py`), and the `MASTER_TRIGGER_RUN_MESSAGE_PATCH_TARGET` spelling
  `src.core.master_trigger.run_message` (`tests/test_spawner_trigger_master_resume_recovery.py`,
  constant defined in `tests/conftest.py`).
  Vulture flags each hook as an unused function at 60% confidence.
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
  `tests/test_ext_usage.py`) — production reads each through the duck-typed
  response surface: the SSE consumers iterate `response.aiter_bytes()` (`src/core/sse.py`), the
  fetch and Web-API paths call `response.raise_for_status()`, and the proxy paths await
  `response.aclose()`. Each double name matches only its own definition, so vulture flags the
  methods as unused.
- `resize` (the pty attachment double `ScriptedTtyAttachment` in
  `tests/test_tui_task_completion.py`) — the resize path calls `attachment.resize(cols, rows)`
  (`src/agents/backends/pty_common.py`), dispatching on the injected double. Vulture flags the
  method as unused.
- `_proc`, `_ws` (the fake `_launch` in `tests/core/test_headless_render.py`) —
  `_WarmRenderer._render_once`/`close` read `self._proc`/`self._ws`
  (`src/core/headless_render.py`); the fake installs them by attribute write, which vulture
  flags as an unused attribute.
- `_sleep` (`tests/test_opencode_backend.py`, installed as `backend._sleep = _record_sleep`) —
  the opencode lock-retry loop awaits `self._sleep(_LOCK_RETRY_BACKOFF_SECONDS)`
  (`src/agents/backends/opencode.py`); the write replaces the instance's `asyncio.sleep` seam
  with a recorder, and vulture flags the write as an unused attribute.
- `cgroup_exit_report` (the backend doubles `ScriptedRelayBackend` and `TerminateFlagBackend`
  in `tests/conftest.py`) — the worker finalize path
  (`self._backend.cgroup_exit_report()`, `src/agents/worker.py`) and the master round's error
  path (`backend.cgroup_exit_report()`, `src/agents/master_cc_run.py`) read the session
  memory-cap attribution off whatever backend the test installed. Both doubles return `None`
  (doubles never run inside a cgroup). Vulture flags the methods as unused.
- `_cron_snapshot` — a production module-global cache reset through a bare module-attribute
  write inside test setup (`core_config._cron_snapshot = core_config._CronSnapshot()` in
  `tests/conftest.py`). The read lives in `src/core/config.py`, so vulture flags the write as an
  unused attribute. Same class as the registry-reset fixtures above, minus the named-fixture wrapper.
- `_reset_api_round_state` (`tests/test_host_auth.py`) — `@pytest.fixture(autouse=True)`
  fixture; pytest invokes it around every test in its module with no in-file reference,
  resetting `api._round_running` and `api._poller.task` before and after each test.
  Vulture flags it as an unused function (60% confidence). Same autouse class as
  `_codex_home_under_tmp` above.
- `_round_running` (the reset writes in `tests/test_host_auth.py`'s `_reset_api_round_state`) —
  a production module-global write from test setup, read in `src/api/host_auth.py`. A tests-only
  vulture scan flags the write as an unused attribute; the combined src+tests scan sees the read
  and stays silent. Same class as the `_cron_snapshot` entry above.- `history` (the `MessageProjection` property in `src/core/message_projection.py`) — kept
  deliberately as the projection's semantics oracle, not an orphan. No production reader consumes
  it: the pagination paths read `tail`/`slice_before`/`cached_page_body`/`pending_draft` and the
  gzip body memos instead. Its consumer is the definitional pin in `tests/test_message_projection.py`
  (`test_projection_history_equals_events_to_messages`, parametrized): `history` must equal
  `events_to_messages(all_events)` because it feeds the same reference path, and that module's
  page-walk and draft-identity tests read it as that reference. `tests/test_scheduler_shared_session_manager.py`
  reads its length once, and `docs/perf_baseline.md`'s projection-parity collector digests it. A
  src-only vulture scan flags it as an unused property; a whole-repo grep finds only the definition,
  the class docstring's definitional sentence, those tests, and the perf doc. Same
  deliberately-retained-oracle class as the `search_sessions` entry above.
- `_get_close_waiter` (and its `stream` parameter) (`src/agents/backends/spawn.py`) — reached by
  the stdlib's duck-typed close contract: `asyncio.StreamWriter.wait_closed()` resolves
  `self._protocol._get_close_waiter(self)` (CPython 3.12.3 `asyncio.streams`), and
  `_StdinPipeProtocol` is the protocol `_wire_writer` hands `connect_write_pipe` for every piped
  backend stdin. The bare `FlowControlMixin` fallback raises `NotImplementedError`, so deleting
  the override — or the parameter the call passes — turns the next `proc.stdin.close()` +
  `wait_closed()` on a piped stdin into that error; the class docstring states the contract.
  Vulture flags the method as an unused method (60% confidence) and `stream` as an
  unused variable (100% confidence); a whole-repo grep finds only the definition. Never delete
  it on that evidence.
- `_unprobed_verdict_stays_idle` (`src/api/sessions.py`, on `SessionDetailResponse`) — pydantic
  `@field_validator` method, registered with pydantic at class-definition time and invoked
  during model validation: it keeps a never-probed row's response `work_state` at the literal
  `'idle'` instead of `null`. The name has exactly zero whole-repo matches outside its
  definition, so vulture flags it as an unused method. Same framework-registered class as the
  `check_sources_and_mode` entry above.
- `_load_legacy_single_event_id` (`src/core/models.py`, on `MasterRunRecord`) — pydantic
  `@model_validator(mode='before')` method: it loads a pre-batching record's single
  `user_event_id` as a one-element `user_event_ids`, so an interrupted turn written by the old
  schema still feeds the restart-replay exclusion set. The name has exactly zero whole-repo
  matches outside its definition, so vulture flags it as an unused method. Same
  framework-registered class as the `check_sources_and_mode` entry above; deleting it would not
  fail validation, it would silently drop the legacy record's replay exclusion.
- `_default_backend_is_registered` (`src/core/config.py`, on `VoiceConfig`) — pydantic
  `@model_validator(mode='after')` method: it rejects a `default_backend` typo against the
  registry's ids at startup. The name has exactly zero whole-repo matches outside its
  definition, so vulture flags it as an unused method. Same framework-registered class as the
  `check_sources_and_mode` entry above.
- `voice_setup` (the module `src/core/voice_setup.py`) — reached by string:
  `scripts/setup.sh` runs `python -m src.core.voice_setup enable` on GPU hosts, so the module
  has no import-site reference anywhere in Python; a reference scan restricted to `.py`/`.js`
  sources reads it as an unreferenced module.
- `_theme`, `_decolor` (attributes set by the no-color arm of `CliHelpFormatter._set_color`,
  `src/cli/help_formatter.py`) — read by stdlib argparse 3.14's own formatting methods
  (`_Section.format_help`, `_format_usage`, `_format_action` read `self._theme` /
  `self._decolor`), so the two assignments are what the stock `_set_color` would have
  installed and the piped arm's help render depends on them: a formatter whose `_set_color`
  installs neither attribute raises AttributeError on the first help render. Vulture flags
  each as an unused attribute and a whole-repo grep finds only the assignment lines. Never
  delete them on that evidence.
- `_reject_legacy_allow_list` (`src/core/config.py`, on `DiscordConfig`) — pydantic
  `@model_validator(mode='before')` method, registered with pydantic at class-definition time
  and invoked during model validation: it rejects the retired `discord.allowed_user_ids` list
  with an error whose text names the `allowed_users` id-to-person map that replaced it. The
  method name has exactly zero whole-repo matches outside its definition, so vulture flags it
  as an unused method. Deleting it keeps the rejection (`DiscordConfig` pins `extra='forbid'`,
  and the error's location path still names `discord.allowed_user_ids`, which is also what the
  plumbing test asserts on) — what goes is the instruction to move each id into `allowed_users`.
  Same framework-registered class as the `check_sources_and_mode` entry above.
- `chunk_size` (`src/api/files.py`, on `_ServedFileResponse`) — class attribute on the
  `FileResponse` subclass the file server's bare-file arm streams with. Starlette 1.0.0 reads
  `self.chunk_size` in every body-streaming path and exposes the knob only as a class
  attribute (no `__init__` parameter), so the 1 MiB assignment is the only way to widen it from
  Starlette's 64 KiB default. Vulture flags it as an unused attribute and a whole-repo grep
  finds only the assignment line; deleting it reverts the serve to 16x more chunk reads per MB
  (wire bytes unchanged, so the suite stays green).
- `row_factory` (`src/core/usage_ledger.py`, the write on `UsageLedger`'s sqlite3 connection) —
  the sqlite3 layer reads `connection.row_factory` when it builds result rows: with the default
  factory every row is a tuple and the ledger readers' `row["path"]`-style name access raises
  TypeError on the first query. Vulture flags the write as an unused attribute and a whole-repo
  grep finds only the assignment line. Never delete it on that evidence.
- `lifespan_context` (`src/core/session_tree_preview.py`, the write on the preview app's
  router) — Starlette's `Router` installs `self.lifespan_context` at construction and runs it
  when the ASGI lifespan scope arrives, so the assignment is the swap that boots the real
  shipped application under the preview home instead of the production lifespan. Vulture flags
  it as an unused attribute and a whole-repo grep finds only the assignment line; deleting it
  makes the preview harness boot the production lifespan against a synthetic home.
- `_reader` (`scripts/browser_harness_session_tree.py`, on the `CDP` websocket client) — the
  only strong reference to the reader task: asyncio's event loop holds tasks only weakly, and
  its own `create_task` doc requires saving a reference to avoid a task disappearing
  mid-execution. Vulture flags it as an unused attribute and a whole-repo grep finds only the
  assignment line; dropping the assignment leaves the reader task collectible mid-loop, so the
  harness would sporadically stop reading frames.
