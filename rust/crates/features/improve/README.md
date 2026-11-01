# `charliebot-improve`

The improve loop: `charliebot improve` runs an iterative worker loop on the task tree as one sequence. The loop's
state lives under `sessions/<sid>/loops/`; the sequence controller owns every Run whose `sequence_ref.kind` is
`improve`. `register` mounts the routes and the controller; the packages list in `charliebot-server` calls it in
place of the improve stub.

The start route passes the improve role to backend resolution. Each pooled iteration cools its member after a quota
failure and selects again on the next iteration.

## Public API

- `register`
- `api::routes` and `api::matches`, the start and stop routes, with `api::ImproveRequest` and `api::ImproveStopRequest`
- `command::ImproveState`, `command::ImproveLoopAlreadyRunningError` and `command::Reservation` with
  `reserve_loop_state`
- `command::resolve_path`, which follows the symlinks in the path's existing prefix
- `command::loops_dir`, `active_lock_path`, `loop_dir`, `loop_goal_path` and `loop_plan_path`
- `command::read_loop_goal` and `read_loop_plan`
- `command::load_loop_state`, `save_loop_state`, `require_loop_state` and `clear_active_loop_lock`
- `command::find_running_loop` and `stop_improve_loop`
- `command::{STATE_FILE, GOAL_FILE, PLAN_FILE, ACTIVE_LOCK_FILE}`
- `judgment::{ITERATION_SUMMARIES_HEADING, RUNNER_FALLBACK_REPORT_MARKER, WORKTREE_CREATE_ERROR_PREFIX}` and
  `judgment::LOOP_FAILURE_ERROR_PREFIX`
- `judgment::commits_section_first_none`, `iter_report_validity`, `invalid_iteration_summary` and `summary_text`
- `judgment::extract_iteration_summary_from_events`, `quota_blocker_match`,
  `failed_iteration_judgments_from_events` and `newest_first_event_iter`
- `landing::commit_delta`, `landing::land_work_branch_after_loop` with `Landing`, and `landing::blocked_loop_summary`
- `sequence::SEQUENCE_KIND`
- `sequence::LoopSpec` and `run_improve_sequence`
- `sequence::create_improve_child` with `improve_child_id`, and `close_ended_loop_child`
- `sequence::reconcile_interrupted_sequences`
- `sequence::render_iteration_reports` with `iteration_context`, and `compose_iteration_description`
- `sequence::{IMPROVE_ITERATION_COMPLETED, IMPROVE_COMPLETED, IMPROVE_FAILED, IMPROVE_STOPPED}`
- `sequence::{loop_owner_ref, improve_child_request_id, iteration_run_request_id, improve_close_request_id}`
- `controller::ImproveSequenceController` and `controller::register`
