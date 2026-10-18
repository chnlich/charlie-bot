# `charliebot-store` API

Baseline: Python tree at `d9fdcd7c9`. The store owns writes to session `metadata.json`,
`data/chat_events.jsonl`, task facts, and immutable prompt bodies. It receives the ordered `MetadataSlot` list and all
six required runtime hooks at construction; absent hooks fail startup. Metadata mutation methods read from disk while
holding the session lock, save atomically, then return a fresh read-back. No public whole-object save
accepts a cached metadata value.

## Metadata and listing (S1, implemented)

| Rust API | Python source |
| --- | --- |
| `Store::get_session(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>>` | `src/runtime/session_store.py::SessionStore.get_session` |
| `Store::read_metadata_fresh(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>>` | `src/runtime/session_store.py::SessionStore.read_metadata_fresh` |
| `Store::session_dir(&self, session_id: &str) -> PathBuf`; `Store::metadata_path(&self, session_id: &str) -> PathBuf` | `src/runtime/session_store.py::session_dir`, `metadata_path` |
| `Store::read_thread_metadata(&self, path: &Path) -> StoreResult<Option<ThreadMetadata>>`; `Store::write_thread_metadata(&self, path: &Path, metadata: &ThreadMetadata) -> StoreResult<()>` | `src/infra/models.py::ThreadMetadata`; legacy thread layout in `src/runtime/runs.py` |
| `Store::persist_cc_session_id(&self, session_id: &str, cc_session_id: &str, native_backend: Option<&str>) -> StoreResult<Option<String>>` | `src/runtime/session_anchors.py::persist_cc_session_id` |
| `Store::persist_native_backend(&self, session_id: &str, native_backend: &str) -> StoreResult<Option<String>>` | `src/runtime/session_anchors.py::persist_native_backend` |
| `Store::persist_native_anchor_provenance(&self, session_id: &str, prompt_hash: &str, native_backend: &str, model: Option<&str>) -> StoreResult<()>` | `src/runtime/session_anchors.py::persist_native_anchor_provenance` |
| `Store::clear_cc_session_anchor(&self, session_id: &str) -> StoreResult<()>` | `src/runtime/session_anchors.py::clear_cc_session_anchor` |
| `Store::persist_account_label(&self, session_id: &str, label: &str) -> StoreResult<Option<String>>` | `src/runtime/session_anchors.py::persist_account_label`; `src/runtime/hooks/backend_types.py::record_account_label`, `account_keeper` |
| `Store::context_state(&self, session_id: &str) -> StoreResult<(Option<i64>, Option<UtcDateTime>)>` | `src/runtime/session_anchors.py::context_state`; the fold tiers of `src/runtime/session_usage.py` |
| `Store::update_thinking_state(&self, session_id: &str, updated_at: UtcDateTime) -> StoreResult<()>` | `src/runtime/session_anchors.py::update_thinking_state`; `src/runtime/session_store.py::save_field_fresh` |
| `Store::mark_unread(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>>` | `src/runtime/session_lifecycle.py::mark_unread`, `_set_unread_flag` |
| `Store::rename_session(&self, session_id: &str, name: &str) -> StoreResult<Option<SessionMetadata>>` | `src/runtime/session_lifecycle.py::rename_session` |
| `Store::set_group(&self, session_id: &str, group: Option<&str>) -> StoreResult<Option<SessionMetadata>>` | `src/runtime/session_lifecycle.py::set_group` |
| `Store::switch_backend(&self, session_id: &str, backend: &str) -> StoreResult<Option<SessionMetadata>>` | `src/runtime/session_lifecycle.py::switch_backend` |
| `Store::archive_session(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>>` | `src/runtime/session_lifecycle.py::archive_session` |
| `Store::unarchive_session(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>>` | `src/runtime/session_lifecycle.py::unarchive_session` |
| `Store::update_slot_fields(&self, session_id: &str, owner: &str, values: Map<String, Value>) -> StoreResult<Option<SessionMetadata>>` | `src/features/chat_threads/thread_entry.py` slot updates; `src/infra/metadata_slots.py::set_fields`; `src/runtime/session_store.py::save_metadata` |
| `Store::update_slot_fields_locked(&self, guard: &ControlGuard, session_id: &str, owner: &str, values: Map<String, Value>) -> StoreResult<Option<SessionMetadata>>` | `src/runtime/task_sessions.py::update_slot_fields`; `src/infra/metadata_slots.py::set_fields`; `src/runtime/session_store.py::save_metadata` |
| `Store::load_session_metas(&self, status: Option<SessionStatus>) -> StoreResult<Vec<SessionMetadata>>` | `src/runtime/session_store.py::load_session_metas` |
| `Store::list_sessions(&self, status: Option<SessionStatus>, starred: Option<bool>, scheduled: Option<bool>) -> StoreResult<MetadataListing>` | `src/runtime/session_listing.py::list_sessions`, `list_sessions_readonly` |
| `Store::list_sessions_readonly(&self, status: SessionStatus) -> StoreResult<Vec<SessionMetadata>>` | `src/runtime/session_listing.py::list_sessions_readonly` |
| `Store::list_group_names(&self) -> StoreResult<Vec<String>>` | `src/runtime/session_listing.py::list_group_names` |
| `Store::sequence_subtree_roots(&self) -> StoreResult<HashMap<String, String>>` | `src/runtime/session_listing.py::sequence_subtree_roots`; `src/runtime/scheduled_sessions.py::sequence_subtree_roots` |
| `Store::view_subtree_roots(&self) -> StoreResult<HashMap<String, HashMap<String, String>>>` | `src/runtime/session_listing.py::view_subtree_roots`; `src/runtime/scheduled_sessions.py::view_subtree_roots` |
| `Store::list_archived_page(&self, group: Option<&str>, limit: usize, before: Option<&str>, before_id: Option<&str>) -> StoreResult<ArchivedPage>` | `src/runtime/session_listing.py::list_archived_page` |
| `Store::archived_context_rows(&self, rows: &[SessionMetadata]) -> StoreResult<Vec<SessionMetadata>>` | `src/runtime/session_listing.py::archived_context_rows` |
| `Store::raw_metadata_scan(&self) -> StoreResult<Vec<RawMetadata>>` | `src/features/usage/storage_cool.py` raw metadata scan |

The `*_at` variants of rename, group, backend switch, archive, unarchive, and native-id persistence
take an explicit `UtcDateTime`. Production wrappers supply the current time; the explicit forms make
cross-language replay deterministic.

Metadata slots are serialized in Python registration order and placement order. An unregistered key
remains in `extra` and is emitted after registered slots. Metadata floats use pydantic's form (`1e16`,
`0.00001`), which differs from `repr` (`1e+16`, `1e-05`); event-log floats use `repr`, as `json.dumps` does. `SessionMetadata` writes omit the Python
transient field set. Atomic metadata writes use a sibling temporary file followed by rename, matching
the Python writer's no-fsync behavior.

## Chat event log (S1, implemented)

| Rust API | Python source |
| --- | --- |
| `Store::chat_events_path(&self, session_id: &str) -> PathBuf` | `src/runtime/chat_events.py::chat_events_path`, `ChatEventStore.get_chat_events_path` |
| `Store::append_chat_event(&self, session_id: &str, event: Value) -> StoreResult<ChatEvent>` | `src/runtime/chat_events.py::ChatEventStore.save_chat_event`; `src/infra/ndjson.py::append_ndjson` |
| `Store::load_chat_events_sync(&self, session_id: &str) -> StoreResult<Vec<ChatEvent>>` | `src/runtime/chat_events.py::ChatEventStore.load_chat_events_sync`; `src/infra/ndjson.py::parse_ndjson_file` |
| `Store::load_all_facts(&self, session_id: &str) -> StoreResult<Vec<ChatEvent>>` | `src/runtime/task_sessions.py::fact_history` (archived segments plus live log) |
| `Store::load_chat_events_range(&self, session_id: &str, start: usize, end: usize) -> StoreResult<(Vec<ChatEvent>, bool)>` | `src/runtime/chat_events.py::ChatEventStore.load_chat_events_range` |
| `Store::find_event_by_id(&self, session_id: &str, event_id: &str) -> StoreResult<Option<ChatEvent>>` | `src/runtime/session_dispatch.py` replay lookup |
| `Store::get_chat_event_count_sync(&self, session_id: &str) -> StoreResult<usize>` | `src/runtime/chat_events.py::get_chat_event_count_sync`; `src/infra/ndjson.py::count_ndjson_lines` |
| `Store::archive_events_before(&self, session_id: &str, cutoff: UtcDateTime) -> StoreResult<Option<PathBuf>>` | `src/runtime/chat_events.py::archive_old_chat_events_sync`; `src/runtime/session_lifecycle.py::recycle_history_before` |
| `Store::append_control_fact(&self, guard: &ControlGuard, session_id: &str, event: Value) -> StoreResult<ChatEvent>` | `src/runtime/control_sink.py::ControlEventSink.append` |
| `Store::control_lock(&self) -> ControlGuard` | `src/runtime/task_sessions.py::control_lock` |

Append uses Python's default JSON separators with `ensure_ascii=False`, appends `id` then an
`isoformat()` timestamp if absent, writes one newline-terminated record, and syncs event data before
returning. Readers skip blank and malformed lines, replace invalid UTF-8, and parse a partial final
line. The control guard is store-bound, non-cloneable, and required by locked operations.

`context_state` returns the context tokens of the claude, snapshot, and persisted `context_reading` tiers.
Python's `resolve_session_usage` asks the backend's own usage resolver first (codex's native rollout), and
that tier is not ported in S1. `load_all_facts` composes like Python's `fact_history`: the archived
count is the total event count minus the live event count, the archived events are the range read
from 0 to that count, and archive files beyond that count stay unread.

## Run records (S2, implemented)

| Rust API (implemented) | Python source |
| --- | --- |
| `register_run_locked(&self, guard: &ControlGuard, session_id: &str, run: RunRecord, task_spec: Option<&str>) -> StoreResult<RunRecord>` | `src/runtime/runs.py::register_run_locked` |
| `create_retry_run_locked(&self, guard: &ControlGuard, session_id: &str, request_id: &str, original_run_id: &str, task_spec_text: Option<&str>, fields: RetryRunFields) -> StoreResult<RunRecord>` | `src/runtime/runs.py::create_retry_run_locked`; caller `src/runtime/task_sessions.py::TaskTreeManager.create_retry` |
| `record_launch(&self, session_id: &str, run_id: &str, pid: u32, pid_start: &str) -> StoreResult<RunRecord>` | `src/runtime/runs.py::record_launch` |
| `record_observation(&self, session_id: &str, run_id: &str, fields: Map<String, Value>) -> StoreResult<RunRecord>` | `src/runtime/runs.py::record_observation` |
| `record_finish_locked(&self, guard: &ControlGuard, session_id: &str, run_id: &str, outcome: RunOutcome) -> StoreResult<RunRecord>` | `src/runtime/runs.py::record_finish_locked` |
| `request_stop(&self, session_id: &str, run_id: &str, reason: &str) -> StoreResult<StopRequest>` | `src/runtime/runs.py::request_stop` |
| `reconcile_stop_request(&self, session_id: &str, run_id: &str) -> StoreResult<StopResult>` | `src/runtime/runs.py::reconcile_stop_request` |
| `claim_input_events(&self, session_id: &str, run_id: &str, event_ids: &[String]) -> StoreResult<RunRecord>` | `src/runtime/session_dispatch.py` run-record `input_event_ids` write |
| `read_run_sync(&self, session_id: &str, run_id: &str) -> StoreResult<Option<RunRecord>>`; `list_run_records_sync(&self, session_id: &str) -> StoreResult<Vec<RunRecord>>` | `src/runtime/runs.py::read_run_sync`, `list_run_records_sync` |
| `list_runs_page_sync(&self, session_id: &str, cursor: Option<&str>, limit: usize, order: RunOrder) -> StoreResult<RunPage>` | `src/runtime/runs.py::list_runs_page_sync` |
| `newest_run_record_sync(&self, session_id: &str) -> StoreResult<Option<RunRecord>>`; `last_ended_run_id(&self, session_id: &str) -> StoreResult<Option<String>>` | `src/runtime/runs.py::newest_run_record_sync`, `last_ended_run_id` |
| `run_records::terminal_outcome(events: &[Value], run_id: &str) -> StoreResult<Option<RunOutcomeKind>>` | `src/runtime/runs.py::terminal_outcome_in_events`, `RunStore.terminal_outcome` |
| `run_display_state(&self, session_id: &str, run_id: &str) -> StoreResult<RunDisplayState>`; `run_blockers(&self, session_id: &str) -> StoreResult<Vec<RunBlocker>>` | `src/runtime/runs.py::run_display_state`, `run_blocker`; collected by `src/runtime/task_sessions.py::_structural_blockers` and `_elone_blockers` |
| `records_generation(&self) -> u64`; `mark_sidebar_dirty(&self, session_id: &str)` | `src/runtime/runs.py::records_generation`, `mark_sidebar_dirty` |

Run records use Python's exact field order and reject unknown fields. Their JSON writes are atomic.
`RunOrder::Descending` reads the `(started_at, id)` order backwards. A page cursor carries its order (`d` is set
only when descending), and a cursor read under the other order is refused with Python's `ValueError` text.
`register_run_locked`, retry creation, finish and `claim_input_events_locked` require `ControlGuard`.
`claim_input_events` acquires the guard for callers that do not hold it. Control facts append through
`append_control_fact` while the guard is held. A finish fact lands before its record mirror.

Registration pins `task_spec` only when it is `Some`: the body lands in `task_spec.md` before the record,
which then carries `task_spec_ref` and `task_spec_hash`. A `None` spec writes neither field. Retry creation
takes `RetryRunFields` (`kind`, `backend`, `model`, `repo_path`, `base_branch`, `branch_name`,
`worktree_path`, `sequence_ref`, `review_of_run_id`, `input_event_ids`) and goes through the same registration,
so a replay of one request returns the first record unchanged.

`RunOutcome` carries the terminal outcome, an optional input list, an optional exit code and an
optional end time. Repeated finish calls keep the first fact's payload and repair a missing record mirror.
A run's terminal outcome is its last `run_finished` fact's outcome, as Python's `terminal_outcome_in_events`
reads it. The display state, the blockers, the stop requests and the finish path read it through
`terminal_outcome`; the run-identity refusal asks only whether any finish fact exists, as Python's does.
Stop requests append their fact under the guard. The process identity check, SIGTERM and bounded
exit wait run after the guard is released.

The store exports the legacy and v2 run-layout names from `src/runtime/runs.py`. It provides
read-only path scans for legacy master logs, legacy thread event logs and v2 run raw logs, plus a
scan of all run records. `run_identity_refusal` reads the run record and full fact history before a
route accepts run-token identity. `records_generation` increments after each run-record write.
`mark_sidebar_dirty` updates the process-local sidebar dirty set and its derived generation.

## Task tree (S3, implemented)

| Rust API (implemented) | Python source |
| --- | --- |
| `create_task(&self, request: CreateTask, caller: TaskCaller, slot_values: Map<String, Value>) -> StoreResult<SessionMetadata>` | `src/runtime/task_sessions.py::create_task` |
| `runtime_gate(&self, request: &CreateTask, caller: &TaskCaller) -> StoreResult<()>` | `src/runtime/task_sessions.py` create-task runtime gate |
| `set_node_backend_locked(&self, guard: &ControlGuard, session_id: &str, backend: &str) -> StoreResult<()>` | `src/runtime/task_sessions.py::set_node_backend` |
| `adopt_metadata_slot_locked(&self, guard: &ControlGuard, session_id: &str, old: &SessionMetadata, owner: &str, fields: Option<&[&str]>) -> StoreResult<SessionMetadata>` | `src/runtime/task_sessions.py::adopt_metadata_slot` |
| `record_native_anchor(&self, session_id: &str, prompt_hash: &str, backend: &str, model: Option<&str>) -> StoreResult<()>`; `record_native_anchor_with_reset(&self, session_id: &str, prompt_hash: &str, backend: &str, model: Option<&str>, reset_anchor: bool) -> StoreResult<()>` | `src/runtime/task_sessions.py::record_native_anchor` |
| `archive_unsuccessful_worker(&self, guard: &ControlGuard, session_id: &str, run_id: &str, outcome: &str) -> StoreResult<bool>` | `src/runtime/task_sessions.py::archive_unsuccessful_worker` |
| `archive_subtree(&self, session_id: &str, caller: TaskCaller) -> StoreResult<Vec<String>>` | `src/runtime/task_sessions.py::archive_subtree` |
| `ensure_prompt_changed_fact(&self, session_id: &str, meta: &SessionMetadata, scope: PromptScope) -> StoreResult<()>` | `src/runtime/task_sessions.py::_ensure_prompt_changed_fact` |
| `load_meta(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>>`; `load_task_meta(&self, session_id: &str) -> StoreResult<SessionMetadata>` | `src/runtime/task_sessions.py::load_meta`, `load_task_meta` |
| `task_state(&self, session_id: &str) -> StoreResult<TaskState>`; `facts_of(&self, session_id: &str) -> StoreResult<TaskFacts>` | `src/runtime/task_sessions.py::task_state`, `facts_of` |
| `archived_of(&self, session_id: &str) -> StoreResult<bool>`; `check_task_authorization(&self, session_id: &str) -> StoreResult<String>` | `src/runtime/task_sessions.py::archived_of`, `check_task_authorization` |
| `ancestor_ids(&self, session_id: &str) -> StoreResult<Vec<String>>`; `descendant_ids(&self, session_id: &str) -> StoreResult<Vec<String>>` | `src/runtime/task_sessions.py::ancestor_ids`, `descendant_ids` |
| `resolve_successor_chain(&self, session_id: &str) -> StoreResult<Vec<SessionMetadata>>`; `deliver_to_successor(&self, session_id: &str, event: ChatEvent) -> StoreResult<Option<String>>` | `src/runtime/session_successor.py::resolve_successor_chain`, `deliver_to_successor` |
| `invalidate_tree_index(&self)`; `require_open_ancestry(&self, session_id: &str) -> StoreResult<Vec<String>>` | `src/runtime/task_sessions.py::invalidate_tree_index`, `_require_open_ancestry` |
| `child_ids(&self, session_id: &str) -> StoreResult<Vec<String>>`; `derived_archived_ids(&self) -> StoreResult<Vec<String>>` | `src/runtime/task_sessions.py::_children_of`, `derived_archived_ids` |
| `task_tree_activity(&self, session_id: &str) -> StoreResult<TaskTreeActivity>` | `src/runtime/task_sessions.py::task_tree_activity` |
| `store_prompt_body(&self, body: &str) -> StoreResult<String>`; `prompt_bodies_dir(&self) -> PathBuf` | `src/runtime/task_sessions.py::prompt_bodies_dir`, `_apply_prompt_change` |
| `rebind_elone_bindings_locked(&self, guard: &ControlGuard, source_session_id: &str, new_session_id: &str) -> StoreResult<bool>` | `src/runtime/task_sessions.py::_rebind_scheduled_tasks` |

`TaskFacts` is the durable event fold: task state, run outcomes and first finish positions, input boundaries and
acknowledgements, close facts, delivered child reports, and the event-id index all come from `load_all_facts`.
`check_task_authorization` returns the nearest user-message node that authorizes delegation; on refusal its
`StoreError` text matches `takeoff_gate.py`. `archive_unsuccessful_worker` returns whether it appended the archive fact.
These return shapes follow the Python methods; the earlier planned table used provisional types for those entries.

Tree-node metadata writes and control-fact writes run under the shared control guard. The locked methods verify its
store identity, and each metadata write re-reads its disk row before mutation. Tree reads use a generation-invalidated,
two-second index. Prompt bodies are immutable files under `prompt_bodies/<sha256>.md`; prompt-change facts point to
those hashes. The store calls required sequence archive and binding-mover hooks for archive, subtree archive, and
elone rebind. `archive_subtree` accepts only `TaskCaller::Operator`, and it takes the control lock itself.
The reset-aware native-anchor method clears the held conversation id and timestamp after persisting provenance when a
launch starts a fresh native context; the planned four-argument method is the continuing-context form.

## Task errors (S3)

Every task-tree refusal is a `StoreError` variant that names the Python class its route maps. The routes set the HTTP
status from the class (`src/runtime/api/sessions.py::_task_http_error`), so the variant decides the status.

| Python class | Rust variant | HTTP | Store producers |
| --- | --- | --- | --- |
| `task_errors.TaskInvalidError` | `StoreError::invalid` (`Invalid`) | 400 | `create_task`: a missing request id, a worker parent |
| `task_errors.TaskNotFoundError`; `runs.RunNotFoundError` | `StoreError::not_found` (`NotFound`) | 404 | `load_task_meta` and every tree read of an absent node; `archive_subtree`; the locked slot, backend, adopt and rebind writes of an absent session; `require_run` |
| `task_errors.TaskForbiddenError`; `takeoff_gate.DelegationBlockedError` | `StoreError::forbidden` (`Forbidden`) | 403 | an agent's create outside its own open manager task (`runtime_gate`); every `check_task_authorization` refusal; `archive_subtree` for a caller that is not the operator |
| `task_errors.TaskArchivedError` | `StoreError::archived` (`Archived`) | 409, the sentence alone | none in the store yet: machine input to an archived node is refused by the dispatch admission (`session_dispatch.admit_input`) |
| `task_errors.TaskConflictError` | `StoreError::conflict` (`Conflict`) | 409, `{message, blockers}` | a closed parent or ancestor (including a create under an archived parent); a relation cycle or hop limit; `archive_subtree`'s unfinished runs |
| `runs.RunIdentityConflictError` | `StoreError::conflict` with one blocker | 409, `{message, blockers}` | the stop path's process identity checks (`follow_through_stop`) |
| none: I/O, serialization, corrupt state, hook and invariant errors | `StoreError::new` (`Failure`) | none | every other error |

`Display` gives the Python text: a conflict joins its blockers with `; `, and an archived refusal reads
`task <id> is archived`. `blockers()` returns a conflict's list and `None` for every other variant. Python's
`TaskArchivedError` is a `TaskConflictError` subclass, but its route answers the sentence alone, so the Rust archived
variant carries no list. A refusal from `check_task_authorization` is `forbidden` on every path, as
`DelegationBlockedError` is a 403 on every route.

`create_task` without a name takes `Session N` from `sessions/.counter`. An unreadable or unparsable counter takes
the session-directory count and is then rewritten with the next number, as Python's `_next_session_name` does.

## Runtime event bus, message model, and busy state (S4, implemented)

| Rust API | Python source |
| --- | --- |
| `prime_aggregator(&self, session_id: &str) -> StoreResult<u64>`; `announce_appended_event(&self, session_id: &str, event: &ChatEvent, epoch: u64) -> StoreResult<()>` | `src/runtime/session_events.py::prime_aggregator`, `announce_appended_event` |
| `persist_and_broadcast(&self, session_id: &str, event: ChatEvent) -> StoreResult<ChatEvent>`; `drop_session_runtime_state(&self, session_id: &str)` | `src/runtime/session_events.py::persist_and_broadcast`, `drop_session_runtime_state` |
| `subscribe(&self, channel: &str) -> Subscription`; `unsubscribe(&self, subscription: &mut Subscription)`; `broadcast(&self, channel: &str, frame: Value) -> StoreResult<()>` | `src/runtime/streaming.py::StreamingManager.subscribe`, `unsubscribe`, `broadcast` |
| `session_channel(session_id: &str) -> String`; `SIDEBAR_CHANNEL`; `STREAM_COALESCE_INTERVAL` | `src/runtime/streaming.py` session/sidebar channels and `_STREAM_COALESCE_INTERVAL` |
| `broadcast_only(&self, session_id: &str, frame: Value) -> StoreResult<()>`; `broadcast_sidebar(&self, session_id: &str, event_type: &str, fields: Map<String, Value>) -> StoreResult<()>`; `broadcast_task_tree_changed(&self, session_id: &str, event_type: Option<&str>) -> StoreResult<()>` | `src/runtime/session_events.py::broadcast_only`, `broadcast_sidebar`, `broadcast_task_tree_changed` |
| `load_chat_events_tail(&self, session_id: &str, limit: usize) -> StoreResult<(Vec<ChatEvent>, usize, bool)>`; `load_chat_events_range(&self, session_id: &str, start: usize, end: usize) -> StoreResult<(Vec<ChatEvent>, bool)>`; `get_chat_event_count_sync(&self, session_id: &str) -> StoreResult<usize>` | `src/runtime/session_events.py` event count, tail, and range reads; `src/runtime/chat_events.py` |
| `get_message_projection(&self, session_id: &str) -> StoreResult<Option<Arc<MessageProjection>>>`; `projection_memo_hit(&self, session_id: &str) -> Option<Arc<MessageProjection>>` | `src/runtime/session_events.py` message projection memo |
| `MessageAggregator::{new, with_renderers, feed, feed_indexed, flush_pending, pending_draft_message}` | `src/runtime/message_aggregator.py::MessageAggregator` |
| `MessageProjection::{new, advanced, history, committed, pending_draft, tail, slice_before}` and bounded page-body cache methods | `src/runtime/message_projection.py::MessageProjection` |
| `RuntimeEventHooks`, `EventRenderer`, and `AfterTurnWork` supplied in `StoreHooks.runtime_events` | `src/runtime/hooks/turn_contributions.py`; feature `event_renderers` and `after_turn` |
| `mark_busy(&self, session_id: &str, since: UtcDateTime) -> StoreResult<(UtcDateTime, bool)>`; `clear_busy(&self, session_id: &str)`; `busy_since(&self, session_id: &str) -> Option<UtcDateTime>` | `src/runtime/thinking_state.py::mark_busy`, `clear_busy`, `busy_since` |
| `mark_run_busy(&self, session_id: &str, run_id: &str, since: UtcDateTime)`; `clear_run_busy(&self, session_id: &str, run_id: &str)`; `note_run_backend(&self, session_id: &str, backend: &str)`; `run_backend(&self, session_id: &str) -> Option<String>` | `src/runtime/thinking_state.py::mark_run_busy`, `clear_run_busy`, `note_run_backend`, `run_backend` |
| `build_scheduled_trigger_event(content: &str) -> ChatEvent`; `build_agent_message_event(&self, session_id: &str, text: &str, provenance: MessageProvenance) -> ChatEvent`; `master_done_input_event_ids(&self, event: &ChatEvent) -> Vec<String>`; `events_to_messages(&self, events: &[ChatEvent]) -> StoreResult<Vec<Value>>`; `events_to_view(&self, events: &[ChatEvent]) -> StoreResult<(Vec<Value>, Option<Value>)>` | `src/runtime/api/message_utils.py::build_scheduled_trigger_event`, `build_agent_message_event`, `master_done_input_event_ids`, `events_to_messages`, `events_to_view` |
| `require_session(&self, session_id: &str) -> StoreResult<SessionMetadata>`; `SESSION_NOT_FOUND_DETAIL` | `src/runtime/api/deps.py::require_session` (404 detail `Session not found`) |
| `handle_compaction_event(&self, session_id: &str, event: &Value) -> StoreResult<Option<ChatEvent>>` | `src/runtime/streaming.py::handle_compaction_events` |
| `normalize_user_message_event`, `serialize_uploaded_files`, `strip_attached_files_block`, `build_agent_input_content`, `stable_history_projection`, `stable_closed_prefix_len` | `src/runtime/message_events.py` |

`persist_and_broadcast` feeds the aggregator after the durable append, broadcasts the event, and
starts each registered turn contribution's `after_turn` work for `master_done`. Stream deltas are
coalesced per channel over 200 ms, with a leading frame and the latest trailing frame; message and
error frames discard a pending preview. The control-sink two-phase path primes before the tree lock,
appends under the lock, then announces after release only when its epoch still matches. Feature event
renderers and after-turn work enter through `StoreHooks.runtime_events`; they are not mutable store hooks.

## Not store-owned

The feature request's plan files, explain results, voice files, host-auth data, usage ledger, trace
cache, models, backup, trigger files, worktrees, and transport files remain owned by their feature or
runtime components. `charliebot-store` supplies metadata and chat-event access in S1 through S3 and the event,
message, projection, and busy-state model in S4. Usage storage's raw metadata scan is read-only and is
exposed as `raw_metadata_scan`.

## Known divergences from Python

- Non-UTC offsets in chat-event timestamps normalize to UTC on read and write. Python's writers
  produce only UTC, so no file the Python server writes carries another offset.
- Slot values are not type-checked on write. `MetadataSlot` carries defaults only, so a typed write
  refusal like `metadata_slots.set_fields`'s pydantic validation lives with the caller.
- Archive rotation has no lock against a concurrent append, so an append racing
  `archive_events_before` can land in either the old or the new live file. Python has the same
  window.
- The session counter parses a signed 64-bit integer. Python's `int()` also accepts digit underscores and larger
  values; here such a counter takes the session-directory count.
- The task-tree writes (`create_task`, `archive_unsuccessful_worker`, `archive_subtree`) do not send Python's
  best-effort task-tree-changed notification after each control fact. The sidebar task owns that wiring.
