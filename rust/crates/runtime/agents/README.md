# charliebot-agents

This crate owns the process transport below the agent backends and the launch machinery they build on. Raw-log children write to inherited append files, and a follower checkpoints processed lines so either server can resume the same run.

## Public API

| Rust item | Python source |
| --- | --- |
| `transport` and its `cgroup`, `cursor`, `follow`, `identity`, `kill`, `recovery`, `rotation`, and `spawn` modules | `src/runtime/agent_process/base.py:36`, `:398`, `src/infra/process.py:22`, `src/runtime/runs.py:101` |
| `cgroup::SessionCgroup`, its fields, `SessionCgroup::oom_score_adj`, and `SessionCgroup::classify_exit` | `src/infra/process.py:209`, `:224` |
| `cgroup::SessionCgroupConfig` and its `from_config` and `disabled` constructors | `src/infra/process.py:322` (the caps `prepare_session_cgroup` takes) |
| `cgroup::system_cgroup_base_dir`, `session_cgroup_name`, `session_cgroup_path` | `src/infra/process.py:96`, `:105`, `:137` |
| `cgroup::ensure_session_cgroup`, `read_memory_events`, `classify_cgroup_exit`, `prepare_session_cgroup` (async; the class walk runs on a blocking thread) | `src/infra/process.py:142`, `:183`, `:294`, `:322` |
| `cgroup::move_process_to_cgroup` (writes `cgroup.procs` and `/proc/<pid>/oom_score_adj`) | `src/runtime/agent_process/base.py:896` |
| `cgroup::cleanup_session_cgroup`, `sweep_stale_session_cgroups`, `log_session_cgroup_startup` | `src/infra/process.py:348`, `:374`, `:407` |
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
| `kill::exit_code` | `src/runtime/agent_process/spawn.py:57` |
| `recovery::TypeLineFilter`, `TypeLineFilter::new` | `src/infra/ndjson.py:463`, `:483` |
| `recovery::parse_raw_lines`, `project_raw_events`, `result_exit_code` | `src/runtime/runs.py:167`, `:200`, `:225` |
| `recovery::project_raw_file`, `scan_result_exit`, `raw_completion_time` | `src/runtime/runs.py:266`, `:288`, `:311` |
| `rotation::rotate_stale_transport` | `src/runtime/agent_process/base.py:596` |
| `spawn::SpawnSpec`, its fields, and `SpawnSpec::new` | `src/runtime/agent_process/base.py:785`, `:878` |
| `spawn::RawStdin`, `SpawnedChild`, and its fields | `src/runtime/agent_process/base.py:936` |
| `spawn::ChildStdin`, `SpawnSetup`, and `spawn_with_setup` | `src/runtime/agent_process/base.py:858-903`; `src/backends/opencode/server.py:236`; `src/backends/claude_code/claude_code.py:300`; `src/backends/claude_code/claude_compaction.py:288-306`. Parent-side setup sets nice to 10; child-side turn-tree setup raises nice by 10. |
| `spawn::spawn_raw_log`, `spawn_piped`, `spawn_one_shot` | `src/runtime/agent_process/base.py:785`, `:812`, `:936`; `src/backends/opencode/opencode.py:505`; `src/backends/codex/codex.py:127`. Parent-death children use a process-lifetime spawner thread; children without that signal use Tokio's blocking pool. |
| `spawn::pin_identity_and_on_spawn` | `src/runtime/agent_process/base.py:919` |
| `spawn::iter_ndjson_events` | `src/runtime/agent_process/base.py:347` |
| `spawn::STDERR_TAIL_BYTES` | `src/runtime/agent_process/base.py:37` |
| `AgentBaseEnvironment` and its `values` field | `src/runtime/agent_environment.py:28-36`, `server.py:582` |
| `agent_environment`, `from_process_environment`, and the root `build_backend` export | `src/runtime/agent_environment.py:53`, `server.py:582`, `src/runtime/hooks/backend_types.py:33-60` |
| `backend_pool::{QUOTA_ERROR_PATTERNS, DEFAULT_COOLDOWN, is_quota_error, BackendPool}` | `src/runtime/backend_pool.py` |
| `backend_pool::{backend_pool, pre_recovery_step, register}` | `src/runtime/backend_pool.py`; `src/features/backend_pool/__init__.py` |
| `backend_pool::METADATA_OWNER` | `src/runtime/backend_pool.py:65` |
| `pre_recovery_step`, `run_session_cgroup_startup`, `session_cgroup_step`, `register_runtime` | `server.py:262-273`; `src/runtime/agent_environment.py:121-153` |
| `agent_one_shot_text(built: BuiltBackend, base_env: &Env, prompt: &str, system_prompt: &str, timeout: Option<Duration>) -> anyhow::Result<String>` | `src/runtime/agent_process/base.py:712`, `:1156-1189` |
| `BuiltBackend` and its `backend` and `launch` fields; `backend_types::build_backend`, root `build_backend`, `build_translate_fallback` | `src/runtime/hooks/backend_types.py:33-60` |
| `launch_loop::SESSION_ID_ENV`, `ProcessResult`, `RunProcess`, `run_launches`, `child_env`, and `has_round_notices` | `src/runtime/launch_loop.py:19-65` |
| `event_builders` and its `make_text_event`, `make_error_event`, `make_master_done_event`, `make_result_event`, `make_tool_use_event`, `make_tool_result_event`, `make_context_reading_event`, `make_compact_boundary_event`, `make_context_compacted_event`, and `make_context_compact_failed_event` | `src/runtime/agent_process/base.py:220-350` |
| `runs::clean_control_characters`, `runs::select_error_hint`, and `runs::run_alive_probe` | `src/runtime/runs.py:148`, `:247` |
| `runner::{DEFAULT_POST_RESULT_TIMEOUT, DEFAULT_CLEANUP_TIMEOUT, OnSpawnFn, EventFn}` | `src/runtime/agent_process/base.py:37`, `:660-661`, `:919`, `:936` |
| `runner::capture_process_diagnostics` | `src/runtime/agent_process/base.py:98-118` (`_capture_proc_diagnostics`; its commands at `:107-110`) |
| `RunRequest`, its fields, `RunRequest::new`, and `RunRequest::gate_child_until_on_spawn` | `src/runtime/agent_process/base.py:653-708`, `:919-936`, `:936-1033` |
| `RunOutcome` and its fields, including post-result hang diagnostics | `src/runtime/agent_process/base.py:98-120`, `:703`, `:1000-1050`, `:1110` |
| `AgentRunner`, `AgentRunner::new`, `run`, `terminate`, and `detach` | `src/runtime/agent_process/base.py:936-1050`, `:1180-1205` |
| `json_loads::{loads, loads_text, LoadError, MAX_DEPTH}` (`charliebot_compat::json_loads`, re-exported here) | `json.loads` on the stored prompt snapshot in `src/runtime/api/sessions.py:1589-1688`; CPython's `_json.c` scanner and `detect_encoding` |
| `cli_base::{LOCAL_NO_PROXY_ENTRIES, SKIP_PERMISSIONS_FLAG, IMAGE_MIME_BY_EXT, CliHost, resolve_binary, prepend_path_dir, merge_local_no_proxy, apply_proxy_env, strip_google_api_keys, framed_prompt, truthy, str_field, required_str, int_field}` | `src/runtime/agent_process/base.py:33` (`SKIP_PERMISSIONS_FLAG`), `:125-218` (`USER_LOCAL_BIN`, `IMAGE_MIME_BY_EXT`, the helpers), `:705-714` (`_effective_prompt`, `_frame_system_prompt`); Python truthiness and `str.strip`, `str.split` |
| `one_shot_text` (the dispatcher) | `src/runtime/agent_process/base.py:1156-1190` (the polymorphic `one_shot_text`; the tool-capable fallback is `agent_one_shot_text`) |
| `RunRequest.uploaded_files` and `RunOutcome` for a run with no process | `src/runtime/agent_process/base.py:653-708`; `src/runtime/master_cc_run.py:575` (the manager turn passes the item's uploads); `charlie_code.py:87-117` (the upload gate) |
| `runner::AgentRunner::exit_code` and the final-stdout run (`TransportKind::FinalStdout`) | `src/backends/antigravity/antigravity_cli.py:69-165` (stdout tee, the cleanup and the exit); `src/runtime/agent_process/base.py:936-1050` |

## Tests

The crate tests process transport with `/bin/sh`, `/bin/sleep`, and synthetic NDJSON. Fake agent programs are not written as executables. Cargo builds `src/bin/fake_program.rs` once, and each fake is a symlink named for its role (`claude`, `gemini`, `charlie-code`, `agy`) that points at that binary, next to a `probe.json` listing the role's steps. The helper reads its role from `argv[0]`, so a bare name resolves on `PATH`. The CLI backend cross-implementation test links fake `gemini`, `charlie-code` and `agy` programs in a temporary directory and runs only the fake `agy` ones. The tests of the backend crates under `rust/crates/backends/` link the same helper, which they find in the target directory that holds their test binary. None starts an agent CLI or server.

## Backend crates

Each agent backend is its own crate under `rust/crates/backends/<name>`, so Cargo and the structure test hold the
backend group boundaries: `charliebot-anthropic-compatible`, `charliebot-antigravity`, `charliebot-charlie-code`,
`charliebot-claude-code`, `charliebot-codex`, `charliebot-gemini`, `charliebot-openai-compatible` and
`charliebot-opencode`.
