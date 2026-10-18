# charliebot-tasks

This crate owns manager turns. `MasterQueue` contains the per-session work queues, consumers, current items, active
runners, and the rotating pool used by after-round naming. Each session consumer runs one manager turn at a time,
persists its translated events through `charliebot-store`, and finishes with the unread decision and session anchors.

The execution task inserts `MasterQueue` into typed startup state and wires chat routes. The recovery task wires
shutdown and re-attachment into server lifespan.

## Public API

| Rust item | Python source |
| --- | --- |
| `MasterQueue`, `MasterQueue::new`, and `MasterQueue::with_light_backend_pool` | `src/runtime/master_cc_state.py:11-17`; `src/runtime/autonamer.py:129` |
| `MasterQueue::session_callbacks` and `master_queue::session_callbacks` | `src/infra/models.py:723-744` |
| `SessionCallbacks` and `PersistAndBroadcastFn`, `MarkUnreadFn`, `PersistNativeIdFn`, `PersistAccountLabelFn`, `ContextStateFn`, `TaskTreeActivityFn`, and `AfterRoundFn` | `src/infra/models.py:723-744` |
| `MasterTurnRequest` and its fields | `src/runtime/master_cc_queue.py:54-118` |
| `MasterResumeRequest` and its fields | `src/runtime/master_cc_queue.py:402-444` |
| `MasterQueue::run_message` | `src/runtime/master_cc_queue.py:355-400` |
| `MasterQueue::enqueue_master_resume` | `src/runtime/master_cc_queue.py:402-444` |
| `MasterQueue::queued_user_event_ids` and `master_queue::queued_user_event_ids` | `src/runtime/master_cc_queue.py:446` |
| `MasterQueue::running_user_event_ids` and `master_state::running_user_event_ids` | `src/runtime/master_cc_state.py:58` |
| `MasterQueue::terminate_active` | `src/runtime/master_cc_state.py:17`; `src/runtime/master_cc_run.py:407-702` |
| `MasterQueue::shutdown` | `src/runtime/master_cc_queue.py:186-354` (consumer cancellation at server stop) |
| `TaskRunBinding` and its fields | `src/runtime/master_cc_state.py:21-29` |
| `MasterResumeRecord` and its fields | `src/infra/models.py:337-350` |
| `TaskSpawnCallback`, `TaskFinishCallback`, and `ResumeLiveness` | `src/runtime/master_cc_state.py:32-49` |
| `master_state` module and its work item | `src/runtime/master_cc_state.py:32-49` |
| `master_queue` module | `src/runtime/master_cc_queue.py:54-446` |
| `master_run` module | `src/runtime/master_cc_run.py:36-957` |
| `LightBackendPool::{select, cool, is_quota_error}` and `RotatingLightBackendPool` | `src/runtime/backend_pool.py:82-123` |
| `autonamer::iter_light_backends` | `src/runtime/autonamer.py:129` |
| `autonamer::name_after_round` | `src/runtime/autonamer.py:197` |
| `autonamer::maybe_auto_name` | `src/runtime/autonamer.py:232` |
| `autonamer::is_default_session_name`, `prefix_session_number_if_default`, and `build_groups_clause` | `src/runtime/autonamer.py:53`, `:62`, `:94` |
| `autonamer::parse_name_and_group` and `fuzzy_match_group` | `src/runtime/autonamer.py:108`, `:141` |
| `spawner_backends::unknown_backend_pin_refusal` | `src/runtime/spawner_backends.py:32` |

## Tests

`tests/turn_golden.rs` runs `tests/python_turn_golden.py` through `uv run python`, then compares the same twelve
scripted streams through the Rust queue after normalizing event ids, timestamps, and elapsed timing fields. It starts
only synthetic `/bin/sh` backends.
