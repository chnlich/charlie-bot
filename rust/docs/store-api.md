# `charliebot-store` API

Baseline: Python tree at `4521a51b2`. The store owns writes to session `metadata.json`,
`data/chat_events.jsonl`, task facts, and immutable prompt bodies. It receives the ordered `MetadataSlot` list and all
six required runtime hooks at construction; absent hooks fail startup. Metadata mutation methods read from disk while
holding the session lock, save atomically, then return a fresh read-back. No public whole-object save
accepts a cached metadata value.
Expired active metadata entries are checked by stat outside the cache mutex. Refresh and eviction apply only to the entry
that was checked, so a concurrent write's cached result stays current.

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
| `Store::context_state(&self, session_id: &str) -> StoreResult<(Option<i64>, Option<UtcDateTime>)>` | Legacy mirror of `src/runtime/session_anchors.py::context_state`; its caller moves to `SessionUsage::context_state` on the agent-runtime line |
| `Store::update_thinking_state(&self, session_id: &str, updated_at: UtcDateTime) -> StoreResult<()>` | `src/runtime/session_anchors.py::update_thinking_state`; `src/runtime/session_store.py::save_field_fresh` |
| `Store::mark_unread(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>>` | `src/runtime/session_lifecycle.py::mark_unread`, `_set_unread_flag` |
| `Store::mark_read(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>>` | `src/runtime/session_lifecycle.py::mark_read`, `_set_unread_flag` |
| `Store::rename_session(&self, session_id: &str, name: &str) -> StoreResult<Option<SessionMetadata>>` | `src/runtime/session_lifecycle.py::rename_session` |
| `Store::set_group(&self, session_id: &str, group: Option<&str>) -> StoreResult<Option<SessionMetadata>>` | `src/runtime/session_lifecycle.py::set_group` |
| `Store::star_session(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>>`; `Store::unstar_session(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>>` | `src/runtime/session_lifecycle.py::star_session`, `unstar_session` |
| `Store::rename_group(&self, old_name: &str, new_name: &str) -> StoreResult<usize>`; `Store::delete_group(&self, group: &str) -> StoreResult<usize>` | `src/runtime/session_lifecycle.py::rename_group`, `delete_group` |
| `Store::rate_round(&self, session_id: &str, round_id: &str, rating: Option<RoundRating>) -> StoreResult<Option<SessionMetadata>>` | `src/runtime/api/sessions.py::rate_round` |
| `Store::delete_session_permanently(&self, session_id: &str) -> StoreResult<bool>` | `src/runtime/session_lifecycle.py::delete_session_permanently` |
| `Store::switch_backend(&self, session_id: &str, backend: &str) -> StoreResult<Option<SessionMetadata>>` | `src/runtime/session_lifecycle.py::switch_backend` |
| `Store::archive_session(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>>` | `src/runtime/session_lifecycle.py::archive_session` |
| `Store::unarchive_session(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>>` | `src/runtime/session_lifecycle.py::unarchive_session` |
| `Store::update_slot_fields(&self, session_id: &str, owner: &str, values: Map<String, Value>) -> StoreResult<Option<SessionMetadata>>` | `src/features/chat_threads/thread_entry.py` slot updates; `src/infra/metadata_slots.py::set_fields`; `src/runtime/session_store.py::save_metadata` |
| `Store::update_slot_fields_at(&self, session_id: &str, owner: &str, values: Map<String, Value>, updated_at: UtcDateTime) -> StoreResult<Option<SessionMetadata>>` | `src/features/chat_threads/thread_entry.py::consume_mention`, `ack_messages`; `src/infra/metadata_slots.py::set_fields`; `src/runtime/session_store.py::save_metadata` |
| `Store::update_slot_fields_locked(&self, guard: &ControlGuard, session_id: &str, owner: &str, values: Map<String, Value>) -> StoreResult<Option<SessionMetadata>>` | `src/runtime/task_sessions.py::update_slot_fields`; `src/infra/metadata_slots.py::set_fields`; `src/runtime/session_store.py::save_metadata` |
| `Store::load_session_metas(&self, status: Option<SessionStatus>) -> StoreResult<Vec<SessionMetadata>>` | `src/runtime/session_store.py::load_session_metas` |
| `Store::list_sessions(&self, status: Option<SessionStatus>, starred: Option<bool>, scheduled: Option<bool>) -> StoreResult<MetadataListing>` | `src/runtime/session_listing.py::list_sessions`, `list_sessions_readonly` |
| `Store::list_sessions_readonly(&self, status: SessionStatus) -> StoreResult<Vec<SessionMetadata>>` | `src/runtime/session_listing.py::list_sessions_readonly` |
| `Store::list_group_names(&self) -> StoreResult<Vec<String>>` | `src/runtime/session_listing.py::list_group_names` |
| `Store::sequence_subtree_roots(&self) -> StoreResult<Arc<HashMap<String, String>>>` | `src/runtime/session_listing.py::sequence_subtree_roots`; `src/runtime/scheduled_sessions.py::sequence_subtree_roots` |
| `Store::view_subtree_roots(&self) -> StoreResult<Arc<HashMap<String, HashMap<String, String>>>>` | `src/runtime/session_listing.py::view_subtree_roots`; `src/runtime/scheduled_sessions.py::view_subtree_roots` |
| `Store::list_archived_page(&self, group: Option<&str>, limit: usize, before: Option<&str>, before_id: Option<&str>) -> StoreResult<ArchivedPage>` | `src/runtime/session_listing.py::list_archived_page` |
| `Store::archived_context_rows(&self, rows: &[SessionMetadata]) -> StoreResult<Vec<SessionMetadata>>` | `src/runtime/session_listing.py::archived_context_rows` |
| `trigger_files::iter_trigger_file_stats(triggers_dir: impl AsRef<Path>) -> io::Result<Vec<(PathBuf, Metadata)>>` | `src/runtime/trigger_files.py::iter_trigger_file_stats` |
| `trigger_files::pending_trigger_state(triggers_dir: impl AsRef<Path>) -> (usize, Option<UtcDateTime>)` | `src/runtime/session_sidebar.py:157` |
| `Store::raw_metadata_scan(&self) -> StoreResult<Vec<RawMetadata>>` | `src/features/usage/storage_cool.py` raw metadata scan |

Session listings apply `Store::derived_archived_ids` after stored-status reads. This overlay reads task-tree facts and leaves metadata unchanged. `SequenceListingFields` supplies controller fields and subtree ownership, borrowing the memoized `Arc` rows: `Store::subtree_roots_shared` keys both subtree maps on the corpus snapshot they were built from and hands out the cached `Arc` maps, which is what `sequence_subtree_roots` and `view_subtree_roots` return.

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
| `Store::load_chat_events_shared_sync(&self, session_id: &str) -> StoreResult<Arc<Vec<ChatEvent>>>` | `src/runtime/chat_events.py::ChatEventStore.load_chat_events_sync` |
| `Store::load_chat_events_delta_sync(&self, session_id: &str, known_generation: Option<u64>, covered: usize) -> StoreResult<ChatEventsDelta>` | `src/runtime/session_usage.py` facts memo: read the appended suffix for the same cached list, otherwise rebuild from the replacement list |
| `Store::load_all_facts(&self, session_id: &str) -> StoreResult<Vec<ChatEvent>>` | `src/runtime/task_sessions.py::fact_history` (archived segments plus live log) |
| `Store::load_all_facts_shared_sync(&self, session_id: &str) -> StoreResult<Arc<Vec<ChatEvent>>>` | `src/runtime/task_sessions.py::fact_history` (archived segments plus live log) |
| `Store::load_chat_events_range(&self, session_id: &str, start: usize, end: usize) -> StoreResult<(Vec<ChatEvent>, bool)>` | `src/runtime/chat_events.py::ChatEventStore.load_chat_events_range` |
| `Store::find_event_by_id(&self, session_id: &str, event_id: &str) -> StoreResult<Option<ChatEvent>>` | `src/runtime/session_dispatch.py` replay lookup |
| `Store::get_chat_event_count_sync(&self, session_id: &str) -> StoreResult<usize>` | `src/runtime/chat_events.py::get_chat_event_count_sync`; `src/infra/ndjson.py::count_ndjson_lines` |
| `Store::archive_events_before(&self, session_id: &str, cutoff: UtcDateTime) -> StoreResult<Option<PathBuf>>` (strict UTF-8 decode; error cases below) | `src/runtime/chat_events.py::archive_old_chat_events_sync`; `src/runtime/session_lifecycle.py::recycle_history_before` |
| `Store::append_control_fact(&self, guard: &ControlGuard, session_id: &str, event: Value) -> StoreResult<ChatEvent>` | `src/runtime/control_sink.py::ControlEventSink.append` |
| `Store::control_lock(&self) -> ControlGuard` | `src/runtime/task_sessions.py::control_lock` |

Append uses Python's default JSON separators with `ensure_ascii=False`, appends `id` then an
`isoformat()` timestamp if absent, writes one newline-terminated record, and syncs event data before
returning. Readers skip blank and malformed lines, replace invalid UTF-8, and parse a partial final
line. Each session has its own event-cache mutex; the cache-map mutex is held only to find or create
that entry. Appends and cold reads hold the session mutex across file I/O, so another session's
event operations do not wait on that I/O. A shared read returns the cached `Arc<Vec<ChatEvent>>`;
reads without an intervening append share its allocation, and copy-on-write keeps an older snapshot
unchanged when an append lands. The control guard is store-bound, non-cloneable, and required by
locked operations.

`load_chat_events_range` keeps Python's range indexes: an unarchived cold read selects physical `\n`-delimited lines,
so blank and malformed lines consume indexes; an unarchived warm read slices the parsed-event cache. The
`/api/sessions/{session_id}/events` fallback uses this range API, while projection and other whole-log callers keep
the shared parsed-event read.

`load_chat_events_delta_sync` returns the cached generation, current event count, the suffix start index, and the
events from that index. Appends retain the generation; cache eviction or replacement gives a new generation.
`SessionUsage` uses this to advance its single event fold by the appended suffix. It asks the registered backend
resolver first (Codex's native rollout), then the event tiers. `Store::context_state` remains unchanged until the
agent-runtime caller switches to `SessionUsage::context_state`. `load_all_facts` and
`load_all_facts_shared_sync` compose like Python's `fact_history`: the archived count is the total event count
minus the live event count, the archived events are the range read from 0 to that count, and archive files beyond
that count stay unread. The shared form returns the live cache allocation directly when there are no archived
events; with archived events it returns a shared allocation containing the composed history.

`archive_events_before` decodes the whole live log as strict UTF-8 before any write, as Python's
`open(encoding="utf-8")` does. Invalid UTF-8 returns a `StoreError` that names the path and the byte offset of the
first invalid byte. Before the split point, a line that parses as JSON but is not an object returns a `StoreError`
(Python's `AttributeError` on `event.get`), and so does a truthy `timestamp` that is not a string (Python's
`TypeError` at the cutoff comparison). Each error returns before any write, so the live log, the archive file and
`archive_offset` stay byte-identical. Lines end at `\r\n`, a lone `\r`, or `\n`, and a final line without an end is
kept. A blank line before the split point (`str_methods::strip` is empty) is dropped. Archived and kept lines are written
with a trailing `\n`. A non-JSON line, or a falsy or unparsable `timestamp`, starts the kept part as in Python.

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
| `claim_input_events_locked(&self, guard: &ControlGuard, session_id: &str, run_id: &str, event_ids: &[String]) -> StoreResult<RunRecord>` | `src/runtime/session_dispatch.py` run-record `input_event_ids` write |
| `read_run_sync(&self, session_id: &str, run_id: &str) -> StoreResult<Option<RunRecord>>`; `list_run_records_sync(&self, session_id: &str) -> StoreResult<Vec<RunRecord>>` | `src/runtime/runs.py::read_run_sync`, `list_run_records_sync` |
| `list_runs_page_sync(&self, session_id: &str, cursor: Option<&str>, limit: usize, order: RunOrder) -> StoreResult<RunPage>` | `src/runtime/runs.py::list_runs_page_sync` |
| `newest_run_record_sync(&self, session_id: &str) -> StoreResult<Option<RunRecord>>`; `last_ended_run_id(&self, session_id: &str) -> StoreResult<Option<String>>` | `src/runtime/runs.py::newest_run_record_sync`, `last_ended_run_id` |
| `run_records::terminal_outcome(events: &[Value], run_id: &str) -> StoreResult<Option<RunOutcomeKind>>` | `src/runtime/runs.py::terminal_outcome_in_events`, `RunStore.terminal_outcome` |
| `run_records::stop_requested_in_events(events: &[Value], run_id: &str, request_id: Option<&str>) -> bool` | `src/runtime/runs.py::stop_requested_in_events` |
| `run_display_state(&self, session_id: &str, run_id: &str) -> StoreResult<RunDisplayState>`; `run_blockers(&self, session_id: &str) -> StoreResult<Vec<RunBlocker>>` | `src/runtime/runs.py::run_display_state`, `run_blocker`; collected by `src/runtime/task_sessions.py::_structural_blockers` and `_elone_blockers` |
| `records_generation(&self) -> u64`; `mark_sidebar_dirty(&self, session_id: &str)` | `src/runtime/runs.py::records_generation`, `mark_sidebar_dirty` |

Run records use Python's exact field order and reject unknown fields. Their JSON writes are atomic.
`RunOrder::Descending` reads the `(started_at, id)` order backwards. A page cursor carries its order (`d` is set
only when descending), and a cursor read under the other order is refused with Python's `ValueError` text.
`register_run_locked`, retry creation, finish and `claim_input_events_locked` require `ControlGuard`. Control facts
append through `append_control_fact` while the guard is held. A finish fact lands before its record mirror.

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
| `Store::session_detail(&self, session_id: &str) -> StoreResult<Value>` | `src/runtime/task_sessions.py:987` |
| `create_task(&self, request: CreateTask, caller: TaskCaller, slot_values: Map<String, Value>) -> StoreResult<SessionMetadata>` | `src/runtime/task_sessions.py::create_task` |
| `runtime_gate(&self, request: &CreateTask, caller: &TaskCaller) -> StoreResult<()>` | `src/runtime/task_sessions.py` create-task runtime gate |
| `set_node_backend_locked(&self, guard: &ControlGuard, session_id: &str, backend: &str) -> StoreResult<()>` | `src/runtime/task_sessions.py::set_node_backend` |
| `task_tree::is_read_only_exempt(task: Option<&TaskSpec>) -> bool` (re-exported) | `src/runtime/takeoff_gate.py::is_read_only_exempt` |
| `adopt_metadata_slot_locked(&self, guard: &ControlGuard, session_id: &str, old: &SessionMetadata, owner: &str, fields: Option<&[&str]>) -> StoreResult<SessionMetadata>` | `src/runtime/task_sessions.py::adopt_metadata_slot` |
| `record_native_anchor(&self, session_id: &str, prompt_hash: &str, backend: &str, model: Option<&str>) -> StoreResult<()>`; `record_native_anchor_with_reset(&self, session_id: &str, prompt_hash: &str, backend: &str, model: Option<&str>, reset_anchor: bool) -> StoreResult<()>` | `src/runtime/task_sessions.py::record_native_anchor` |
| `archive_unsuccessful_worker(&self, guard: &ControlGuard, session_id: &str, run_id: &str, outcome: &str) -> StoreResult<bool>` | `src/runtime/task_sessions.py::archive_unsuccessful_worker` |
| `archive_subtree(&self, session_id: &str, caller: TaskCaller) -> StoreResult<Vec<String>>` | `src/runtime/task_sessions.py::archive_subtree` |
| `delete_permanently(&self, session_id: &str, caller: TaskCaller) -> StoreResult<bool>` | `src/runtime/task_sessions.py::delete_permanently`, `_deletion_blockers_locked` |
| `patch_task(&self, session_id: &str, patch: PatchTask, caller: TaskCaller, guards: &dyn StructuralBlockers) -> StoreResult<SessionMetadata>` | `src/runtime/task_sessions.py::patch_task` |
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
`mark_read` keeps `updated_at` and broadcasts `unread_changed` only when the unread flag flips. Permanent deletion removes
the session directory, clears runtime state, and removes an empty session cgroup.
`patch_task` accepts only `TaskCaller::Operator` too. Its pending-input and undelivered-report guards come through
`StructuralBlockers`, which the task dispatcher implements: the dispatcher owns the input boundary, and the store cannot
call it. The field write re-reads the row, so a rename that lands meanwhile is kept.
The reset-aware native-anchor method clears the held conversation id and timestamp after persisting provenance when a
launch starts a fresh native context; the planned four-argument method is the continuing-context form.

## Session fork and elone (S3, implemented)

| Rust API | Python source |
| --- | --- |
| `fork_session(&self, parent_id: &str, event_index: Option<i64>, backend: Option<&str>) -> StoreResult<SessionMetadata>` | `src/runtime/session_fork.py::SessionFork.fork_session`, `_spawn_with_history` |
| `elone_session(&self, parent_id: &str, event_index: i64, backend: Option<&str>, redelivery: &dyn CloseReportRedelivery) -> StoreResult<SessionMetadata>` | `src/runtime/session_fork.py::SessionFork.elone_session`; `src/runtime/task_sessions.py::run_elone_replacement` |
| `CloseReportRedelivery::redeliver_close_reports_locked(&self, guard: &ControlGuard, session_id: &str) -> StoreHookFuture<'_, ()>` (implemented by `Dispatcher`) | `src/runtime/task_sessions.py::run_elone_replacement`, subtree close-report redelivery |
| `RuntimeEventHooks::copy_on_fork(&self, parent_dir: &Path, child_dir: &Path) -> StoreResult<()>` | `src/runtime/session_fork.py::_copy_on_fork_sync` |
| `FORK_BOOTSTRAP_OPENER`, `ELONE_BOOTSTRAP_OPENER`, `HISTORY_LOCATION_NOTE` | the bootstrap constants in `src/runtime/session_fork.py` |

The child's chat log is born whole: the parent's raw lines for `[0, end)`, then the `clone_start` marker and the child's
`task_created` fact, written as one atomic stream. Elone runs under the control lock: it validates the blockers, redelivers
the subtree's close reports through `CloseReportRedelivery`, moves the source's children, closes the source as archived,
and archives it. The session-api route resolves the backend before either call, and it admits the bootstrap prompt and
dispatches the successor's run after the call returns.

## Task errors (S3)

Every task-tree refusal is a `StoreError` variant that names the Python class its route maps. The routes set the HTTP
status from the class (`src/runtime/api/sessions.py::_task_http_error`), so the variant decides the status.

| Python class | Rust variant | HTTP | Store producers |
| --- | --- | --- | --- |
| `task_errors.TaskInvalidError` | `StoreError::invalid` (`Invalid`) | 400 | `create_task`: a missing request id, a worker parent |
| `task_errors.TaskNotFoundError`; `runs.RunNotFoundError` | `StoreError::not_found` (`NotFound`) | 404 | `load_task_meta` and every tree read of an absent node; `archive_subtree`, `delete_permanently`; the locked slot, backend, adopt and rebind writes of an absent session; `require_run` |
| `task_errors.TaskForbiddenError`; `takeoff_gate.DelegationBlockedError` | `StoreError::forbidden` (`Forbidden`) | 403 | an agent's create outside its own open manager task (`runtime_gate`); every `check_task_authorization` refusal; `archive_subtree` and `delete_permanently` for a caller that is not the operator |
| `task_errors.TaskArchivedError` | `StoreError::archived` (`Archived`) | 409, the sentence alone | none in the store yet: machine input to an archived node is refused by the dispatch admission (`session_dispatch.admit_input`) |
| `task_errors.TaskConflictError` | `StoreError::conflict` (`Conflict`) | 409, `{message, blockers}` | a closed parent or ancestor (including a create under an archived parent); a relation cycle or hop limit; `archive_subtree`'s unfinished runs; `delete_permanently`'s saved references and content |
| `runs.RunIdentityConflictError` | `StoreError::conflict` with one blocker | 409, `{message, blockers}` | the stop path's process identity checks (`follow_through_stop`) |
| none: I/O, serialization, corrupt state, hook and invariant errors | `StoreError::new` (`Failure`) | none | every other error |

`Display` gives the Python text: a conflict joins its blockers with `; `, and an archived refusal reads
`task <id> is archived`. `blockers()` returns a conflict's list and `None` for every other variant. Python's
`TaskArchivedError` is a `TaskConflictError` subclass, but its route answers the sentence alone, so the Rust archived
variant carries no list. A refusal from `check_task_authorization` is `forbidden` on every path, as
`DelegationBlockedError` is a 403 on every route.

`create_task` without a name takes `Session N` from `sessions/.counter`. An unreadable or unparsable counter takes
the session-directory count and is then rewritten with the next number, as Python's `_next_session_name` does.

Each control fact notifies the sidebar with its type after it is durable: `append_control_fact` sends
`task_tree_changed` with the fact's type, and `create_task` sends `task_created` after its publish and index
invalidation. A replayed create notifies nothing. A failed notification logs `task_tree_changed_broadcast_failed` and
the write still succeeds.

## Runtime event bus, message model, and busy state (S4, implemented)

| Rust API | Python source |
| --- | --- |
| `prime_aggregator(&self, session_id: &str) -> StoreResult<u64>`; `announce_appended_event(&self, session_id: &str, event: &ChatEvent, epoch: u64) -> StoreResult<()>` | `src/runtime/session_events.py::prime_aggregator`, `announce_appended_event` |
| `persist_and_broadcast(&self, session_id: &str, event: ChatEvent) -> StoreResult<ChatEvent>`; `drop_session_runtime_state(&self, session_id: &str)` | `src/runtime/session_events.py::persist_and_broadcast`, `drop_session_runtime_state` |
| `subscribe(&self, channel: &str) -> Subscription`; `unsubscribe(&self, subscription: &mut Subscription)`; `broadcast(&self, channel: &str, frame: Value) -> StoreResult<()>` | `src/runtime/streaming.py::StreamingManager.subscribe`, `unsubscribe`, `broadcast` |
| `register_socket(&self) -> SocketHandle`; `close_subscribers(&self)` (async: closes every registered socket with code 1000 and waits for each close frame) | `src/runtime/streaming.py::StreamingManager._connections`, `close_all` |
| `session_channel(session_id: &str) -> String`; `SIDEBAR_CHANNEL`; `STREAM_COALESCE_INTERVAL` | `src/runtime/streaming.py` session/sidebar channels and `_STREAM_COALESCE_INTERVAL` |
| `broadcast_only(&self, session_id: &str, frame: Value) -> StoreResult<()>`; `broadcast_sidebar(&self, session_id: &str, event_type: &str, fields: Map<String, Value>) -> StoreResult<()>`; `broadcast_task_tree_changed(&self, session_id: &str, event_type: Option<&str>) -> StoreResult<()>` | `src/runtime/session_events.py::broadcast_only`, `broadcast_sidebar`, `broadcast_task_tree_changed` |
| `notify_task_tree_changed(&self, session_id: &str, event_type: Option<&str>)` (best effort, no result) | `src/runtime/control_sink.py::ControlEventSink.notify_tree_changed` |
| `load_chat_events_tail(&self, session_id: &str, limit: usize) -> StoreResult<(Vec<ChatEvent>, usize, bool)>`; `load_chat_events_range(&self, session_id: &str, start: usize, end: usize) -> StoreResult<(Vec<ChatEvent>, bool)>`; `get_chat_event_count_sync(&self, session_id: &str) -> StoreResult<usize>` | `src/runtime/session_events.py` event count, tail, and range reads; `src/runtime/chat_events.py` |
| `get_message_projection(&self, session_id: &str) -> StoreResult<Option<Arc<MessageProjection>>>` | `src/runtime/session_events.py` message projection memo |
| `Store::new_message_aggregator(&self, event_index_offset: usize, emit_stream_deltas: bool) -> MessageAggregator`; `MessageAggregator::{new, with_renderers, feed, feed_indexed, flush_pending, pending_draft_message}` | `src/runtime/message_aggregator.py::MessageAggregator` |
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

## Test write faults (`write-faults` feature, test-only)

The `write-faults` feature compiles `charliebot_store::write_faults`. It fails the store's own writes on demand, so a
test can drive the out-of-space landing retry through a live store write. The Python equivalents are
`inject_chat_append_fault` and `inject_run_record_write_fault` in `tests/test_task_execution.py`, which patch
`append_ndjson` and `RunStore.write_record`.

The feature is off by default, and no production crate enables it. The seam also compiles in this crate's own test
builds, so its unit tests cover the injectors. A crate that needs the seam in its tests adds a `[dev-dependencies]`
entry, `charliebot-store = { path = "...", features = ["write-faults"] }`. No other build compiles the seam.

| Rust API | Python source |
| --- | --- |
| `inject_chat_append_fault(store: &Store, event_type: &str, session_ids: &[&str], times: usize, errno: i32) -> WriteFault` | `inject_chat_append_fault` |
| `inject_run_record_write_fault(store: &Store, only_finished: bool, times: usize, errno: i32) -> WriteFault` | `inject_run_record_write_fault` |
| `WriteFault::hits(&self) -> Vec<Hit>` | the returned `list[bool]` (`True` is a failed write) |

- A chat-append fault matches an append whose event `type` is `event_type` and whose session is in `session_ids`. The
  first `times` matching appends fail, and every other append writes through. The check runs in `append_chat_event`,
  and in the first append of a new task node (`publish_task_node`), before the log is written.
- A run-record fault matches the record writes that set `ended_at` when `only_finished` is set, and every record write
  otherwise. The check runs at the top of `write_run_record`, before the disk is touched, so a failed record write
  leaves no record file. A caller may already have created the run directory, as it would after a real failure.
- A failed write returns a `StoreError` shaped as the store's I/O errors are: `<path>: <io::Error::from_raw_os_error>`.
  `charliebot-tasks::is_out_of_space_error` reads it as a disk error.
- An injector is scoped to one `Store` value. Its `WriteFault` guard removes the fault on drop. When two faults match
  one write, each decides it, and the write fails with the errno of the first fault that fails it.
- `hits` has one entry per matching write, in write order: `Hit::Failed` for a write the fault failed, and
  `Hit::Written` for a match after its `times` were spent.

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
- One explicit Python task-tree notification has no sender in this crate. A run's first launch (`runs.py:891`) is
  not sent by `record_launch`. It will send through `notify_task_tree_changed`. A fork sends its own
  `broadcast_task_tree_changed` for the child (`session_fork.py:435`; `session_fork.rs:302`), and
  the metadata patch (`task_sessions.py:1424`) sends from `patch_task`.
