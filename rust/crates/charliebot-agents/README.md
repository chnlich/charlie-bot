# charliebot-agents

This crate owns the process transport below the agent backends. Raw-log children write to inherited append files, and a follower checkpoints processed lines so either server can resume the same run.

## Public API

| Rust item | Python source |
| --- | --- |
| `transport` and its `cgroup`, `cursor`, `follow`, `identity`, `kill`, `recovery`, `rotation`, and `spawn` modules | `src/runtime/agent_process/base.py:36`, `:398`, `src/infra/process.py:22`, `src/runtime/runs.py:101` |
| `cgroup::SessionCgroup`, its fields, and `SessionCgroup::classify_exit` | `src/infra/process.py:226`, `:239` |
| `cgroup::system_cgroup_base_dir`, `session_cgroup_name`, `session_cgroup_path` | `src/infra/process.py:90`, `:100`, `:105` |
| `cgroup::ensure_session_cgroup`, `read_memory_events`, `classify_cgroup_exit`, `prepare_session_cgroup` | `src/infra/process.py:110`, `:144`, `:244`, `:272` |
| `cgroup::move_process_to_cgroup` | `src/runtime/agent_process/base.py:878` |
| `cgroup::cleanup_session_cgroup`, `sweep_stale_session_cgroups`, `log_session_cgroup_startup` | `src/infra/process.py:288`, `:307`, `:334` |
| `cursor::CURSOR_NAME` | `src/runtime/runs.py:49` |
| `cursor::read_raw_cursor`, `CURSOR_FIELD_BYTES` | `src/runtime/runs.py:324`, `:332` |
| `cursor::RawCursorWriter`, `RawCursorWriter::new`, `write`, and `close` | `src/runtime/runs.py:335`, `:347`, `:351`, `:357` |
| `follow::DEFAULT_BUFFER_LIMIT`, `TAIL_POLL_INTERVAL`, `SilenceCallback`, `FollowSummary`, and its fields | `src/runtime/agent_process/base.py:36`, `:95`, `:398` |
| `follow::tail_follow_events` | `src/runtime/agent_process/base.py:398` |
| `identity::ProcessIdentity` and its fields, `pin_process_identity` | `src/runtime/agent_process/base.py:919` |
| `identity::is_run_alive` | `src/runtime/runs.py:120` |
| `identity::ProcessStat` and its fields, `read_pid_stat`, `read_host_boot_time` | `src/runtime/runs.py:92`, `:101` |
| `kill::KILL_ESCALATION_GRACE`, `KILL_ESCALATION_POLL` | `src/infra/timeouts.py:181`, `:182` |
| `kill::kill_process_group`, `kill_group_escalating`, `wait_or_kill_group` | `src/infra/process.py:22`, `:39`, `:53` |
| `kill::terminate_child` | `src/runtime/agent_process/base.py:1046`, `:1191` |
| `kill::detach` | `src/runtime/agent_process/base.py:1198` |
| `kill::python_exit_code` | `src/runtime/agent_process/spawn.py:57` |
| `recovery::TypeLineFilter`, `TypeLineFilter::new` | `src/infra/ndjson.py:463`, `:483` |
| `recovery::parse_raw_lines`, `project_raw_events`, `result_exit_code` | `src/runtime/runs.py:167`, `:200`, `:225` |
| `recovery::project_raw_file`, `scan_result_exit`, `raw_completion_time` | `src/runtime/runs.py:266`, `:288`, `:311` |
| `rotation::rotate_stale_transport` | `src/runtime/agent_process/base.py:596` |
| `spawn::SpawnSpec`, its fields, and `SpawnSpec::new` | `src/runtime/agent_process/base.py:785`, `:878` |
| `spawn::RawStdin`, `SpawnedChild`, and its fields | `src/runtime/agent_process/base.py:936` |
| `spawn::spawn_raw_log`, `spawn_piped`, `spawn_one_shot` | `src/runtime/agent_process/base.py:785`, `:812`, `:936` |
| `spawn::pin_identity_and_on_spawn` | `src/runtime/agent_process/base.py:919` |
| `spawn::iter_ndjson_events` | `src/runtime/agent_process/base.py:347` |
| `spawn::STDERR_TAIL_BYTES`, `tee_stream`, and `stream_stderr` | `src/runtime/agent_process/base.py:37`, `:60`, `:1064` |
| `VENV_BIN_ENV`, `REPO_ROOT_ENV`, `ENTRY_POINT_RELATIVE`, `AgentBaseEnvironment` and its `values` field | `src/runtime/agent_environment.py:28-36`, `server.py:582` |
| `agent_environment`, `from_process_environment`, and the root `build_backend` export | `src/runtime/agent_environment.py:53`, `server.py:582`, `src/runtime/hooks/backend_types.py:33-60` |
| `pre_recovery_step`, `run_session_cgroup_startup`, `session_cgroup_step`, and `register_runtime` | `server.py:262-273`; `src/runtime/agent_environment.py:121-153` |
| `agent_one_shot_text(built: BuiltBackend, prompt: &str, system_prompt: &str, timeout: Option<Duration>) -> anyhow::Result<String>` | `src/runtime/agent_process/base.py:712`, `:1156-1189` |
| `BuiltBackend` and its `backend` and `launch` fields; `backend_types::build_backend`, root `build_backend`, `build_translate_fallback` | `src/runtime/hooks/backend_types.py:33-60` |
| `backends::claude_code` and `backends::claude_code::register` | `src/backends/claude_code/__init__.py:17-75` |
| `ClaudeCodeBackend`, `ClaudeCodeBackend::build_command`, and its `AgentBackend` methods | `src/backends/claude_code/claude_code.py:240-292`; `src/runtime/agent_process/base.py:641-770` |
| `HEADLESS_DISALLOWED_TOOLS`, `DISALLOWED_TOOLS_FLAG`, `SKIP_PERMISSIONS_FLAG`, `INSTRUCTIONS_FILE_NAME`, `INSTRUCTIONS_WRITTEN_EVENT` | `src/backends/claude_code/claude_code.py:24-37`, `:276-292` |
| `claude_child_env`, `headless_claude_env`, `claude_reading_limits`, `claude_reading_limits_from`, and `out_of_family_served_models` | `src/backends/claude_code/claude_code.py:58-68`, `:103-167`, `:198-225`; `src/backends/claude_code/claude_launch.py:43-53` |
| `claude_launch::{AUTO_COMPACT_WINDOW_ENV, AUTOCOMPACT_PCT_OVERRIDE_ENV, MAX_CONTEXT_TOKENS_ENV, SIMPLE_SYSTEM_PROMPT_ENV, AUTO_COMPACT_DEFAULT, COMPACT_OUTPUT_RESERVE, COMPACT_CONTEXT_RESERVE, DISABLE_CONNECTOR_SETTINGS_KEY, FAST_MODE_SETTINGS_KEY, HEADLESS_FORWARDED_ENV, headless_claude_env}` | `src/backends/claude_code/claude_launch.py:9-53` |
| `claude_lifecycle::EXCLUDE_DYNAMIC_SECTIONS_FLAG`, `ClaudeCliLifecycle`, `ClaudeCodeLifecycle`, `ClaudeCodeLifecycle::new`, and `Default` | `src/backends/claude_code/claude_lifecycle.py:42`, `:75-125`, `:128-155`, `:321-334` |
| `event_types::CLAUDE_ACCOUNT_LOGIN_REQUIRED` | `src/backends/claude_code/event_types.py:1-3` |
| `ClaudeCodeFactory` and its `BackendFactory::build` implementation | `src/backends/claude_code/factory.py:10-35` |
| `options::option_model` | `src/backends/claude_code/options.py:8-18` |
| `login_dirs::{CLAUDE_CONFIG_DIR_ENV, default_claude_dir, claude_config_dir, expanduser}` | `src/backends/claude_code/login_dirs.py:18-39`; `pathlib.Path.expanduser` used in `claude_code.py:256` |
| `claude_code::one_shot_text` implementation | `src/backends/claude_code/claude_code.py:294-320` |
| `launch_loop::SESSION_ID_ENV`, `ProcessResult`, `RunProcess`, `run_launches`, `child_env`, and `has_round_notices` | `src/runtime/launch_loop.py:19-65` |
| `event_builders` and its `make_text_event`, `make_error_event`, `make_master_done_event`, `make_result_event`, `make_tool_use_event`, `make_tool_result_event`, `make_context_reading_event`, `make_compact_boundary_event`, `make_context_compacted_event`, and `make_context_compact_failed_event` | `src/runtime/agent_process/base.py:220-350` |
| `runs::clean_control_characters`, `runs::select_error_hint`, and `runs::run_alive_probe` | `src/runtime/runs.py:148`, `:247` |
| `runner::{RAW_LOG_NAME, STDERR_LOG_NAME, DEFAULT_POST_RESULT_TIMEOUT, DEFAULT_CLEANUP_TIMEOUT, OnSpawnFn, EventFn}` | `src/runtime/runs.py:49`, `src/runtime/agent_process/base.py:37`, `:660-661`, `:919`, `:936` |
| `RunRequest`, its fields, `RunRequest::new`, and `RunRequest::from_features` | `src/runtime/agent_process/base.py:653-708`, `:936-1033` |
| `RunOutcome` and its fields, including post-result hang diagnostics | `src/runtime/agent_process/base.py:98-120`, `:703`, `:1000-1050`, `:1110` |
| `AgentRunner`, `AgentRunner::new`, `run`, `terminate`, and `detach` | `src/runtime/agent_process/base.py:936-1050`, `:1180-1205` |

## Tests

The crate tests process transport with `/bin/sh`, `/bin/sleep`, and synthetic NDJSON. The cross-implementation test runs a Python helper through `uv run python`; it starts no agent CLI or server.
