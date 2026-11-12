# charliebot-tasks

This crate owns manager turns. `MasterQueue` contains the per-session work queues, consumers, current items, running
turns, and the shared backend pool used by after-round naming. Each session consumer runs one manager turn at a time,
persists its translated events through `charliebot-store`, and finishes with the unread decision and session anchors.

The execution task inserts `MasterQueue` into typed startup state and wires chat routes. The recovery task wires
shutdown and re-attachment into server lifespan.

## Public API

- `MasterQueue` and `MasterQueue::new(Arc<BackendPool>)`
- `MasterQueue::session_callbacks` and `master_queue::session_callbacks`
- `SessionCallbacks` and `PersistAndBroadcastFn`, `MarkUnreadFn`, `PersistNativeIdFn`, `PersistAccountLabelFn`, `ContextStateFn`, `TaskTreeActivityFn`, and `AfterRoundFn`
- `MasterTurnRequest` and its fields
- `MasterResumeRequest` and its fields
- `MasterQueue::run_message`
- `MasterQueue::enqueue_master_resume`
- `MasterQueue::session_is_busy` and `master_queue::session_is_busy`
- `MasterQueue::shutdown`, which cancels the consumers at server stop
- `TaskRunBinding` and its fields
- `MasterResumeRecord` and its fields
- `TaskSpawnCallback`, `TaskFinishCallback`, and `ResumeLiveness`
- `master_state` module and its work item
- `master_queue` module
- `master_run` module
- `autonamer::iter_light_backends`
- `autonamer::name_after_round`
- `autonamer::maybe_auto_name`
- `autonamer::is_default_session_name`, `prefix_session_number_if_default`, and `build_groups_clause`
- `autonamer::parse_name_and_group` and `fuzzy_match_group`
- `spawner_backends::unknown_backend_pin_refusal`
- `spawner_backends::resolve_backend_option`
- `spawner_backends::resolve_session_default_backend_model`
- `spawner_backends::resolve_requested_subagent_backend_model`, `SubagentRole`, `BackendResolutionValueError`
- `spawner_backends::select_verify_backend`
- `review` module
- `review::worker_summary_from_events_log`
- `review::worker_error_from_events_log`
- `review::extract_review_context`
- `review::select_reviewer_backend`
- `review::ReviewScope`
- `review::ReviewerBackendSelection`
- `task_sessions::create_retry`
- `session_anchors::manager_context_reset_note` and `manager_switch_reset_note`
- `session_anchors::worker_switch_reset_note` and `backend_switch_reset_reason`
- `execution::LaunchSettlement`
- `execution::TaskExecutionAdapter` and `TaskExecutionAdapter::new`
- `TaskExecutionAdapter::take_quota_failure`
- `TaskExecutionAdapter::call`
- `TaskExecutionAdapter::launch`, `execute_run`, and `launch_and_settle`
- `execution::pre_recovery_step` and crate `register`
- `execution::apply_sequence_wake_duties`
- `execution::native_continuation_matches`
- `execution::compose_input_prompt`
- `execution::is_out_of_space_error`, `free_disk_gib`
- `routes::routes`, `routes::ROUTE_PATTERNS`, and `register`
- `dispatch::input_event_type_for_caller`, `agent_provenance`
- `Dispatcher::pending_inputs` input-id deduplication
- `Dispatcher::deliver_child_report_locked_as_actor`

## Task completion

- `completion::LandingEvidence` and its fields
- `completion::CompletionEvidence` and its fields
- `completion::SequenceCloseOutcome`
- `completion::TaskCompletion` and `TaskCompletion::new`
- `completion::RUN_REF_PREFIX`
- `TaskCompletion::complete_task`
- `TaskCompletion::cancel_task`
- `TaskCompletion::restore_task`, `restore_chain`, and `restore_chain_locked`
- `TaskCompletion::acknowledge_inputs`
- `TaskCompletion::close_sequence_child`
- `TaskCompletion::recheck_close_requests`
- `TaskCompletion::evaluate_automatic_completion`
- `TaskCompletion::after_run_finished`
- `TaskCompletion::pause_reason`
- `RunFinishedCompletion for TaskCompletion`
- `SequenceCompletion for TaskCompletion`
- `completion::task_completion`
- `completion::recheck_close_requests`
- `completion::after_run_finished`
- `completion::pause_reason`
- `git::git_verify_commit_landed`

## Git worktree and remote helpers

The git helpers task runs and the improve loop call. One subprocess path (`git::git_proc_bytes`) serves the whole
module; `git_verify_commit_landed` runs on it too. The `test-hooks` feature shrinks the worktree-add and per-attempt
remote limits and injects a test git program; production builds use the timeout constants listed below.

- `git::git_worktree_dir_name`
- `git::git_current_branch`
- `git::BaseBranchResolutionError`
- `git::BaseResolution` and its fields
- `git::git_stdout` (crate-public)
- `git::git_rev_parse_optional` (crate-public)
- `git::git_remote_default_branch_and_tip`
- `git::resolve_base_branch`
- `git::git_create_worktree`
- `git::git_worktree_remove`
- `git::git_worktree_remove_reporting`
- `git::git_worktree_prune`
- `git::git_rev_parse`
- `git::git_rev_list_count`
- `git::git_push_branch`
- `git::git_push_refspec`
- `git::SUBPROCESS_GIT_READ_TIMEOUT_ASYNC`
- `git::SUBPROCESS_GIT_WRITE_TIMEOUT`
- `git::SUBPROCESS_GIT_REMOTE_ATTEMPT_TIMEOUT`
- `git::GIT_REMOTE_MAX_ATTEMPTS`

## Input dispatch and master triggers

`dispatch::Dispatcher` owns durable task inputs, run claims and child reports over the shared store. Its executor and
post-finish completion traits are supplied by the execution and completion stages. The dispatcher also implements
`charliebot_registry::sequence::SequenceDispatch` for sequence controllers.

- `dispatch` module
- `dispatch::DispatchExecutor` and `DispatchExecutor::launch`
- `dispatch::RunFinishedCompletion` and `RunFinishedCompletion::after_run_finished`
- `dispatch::Dispatcher`, `Dispatcher::new`
- `Dispatcher::set_executor`, which registers the executor
- `Dispatcher::admit_input`
- `Dispatcher::pending_inputs`, `pending_input_blockers`
- `Dispatcher::claim_input_batch_locked`
- `Dispatcher::dispatch_pending`
- `Dispatcher::wake_parent`
- `Dispatcher::finish_run`
- `Dispatcher::report_source_event`
- `Dispatcher::child_report_provenance`, `child_report_provenances`
- `Dispatcher::deliver_child_report`
- `Dispatcher::deliver_child_report_locked`
- `Dispatcher::recover_pending_reports`, `recover_pending_reports_locked`
- `Dispatcher::undelivered_report_blockers`
- `dispatch::note_run_liveness`
- `dispatch::unprocessed_input_blocker`
- `dispatch::inputs_not_pending_conflict`
- `dispatch::child_report_text`
- `dispatch::dispatcher(&FeatureContext)`
- `master_trigger` module
- `master_trigger::TriggerMasterRequest` and its fields
- `TriggerMasterRequest::new`, which sets the defaults of the optional fields
- `master_trigger::trigger_master`

## Startup recovery and shutdown

`recovery::recovery_step`, registered as `task_recovery`, runs after the pre-recovery step. It re-attaches manager
turns that are still alive, drains runs that exited while the server was down, and replays the follow-ups their crash
window lost. `recovery::let_go_step`, registered as `task_execution`, runs after services stop. It first cancels each
run launch and waits for it to end: the run keeps no terminal fact, a started agent keeps running, and the next boot
launches or re-attaches the run. A launch requested after this point starts no task. It then cancels the queue's
consumers: a turn whose record is durable detaches, and an uncovered turn terminates.

- `recovery::recovery_step`
- `recovery::reconcile_task_tree`
- `recovery::reconcile_node`
- `recovery::replay_followups` (private)
- `recovery::replay_owes_review`
- `recovery::closed_task_stop_request_id`
- `recovery::resume_run` (private `resume_run_checked`, `enqueue_manager_resume`)
- `recovery::repair_end_metadata`
- `recovery::let_go_step`
- `TaskExecutionAdapter::let_go_launches`
- `recovery::start_end_landing_retry`, `retry_while_out_of_space`, `RUN_END_LANDING_RETRY_INTERVAL`

A manager turn that is alive enqueues a master resume through `MasterQueue::enqueue_master_resume`, and the follow is
tracked and never awaited, so the boot pass does not wait on the queue. The resumed record carries no input ids.

A re-attached or drained worker run ends through the end handling its first launch uses: `execute_worker_run` and
`resume_worker_run` both call `TaskExecutionAdapter::end_worker_run`, which records the run's end and then takes the
first-launch follow-up, either the pool failover or the parent report. A re-attached process still alive when its
follow ends is terminated first. A manager resume whose follow returns an error lands the first launch's failure
through `land_launch_failure`: the failed end record, the "Turn failed" notice and the node's pending inputs. The
follow checkpoints a line whose event ended the run as a quota or temporary-unavailable rejection (such as an `is_error`
result with a 429 or 5xx status), so a later boot does not append that line again.

### Follow-ups that return a named error

The follow-ups that have no replay yet return an error that names the owner and the step. Startup logs each one as
`task_recovery_node_failed` and continues with the other nodes.

| Owner | Rust call site | Follow-up that is not replayed |
| --- | --- | --- |
| cold-read | `replay_followups` for a cold-read run that succeeded, failed without later work, or was interrupted | cold-read delivery, failure chain and interrupted report |
| improve | `replay_followups` for an improve sequence run, queued, launched or terminal; no terminal fact is written | the loop's close step (the improve sequence controller's `recover_run`) |

`tests/recovery.rs` drives each row through the full execution stack.

### Sequence runs whose kind no controller claims

A sequence Run whose kind no registered controller claims (its package was deleted) is never launched by the dispatcher
(`dispatch::Dispatcher::dispatch_pending`). Its other three paths handle it as follows:

| Path | Rust call site | Result |
| --- | --- | --- |
| finish | `execution::after_worker_run_inner` | logs `sequence_run_without_controller` with the session, run and owner_ref, then returns |
| launch | `execution::execute_worker_run` | fails before any process starts; the error event names the owner_ref |
| recovery | `recovery::replay_followups` | an unlaunched run gets one `interrupted` finish; a terminal or unlaunched run logs the warning; a launched run is followed as any other |

For a kind in `UNLANDED_SEQUENCE_KINDS`, each path fails by name instead, because its controller has not landed.
`tests/dispatch.rs` and `tests/recovery.rs` cover these paths.

## Task-run worker

`worker::Worker` runs one task run's process. A run is one process, or a chain of processes when the backend lifecycle
relays it through `launch_loop::run_launches`. Each process's events pass through a run sink: they are appended to the
run's events log and broadcast on the thread's channel, and the compaction events that `handle_compaction_events`
synthesizes follow each one. The terminal events close the run: hang diagnostics, stderr, the cgroup report, then
`complete` or `error`. Nothing outside the tests constructs `Worker` yet.

- `worker::Worker` and `Worker::new(Arc<FeatureContext>, WorkerSpec)`
- `worker::WorkerSpec` and its fields
- `Worker::set_on_session_event`, `SessionEventFn`
- `worker::SpawnedFn`
- `Worker::account_relays`
- `Worker::run`
- `Worker::resume`
- `Worker::terminate`, `Worker::detach`
- `Worker::build_backend` (private)
- `Worker::launch_context` (private)
- `Worker::persist_and_broadcast` (private)
- `Worker::emit_terminal_events` (private)
- `handle_event` (private)
- `handle_compaction_events` (private)
- `worker::QuotaExhaustedError`, `QuotaExhaustedError::new`, `resets_at`
- `worker::HARNESS_INSTRUCTIONS_NAME`
- `worker::HANG_DIAGNOSTICS_NAME`
- `worker::NO_OUTPUT_REPORT_THRESHOLD` (private)
- `worker::STDERR_LOG_CHARS` (private)
- `worker::GIT_STASH_GUARD_DIR` (private)
- `worker::QUOTA_MESSAGE_FALLBACK` (private)

`run_support` is crate-private. It holds the helpers that `master_run` and `worker` share: the runner's event channel
(`StreamEvent`, `stream_event_channel`), the stderr tail, the subprocess buffer limit, the resume routing of a backend
type, and a launch's extra flags.

Test hooks under the `test-hooks` feature: `Worker::with_hang_timeouts` shortens the follow after a result, and
`Worker::build_process_backend_for_test` and `Worker::process_event_for_test` expose the build and event paths. The
hooks let the tests reach these paths directly, without mocks.

Behavior notes:

- The hang-diagnostics, stderr and cgroup terminal events are appended through the run's held events-log handle with
  `sync_data`, not through `charliebot-store`'s `ndjson::append`, which is crate-private. The line bytes are the same
  compact JSON.
- A process ended by a signal reports the signal as its exit code (`-15` for SIGTERM).

## Prompt assembly

`prompts` assembles the managed instructions of every agent run. For the same sources it produces the same blocks,
`prompt_hash` and `char_count` as the server it replaced. The two exports below are the entry points for the launch path
and the run-context read. On this branch neither has a caller outside `tests/task_prompts.rs`.

| Export | Signature |
| --- | --- |
| `prompts::assemble_coherent_snapshot` | `async fn(store: &Store, env: &PromptEnv<'_>, meta: &SessionMetadata, kind: &str, option: &BackendOption) -> anyhow::Result<CoherentSnapshot>`, where `CoherentSnapshot { snapshot, overlay_error, declared }` |
| `prompts::read_run_context` | `fn(store: &Store, session_id: &str, run_id: &str) -> Result<serde_json::Value, ContextReadError>`, where `ContextReadError { status: u16, detail: String }` carries 404 or 500 |

`PromptEnv { cfg, registry, checkout }` gathers the inputs of one assembly. `checkout` is `charliebot_core::RepoRoot`,
the validated checkout root held in typed startup state.

- `prompts::SNAPSHOT_FILENAME`, `prompts::LAUNCH_TEXT_FILENAME`
- `charliebot_registry::turn::SCOPE_BASE`, `SCOPE_MEMORY`, `SCOPE_SUBTREE`, `SCOPE_NODE`, `SCOPE_TASTE`,
  `DELIVERY_FULL`, `DELIVERY_INDEX`
- `prompts::MANAGER_KINDS`, `prompts::WORKER_KINDS`
- `prompts::COHERENCE_PASSES`
- `prompts::MAX_AGENT_MESSAGE_WEIGHTED_CHARS`
- `prompts::HOST_SUPPLEMENT_FILENAME`
- `charliebot_core::home::PROMPT_BODIES_DIR_NAME`
- `prompts::TaskPromptError`
- `charliebot_core::RepoRoot`
- `prompts::PromptEnv`
- `prompts::PromptSource`, `prompts::RuleSegment` (re-exported, not redefined)
- `prompts::PromptBlock`
- `PromptSnapshot::instructions_text`, `char_count`, `prompt_hash`, `to_json_dict`, `from_json_dict`
- `prompts::assemble_snapshot`
- `prompts::build_segments`, `prompts::RuleInputs`
- `prompts::ChainLink { session_id, subtree_ref }`
- `prompts::OverlayError::class_name`
- `prompts::prompt_task_type`
- `prompts::read_local_rule_body`
- `prompts::resolve_launch_overlay`
- `prompts::capture_prompt_chain`
- `prompts::assemble_coherent_snapshot`, `prompts::CoherentSnapshot`
- `prompts::read_run_context`, `prompts::ContextReadError`
- `prompts::render_session_info`
- `prompts::binding_intro`
- `prompts::render_worktree_bindings`
- `prompts::render_task_body`
- `prompts::render_worktree_persistence`
- `prompts::review_task_context`
- `prompts::repo_less_review_task_context`
- `spawner_prompt::workflow_prompt_sections`, `repo_less_workflow_sections`
- `spawner_prompt::REPO_LESS_SOURCE_FILES_SECTION`
- `spawner_prompt::workflow_rule_section_ids`
- `spawner_prompt::load_marker_sections`
- `spawner_prompt::load_worker_prompt_sections`
- `spawner_prompt::substitute_tokens`, `spawner_prompt::Tokens`
- `spawner_prompt::verify_contract_tokens`
- `spawner_prompt::worktree_binding_tokens`
- `spawner_prompt::read_text`, `spawner_prompt::TextReadError`
- `verify_trailer::VERIFY_RESULT_TRAILER_PATTERN`, `verify_result_trailer_expected`
- `review_text::review_rules_text`
- `review_text::review_numbered_steps`
- `review_text::review_published_branch`
- `review_text::review_landing_target`
- `review_text::review_spec_text`
- `review_text::review_context_lines`
- `review_text::review_log_pointer`
- `review_text::review_git_venue`
- `cold_read::PROMPT_FILENAME`, `TRIGGER_PLACEHOLDER`, `EVIDENCE_FILENAME`
- `cold_read::load_texts`
- `cold_read::probe_input`
- `cold_read::format_report`, `cold_read::ColdReadOutcome` (`ColdReadOutcome` has the `Answered` and `AllFailed` branches that `format_report` renders)
- `cold_read::page_sha256`
- `execution::TaskExecutionAdapter::after_cold_read_run` (`pub(crate)`)
- `recovery::replay_followups` cold-read rows (private)
- `text_length::weighted_length`
- `routes::session_message`

Behavior notes:

- Malformed snapshot JSON other than invalid values and literals reports serde_json's parse message in the run-context
  `unreadable` detail.
- A snapshot field with the wrong type reports the Rust message.
- Overlay and template read errors reproduce the common `OSError` and `UnicodeDecodeError` texts.
- Assembly reads its files on the calling thread.
- The cold-read launch and report read the page and the evidence file on the calling thread.
- The memory contribution is not implemented, so no rule source here emits `SCOPE_MEMORY`.

## Tests

`tests/prompt_snapshot_golden.rs` compares the recorded 18-case golden matrix (`fixtures/prompt_snapshot_golden.json`,
recorded from the retired reference server) byte for byte, with each `prompt_hash` and `char_count`, and the damaged-
snapshot error texts. `tests/task_prompts.rs` covers the task-tree and run-context cases over a temporary store; the
staged-memory case is not covered. Unit tests cover the verify trailer, the worker prompt loader, the review steps and
the snapshot dedup and hash.

`tests/turn_golden.rs` compares the recorded twelve scripted streams (`fixtures/turn_golden.json`) through the
Rust queue after normalizing event ids, timestamps, and elapsed timing fields. It starts only synthetic `/bin/sh`
backends.

`recovery.rs` unit tests cover the review-owed rule
(`a_review_is_owed_only_to_the_newest_unfollowed_work_run_of_an_open_task`), the closed-task stop id, and the
end-landing retry rounds. `tests/recovery.rs` covers the manager-turn MASTER_DONE out-of-space case over the store's
write-fault seam (`write-faults` feature) and a live `/bin/sh` turn, with the retry interval replaced through the
`test-hooks` feature. Other startup-recovery cases are not covered yet. The cases that replay a named follow-up above
(failed-run report, review spawn) need that replay before they can pass, and the rest need a store fixture with live
run processes.

`tests/delegate.rs` covers the artifact check's cold-read delegation cases and the session-message relay cases.
`tests/worker_run.rs` covers the cold-read run cases: the instructions and input, the report with its attempts, the
empty answer, the next-backend retry and the all-failed archive. `tests/recovery.rs` covers the five cold-read replay
cases.

`tests/worker.rs` covers the account relay, backend build, hang diagnostics, event line, quota scan and non-Claude build
cases. It runs scripted `/bin/sh` processes, which the process-group guard kills when a test ends, and it starts no agent
CLI. The resume path has a case with no counterpart in the reference suite: a fake agent writes a result and stays alive,
and the case checks the diagnostics file, the group's exit and the terminal event. Three cases observe their behavior
differently. The falls-back case raises the below-line target's reading from a subscriber on the thread channel, when the
safe point's `user` frame is broadcast. The watch has observed that event by then, and the move reads the pool only after
the process exits, so the reading rises between the gate and the move. The case does not stub `claude_accounts.select`.
The hang-diagnostics case checks the written file's shape rather than its content, since the runner captures the
diagnostics itself. The compaction case checks the events log, not a spy on `compact_with_sonnet`.
