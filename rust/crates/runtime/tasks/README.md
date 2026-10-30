# charliebot-tasks

This crate owns manager turns. `MasterQueue` contains the per-session work queues, consumers, current items, running
turns, and the shared backend pool used by after-round naming. Each session consumer runs one manager turn at a time,
persists its translated events through `charliebot-store`, and finishes with the unread decision and session anchors.

The execution task inserts `MasterQueue` into typed startup state and wires chat routes. The recovery task wires
shutdown and re-attachment into server lifespan.

## Public API

| Rust item | Python source |
| --- | --- |
| `MasterQueue` and `MasterQueue::new(Arc<BackendPool>)` | `src/runtime/master_cc_state.py:11-17`; `src/runtime/backend_pool.py` |
| `MasterQueue::session_callbacks` and `master_queue::session_callbacks` | `src/infra/models.py:723-744` |
| `SessionCallbacks` and `PersistAndBroadcastFn`, `MarkUnreadFn`, `PersistNativeIdFn`, `PersistAccountLabelFn`, `ContextStateFn`, `TaskTreeActivityFn`, and `AfterRoundFn` | `src/infra/models.py:723-744` |
| `MasterTurnRequest` and its fields | `src/runtime/master_cc_queue.py:54-118` |
| `MasterResumeRequest` and its fields | `src/runtime/master_cc_queue.py:402-444` |
| `MasterQueue::run_message` | `src/runtime/master_cc_queue.py:355-400` |
| `MasterQueue::enqueue_master_resume` | `src/runtime/master_cc_queue.py:402-444` |
| `MasterQueue::queued_user_event_ids` and `master_queue::queued_user_event_ids` | `src/runtime/master_cc_queue.py:446` |
| `MasterQueue::running_user_event_ids` and `master_state::running_user_event_ids` | `src/runtime/master_cc_state.py:58` |
| `MasterQueue::shutdown` | `src/runtime/master_cc_queue.py:186-354` (consumer cancellation at server stop) |
| `TaskRunBinding` and its fields | `src/runtime/master_cc_state.py:21-29` |
| `MasterResumeRecord` and its fields | `src/infra/models.py:337-350` |
| `TaskSpawnCallback`, `TaskFinishCallback`, and `ResumeLiveness` | `src/runtime/master_cc_state.py:32-49` |
| `master_state` module and its work item | `src/runtime/master_cc_state.py:32-49` |
| `master_queue` module | `src/runtime/master_cc_queue.py:54-446` |
| `master_run` module | `src/runtime/master_cc_run.py:36-846` |
| `autonamer::iter_light_backends` | `src/runtime/autonamer.py:129` |
| `autonamer::name_after_round` | `src/runtime/autonamer.py:197` |
| `autonamer::maybe_auto_name` | `src/runtime/autonamer.py:232` |
| `autonamer::is_default_session_name`, `prefix_session_number_if_default`, and `build_groups_clause` | `src/runtime/autonamer.py:53`, `:62`, `:94` |
| `autonamer::parse_name_and_group` and `fuzzy_match_group` | `src/runtime/autonamer.py:108`, `:141` |
| `spawner_backends::unknown_backend_pin_refusal` | `src/runtime/spawner_backends.py:32` |
| `spawner_backends::resolve_backend_option` | `src/runtime/spawner_backends.py:22` |
| `spawner_backends::resolve_session_default_backend_model` | `src/runtime/spawner_backends.py:49` |
| `spawner_backends::resolve_requested_subagent_backend_model`, `SubagentRole`, `BackendResolutionValueError` | `src/runtime/spawner_backends.py:11-13`, `:85` |
| `spawner_backends::select_verify_backend` | `src/runtime/spawner_backends.py:117` |
| `review` module | `src/runtime/review.py:170-264` |
| `review::worker_summary_from_events_log` | `src/runtime/review.py:170-198` |
| `review::worker_error_from_events_log` | `src/runtime/review.py:201-218` |
| `review::extract_review_context` | `src/runtime/review.py:221-233` |
| `review::select_reviewer_backend` | `src/runtime/review.py:236-264` |
| `review::ReviewerBackendSelection` | `src/runtime/review.py:254-264` |
| `task_sessions::create_retry` | `src/runtime/task_sessions.py:1328` |
| `session_anchors::context_reset_note` and `backend_switch_reset_reason` | `src/runtime/session_anchors.py:46`, `:36` |
| `execution::LaunchSettlement` | `src/runtime/task_execution.py:170` |
| `execution::TaskExecutionAdapter` and `TaskExecutionAdapter::new` | `src/runtime/task_execution.py:388-433` |
| `TaskExecutionAdapter::take_quota_failure` | `src/runtime/task_execution.py:2109` |
| `TaskExecutionAdapter::call` | `src/runtime/task_execution.py:435-523` |
| `TaskExecutionAdapter::launch`, `execute_run`, and `launch_and_settle` | `src/runtime/task_execution.py:589-605`, `:749-865`, `:1074-1124` |
| `execution::pre_recovery_step` and crate `register` | `src/runtime/task_execution.py:2625-2638` |
| `execution::apply_sequence_wake_duties` | `src/runtime/task_execution.py:380-386` |
| `execution::native_continuation_matches` | `src/runtime/task_execution.py:111-129` |
| `execution::compose_input_prompt` | `src/runtime/task_execution.py:237-280` |
| `execution::is_out_of_space_error`, `free_disk_gib` | `src/runtime/task_execution.py:193-234` |
| `routes::routes`, `routes::ROUTE_PATTERNS`, and `register` | `src/runtime/api/chat.py:48-131`; `src/runtime/api/sessions.py:1689-1715`, `:1829-1857` |
| `dispatch::input_event_type_for_caller`, `agent_provenance` | `src/runtime/session_dispatch.py:812-850` |
| `Dispatcher::pending_inputs` input-id deduplication | `src/runtime/session_dispatch.py:374-379` |
| `Dispatcher::deliver_child_report_locked_as_actor` | `src/runtime/task_completion.py:752` |

## Task completion

| Rust item | Python source |
| --- | --- |
| `completion::LandingEvidence` and its fields | `src/runtime/task_completion.py:128` |
| `completion::CompletionEvidence` and its fields | `src/runtime/task_completion.py:143` |
| `completion::SequenceCloseOutcome` | `src/runtime/task_completion.py:764` |
| `completion::TaskCompletion` and `TaskCompletion::new` | `src/runtime/task_completion.py:168` |
| `completion::RUN_REF_PREFIX` | `src/runtime/task_completion.py:121` |
| `TaskCompletion::complete_task` | `src/runtime/task_completion.py:490` |
| `TaskCompletion::cancel_task` | `src/runtime/task_completion.py:1176` |
| `TaskCompletion::restore_task`, `restore_chain`, and `restore_chain_locked` | `src/runtime/task_completion.py:1264`, `:1287`, `:1297` |
| `TaskCompletion::acknowledge_inputs` | `src/runtime/task_completion.py:1074` |
| `TaskCompletion::close_sequence_child` | `src/runtime/task_completion.py:764` |
| `TaskCompletion::recheck_close_requests` | `src/runtime/task_completion.py:870` |
| `TaskCompletion::evaluate_automatic_completion` | `src/runtime/task_completion.py:950` |
| `TaskCompletion::after_run_finished` | `src/runtime/task_completion.py:1023` |
| `TaskCompletion::pause_reason` | `src/runtime/task_completion.py:1005` |
| `RunFinishedCompletion for TaskCompletion` | `src/runtime/task_completion.py:1023-1055` |
| `SequenceCompletion for TaskCompletion` | `src/runtime/task_completion.py:764-868`, `:950-1023`, `:1264-1312` |
| `completion::task_completion` | `src/runtime/task_completion.py:168` |
| `completion::recheck_close_requests` | `src/runtime/task_completion.py:870` |
| `completion::after_run_finished` | `src/runtime/task_completion.py:1023` |
| `completion::pause_reason` | `src/runtime/task_completion.py:1005` |
| `git::git_verify_commit_landed` | `src/infra/git.py:683` |

## Git worktree and remote helpers

The git helpers task runs and the improve loop call. One subprocess path (`git::git_proc_bytes`, Python
`_git_proc_bytes`) serves the whole module; `git_verify_commit_landed` runs on it too. The
`test-hooks` feature shrinks the worktree-add and per-attempt remote limits and injects a test
git program; production builds keep the Python values.

| Rust item | Python source |
| --- | --- |
| `git::git_worktree_dir_name` | `src/infra/git.py:32` |
| `git::git_current_branch` | `src/infra/git.py:37` |
| `git::BaseBranchResolutionError` | `src/infra/git.py:57` |
| `git::BaseResolution` and its fields | `src/infra/git.py:68` |
| `git::git_stdout` (crate-public) | `src/infra/git.py:102` |
| `git::git_rev_parse_optional` (crate-public) | `src/infra/git.py:118` |
| `git::git_remote_default_branch_and_tip` | `src/infra/git.py:173` |
| `git::resolve_base_branch` | `src/infra/git.py:200` |
| `git::git_create_worktree` | `src/infra/git.py:360` |
| `git::git_worktree_remove` | `src/infra/git.py:532` |
| `git::git_worktree_remove_reporting` | `src/infra/git.py:575` |
| `git::git_worktree_prune` | `src/infra/git.py:623` |
| `git::git_rev_parse` | `src/infra/git.py:658` |
| `git::git_rev_list_count` | `src/features/improve/improve_command.py:303-330` |
| `git::git_push_branch` | `src/infra/git.py:727` |
| `git::git_push_refspec` | `src/infra/git.py:733` |
| `git::SUBPROCESS_GIT_READ_TIMEOUT_ASYNC` | `src/infra/timeouts.py:14` |
| `git::SUBPROCESS_GIT_WRITE_TIMEOUT` | `src/infra/timeouts.py:20` |
| `git::SUBPROCESS_GIT_REMOTE_ATTEMPT_TIMEOUT` | `src/infra/timeouts.py:27` |
| `git::GIT_REMOTE_MAX_ATTEMPTS` | `src/infra/timeouts.py:28` |

## Input dispatch and master triggers

`dispatch::Dispatcher` owns durable task inputs, run claims and child reports over the shared store. Its executor and
post-finish completion traits are supplied by the execution and completion stages. The dispatcher also implements
`charliebot_registry::sequence::SequenceDispatch` for sequence controllers.

| Rust item | Python source |
| --- | --- |
| `dispatch` module | `src/runtime/session_dispatch.py:1` |
| `dispatch::DispatchExecutor` and `DispatchExecutor::launch` | `src/runtime/session_dispatch.py:157-161`, `:481-515` |
| `dispatch::RunFinishedCompletion` and `RunFinishedCompletion::after_run_finished` | `src/runtime/session_dispatch.py:52-60`; completion behavior at `src/runtime/task_completion.py:1023-1055` |
| `dispatch::Dispatcher`, `Dispatcher::new` | `src/runtime/session_dispatch.py:152-161` |
| `Dispatcher::set_executor` | `src/runtime/task_execution.py:2625-2632` (executor registration) |
| `Dispatcher::admit_input` | `src/runtime/session_dispatch.py:167-278` |
| `Dispatcher::pending_inputs`, `pending_input_blockers` | `src/runtime/session_dispatch.py:343-387` |
| `Dispatcher::claim_input_batch_locked` | `src/runtime/session_dispatch.py:393-424` |
| `Dispatcher::dispatch_pending` | `src/runtime/session_dispatch.py:430-515` |
| `Dispatcher::wake_parent` | `src/runtime/session_dispatch.py:517-528` |
| `Dispatcher::finish_run` | `src/runtime/session_dispatch.py:530-593` |
| `Dispatcher::report_source_event` | `src/runtime/session_dispatch.py:599-613` |
| `Dispatcher::child_report_provenance`, `child_report_provenances` | `src/runtime/session_dispatch.py:309-341` |
| `Dispatcher::deliver_child_report` | `src/runtime/session_dispatch.py:615-673` |
| `Dispatcher::deliver_child_report_locked` | `src/runtime/session_dispatch.py:675-713` |
| `Dispatcher::recover_pending_reports`, `recover_pending_reports_locked` | `src/runtime/session_dispatch.py:751-800` |
| `Dispatcher::undelivered_report_blockers` | `src/runtime/session_dispatch.py:802-809` |
| `dispatch::note_run_liveness` | `src/runtime/task_sessions.py:602-622`, `src/runtime/runs.py:556-570` |
| `dispatch::unprocessed_input_blocker` | `src/runtime/session_dispatch.py:140-144` |
| `dispatch::inputs_not_pending_conflict` | `src/runtime/session_dispatch.py:147-149` |
| `dispatch::child_report_text` | `src/runtime/session_dispatch.py:119-131` |
| `dispatch::dispatcher(&FeatureContext)` | `src/runtime/task_sessions.py:357`, `src/runtime/task_execution.py:2625-2638` |
| `master_trigger` module | `src/runtime/master_trigger.py:1` |
| `master_trigger::TriggerMasterRequest` and its fields | `src/runtime/master_trigger.py:41-53` |
| `TriggerMasterRequest::new` | `src/runtime/master_trigger.py:41-53` (default arguments) |
| `master_trigger::trigger_master` | `src/runtime/master_trigger.py:41-87` |

## Startup recovery and shutdown

`recovery::recovery_step`, registered as `task_recovery`, runs after the pre-recovery step. It re-attaches manager
turns that are still alive, drains runs that exited while the server was down, and replays the follow-ups their crash
window lost. `recovery::let_go_step`, registered as `task_execution`, runs after services stop. It first cancels each
run launch and waits for it to end: the run keeps no terminal fact, a started agent keeps running, and the next boot
launches or re-attaches the run. A launch requested after this point starts no task. It then cancels the queue's
consumers: a turn whose record is durable detaches, and an uncovered turn terminates.

| Rust item | Python source |
| --- | --- |
| `recovery::recovery_step` | `src/runtime/task_recovery.py:46`; `server.py:215-225` |
| `recovery::reconcile_task_tree` | `src/runtime/task_recovery.py:46-85` |
| `recovery::reconcile_node` | `src/runtime/task_execution.py:2668-2774` |
| `recovery::replay_followups` (private) | `src/runtime/task_execution.py:2802-2914` |
| `recovery::replay_owes_review` | `src/runtime/task_execution.py:2775-2801` |
| `recovery::closed_task_stop_request_id` | `src/runtime/task_execution.py:210-219` |
| `recovery::resume_run` (private `resume_run_checked`, `enqueue_manager_resume`) | `src/runtime/task_execution.py:1919-2034` |
| `recovery::repair_end_metadata` | `src/runtime/task_execution.py:2084` |
| `recovery::let_go_step` | `src/runtime/master_cc_run.py:648-691`; `server.py:228-300` |
| `TaskExecutionAdapter::let_go_launches` | `asyncio/runners.py:66-75`; `src/runtime/task_execution.py:714-731` |
| `recovery::start_end_landing_retry`, `retry_while_out_of_space`, `RUN_END_LANDING_RETRY_INTERVAL` | `src/runtime/task_execution.py:611-660` |

A manager turn that is alive enqueues a master resume through `MasterQueue::enqueue_master_resume`, and the follow is
tracked and never awaited, so the boot pass does not wait on the queue. The resumed record carries no input ids, as
`task_execution._resume_manager_turn` (`task_execution.py:2006-2034`) builds it.

### Follow-ups that return a named error

The remaining unported follow-ups return an error that names the owner and the step. Startup logs each one as
`task_recovery_node_failed` and continues with the other nodes.

| Owner | Rust call site | Follow-up that Python replays (`task_execution.py:2802-2914`) |
| --- | --- | --- |
| cold-read | `replay_followups` for a cold-read run that succeeded, failed without later work, or was interrupted | cold-read delivery, failure chain and interrupted report |
| improve | `replay_followups` for an improve sequence run, queued, launched or terminal; no terminal fact is written | the loop's close step (`ImproveSequenceController::recover_run`, `src/features/improve/sequence_controller.py:66-79`) |

`tests/recovery.rs` drives each row through the full execution stack; its `// Python:` comments name the cases it follows.

### Sequence runs whose kind no controller claims

A sequence Run whose kind no registered controller claims (its package was deleted) is never launched by the dispatcher
(`dispatch::Dispatcher::dispatch_pending`). Its other three paths match Python's `sequence_run_without_controller`
handling:

| Path | Rust call site | Result | Python |
| --- | --- | --- | --- |
| finish | `execution::after_worker_run_inner` | logs `sequence_run_without_controller` with the session, run and owner_ref, then returns | `task_execution.py:2168-2176` |
| launch | `execution::execute_worker_run` | fails before any process starts; the error event names the owner_ref | `task_execution.py:1390-1394` |
| recovery | `recovery::replay_followups` | an unlaunched run gets one `interrupted` finish; a terminal or unlaunched run logs the warning; a launched run is followed as any other | `task_execution.py:2831-2847` |

For a kind in `UNLANDED_SEQUENCE_KINDS`, each path fails by name instead, because its controller has not landed.
`tests/dispatch.rs` and `tests/recovery.rs` port the seven cases of `tests/test_sequence_controller_runs.py`.

## Task-run worker

`worker::Worker` runs one task run's process (`src/runtime/worker.py`). A run is one process, or a chain of processes
when the backend lifecycle relays it through `launch_loop::run_launches`. Each process's events pass through a run
sink: they are appended to the run's events log and broadcast on the thread's channel, and the compaction events the
Python path synthesizes follow each one. The terminal events close the run: hang diagnostics, stderr, the cgroup
report, then `complete` or `error`. Nothing outside the tests constructs `Worker` yet.

| Rust item | Python source |
| --- | --- |
| `worker::Worker` and `Worker::new(Arc<FeatureContext>, WorkerSpec)` | `src/runtime/worker.py:78`, `:88-128` |
| `worker::WorkerSpec` and its fields | `src/runtime/worker.py:88-128` (`Worker.__init__` arguments) |
| `Worker::set_on_session_event`, `SessionEventFn` | `src/runtime/worker.py:88-128` (`on_session_event`) |
| `worker::SpawnedFn` | `src/runtime/worker.py:97` (`on_spawned`), `:238-246` (`_on_spawn` awaits it with the thread node) |
| `Worker::account_relays` | `src/runtime/worker.py:130` |
| `Worker::run` | `src/runtime/worker.py:223-310` |
| `Worker::resume` | `src/runtime/worker.py:311-375` |
| `Worker::terminate`, `Worker::detach` | `src/runtime/worker.py:446-455` |
| `Worker::build_backend` (private) | `src/runtime/worker.py:133-194` |
| `Worker::launch_context` (private) | `src/runtime/worker.py:195-222` |
| `Worker::persist_and_broadcast` (private) | `src/runtime/worker.py:377-382` |
| `Worker::emit_terminal_events` (private) | `src/runtime/worker.py:388-445` |
| `handle_event` (private; `Worker._process_event`) | `src/runtime/worker.py:456-520` |
| `handle_compaction_events` (private) | `src/runtime/streaming.py:161` (`handle_compaction_events`) |
| `worker::QuotaExhaustedError`, `QuotaExhaustedError::new`, `resets_at` | `src/runtime/worker.py:33-39` |
| `worker::HARNESS_INSTRUCTIONS_NAME` | `src/runtime/runs.py:52` |
| `worker::HANG_DIAGNOSTICS_NAME` | `src/runtime/worker.py:399` |
| `worker::NO_OUTPUT_REPORT_THRESHOLD` (private) | `src/infra/timeouts.py:167` |
| `worker::STDERR_LOG_CHARS` (private) | `src/runtime/worker.py:426` |
| `worker::GIT_STASH_GUARD_DIR` (private) | `src/runtime/worker.py:25` |
| `worker::QUOTA_MESSAGE_FALLBACK` (private) | `src/runtime/worker.py:513` |

`run_support` is crate-private. It holds the helpers that `master_run` and `worker` share: the runner's event channel
(`StreamEvent`, `stream_event_channel`), the stderr tail, the subprocess buffer limit, the resume routing of a backend
type, and a launch's extra flags.

Test hooks under the `test-hooks` feature: `Worker::with_hang_timeouts` shortens the follow after a result, and
`Worker::build_process_backend_for_test` and `Worker::process_event_for_test` expose the build and event paths. The
Python tests reach the same paths through mocks, so the hooks have no Python counterpart.

Known divergences from Python:

- The hang-diagnostics, stderr and cgroup terminal events are appended through the run's held events-log handle with
  `sync_data`, not through `charliebot-store`'s `ndjson::append`, which is crate-private. The line bytes are the same
  compact JSON.
- A process ended by a signal reports the signal as its exit code (`-15` for SIGTERM). The Python hang test injects an
  exit code of 143 with its fake backend.

## Prompt assembly

`prompts` assembles the managed instructions of every agent run. For the same sources it produces the same blocks,
`prompt_hash` and `char_count` as the Python server. The two exports below are the entry points for the launch path and
the run-context read. On this branch neither has a caller outside `tests/task_prompts.rs`.

| Export | Signature |
| --- | --- |
| `prompts::assemble_coherent_snapshot` | `async fn(store: &Store, env: &PromptEnv<'_>, meta: &SessionMetadata, kind: &str, option: &BackendOption) -> anyhow::Result<CoherentSnapshot>`, where `CoherentSnapshot { snapshot, overlay_error, declared }` |
| `prompts::read_run_context` | `fn(store: &Store, session_id: &str, run_id: &str) -> Result<serde_json::Value, ContextReadError>`, where `ContextReadError { status: u16, detail: String }` carries 404 or 500 |

`PromptEnv { cfg, registry, checkout }` gathers the inputs of one assembly. `checkout` is `charliebot_core::RepoRoot`,
the validated checkout root held in typed startup state. Python reads this path from `cfg.charlie_bot_repo`.

| Rust item | Python source |
| --- | --- |
| `prompts::SNAPSHOT_FILENAME`, `prompts::LAUNCH_TEXT_FILENAME` | `src/runtime/task_prompts.py:75-76` |
| `prompts::SCOPE_BASE`, `SCOPE_MEMORY`, `SCOPE_SUBTREE`, `SCOPE_NODE`, `DELIVERY_FULL` | `src/runtime/task_prompts.py:78-82` |
| `prompts::MANAGER_KINDS`, `prompts::WORKER_KINDS` | `src/runtime/task_prompts.py:86`, `:91` |
| `prompts::COHERENCE_PASSES` | `src/runtime/task_prompts.py:96` (`_COHERENCE_PASSES`) |
| `prompts::MAX_AGENT_MESSAGE_WEIGHTED_CHARS` | `src/infra/constants.py:56` |
| `prompts::HOST_SUPPLEMENT_FILENAME` | `src/infra/config.py:287` (`claude_md_file`) |
| `prompts::PROMPT_BODIES_DIR` | `src/runtime/task_prompts.py:462` (`charliebot_home / "prompt_bodies"`) |
| `prompts::TaskPromptError` | `src/runtime/task_prompts.py:99` |
| `charliebot_core::RepoRoot` | `src/infra/config.py:291-293` (`charlie_bot_repo`) |
| `prompts::PromptEnv` (new struct) | the `cfg` argument of `src/runtime/task_prompts.py:436` |
| `prompts::PromptSource`, `prompts::RuleSegment` (re-exported, not redefined) | `src/runtime/task_prompts.py:109`, `:206` |
| `prompts::PromptBlock` | `src/runtime/task_prompts.py:117` |
| `PromptSnapshot::instructions_text`, `char_count`, `prompt_hash`, `to_json_dict`, `from_json_dict` | `src/runtime/task_prompts.py:131`, `:136`, `:140`, `:158`, `:174` |
| `prompts::assemble_snapshot` | `src/runtime/task_prompts.py:403` |
| `prompts::build_segments`, `prompts::RuleInputs` | `src/runtime/task_prompts.py:436` (the `overlay`, `chain` and `node_ref` arguments) |
| `prompts::ChainLink { session_id, subtree_ref }` | the `(session_id, subtree_prompt_ref)` pairs of `src/runtime/task_execution.py:294-313` |
| `prompts::OverlayError::python_class_name` | `src/runtime/task_prompts.py:241` (`_overlay_segment`) |
| `prompts::prompt_task_type` | `src/runtime/task_prompts.py:312` |
| `prompts::read_local_rule_body` | `src/runtime/task_prompts.py:258` |
| `prompts::resolve_launch_overlay` | `src/runtime/task_execution.py:279` |
| `prompts::capture_prompt_chain` | `src/runtime/task_execution.py:294` |
| `prompts::assemble_coherent_snapshot`, `prompts::CoherentSnapshot` | `src/runtime/task_execution.py:336` |
| `prompts::read_run_context`, `prompts::ContextReadError` | `src/runtime/api/sessions.py` (`get_run_context`) |
| `prompts::render_session_info` | `src/runtime/task_prompts.py:477` |
| `prompts::binding_intro` | `src/runtime/task_prompts.py:483` |
| `prompts::render_worktree_bindings` | `src/runtime/task_prompts.py:490` |
| `prompts::render_task_body` | `src/runtime/task_prompts.py:515` |
| `prompts::render_worktree_persistence` | `src/runtime/task_prompts.py:521` |
| `prompts::review_task_context` | `src/runtime/task_prompts.py:527` |
| `prompts::repo_less_review_task_context` | `src/runtime/task_prompts.py:548` |
| `spawner_prompt::workflow_prompt_sections`, `repo_less_workflow_sections` | `src/runtime/spawner_prompt.py:19`, `:30` (`WORKFLOW_PROMPT_SECTION`, `REPO_LESS_WORKFLOW_SECTION`) |
| `spawner_prompt::REPO_LESS_SOURCE_FILES_SECTION` | `src/runtime/spawner_prompt.py:35` |
| `spawner_prompt::workflow_rule_section_ids` | `src/runtime/spawner_prompt.py:38` |
| `spawner_prompt::load_marker_sections` | `src/runtime/spawner_prompt.py:107` |
| `spawner_prompt::load_worker_prompt_sections` | `src/runtime/spawner_prompt.py:112` |
| `spawner_prompt::substitute_tokens`, `spawner_prompt::Tokens` | `src/runtime/spawner_prompt.py:133` |
| `spawner_prompt::verify_contract_tokens` | `src/runtime/spawner_prompt.py:143` |
| `spawner_prompt::worktree_binding_tokens` | `src/runtime/spawner_prompt.py:154` |
| `spawner_prompt::read_text`, `spawner_prompt::TextReadError` | `src/runtime/task_prompts.py:218` (`Path.read_text`, its error texts) |
| `verify_trailer::VERIFY_RESULT_TRAILER_PATTERN`, `verify_result_trailer_re`, `verify_result_trailer_expected` | `src/runtime/verify_trailer.py:14-15` |
| `review_text::review_rules_text` | `src/runtime/review.py:65` |
| `review_text::review_numbered_steps` | `src/runtime/review.py:77` |
| `review_text::review_published_branch` | `src/runtime/review.py:107` |
| `review_text::review_landing_target` | `src/runtime/review.py:115` |
| `review_text::review_spec_text` | `src/runtime/review.py:125` |
| `review_text::review_context_lines` | `src/runtime/review.py:139` |
| `review_text::review_log_pointer` | `src/runtime/review.py:156` |
| `review_text::review_git_venue` | `src/runtime/review.py:163` |
| `cold_read::PROMPT_FILENAME`, `TRIGGER_PLACEHOLDER`, `EVIDENCE_FILENAME` | `src/runtime/cold_read.py:21`, `:24`, `:29` |
| `cold_read::load_texts` | `src/runtime/cold_read.py:32` |
| `cold_read::probe_input` | `src/runtime/cold_read.py:43` |
| `cold_read::format_report`, `cold_read::ColdReadOutcome` | `src/runtime/cold_read.py:49` (`ColdReadOutcome` has no Python class; `Answered` and `AllFailed` are the two branches of `format_report`) |
| `cold_read::page_sha256` | `src/runtime/task_execution.py:1744-1748` (the sha256 `_build_cold_read_context` records) |
| `execution::TaskExecutionAdapter::after_cold_read_run` (`pub(crate)`) | `src/runtime/task_execution.py:2454-2497` |
| `recovery::replay_followups` cold-read rows (private) | `src/runtime/task_execution.py:2863-2890` |
| `text_length::weighted_length` | `src/infra/text_length.py:15` |
| `routes::session_message` | `src/runtime/api/internal.py:320-375` |

Known divergences from Python:

- Other malformed snapshot JSON errors beyond invalid values and literals report serde_json's parse message in the
  run-context `unreadable` detail, where Python reports `JSONDecodeError`'s.
- A snapshot field with the wrong type reports the Rust message, not Python's `TypeError` text.
- Overlay and template read errors reproduce the common `OSError` and `UnicodeDecodeError` texts.
- Assembly reads its files on the calling thread, where Python uses `asyncio.to_thread`.
- The cold-read launch and report read the page and the evidence file on the calling thread, where Python uses
  `asyncio.to_thread`.
- The Rust `Store` replaces Python's `TaskTreeManager`.
- The memory contribution (`src/features/memory/turn_contribution.py`) is not ported, so no rule source here emits
  `SCOPE_MEMORY`.

## Tests

`tests/prompt_snapshot_golden.rs` compares the recorded 18-case golden matrix (`fixtures/prompt_snapshot_golden.json`,
recorded from the deleted Python reference) byte for byte, with each `prompt_hash` and `char_count`, and the
damaged-snapshot error texts. `tests/task_prompts.rs` ports the task-tree and run-context cases of the Python tests over
a temporary store; the staged-memory case is not ported. Unit tests cover the verify trailer, the worker prompt loader,
the review steps and the snapshot dedup and hash.

`tests/turn_golden.rs` compares the recorded twelve scripted streams (`fixtures/turn_golden.json`) through the
Rust queue after normalizing event ids, timestamps, and elapsed timing fields. It starts only synthetic `/bin/sh`
backends.

`recovery.rs` unit tests cover the review-owed rule
(`a_review_is_owed_only_to_the_newest_unfollowed_work_run_of_an_open_task`), the closed-task stop id, and the
end-landing retry rounds. `tests/recovery.rs` ports the manager-turn MASTER_DONE out-of-space case over the store's
write-fault seam (`write-faults` feature) and a live `/bin/sh` turn, with the retry interval replaced through the
`test-hooks` feature. The other Python startup-recovery cases in `tests/test_task_recovery.py`,
`tests/test_task_execution.py`, `tests/test_task_completion.py`, `tests/test_task_prompt_change_durable.py`,
`tests/test_home_writer_fence.py` and the startup cases of `tests/test_task_sequences.py` are not ported yet.
The cases that replay a named follow-up above (failed-run report, review spawn) need that replay
before they can pass, and the rest need a store fixture with live run processes.

`tests/delegate.rs` ports the artifact check's cold-read delegation cases and the session-message relay cases of
`tests/test_agent_message.py`. `tests/worker_run.rs` ports the cold-read run cases: the instructions and input, the
report with its attempts, the empty answer, the next-backend retry and the all-failed archive. `tests/recovery.rs`
ports the five cold-read replay cases. Each carries its `// Python:` comment.

`tests/worker.rs` ports the cases of `tests/test_worker_account_relay.py`, `tests/test_worker_backend_build.py`,
`tests/test_worker_diagnostics.py`, `tests/test_worker_event_line.py` and `tests/test_worker_quota_scan.py`, and the
non-Claude build case of `tests/test_backend_hooks.py`. Each test carries its `// Python:` comment. It runs scripted
`/bin/sh` processes, which the process-group guard kills when a test ends, and it starts no agent CLI. All twenty-one
ported cases pass. The resume case of `src/runtime/worker.py:357-372` has no Python test: a fake agent writes a result
and stays alive, and the case checks the diagnostics file, the group's exit and the terminal event. Three differ in
how they observe the Python behavior. The falls-back case raises the below-line target's reading from a subscriber on
the thread channel, when the safe point's `user` frame is broadcast. The watch has observed that event by then, and
the move reads the pool only after the process exits, so the reading rises between the gate and the move. The Python
case stubs `claude_accounts.select` instead. The hang-diagnostics case checks the written file's shape rather than its
content, since the runner captures the diagnostics itself. The compaction case checks the events log, not a spy on
`compact_with_sonnet`.
