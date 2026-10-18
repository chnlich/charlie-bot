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

## Tests

The crate tests process transport with `/bin/sh`, `/bin/sleep`, and synthetic NDJSON. The cross-implementation test runs a Python helper through `uv run python`; it starts no agent CLI or server.
