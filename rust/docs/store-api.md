# `charliebot-store` API

Baseline: Python tree at `d9fdcd7c9`. The store owns writes to session `metadata.json` and
`data/chat_events.jsonl`. It receives the ordered `MetadataSlot` list and all three required runtime
hooks at construction; absent hooks fail startup. Metadata mutation methods read from disk while
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
that tier is not ported in S1. `load_all_facts` reads every `chat_events.*.jsonl` archive file; Python's
`fact_history` reads the first `archive_offset` archived events.

## Run records (S2, implemented)

| Rust API (implemented) | Python source |
| --- | --- |
| `register_run_locked(&self, guard: &ControlGuard, session_id: &str, run: RunRecord, task_spec: &str) -> StoreResult<RunRecord>` | `src/runtime/runs.py::register_run_locked` |
| `create_retry_run_locked(&self, guard: &ControlGuard, session_id: &str, request_id: &str, previous: &RunRecord) -> StoreResult<RunRecord>` | `src/runtime/runs.py::create_retry_run_locked` |
| `record_launch(&self, session_id: &str, run_id: &str, pid: u32, pid_start: &str) -> StoreResult<RunRecord>` | `src/runtime/runs.py::record_launch` |
| `record_observation(&self, session_id: &str, run_id: &str, fields: Map<String, Value>) -> StoreResult<RunRecord>` | `src/runtime/runs.py::record_observation` |
| `record_finish_locked(&self, guard: &ControlGuard, session_id: &str, run_id: &str, outcome: RunOutcome) -> StoreResult<RunRecord>` | `src/runtime/runs.py::record_finish_locked` |
| `request_stop(&self, session_id: &str, run_id: &str, reason: &str) -> StoreResult<StopRequest>` | `src/runtime/runs.py::request_stop` |
| `reconcile_stop_request(&self, session_id: &str, run_id: &str) -> StoreResult<StopResult>` | `src/runtime/runs.py::reconcile_stop_request` |
| `claim_input_events(&self, session_id: &str, run_id: &str, event_ids: &[String]) -> StoreResult<RunRecord>` | `src/runtime/session_dispatch.py` run-record `input_event_ids` write |
| `read_run_sync(&self, session_id: &str, run_id: &str) -> StoreResult<Option<RunRecord>>`; `list_run_records_sync(&self, session_id: &str) -> StoreResult<Vec<RunRecord>>` | `src/runtime/runs.py::read_run_sync`, `list_run_records_sync` |
| `list_runs_page_sync(&self, session_id: &str, cursor: Option<&str>, limit: usize) -> StoreResult<RunPage>` | `src/runtime/runs.py::list_runs_page_sync` |
| `newest_run_record_sync(&self, session_id: &str) -> StoreResult<Option<RunRecord>>`; `last_ended_run_id(&self, session_id: &str) -> StoreResult<Option<String>>` | `src/runtime/runs.py::newest_run_record_sync`, `last_ended_run_id` |
| `run_display_state(&self, session_id: &str, run_id: &str) -> StoreResult<RunDisplayState>`; `run_blocker(&self, session_id: &str) -> StoreResult<Option<RunBlocker>>` | `src/runtime/runs.py::run_display_state`, `run_blocker` |
| `records_generation(&self) -> u64`; `mark_sidebar_dirty(&self, session_id: &str)` | `src/runtime/runs.py::records_generation`, `mark_sidebar_dirty` |

Run records use Python's exact field order and reject unknown fields. Their JSON writes are atomic.
`register_run_locked`, retry creation, finish and `claim_input_events_locked` require `ControlGuard`.
`claim_input_events` acquires the guard for callers that do not hold it. Control facts append through
`append_control_fact` while the guard is held. A finish fact lands before its record mirror.

`RunOutcome` carries the terminal outcome, an optional input list, an optional exit code and an
optional end time. Repeated finish calls keep the first fact and repair a missing record mirror.
Stop requests append their fact under the guard. The process identity check, SIGTERM and bounded
exit wait run after the guard is released.

The store exports the legacy and v2 run-layout names from `src/runtime/runs.py`. It provides
read-only path scans for legacy master logs, legacy thread event logs and v2 run raw logs, plus a
scan of all run records. `run_identity_refusal` reads the run record and full fact history before a
route accepts run-token identity. `records_generation` increments after each run-record write.
`mark_sidebar_dirty` updates the process-local sidebar dirty set and its derived generation.

## Planned task tree (S3)

| Rust API planned | Python source |
| --- | --- |
| `create_task(&self, request: CreateTask, caller: TaskCaller, slot_values: Map<String, Value>) -> StoreResult<SessionMetadata>` | `src/runtime/task_sessions.py::create_task` |
| `runtime_gate(&self, request: &CreateTask, caller: &TaskCaller) -> StoreResult<()>` | `src/runtime/task_sessions.py` create-task runtime gate |
| `set_node_backend_locked(&self, guard: &ControlGuard, session_id: &str, backend: &str) -> StoreResult<()>` | `src/runtime/task_sessions.py::set_node_backend` |
| `adopt_metadata_slot_locked(&self, guard: &ControlGuard, session_id: &str, old: &SessionMetadata, owner: &str, fields: Option<&[&str]>) -> StoreResult<SessionMetadata>` | `src/runtime/task_sessions.py::adopt_metadata_slot` |
| `record_native_anchor(&self, session_id: &str, prompt_hash: &str, backend: &str, model: Option<&str>) -> StoreResult<()>` | `src/runtime/task_sessions.py::record_native_anchor` |
| `archive_unsuccessful_worker(&self, guard: &ControlGuard, session_id: &str, run_id: &str, outcome: &str) -> StoreResult<()>` | `src/runtime/task_sessions.py::archive_unsuccessful_worker` |
| `ensure_prompt_changed_fact(&self, session_id: &str, meta: &SessionMetadata, scope: PromptScope) -> StoreResult<()>` | `src/runtime/task_sessions.py::_ensure_prompt_changed_fact` |
| `load_meta(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>>`; `load_task_meta(&self, session_id: &str) -> StoreResult<Option<TaskMetadata>>` | `src/runtime/task_sessions.py::load_meta`, `load_task_meta` |
| `task_state(&self, session_id: &str) -> StoreResult<TaskState>`; `facts_of(&self, session_id: &str) -> StoreResult<Vec<ChatEvent>>` | `src/runtime/task_sessions.py::task_state`, `facts_of` |
| `archived_of(&self, session_id: &str) -> StoreResult<bool>`; `check_task_authorization(&self, session_id: &str, actor: &TaskActor) -> StoreResult<()>` | `src/runtime/task_sessions.py::archived_of`, `check_task_authorization` |
| `ancestor_ids(&self, session_id: &str) -> StoreResult<Vec<String>>`; `descendant_ids(&self, session_id: &str) -> StoreResult<Vec<String>>` | `src/runtime/task_sessions.py::ancestor_ids`, `descendant_ids` |
| `resolve_successor_chain(&self, session_id: &str) -> StoreResult<Vec<SessionMetadata>>`; `deliver_to_successor(&self, session_id: &str, event: ChatEvent) -> StoreResult<Option<String>>` | `src/runtime/session_successor.py::resolve_successor_chain`, `deliver_to_successor` |
| `invalidate_tree_index(&self)`; `require_open_ancestry(&self, session_id: &str) -> StoreResult<()>` | `src/runtime/task_sessions.py::invalidate_tree_index`, `_require_open_ancestry` |

The task-tree index helpers (`_get_index`, `_index_meta`, `_ancestors`, `_children_of`,
`_descendants`) and `task_state` share a cached index owned by the task-tree stage. The S1
`update_slot_fields` entry already requires a guard and performs its own fresh per-session RMW.

## Planned event bus, runtime state, and message helpers (S4)

| Rust API planned | Python source |
| --- | --- |
| `prime_aggregator(&self, session_id: &str) -> StoreResult<u64>`; `announce_appended_event(&self, session_id: &str, event: &ChatEvent, epoch: u64) -> StoreResult<()>` | `src/runtime/session_events.py::prime_aggregator`, `announce_appended_event` |
| `persist_and_broadcast(&self, session_id: &str, event: ChatEvent) -> StoreResult<ChatEvent>` | `src/runtime/session_events.py::persist_and_broadcast` |
| `broadcast_only(&self, session_id: &str, frame: Value) -> StoreResult<()>`; `broadcast_task_tree_changed(&self, session_id: &str, event_type: Option<&str>) -> StoreResult<()>` | `src/runtime/session_events.py::broadcast_only`, `broadcast_task_tree_changed` |
| `mark_busy(&self, session_id: &str, since: UtcDateTime) -> StoreResult<(UtcDateTime, bool)>`; `clear_busy(&self, session_id: &str)`; `busy_since(&self, session_id: &str) -> Option<UtcDateTime>` | `src/runtime/thinking_state.py::mark_busy`, `clear_busy`, `busy_since` |
| `mark_run_busy(&self, session_id: &str, run_id: &str, since: UtcDateTime)`; `clear_run_busy(&self, session_id: &str, run_id: &str)`; `note_run_backend(&self, session_id: &str, backend: &str)`; `run_backend(&self, session_id: &str) -> Option<String>` | `src/runtime/thinking_state.py::mark_run_busy`, `clear_run_busy`, `note_run_backend`, `run_backend` |
| `task_tree_activity(&self, session_id: &str) -> StoreResult<TaskTreeActivity>` | `src/runtime/task_sessions.py::task_tree_activity` |
| `build_agent_message_event(&self, session_id: &str, text: &str, provenance: MessageProvenance) -> ChatEvent` | `src/runtime/api/message_utils.py::build_agent_message_event` |
| `master_done_input_event_ids(&self, event: &ChatEvent) -> Vec<String>`; `events_to_messages(&self, events: &[ChatEvent]) -> Vec<Message>` | `src/runtime/api/message_utils.py::master_done_input_event_ids`, `events_to_messages` |
| `register_hook(&mut self, hook: StoreHook) -> StoreResult<()>`; `hooks(&self) -> &[StoreHook]` | `src/runtime/hooks/sequence_controllers.py`, `backend_types.py`, and feature hook registries |

`persist_and_broadcast` feeds the aggregator after the durable append, broadcasts the event, and
invokes each turn contribution's `after_turn` for `master_done`. The control-sink two-phase path
primes before the tree lock, appends under the lock, then announces after release only when its
epoch still matches. Neither behavior is part of S1.

## Not store-owned

The feature request's plan files, explain results, voice files, host-auth data, usage ledger, trace
cache, models, backup, trigger files, worktrees, and transport files remain owned by their feature or
runtime components. `charliebot-store` supplies only metadata and chat-event access in S1. Usage
storage's raw metadata scan is read-only and is exposed as `raw_metadata_scan`.
