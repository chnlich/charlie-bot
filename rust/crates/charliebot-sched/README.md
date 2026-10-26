# `charliebot-sched`

Scheduled cron tasks and delayed or watched triggers for the Rust server. The crate owns the five `/api/cron/tasks`
routes, cron task loading and validation, the 60-second scheduler, scheduled handlers, task bindings and sequence
recovery. It also owns the internal schedule and cancel routes, durable trigger records, waiters and startup recovery.
The trigger step is registered after task recovery has completed. It also owns the improve loop (`improve`): its
internal start and stop routes, the loop's durable state, the judgments over an iteration's events, the fast-forward
landing, and the sequence controller that runs and closes each loop.

## Public API and Python sources

| Rust item | Python source |
| --- | --- |
| `register` | `server.py:279-282`; routes are mounted through `internal.router` at `server.py:351` |
| `triggers::MAX_PENDING_TRIGGERS` | `src/runtime/triggers.py:77` |
| `triggers::TriggerStatus` | `src/infra/models.py:84-88` |
| `triggers::LocalPid` | `src/infra/models.py:273-285` |
| `triggers::RemotePid` | `src/infra/models.py:288-299` |
| `triggers::SlurmJob` | `src/infra/models.py:301-310` |
| `triggers::WatchTarget` | `src/infra/models.py:312-315` |
| `triggers::PendingTrigger` | `src/infra/models.py:318-330` |
| `triggers::ScheduleTriggerRequest` | `src/infra/models.py:698-706` |
| `triggers::TriggerCreateError` | `src/runtime/triggers.py:84-88` |
| `triggers::TriggerManager` and `TriggerManager::new` | `src/runtime/triggers.py:440-459` |
| `TriggerManager::create_trigger` | `src/runtime/triggers.py:461-593` |
| `TriggerManager::list_triggers` | `src/runtime/triggers.py:595-660` |
| `TriggerManager::cancel_trigger` | `src/runtime/triggers.py:662-672` |
| `TriggerManager::recover_pending` | `src/runtime/triggers.py:674-694` |
| `triggers::routes` | `src/runtime/api/internal.py:258-317` |
| `triggers::ROUTE_PATTERNS` | `src/runtime/api/internal.py:258-317` |
| `triggers::trigger_step` | `server.py:275-285` |
| `cron::register` | `src/features/cron/__init__.py:5-14` |
| `cron::config::DEFAULT_TIMEZONE` | `src/features/cron/config.py:14` |
| `cron::config::StepConfig` | `src/features/cron/config.py:17-37` |
| `cron::config::ScheduledTaskFields` | `src/features/cron/config.py:40-70` |
| `cron::config::ScheduledTaskConfig` and its source/backend validation | `src/features/cron/config.py:73-145` |
| `cron::config::ScheduledTaskError` | `src/features/cron/config.py:148-160` |
| `GET /api/cron/tasks` | `src/features/cron/api.py:163-196` |
| `PUT /api/cron/tasks/{name}` | `src/features/cron/api.py:257-267` |
| `POST /api/cron/tasks` | `src/features/cron/api.py:270-300` |
| `POST /api/cron/tasks/{name}/run` | `src/features/cron/api.py:302-327` |
| `DELETE /api/cron/tasks/{name}` | `src/features/cron/api.py:330-344` |
| `scheduler` service and 60-second tick | `src/features/cron/scheduler.py:26`, `:49-81`, `:108-152` |
| Cron sequence binding, stale-anchor wake duties, weekly chat-history recycle and recovery hooks | `src/features/cron/sequence_controller.py:61-208` |
| Cron startup backend reference check | `src/features/cron/backend_refs.py:7-25` |
| Scheduled handler registration and lookup | `src/runtime/hooks/scheduled_handlers.py:14-29` |

`TriggerManager::list_triggers` has the public signature
`pub async fn list_triggers(&self, session_id: &str) -> anyhow::Result<Vec<PendingTrigger>>`.

The models keep Python's discriminator and field order so existing Python-written trigger files can be recovered,
and Rust-written records can be read by Python. A scheduled trigger reaches the existing dispatch admission path with
its trigger id as the stable input id.

## Improve loop

The loop's state lives under `sessions/<sid>/loops/`; the sequence controller owns every Run whose `sequence_ref.kind`
is `improve`. `register` mounts the routes and the controller; the packages list in `charliebot-server` calls it in
place of the improve stub.

| Rust item | Python source |
| --- | --- |
| `improve::register` | `src/features/improve/__init__.py:5` |
| `improve::api::routes` and `improve::api::matches` | `src/features/improve/api.py:40` (stop), `:59` (start) |
| `improve::api::ImproveRequest` | `src/features/improve/api.py:18` |
| `improve::api::ImproveStopRequest` | `src/features/improve/api.py:33` |
| `improve::command::ImproveState` | `src/features/improve/improve_command.py:33` |
| `improve::command::ImproveLoopAlreadyRunningError` | `src/features/improve/improve_command.py:64` |
| `improve::command::Reservation` and `reserve_loop_state` | `src/features/improve/improve_command.py:435` |
| `improve::command::resolve_path` | `src/features/improve/improve_command.py:435` (Python's non-strict `Path.resolve()`) |
| `improve::command::loops_dir`, `active_lock_path`, `loop_dir` | `src/features/improve/improve_command.py:80`, `:84` |
| `improve::command::loop_goal_path` and `loop_plan_path` | `src/features/improve/improve_command.py:100`, `:105` |
| `improve::command::read_loop_goal` and `read_loop_plan` | `src/features/improve/improve_command.py:110`, `:123` |
| `improve::command::load_loop_state` and `require_loop_state` | `src/features/improve/improve_command.py:165`, `:173` |
| `improve::command::save_loop_state` and `clear_active_loop_lock` | `src/features/improve/improve_command.py:181`, `:191` |
| `improve::command::find_running_loop` and `stop_improve_loop` | `src/features/improve/improve_command.py:219`, `:235` |
| `improve::command::{STATE_FILE, GOAL_FILE, PLAN_FILE, ACTIVE_LOCK_FILE}` | `src/features/improve/improve_command.py:80`, `:92`, `:96` |
| `improve::judgment::{ITERATION_SUMMARIES_HEADING, RUNNER_FALLBACK_REPORT_MARKER}` | `src/features/improve/improve_command.py:23`, `:24` |
| `improve::judgment::{WORKTREE_CREATE_ERROR_PREFIX, LOOP_FAILURE_ERROR_PREFIX}` | `src/features/improve/improve_command.py:25`, `:26` |
| `improve::judgment::commits_section_first_none` | `src/features/improve/improve_command.py:250` |
| `improve::judgment::iter_report_validity` | `src/features/improve/improve_command.py:266` |
| `improve::judgment::invalid_iteration_summary` | `src/features/improve/improve_command.py:289` |
| `improve::judgment::summary_text` | `src/features/improve/improve_command.py:333` |
| `improve::judgment::extract_iteration_summary` | `src/features/improve/improve_command.py:347` |
| `improve::judgment::quota_blocker_match` | `src/features/improve/improve_command.py:512` |
| `improve::judgment::failed_iteration_judgments` | `src/features/improve/improve_command.py:544` |
| `improve::judgment::newest_first_events` | `src/features/improve/improve_command.py:570` |
| `improve::landing::commit_delta` | `src/features/improve/improve_command.py:303` |
| `improve::landing::land_work_branch_after_loop` and `Landing` | `src/features/improve/improve_command.py:370` |
| `improve::landing::blocked_loop_summary` | `src/features/improve/improve_command.py:423` |
| `improve::sequence::SEQUENCE_KIND` | `src/features/improve/sequence_controller.py:32` |
| `improve::sequence::{IMPROVE_ITERATION_COMPLETED, IMPROVE_COMPLETED, IMPROVE_FAILED, IMPROVE_STOPPED}` | `src/features/improve/improve_sequence.py:71`, `:72`, `:73`, `:75` |
| `improve::sequence::{loop_owner_ref, improve_child_request_id, iteration_run_request_id}` | `src/features/improve/improve_sequence.py:78`, `:84`, `:89` |
| `improve::sequence::improve_close_request_id` | `src/features/improve/improve_sequence.py:95` |
| `improve::sequence::improve_child_id` and `create_improve_child` | `src/features/improve/improve_sequence.py:101` |
| `improve::sequence::render_iteration_reports` and `iteration_context` | `src/features/improve/improve_sequence.py:148`, `:156` |
| `improve::sequence::compose_iteration_description` | `src/features/improve/improve_sequence.py:191` |
| `improve::sequence::LoopSpec` and `run_improve_sequence` | `src/features/improve/improve_sequence.py:270` |
| `improve::sequence::close_ended_loop_child` | `src/features/improve/improve_sequence.py:701` |
| `improve::sequence::reconcile_interrupted_sequences` | `src/features/improve/improve_sequence.py:872` |
| `improve::controller::ImproveSequenceController` | `src/features/improve/sequence_controller.py:31` |
| `improve::controller::register` | `src/features/improve/__init__.py:5` |

The start route passes the improve role to backend resolution. Each pooled iteration cools its member after a quota
failure and selects again on the next iteration.
