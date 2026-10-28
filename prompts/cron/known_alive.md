# Known-alive symbols (code-health cron appendix)

`prompts/cron/code_health.md` Step 4 points here. These symbols look dead to static tools
but are reached by string reference or kept deliberately. Never delete them on static-tool
evidence alone. Append an entry whenever a code-health run confirms a symbol is reached by
string, and land that edit in the same PR. Entries anchor each symbol by name and file only;
code_health.md Step 1 bans coordinate citations, so no line numbers appear here.

Known-alive symbols:
- `_clean_ceiling_env` — pytest fixture in `tests/test_session_usage.py`, reached by string via
  `@pytest.mark.usefixtures("_clean_ceiling_env")`; invisible to static dead-code tools.
- `_handle_agent_message`, `_handle_reasoning`, `_handle_tool_item`, `_handle_file_change`,
  `_handle_mcp_tool_call`, `_handle_todo_list`, `_handle_error` — Codex backend
  item-event handlers in `src/backends/codex/codex.py`, reached by string via the `_ITEM_HANDLERS`
  name list and `getattr(self, handler_name)` dispatch in `_translate_item_event`.
- `_fresh_credential_read_warning_registry`, `_fresh_unknown_limit_shape_registry`, `_fresh_usage_cache`,
  `_fresh_user_agent_cache`, `_fresh_claude_accounts`, `_fresh_codex_account` (`tests/test_ext_usage.py`),
  `_fresh_pool_state` (`tests/test_claude_accounts.py`),
  `_reset_config_caches` (`tests/test_charliebot_home.py`) — pytest `autouse=True` fixtures,
  reached by pytest's fixture-name discovery only: zero whole-repo matches outside their
  definitions, so vulture flags them as unused functions. Most are single-line
  `fresh_state_fixture(...)` assignments in their module (built by the conftest factory of the
  same name) rather than `def` fixtures; vulture stays silent on those assignments — its
  underscore-name ignore covers underscore-prefixed variables — so the underscore-prefixed ones
  rely on fixture-name discovery alone, while `def` forms surface as unused functions. Vulture
  also flags
  `pidfd_open_available` (`tests/conftest.py`, the pid-watch tests' skip gate, requested
  by name in `tests/test_trigger_pid_watch.py`), but it is named in the parameter lists of the
  tests that use it, so the Step 3 grep already finds its references; no list entry needed.
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
- `register` (the `__init__.py` of each package listed in `src/app/registrations.py`) and
  `setup_step` (the attribute named in a `wiring.register_setup_step` call) — reached by
  string: `register_all()` and `wiring.setup_steps()` import the module from its path string
  and read the attribute by name. The names have zero whole-repo matches outside their
  definitions, so vulture flags them as unused.
- `check_sources_and_mode` — pydantic `@model_validator` method on `ScheduledTaskConfig`
  in `src/features/cron/config.py`, registered with pydantic at class-definition time and invoked during
  model validation (it enforces the prompt-source and mode rules). The method name has
  exactly zero whole-repo matches outside its definition, so vulture flags it as an unused
  method.
- `setup_step` (`src/features/cron/seed.py`) — production-scope vulture (`src/`) flags it
  as an unused function; it is reached by string: `src/features/cron/__init__.py` registers it through
  `wiring.register_setup_step("src.features.cron.seed", attr="setup_step")`, and `scripts/setup.sh`
  runs the registered steps through `src.app.setup`. Its callee `seed_default_cron_tasks` rides along.
  The server-start path never calls the pair, by design: only the setup command seeds cron config.
  `tests/test_cron_defaults.py` asserts that `seed_default_cron_tasks` stays out of
  `init_charliebot_home.__code__.co_names`.
- Vulture flags pydantic response-model
  fields served to `web/` (`schedule_cron`/`schedule_enabled`/`schedule_next_run`/
  `schedule_timezone`/`schedule_project`/`schedule_allow_failure` on `SessionMetadata`,
  `parent_session_id` likewise,
  `fired_at` on `PendingTrigger`) as unused variables/attributes, but every
  one of those names is grep-findable in repo (`TRANSIENT_METADATA_FIELDS`, tests, web JS,
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
- The `if False:` / `yield {}` marker in `tests/test_worker_diagnostics.py` is flagged as a
  100%-confidence unsatisfiable `if` condition; the unreachable branch is what keeps the fake
  backend's `run()` an async generator (the consumer's `async for` would TypeError a plain
  coroutine), as the site's inline comment states. The condition is the point; nothing to
  delete. The same marker takes a second form: an unreachable `yield` after a `raise` in
  `tests/test_task_run_backend_types.py` and `tests/test_turn_contributions.py`, which vulture
  reads as unreachable code after `raise` at 100% confidence; the yield is again what keeps each
  fake backend's `run()` an async generator.
- `model_config` (the pydantic v2 `ConfigDict` class attribute, assigned on the pydantic
  `BaseModel` classes of `src/infra/backend_models.py`, `src/infra/config.py`, `src/infra/models.py`,
  `src/backends/claude_code/claude_config.py`, `src/features/artifacts/config.py`,
  `src/features/code_server/config.py`, `src/features/discord/config.py`,
  `src/features/slack/config.py`, `src/features/voice/config.py`, and `src/features/cron/config.py`) —
  `ModelMetaclass` consumes it by attribute name at class-definition time. Every assignment pins
  `extra='forbid'`, which turns an unknown config or request key into a validation error, except
  `CharlieBotConfig` in `src/infra/config.py`, which pins `extra='allow'` so the sections that
  packages register ride as extras (`_parse_package_sections` refuses every other unknown key).
  Vulture flags each production assignment as an unused variable.
- `return_value`, `side_effect` attribute writes across `tests/` (e.g.
  `store.get_session.return_value = ...` in `tests/test_cli_improve.py`,
  `callbacks.persist_account_label.side_effect = ...` in `tests/test_claude_accounts.py`) — `unittest.mock`
  configuration attributes the library reads when the configured mock is called
  (`return_value` supplies the call result, `side_effect` overrides it with an iterable,
  callable, or exception). Nothing in the repo reads the names back, so vulture flags such
  writes as unused attributes where it reaches them (the `side_effect` write in
  `tests/test_claude_accounts.py`, 60% confidence). The same two names also appear as
  `AsyncMock(return_value=...)`/`patch(..., side_effect=...)` keyword arguments, which vulture
  does not flag.
- `file-chip--failed`, `file-chip--uploaded`, `file-chip--uploading` (`web/static/css/styles.css`)
  — reached by string construction: `file-upload.js` emits
  ``file-chip file-chip--${file.status}``, and `tests/chat_attachments_render.test.js` asserts
  the failed and uploaded variants by name. A dead-selector scan that greps each full class
  name finds no template or JS literal, so it flags them as unused CSS.
- `turn-row-tag-trigger`, `turn-row-tag-turn`, `turn-row-tag-worker`, `turn-row-tag-you`
  (`web/static/css/styles.css`) — reached by string construction: `chat/rendering.js` emits
  ``turn-row-tag turn-row-tag-' + label.toLowerCase()``, with the labels
  `You`/`Turn`/`Worker`/`Trigger` supplying the suffixes; `tests/chat_session_bump.test.js`
  asserts the wrapper class by name. A dead-selector scan grepping each full class name
  finds only its definition, so it flags them as unused CSS.
- `activate` (`src/features/terminal/static/terminal_clipboard.js`, the object addon passed to
  `term.loadAddon`) — xterm.js lifecycle dispatch: the library's `_addonManager.loadAddon`
  ends with `t.activate(e)`, invoking the addon's `activate` method by that name
  (verified against the CDN build the template loads, xterm 5.3.0). It is what attaches
  the copy-on-select `mouseup` listener; deleting it silently kills terminal
  copy-on-select. A JS unreferenced-method scan finds only the definition, so it flags
  the method as dead; `dispose` in the same object literal is reached the same way.
- `postCommentMessage` (`web/static/js/comment_post.js`) — cross-file browser global:
  `diff_comments.js` and `artifact-comments.js` call it as a bare identifier resolved
  through the page's script-tag global scope, each page loading the file before the
  widget (`src/features/diff_view/templates/diff.html`). A per-file dead-function scan finds only the
  definition, so it flags the function as unused.
- `render` (`FastJsonResponse` in `src/infra/responses.py`) — template-method override of
  starlette `JSONResponse.render`: the base `Response.__init__` calls `self.render(content)`
  by that name when a response is built. As a Python identifier the name has zero
  matches outside its definition, so a Python-scoped dead-method scan (vulture) flags it as
  an unused method; a whole-repo grep is noisier — `render` is also ordinary web-JS DOM code
  (`plan-panel.js` `function render()`)
  that names unrelated methods.
- `_isolate_profile` (`tests/conftest.py`) — `@pytest.fixture(autouse=True)` in conftest,
  so pytest applies it to every test in the tree with no in-file reference: it pins
  `CHARLIEBOT_HOME` at a fresh temp profile and resets the config caches around each
  test. Vulture flags it as an unused function. Same autouse class as
  the `_fresh_*` entry above.
- `_codex_home_under_tmp` (`tests/test_session_usage.py`) — `@pytest.fixture(autouse=True)`
  fixture; pytest invokes it around every test in its module with no in-file reference,
  pinning the codex resolver's default home under tmp_path so the seeded rollout tree
  resolves there. Vulture flags it as an unused function. Same autouse class as
  `_reset_config_caches` above.
- `require_model` (`src/infra/backend_models.py`) — pydantic `@model_validator(mode='after')`
  method on `BackendOption`, registered with pydantic at class-definition time and invoked during
  model validation: it rejects a backend config entry whose type requires a `model` but declares
  none. The only exact-name matches outside the definition are this list and the
  `option_default_model` docstring's reference (`src/infra/backend_models.py`), so vulture
  flags it as an unused method. Same framework-registered class as the
  `check_sources_and_mode` entry above.
- `_expand_tilde` (`PathsConfig` in `src/infra/config.py` and `PublishConfig` in
  `src/features/artifacts/config.py`) —
  pydantic `@model_validator(mode='after')` methods, registered with pydantic at
  class-definition time and invoked during model validation: each expands `~` in its
  section's path settings against the process HOME. The method name has exactly zero
  whole-repo matches outside the three definitions, so vulture flags each as an unused
  method. Same framework-registered class as the `check_sources_and_mode` entry
  above.
- `drain`, `wait_closed` (the stdin mocks of `stub_subprocess_spawn` in `tests/conftest.py`)
  — attribute writes on the MagicMock asyncio subprocess the helper installs on a spawn
  patch target: `AgentBackend._write_stdin_prompt` (src/runtime/agent_process/base.py) awaits
  them by attribute read when a backend feeds a prompt over stdin, so nothing in the repo
  reads the names statically. Vulture flags each write as an unused attribute: the only
  reader is the runtime attribute access on the double, which static analysis cannot tie
  back to this write site.
- `search_sessions` (the `SessionSearch` method in `src/runtime/session_search.py`) — deliberately
  retained two-tier search API, not an orphan. No Python route calls it now that the
  `/api/sessions/search` route is served by the Rust server, but the archived-pagination tests
  (`tests/test_archived_pagination.py`) drive the wrapper as the semantics reference. A src-only
  vulture scan flags it as an unused method; a whole-repo grep finds only those tests and one
  docstring cross-reference in `src/runtime/session_store.py`.
- `client` (`src/features/slack/slack_listener.py`, the `TYPE_CHECKING`-guarded
  `from websockets.asyncio import client` import) — reached by string: the `_expect_hello`
  and `_serve_socket_mode` parameters are annotated `client.ClientConnection`, and that
  import is what resolves the forward reference for type checkers and IDEs. No type checker
  runs in CI, so a deletion stays suite-green while leaving the annotations unresolved.
  Vulture reads the annotations as a use and does not flag the import; the
  no-type-checker fact is what keeps a deletion suite-green, so the guard stays.
- `__getattr__` (`src/runtime/worker.py`, `src/runtime/autonamer.py`) — the PEP 562 lazy-import
  hooks. Each is a one-line delegate to the shared `deferred_module_getattr`
  (`src/infra/deferred.py`) serving the module's deferred `build_backend`. The worker hook resolves the
  `WORKER_BUILD_BACKEND_PATCH_TARGET` spelling `src.runtime.worker.build_backend` (constant defined
  in `tests/conftest.py`); the autonamer hook resolves the attribute-form patch
  `monkeypatch.setattr(autonamer, "build_backend", ...)` in `tests/test_master_cc_consumer.py`,
  whose existence check reads the attribute before the loader runs. Vulture flags each hook as an
  unused function at 60% confidence.
- `open_connection`, `post_message`, `add_reaction`, `get_thread_replies` (the
  Slack-client double `FakeSlackClient` in `tests/conftest.py`, shared by
  `tests/test_slack_listener.py`, `tests/test_slack_delivery.py`, and
  `tests/core/test_slack_thread_follow.py`) — the summon, reply, follow, and ack paths in
  `src/features/slack/slack_listener.py` dispatch every Slack Web API call on the injected client
  (`client.post_message(...)`, `client.get_thread_replies(...)`,
  `client.add_reaction(...)`, `client.open_connection()`), so each method is reached
  only through that dynamic dispatch. The double's docstring pins the surface ("implements only
  what the listener paths may call"), so a missing method fails with an AttributeError by
  construction, never silently. Vulture flags each method as unused (60% confidence); the names
  match only the double and the real `SlackClient` in `src/features/slack/slack_listener.py`.
- `cgroup_exit_report` (the backend doubles `ScriptedRelayBackend` and `TerminateFlagBackend`
  in `tests/conftest.py`) — the worker finalize path
  (`self._backend.cgroup_exit_report()`, `src/runtime/worker.py`) and the master round's error
  path (`backend.cgroup_exit_report()`, `src/runtime/master_cc_run.py`) read the session
  memory-cap attribution off whatever backend the test installed. Both doubles return `None`
  (doubles never run inside a cgroup). Vulture flags the methods as unused.
- `_cron_snapshot` — a production module-global cache reset through a bare module-attribute
  write inside test setup (`cron_loader._cron_snapshot = cron_loader._CronSnapshot()` in
  `tests/conftest.py`). The read lives in `src/features/cron/loader.py`, so vulture flags the write as an
  unused attribute. Same class as the registry-reset fixtures above, minus the named-fixture wrapper.
- `history` (the `MessageProjection` property in `src/runtime/message_projection.py`) — kept
  deliberately as the projection's semantics oracle, not an orphan. No production reader consumes
  it: the pagination paths read `tail`/`slice_before`/`cached_page_body`/`pending_draft` and the
  gzip body memos instead. Its consumer is the definitional pin in `tests/test_message_projection.py`
  (`test_projection_history_equals_events_to_messages`, parametrized): `history` must equal
  `events_to_messages(all_events)` because it feeds the same reference path, and that module's
  page-walk and draft-identity tests read it as that reference. A
  src-only vulture scan flags it as an unused property; a whole-repo grep finds only the definition,
  the class docstring's definitional sentence, and those tests. Same
  deliberately-retained-oracle class as the `search_sessions` entry above.
- `_get_close_waiter` (and its `stream` parameter) (`src/runtime/agent_process/spawn.py`) — the
  stdlib's duck-typed close contract: `asyncio.StreamWriter.wait_closed()` awaits
  `self._protocol._get_close_waiter(self)` in `asyncio.streams`. `_StdinPipeProtocol` is the
  protocol `_wire_writer` gives `connect_write_pipe` for every piped backend stdin, so the
  subprocess-stdin close path (`src/runtime/agent_process/base.py` awaits `stdin.wait_closed()`)
  dispatches through it. The bare `FlowControlMixin` raises `NotImplementedError` without the
  override. Deleting the override, or the parameter the call passes, turns the next
  `proc.stdin.close()` plus `wait_closed()` into that error. The class docstring states the
  contract. A single-file vulture scan of `spawn.py` flags the method as unused (60% confidence)
  and `stream` as an unused variable (100% confidence). A whole-repo grep finds only the
  definition. Never delete it on that evidence.
- `check_default_backend` (`src/features/voice/config_check.py`), `check_claude_pools`
  (`src/backends/claude_code/claude_config.py`), `AccountsConfig` (same file),
  each option model class in
  `src/backends/*/options.py`, `snapshot_reading_limits` (`src/backends/opencode/opencode_limits.py`), and
  `claude_reading_limits` (`src/backends/claude_code/claude_code.py`) —
  reached by string: the `register()` of each package passes a "module:attr" string to
  `config_registry.register_config_check`, `register_config_section` or `register_option_model`
  (the option strings ride `backend_type_registration.register_backend_type(options=...)`) or to
  `backend_lifecycle_registration.register_reading_limits`, and the registry imports the attribute by that
  string at the first config parse or first use. The names have no whole-repo matches outside their
  definitions, so vulture flags them as unused. `check_default_backend` rejects a
  `voice.default_backend` typo against the transcription registry's ids at startup.
- `_reject_legacy_allow_list` (`DiscordConfig` in `src/features/discord/config.py`) — pydantic
  `@model_validator(mode="before")` classmethod: it rejects the retired
  `discord.allowed_user_ids` list at startup, naming `discord.allowed_users` as its
  successor. The name has exactly zero whole-repo matches outside its definition, so
  vulture flags it as an unused method. Same framework-registered class as the
  `check_sources_and_mode` entry above; deleting it would still fail validation
  (`DiscordConfig` sets `extra='forbid'`, so the retired key dies as an unknown
  field), but the error would no longer name `allowed_users`, the successor the
  operator must move each id into.
- `run_scheduled_backup` (`src/features/backup/backup.py`), `run_scheduled_usage_ledger`
  (`src/features/usage/usage_ledger.py`), `run_scheduled_cool_storage`
  (`src/features/usage/storage_cool.py`) and `run_scheduled`
  (`src/features/disk_sweep/sweep.py`) — reached by string: each feature package's
  `register()` passes the function to `scheduled_handlers.register_handler(<name>, <module>,
  attr=<attr>)` (`src/features/backup/__init__.py`, `src/features/usage/__init__.py`,
  `src/features/disk_sweep/__init__.py`), and a
  registered task with `handler: <name>` awaits `getattr(import_module(module), attr)()`
  (`src/runtime/hooks/scheduled_handlers.py`). Vulture flags each of the four as an unused
  function at 60% confidence.
- `PoolNodeMetadata` (`src/runtime/backend_pool.py`), `CronMetadata`
  (`src/features/cron/metadata.py`), `SlackSessionFields` (`src/features/slack/metadata.py`) and
  `WebReplyMetadata` (`src/features/web_reply/metadata.py`) — reached by string: each owning
  package's `register()` passes its "module:Class" string to
  `metadata_slot_registration.register_metadata_fields` (for example `("backend_pool",
  "src.runtime.backend_pool:PoolNodeMetadata")` in `src/features/backend_pool/__init__.py`), and
  `metadata_slots` imports the model from that string on its first call. A whole-repo grep finds
  only the definitions and the registrations, so vulture flags each class as unused.
  `DiscordSessionFields` (`src/features/discord/metadata.py`) and the claude_code pair
  (`ClaudeSessionFields`, `ClaudeThreadFields` in `src/backends/claude_code/claude_metadata.py`)
  register through the same call; a direct reference (`cast` in `discord_commands.py`, the
  module's own `isinstance` checks) keeps vulture silent on them.
- `last_run_status` (`CronMetadata` in `src/features/cron/metadata.py`) and `slack_watermark_ts`
  (`SlackSessionFields` in `src/features/slack/metadata.py`) — fields of string-registered slot
  models, set by keyword and read by string. The scheduler writes `last_run_status` through
  `update_slot_fields(..., last_run_status=...)` (`src/features/cron/scheduler.py`), and
  `src/features/cron/static/cron.js` renders `s.last_run_status` off the payload. The slack
  listener names `slack_watermark_ts` by string as the shared mention path's `watermark_field`
  (`src/features/slack/slack_listener.py`), and `thread_entry.py` reads and writes it through
  `getattr(fields, platform.watermark_field)`. Vulture sees neither keyword nor string reach, so
  it flags each declaration as an unused variable.
- `__getattr__` (`src/infra/constants.py`,
  `src/infra/models.py`) — the PEP 562 lazy-attribute hooks, same class as the
  worker/autonamer entry above. The constants hook builds `REPO_ROOT` on first read so the CLI
  import floor pays no pathlib chain; the models hook binds the `_BACKEND_REEXPORTS` names
  (`BackendOption`, `option_default_model`) from `src.infra.backend_models` on first read,
  keeping the backend stack off the session-model import path. Vulture flags each hook as an
  unused function at 60% confidence.
- `CONTRIBUTION` (the module-level turn-contribution instance of
  `src/backends/claude_code/turn_contribution.py` and of the `turn_contribution.py` in the
  `src/features/` packages `chat_threads`, `cron`, `discord`, `memory`, `slack`, `voice` and
  `web_reply`) — reached by string: each package's `register()` passes
  "src.<package>.turn_contribution:CONTRIBUTION" to
  `turn_contributions.register_turn_contribution`, and the hook imports the attribute from that
  string when a turn is composed (`src/runtime/hooks/turn_contributions.py`). Vulture flags
  each instance as an unused variable at 60% confidence.
- `ClaudeCodeLifecycle` (`src/backends/claude_code/claude_lifecycle.py`) and
  `CodexUsageResolver` (`src/backends/codex/codex_usage.py`) — reached by string: the claude_code
  `register()` passes `lifecycle="src.backends.claude_code.claude_lifecycle:ClaudeCodeLifecycle"`
  to `backend_type_registration.register_backend_type`, and the codex `register()` passes
  `"src.backends.codex.codex_usage:CodexUsageResolver"` to
  `backend_lifecycle_registration.register_usage_resolver`; each registry resolves the class from
  its string, the lifecycle at backend launch and the resolver at a usage read. Vulture flags
  each class as unused at 60% confidence.
- `_one_lane` (`PublishConfig` in `src/features/artifacts/config.py`), `_run_package_checks`
  (`CharlieBotConfig` in `src/infra/config.py`), `_cold_read_matches_task_type` (`TaskSpec` in
  `src/infra/models.py`), `_registered_keys_only` (`CreateSessionRequest` in `src/infra/models.py`)
  and `_serialize_with_slots` (`ThreadMetadata` and `SessionMetadata` in `src/infra/models.py`) —
  pydantic `@model_validator(mode="after")` methods, plus `@model_serializer(mode="wrap")` for
  `_serialize_with_slots`, registered with pydantic at class-definition time and invoked during
  validation and serialization. Each name has exactly zero whole-repo matches outside its
  definition, so vulture flags each as an unused method. Same framework-registered class as the
  `check_sources_and_mode` entry above.
- `api_base_is_root` (`CcAnthropicCompatibleBackend` in
  `src/backends/anthropic_compatible/options.py`) — pydantic `@field_validator("api_base")`
  classmethod, registered with pydantic at class-definition time and invoked during validation.
  It refuses an `api_base` that ends in `/v1`, because Claude Code appends `/v1/messages` to the
  base. The name has exactly zero whole-repo matches outside its definition, so vulture flags it
  as an unused method.
- `row_factory` (the `UsageLedger` connection write in `src/features/usage/usage_ledger.py`) and
  `longrepr` (the pytest `TestReport` writes in `tests/conftest.py`) — attribute writes on library
  objects the library itself reads back: sqlite3 applies the connection's row factory to every
  cursor it creates, and pytest renders `longrepr` when a test fails. Nothing in the repo reads
  the attributes back, so vulture flags each write as an unused attribute.
- `exc_type`, `tb` (the `UsageLedger.__exit__` signature in
  `src/features/usage/usage_ledger.py`) and `exitstatus` (`pytest_terminal_summary` in
  `tests/conftest.py`) — protocol-mandated parameters the body does not read: the `with`
  statement passes the exception triple to `__exit__`, and pluggy passes the exit status to the
  hook. Vulture flags each at 100% confidence as an unused variable; dropping one from the
  signature breaks the protocol.
- `pytest_addoption`, `pytest_runtest_protocol`, `pytest_terminal_summary`,
  `pytest_collection_finish`, `pytest_plugins` and `pytest_configure` (`tests/conftest.py`), plus
  `pytest_load_initial_conftests` (`src/infra/pytest_sandbox.py`) — pytest hook bindings
  pluggy resolves by module attribute name, same class as the `pytest_runtest_setup/call/teardown`
  entry above; `pytest_plugins` is the module-level list pytest reads at collection. The sandbox
  module reaches pytest through the `pytest11` entry point, and its
  `pytest_load_initial_conftests` applies the sandbox before any conftest loads. Vulture
  flags each binding as an unused function or variable.
- `empty_registry` (`tests/test_usage_sources.py` and `tests/test_sidebar_contributions.py`,
  reached by string via `@pytest.mark.usefixtures("empty_registry")`; the
  `tests/test_turn_contributions.py` copy, injected by parameter name for its registry-reset side
  effect) and `usage_services_only` (`tests/test_ext_usage.py`, same usefixtures string form) —
  pytest fixtures, same class as the `_clean_ceiling_env` entry above. `probe_slots`
  (`tests/test_metadata_slots.py`) draws the same pair of flags: six tests request it through
  `@pytest.mark.usefixtures("probe_slots")` and the `create_env` fixture takes it as an unread
  injected parameter. Vulture flags each
  definition as an unused function and, at the parameter-injection sites, each unread parameter
  as an unused variable at 100% confidence; `stop_servers` (`tests/test_opencode_backend.py`)
  draws the same pair of flags as a `pytest_asyncio` fixture the file's tests request by
  parameter name for its teardown alone.
- `_reset_backend_pool` (`tests/conftest.py`) and `fakechat_metadata_access`
  (`tests/core/test_thread_entry.py`) — pytest `autouse=True` fixtures, reached by pytest's
  fixture-name discovery only, same class as the `_fresh_*` entry above. Vulture flags each as an
  unused function at 60% confidence.
- `first_step`, `second_step` and `lone_step` (`tests/test_setup_runner.py`) — reached by string:
  the tests pass `attr="first_step"` and `attr="second_step"` to `wiring.register_setup_step` and
  `f"{__name__}:lone_step"` to `setup_runner.main --step`, and the runner resolves the function
  from the string, same class as the `setup_step` entry above. Vulture flags each as an unused
  function at 60% confidence.
- `OPENS_A` and `OPENS_B` (`tests/test_turn_contributions.py`) — `_Opens` instances reached by
  string: the tests register them through `register(opens_a="OPENS_A")`, which builds
  `f"{__name__}:OPENS_A"` for `turn_contributions.register_turn_contribution`, and the hook
  imports the attribute from that string when a turn is composed, same class as the
  `CONTRIBUTION` entry above. Vulture flags each instance as an unused variable at 60%
  confidence.
- `FieldOfTheMetadataModel`, `FieldOfAnotherOwner` and `LateSessionFields`
  (`tests/test_metadata_slots.py`) — reached by string: the tests pass each name to
  `metadata_slot_registration.register_metadata_fields` through the `_probe(...)` prefix
  (`f"{__name__}:{name}"`), and the registry imports the class from that string on the first
  metadata_slots call, same class as the `PoolNodeMetadata` entry above. Vulture flags each class
  as unused at 60% confidence. The probe classes' fields draw their own flags: `probe_tag`
  (`ProbeThreadFields`) and `late_note` (`LateSessionFields`) are read through
  `metadata_slots.set_fields` keyword arguments and `check_registered`'s dict, never as
  attributes, so vulture flags each field as an unused variable at 60% confidence.
- `CronSequenceController` (`src/features/cron/sequence_controller.py`) and
  `ImproveSequenceController` (`src/features/improve/sequence_controller.py`) — reached by
  string: each package's `register()` passes `"src.features.<package>.sequence_controller:<class>"`
  to `sequence_controllers.register_sequence_controller`, and the registry imports and instantiates
  the class from that string on first use. Tests import both classes directly, so only a reference
  scan that excludes `tests/` flags them; vulture reads each as an unused class at 60% confidence.
  Same string-registration class as the `AccountsConfig` entry above.
- `SlackConfig` (`src/features/slack/config.py`), `VoiceConfig`
  (`src/features/voice/config.py`), `CodeServerConfig` (`src/features/code_server/config.py`) and
  `DiscordConfig` (`src/features/discord/config.py`) — reached by string: each package's
  `register()` passes `"src.features.<package>.config:<class>"` to
  `config_registry.register_config_section`, and the registry imports the class from that string on
  the first `section_models()` call. The names have no whole-repo matches outside their definitions
  and those registration strings, so vulture flags each as an unused class at 60% confidence. Same
  string-registration class as the `AccountsConfig` entry above; `DiscordConfig` is the class the
  `_reject_legacy_allow_list` entry's validator lives on.
- `context_refs` (`TaskSpec` in `src/infra/models.py`), `reviewer_context_file`
  (`DelegateInvocationMetadata` in `src/infra/models.py`), `child_count` and
  `open_descendant_count` (`SessionRow` in `src/infra/models.py`), and
  `pending_trigger_count` (`SessionMetadata` in `src/infra/models.py`, written by the sidebar
  probe in `src/runtime/session_sidebar.py`) — pydantic model fields nothing in Python
  attribute-reads; every consumer reads the serialized key.
  `web/static/js/sidebar/modals.js` renders `t.context_refs`,
  `web/static/js/chat/rendering.js` reads `invocation.reviewer_context_file`, and
  `web/static/js/sidebar/status.js` with `web/static/js/chat/triggers-tray.js` read
  `pending_trigger_count` off status payloads. `child_count`/`open_descendant_count` ride every
  tree-page row and the tree-changed signal. Vulture flags each field declaration (plus the
  sidebar probe's attribute write) as an unused variable or attribute.
- `has_pending_trigger` and `has_pending_plan_approval` (`SessionMetadata` in
  `src/infra/models.py`, written by the sidebar probe in `src/runtime/session_sidebar.py`
  through the `sidebar_state.HAS_PENDING_*` string constants) — the same wire-read class as the
  `pending_trigger_count` entry above: `web/static/js/sidebar/status.js` reads
  `session.has_pending_trigger` and `session.has_pending_plan_approval` off status payloads. Vulture
  flags each declaration (plus the probe's attribute writes) as an unused variable or attribute.
- `subtree_prompt` and `node_prompt` (`PatchSessionTaskRequest` in `src/infra/models.py`) —
  read by string: `task_sessions.patch_task` tests `req.model_fields_set` for the two field
  names and reads each value through `getattr(req, f"{scope}_prompt")`, and the task-prompt
  tests construct the request with each keyword. Vulture flags each field as an unused variable
  at 60% confidence.
- `stray` (`tests/test_metadata_slots.py`) — attribute write of a key no owner registers on an
  `extra="allow"` metadata model; the same test reads it back through `model_dump_json()` key
  order and `model_extra` when asserting the copy and ordering rules. Vulture flags the write as
  an unused attribute at 60% confidence.