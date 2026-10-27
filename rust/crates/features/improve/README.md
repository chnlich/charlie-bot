# `charliebot-improve`

The improve loop: `charliebot improve` runs an iterative worker loop on the task tree as one sequence. The loop's
state lives under `sessions/<sid>/loops/`; the sequence controller owns every Run whose `sequence_ref.kind` is
`improve`. `register` mounts the routes and the controller; the packages list in `charliebot-server` calls it in
place of the improve stub.

The start route passes the improve role to backend resolution. Each pooled iteration cools its member after a quota
failure and selects again on the next iteration.

## Public API and Python sources

| Rust item | Python source |
| --- | --- |
| `register` | `src/features/improve/__init__.py:5` |
| `api::routes` and `api::matches` | `src/features/improve/api.py:40` (stop), `:59` (start) |
| `api::ImproveRequest` | `src/features/improve/api.py:18` |
| `api::ImproveStopRequest` | `src/features/improve/api.py:33` |
| `command::ImproveState` | `src/features/improve/improve_command.py:33` |
| `command::ImproveLoopAlreadyRunningError` | `src/features/improve/improve_command.py:64` |
| `command::Reservation` and `reserve_loop_state` | `src/features/improve/improve_command.py:435` |
| `command::resolve_path` | `src/features/improve/improve_command.py:435` (Python's non-strict `Path.resolve()`) |
| `command::loops_dir`, `active_lock_path`, `loop_dir` | `src/features/improve/improve_command.py:80`, `:84` |
| `command::loop_goal_path` and `loop_plan_path` | `src/features/improve/improve_command.py:100`, `:105` |
| `command::read_loop_goal` and `read_loop_plan` | `src/features/improve/improve_command.py:110`, `:123` |
| `command::load_loop_state` and `require_loop_state` | `src/features/improve/improve_command.py:165`, `:173` |
| `command::save_loop_state` and `clear_active_loop_lock` | `src/features/improve/improve_command.py:181`, `:191` |
| `command::find_running_loop` and `stop_improve_loop` | `src/features/improve/improve_command.py:219`, `:235` |
| `command::{STATE_FILE, GOAL_FILE, PLAN_FILE, ACTIVE_LOCK_FILE}` | `src/features/improve/improve_command.py:80`, `:92`, `:96` |
| `judgment::{ITERATION_SUMMARIES_HEADING, RUNNER_FALLBACK_REPORT_MARKER}` | `src/features/improve/improve_command.py:23`, `:24` |
| `judgment::{WORKTREE_CREATE_ERROR_PREFIX, LOOP_FAILURE_ERROR_PREFIX}` | `src/features/improve/improve_command.py:25`, `:26` |
| `judgment::commits_section_first_none` | `src/features/improve/improve_command.py:250` |
| `judgment::iter_report_validity` | `src/features/improve/improve_command.py:266` |
| `judgment::invalid_iteration_summary` | `src/features/improve/improve_command.py:289` |
| `judgment::summary_text` | `src/features/improve/improve_command.py:333` |
| `judgment::extract_iteration_summary` | `src/features/improve/improve_command.py:347` |
| `judgment::quota_blocker_match` | `src/features/improve/improve_command.py:512` |
| `judgment::failed_iteration_judgments` | `src/features/improve/improve_command.py:544` |
| `judgment::newest_first_events` | `src/features/improve/improve_command.py:570` |
| `landing::commit_delta` | `src/features/improve/improve_command.py:303` |
| `landing::land_work_branch_after_loop` and `Landing` | `src/features/improve/improve_command.py:370` |
| `landing::blocked_loop_summary` | `src/features/improve/improve_command.py:423` |
| `sequence::SEQUENCE_KIND` | `src/features/improve/sequence_controller.py:32` |
| `sequence::{IMPROVE_ITERATION_COMPLETED, IMPROVE_COMPLETED, IMPROVE_FAILED, IMPROVE_STOPPED}` | `src/features/improve/improve_sequence.py:71`, `:72`, `:73`, `:75` |
| `sequence::{loop_owner_ref, improve_child_request_id, iteration_run_request_id}` | `src/features/improve/improve_sequence.py:78`, `:84`, `:89` |
| `sequence::improve_close_request_id` | `src/features/improve/improve_sequence.py:95` |
| `sequence::improve_child_id` and `create_improve_child` | `src/features/improve/improve_sequence.py:101` |
| `sequence::render_iteration_reports` and `iteration_context` | `src/features/improve/improve_sequence.py:148`, `:156` |
| `sequence::compose_iteration_description` | `src/features/improve/improve_sequence.py:191` |
| `sequence::LoopSpec` and `run_improve_sequence` | `src/features/improve/improve_sequence.py:270` |
| `sequence::close_ended_loop_child` | `src/features/improve/improve_sequence.py:701` |
| `sequence::reconcile_interrupted_sequences` | `src/features/improve/improve_sequence.py:872` |
| `controller::ImproveSequenceController` | `src/features/improve/sequence_controller.py:31` |
| `controller::register` | `src/features/improve/__init__.py:5` |
