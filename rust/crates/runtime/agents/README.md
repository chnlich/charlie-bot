# charliebot-agents

This crate owns the process transport below the agent backends and the launch machinery they build on. Raw-log children write to inherited append files, and a follower checkpoints processed lines so either server can resume the same run.

## Public API

- `transport` and its `cgroup`, `cursor`, `follow`, `identity`, `kill`, `recovery`, `rotation`, and `spawn` modules
- `cgroup::SessionCgroup`, its fields, `SessionCgroup::oom_score_adj`, and `SessionCgroup::classify_exit`
- `cgroup::SessionCgroupConfig` and its `from_config` and `disabled` constructors
- `cgroup::system_cgroup_base_dir`, `session_cgroup_name`, `session_cgroup_path`
- `cgroup::ensure_session_cgroup`, `read_memory_events`, `classify_cgroup_exit`, `prepare_session_cgroup` (async; the class walk runs on a blocking thread)
- `cgroup::move_process_to_cgroup` (writes `cgroup.procs` and `/proc/<pid>/oom_score_adj`)
- `cgroup::sweep_stale_session_cgroups`, `log_session_cgroup_startup`
- `cursor::read_raw_cursor`, `CURSOR_FIELD_BYTES`
- `cursor::RawCursorWriter`, `RawCursorWriter::new`, `write`, and `close`
- `follow::DEFAULT_BUFFER_LIMIT`, `TAIL_POLL_INTERVAL`, `SilenceCallback`, `FollowSummary`, and its fields
- `follow::tail_follow_events`
- `identity::ProcessIdentity` and its fields, `pin_process_identity`
- `identity::is_run_alive`
- `identity::ProcessStat` and its fields, `read_pid_stat`, `read_host_boot_time`
- `kill::KILL_ESCALATION_GRACE`, `KILL_ESCALATION_POLL`
- `kill::kill_process_group`, `kill_group_escalating`, `wait_or_kill_group`
- `kill::terminate_child`
- `kill::detach`
- `recovery::TypeLineFilter`, `TypeLineFilter::new`
- `recovery::parse_raw_lines`, `project_raw_events`, `result_exit_code`
- `recovery::project_raw_file`, `scan_result_exit`, `raw_completion_time`
- `rotation::rotate_stale_transport`
- `spawn::SpawnSpec`, its fields, and `SpawnSpec::new`
- `spawn::RawStdin`, `SpawnedChild`, and its fields
- `spawn::ChildStdin`, `SpawnSetup`, and `spawn_with_setup`. Parent-side setup sets nice to 10; child-side turn-tree setup raises nice by 10.
- `spawn::spawn_raw_log`, `spawn_one_shot`. Parent-death children use a process-lifetime spawner thread; children without that signal use Tokio's blocking pool.
- `spawn::pin_identity_and_on_spawn`
- `spawn::iter_ndjson_events`
- `spawn::STDERR_TAIL_BYTES`
- `AgentBaseEnvironment` and its `values` field
- `agent_environment`, `from_process_environment`, and the root `build_backend` export
- `backend_pool::{QUOTA_ERROR_PATTERNS, is_quota_error, BackendPool}`
- `backend_availability::{BackendAvailability, AvailabilityRow, DEFAULT_COOLDOWN, REASON_QUOTA_ERROR, REASON_TIMEOUT, backend_availability}`
- `backend_pool::{backend_pool, pre_recovery_step, register}`
- `backend_pool::METADATA_OWNER`
- `pre_recovery_step`, `run_session_cgroup_startup`, `session_cgroup_step`, `register_runtime`
- `agent_one_shot_text(built: BuiltBackend, base_env: &Env, prompt: &str, system_prompt: &str, timeout: Option<Duration>) -> anyhow::Result<String>`
- `BuiltBackend` and its `backend` and `launch` fields; `backend_types::build_backend`, root `build_backend`, `build_translate_fallback`
- `launch_loop::SESSION_ID_ENV`, `ProcessResult`, `RunProcess`, `run_launches`, `child_env`, and `has_round_notices`
- `event_builders` and its `make_text_event`, `make_error_event`, `make_master_done_event`, `make_result_event`, `make_tool_use_event`, `make_tool_result_event`, `make_context_reading_event`, `make_compact_boundary_event`, `make_context_compacted_event`, and `make_context_compact_failed_event`
- `runs::clean_control_characters`, `runs::select_error_hint`, and `runs::run_alive_probe`
- `runner::{DEFAULT_POST_RESULT_TIMEOUT, DEFAULT_CLEANUP_TIMEOUT, OnSpawnFn, EventFn}`
- `runner::capture_process_diagnostics`
- `RunRequest`, its fields, `RunRequest::new`, and `RunRequest::gate_child_until_on_spawn`
- `RunOutcome` and its fields, including post-result hang diagnostics
- `AgentRunner`, `AgentRunner::new`, `run`, `terminate`, and `detach`
- `json_loads::{loads, loads_text, LoadError, MAX_DEPTH}` (`charliebot_core::json_loads`, read by the golden test). Outcomes and error messages match the recorded decoder outputs.
- `cli_base::{LOCAL_NO_PROXY_ENTRIES, SKIP_PERMISSIONS_FLAG, IMAGE_MIME_BY_EXT, CliHost, resolve_binary, prepend_path_dir, merge_local_no_proxy, apply_proxy_env, strip_google_api_keys, framed_prompt, str_field, required_str, int_field}`
- `one_shot_text` (the dispatcher; the tool-capable fallback is `agent_one_shot_text`)
- `RunRequest.uploaded_files` and `RunOutcome` for a run with no process; the manager turn passes the item's uploads
- `runner::AgentRunner::exit_code` and the final-stdout run (`TransportKind::FinalStdout`)

## Tests

The crate tests process transport with `/bin/sh`, `/bin/sleep`, and synthetic NDJSON. Fake agent programs are not written as executables. Cargo builds `src/bin/fake_program.rs` once, and each fake is a symlink named for its role (`claude`, `gemini`, `charlie-code`, `agy`) that points at that binary, next to a `probe.json` listing the role's steps. The helper reads its role from `argv[0]`, so a bare name resolves on `PATH`. The `cli_backends_golden` test links fake `gemini`, `charlie-code` and `agy` programs in a temporary directory and runs only the fake `agy` ones. The tests of the backend crates under `rust/crates/backends/` link the same helper, which they find in the target directory that holds their test binary. None starts an agent CLI or server.

## Backend crates

Each agent backend is its own crate under `rust/crates/backends/<name>`, so Cargo and the structure test hold the
backend group boundaries: `charliebot-anthropic-compatible`, `charliebot-antigravity`, `charliebot-charlie-code`,
`charliebot-claude-code`, `charliebot-codex`, `charliebot-devin`, `charliebot-gemini`, `charliebot-openai-compatible` and
`charliebot-opencode`.
