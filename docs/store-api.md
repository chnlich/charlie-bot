# `charliebot-store` API

The store owns writes to session `metadata.json`,
`data/chat_events.jsonl`, task facts, and immutable prompt bodies. It receives the ordered `MetadataSlot` list and all
six required runtime hooks at construction; absent hooks fail startup. Metadata mutation methods read from disk while
holding the session lock, save atomically, then return a fresh read-back. No public whole-object save
accepts a cached metadata value.
Expired active metadata entries are checked by stat outside the cache mutex. Refresh and eviction apply only to the entry
that was checked, so a concurrent write's cached result stays current.

## Metadata and listing (S1, implemented)

- `Store::get_session(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>>`
- `Store::read_metadata_fresh(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>>`
- `Store::session_dir(&self, session_id: &str) -> PathBuf`; `Store::metadata_path(&self, session_id: &str) -> PathBuf`
- `Store::persist_cc_session_id(&self, session_id: &str, cc_session_id: &str, native_backend: Option<&str>) -> StoreResult<Option<String>>`
- `Store::persist_native_backend(&self, session_id: &str, native_backend: &str) -> StoreResult<Option<String>>`
- `Store::persist_native_anchor_provenance(&self, session_id: &str, prompt_hash: &str, native_backend: &str, model: Option<&str>) -> StoreResult<()>`
- `Store::clear_cc_session_anchor(&self, session_id: &str) -> StoreResult<()>`
- `Store::persist_account_label(&self, session_id: &str, label: &str) -> StoreResult<Option<String>>`
- `Store::context_state(&self, session_id: &str) -> StoreResult<(Option<i64>, Option<UtcDateTime>)>` (legacy form; the agent-runtime caller moves to `SessionUsage::context_state`)
- `Store::update_thinking_state(&self, session_id: &str, updated_at: UtcDateTime) -> StoreResult<()>`
- `Store::mark_unread(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>>`
- `Store::mark_read(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>>`
- `Store::rename_session(&self, session_id: &str, name: &str) -> StoreResult<Option<SessionMetadata>>`
- `Store::set_group(&self, session_id: &str, group: Option<&str>) -> StoreResult<Option<SessionMetadata>>`
- `Store::star_session(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>>`; `Store::unstar_session(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>>`
- `Store::rename_group(&self, old_name: &str, new_name: &str) -> StoreResult<usize>`; `Store::delete_group(&self, group: &str) -> StoreResult<usize>`
- `Store::rate_round(&self, session_id: &str, round_id: &str, rating: Option<RoundRating>) -> StoreResult<Option<SessionMetadata>>`
- `Store::delete_session_permanently(&self, session_id: &str) -> StoreResult<bool>`
- `Store::switch_backend(&self, session_id: &str, backend: &str) -> StoreResult<Option<SessionMetadata>>`
- `Store::archive_session(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>>`
- `Store::unarchive_session(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>>`
- `Store::update_slot_fields(&self, session_id: &str, owner: &str, values: Map<String, Value>) -> StoreResult<Option<SessionMetadata>>`
- `Store::update_slot_fields_at(&self, session_id: &str, owner: &str, values: Map<String, Value>, updated_at: UtcDateTime) -> StoreResult<Option<SessionMetadata>>`
- `Store::update_slot_fields_locked(&self, guard: &ControlGuard, session_id: &str, owner: &str, values: Map<String, Value>) -> StoreResult<Option<SessionMetadata>>`
- `Store::load_session_metas(&self, status: Option<SessionStatus>) -> StoreResult<Vec<SessionMetadata>>`
- `Store::list_sessions(&self, status: Option<SessionStatus>, starred: Option<bool>, scheduled: Option<bool>) -> StoreResult<MetadataListing>`
- `Store::list_sessions_readonly(&self, status: SessionStatus) -> StoreResult<Vec<SessionMetadata>>`
- `Store::list_group_names(&self) -> StoreResult<Vec<String>>`
- `Store::sequence_subtree_roots(&self) -> StoreResult<Arc<HashMap<String, String>>>`
- `Store::view_subtree_roots(&self) -> StoreResult<Arc<HashMap<String, HashMap<String, String>>>>`
- `Store::list_archived_page(&self, group: Option<&str>, limit: usize, before: Option<&str>, before_id: Option<&str>) -> StoreResult<ArchivedPage>`
- `Store::archived_context_rows(&self, rows: &[SessionMetadata]) -> StoreResult<Vec<SessionMetadata>>`
- `trigger_files::iter_trigger_file_stats(triggers_dir: impl AsRef<Path>) -> io::Result<Vec<(PathBuf, Metadata)>>`
- `trigger_files::pending_trigger_state(triggers_dir: impl AsRef<Path>) -> (usize, Option<UtcDateTime>)`
- `Store::raw_metadata_scan(&self) -> StoreResult<Vec<RawMetadata>>`

Session listings apply `Store::derived_archived_ids` after stored-status reads. This overlay reads task-tree facts and leaves metadata unchanged. `SequenceListingFields` supplies controller fields and subtree ownership, borrowing the memoized `Arc` rows: `Store::subtree_roots_shared` keys both subtree maps on the corpus snapshot they were built from and hands out the cached `Arc` maps, which is what `sequence_subtree_roots` and `view_subtree_roots` return.

The `*_at` variants of rename, group, backend switch, archive, unarchive, and native-id persistence
take an explicit `UtcDateTime`. Production wrappers supply the current time; the explicit forms make
replay deterministic.

Metadata slots are serialized in registration order, each placed by its `after` key. An unregistered key
remains in `extra` and is emitted after registered slots. Metadata floats render in the model JSON form
(`1e16`, `0.00001`), which differs from the event-log form (`1e+16`, `1e-05`). `SessionMetadata` writes omit the
runtime-only transient fields. Atomic metadata writes use a sibling temporary file followed by rename, with
no fsync.

## Chat event log (S1, implemented)

- `Store::chat_events_path(&self, session_id: &str) -> PathBuf`
- `Store::append_chat_event(&self, session_id: &str, event: Value) -> StoreResult<ChatEvent>`
- `Store::load_chat_events_sync(&self, session_id: &str) -> StoreResult<Vec<ChatEvent>>`
- `Store::load_chat_events_shared_sync(&self, session_id: &str) -> StoreResult<Arc<Vec<ChatEvent>>>`
- `Store::load_chat_events_delta_sync(&self, session_id: &str, known_generation: Option<u64>, covered: usize) -> StoreResult<ChatEventsDelta>` (read the appended suffix for the same cached list, otherwise rebuild from the replacement list)
- `Store::load_all_facts(&self, session_id: &str) -> StoreResult<Vec<ChatEvent>>` (archived segments plus live log)
- `Store::load_all_facts_shared_sync(&self, session_id: &str) -> StoreResult<Arc<Vec<ChatEvent>>>` (archived segments plus live log)
- `Store::load_chat_events_range(&self, session_id: &str, start: usize, end: usize) -> StoreResult<(Vec<ChatEvent>, bool)>`
- `Store::find_event_by_id(&self, session_id: &str, event_id: &str) -> StoreResult<Option<ChatEvent>>` (replay lookup)
- `Store::get_chat_event_count_sync(&self, session_id: &str) -> StoreResult<usize>`
- `Store::archive_events_before(&self, session_id: &str, cutoff: UtcDateTime) -> StoreResult<Option<PathBuf>>` (strict UTF-8 decode; error cases below)
- `Store::append_control_fact(&self, guard: &ControlGuard, session_id: &str, event: Value) -> StoreResult<ChatEvent>`
- `Store::control_lock(&self) -> ControlGuard`

Append writes each event as one JSON line with the `, `/`: ` separators and non-ASCII characters unescaped, appends
`id` then an `isoformat` timestamp if absent, writes one newline-terminated record, and syncs event data before
returning. Readers skip blank and malformed lines, replace invalid UTF-8, and parse a partial final
line. Each session has its own event-cache mutex; the cache-map mutex is held only to find or create
that entry. Appends and cold reads hold the session mutex across file I/O, so another session's
event operations do not wait on that I/O. A shared read returns the cached `Arc<Vec<ChatEvent>>`;
reads without an intervening append share its allocation, and copy-on-write keeps an older snapshot
unchanged when an append lands. The control guard is store-bound, non-cloneable, and required by
locked operations.

`load_chat_events_range` keeps the physical range indexes: an unarchived cold read selects physical `\n`-delimited lines,
so blank and malformed lines consume indexes; an unarchived warm read slices the parsed-event cache. The
`/api/sessions/{session_id}/events` fallback uses this range API, while projection and other whole-log callers keep
the shared parsed-event read.

`load_chat_events_delta_sync` returns the cached generation, current event count, the suffix start index, and the
events from that index. Appends retain the generation; cache eviction or replacement gives a new generation.
`SessionUsage` uses this to advance its single event fold by the appended suffix. It asks the registered backend
resolver first (Codex's native rollout), then the event tiers. `Store::context_state` remains unchanged until the
agent-runtime caller switches to `SessionUsage::context_state`. `load_all_facts` and
`load_all_facts_shared_sync` compose the archived and live segments: the archived count is the total event count
minus the live event count, the archived events are the range read from 0 to that count, and archive files beyond
that count stay unread. The shared form returns the live cache allocation directly when there are no archived
events; with archived events it returns a shared allocation containing the composed history.

`archive_events_before` decodes the whole live log as strict UTF-8 before any write. Invalid UTF-8 returns a
`StoreError` that names the path and the byte offset of the first invalid byte. Before the split point, a line that
parses as JSON but is not an object returns a `StoreError`, and so does a truthy `timestamp` that is not a string.
Each error returns before any write, so the live log, the archive file and
`archive_offset` stay byte-identical. Lines end at `\r\n`, a lone `\r`, or `\n`, and a final line without an end is
kept. A blank line before the split point (`str_methods::strip` is empty) is dropped. Archived and kept lines are written
with a trailing `\n`. A non-JSON line, or a falsy or unparsable `timestamp`, starts the kept part.

## Run records (S2, implemented)

- `register_run_locked(&self, guard: &ControlGuard, session_id: &str, run: RunRecord, task_spec: Option<&str>) -> StoreResult<RunRecord>`
- `create_retry_run_locked(&self, guard: &ControlGuard, session_id: &str, request_id: &str, original_run_id: &str, task_spec_text: Option<&str>, fields: RetryRunFields) -> StoreResult<RunRecord>`
- `record_launch(&self, session_id: &str, run_id: &str, pid: u32, pid_start: &str) -> StoreResult<RunRecord>`
- `record_observation(&self, session_id: &str, run_id: &str, fields: Map<String, Value>) -> StoreResult<RunRecord>`
- `record_finish_locked(&self, guard: &ControlGuard, session_id: &str, run_id: &str, outcome: RunOutcome) -> StoreResult<RunRecord>`
- `request_stop(&self, session_id: &str, run_id: &str, reason: &str) -> StoreResult<StopRequest>`
- `reconcile_stop_request(&self, session_id: &str, run_id: &str) -> StoreResult<StopResult>`
- `claim_input_events_locked(&self, guard: &ControlGuard, session_id: &str, run_id: &str, event_ids: &[String]) -> StoreResult<RunRecord>`
- `read_run_sync(&self, session_id: &str, run_id: &str) -> StoreResult<Option<RunRecord>>`; `list_run_records_sync(&self, session_id: &str) -> StoreResult<Vec<RunRecord>>`
- `list_runs_page_sync(&self, session_id: &str, cursor: Option<&str>, limit: usize, order: RunOrder) -> StoreResult<RunPage>`
- `newest_run_record_sync(&self, session_id: &str) -> StoreResult<Option<RunRecord>>`; `last_ended_run_id(&self, session_id: &str) -> StoreResult<Option<String>>`
- `run_records::terminal_outcome(events: &[Value], run_id: &str) -> StoreResult<Option<RunOutcomeKind>>`
- `run_records::stop_requested_in_events(events: &[Value], run_id: &str, request_id: Option<&str>) -> bool`
- `run_display_state(&self, session_id: &str, run_id: &str) -> StoreResult<RunDisplayState>`; `run_blockers(&self, session_id: &str) -> StoreResult<Vec<RunBlocker>>`
- `records_generation(&self) -> u64`; `mark_sidebar_dirty(&self, session_id: &str)`

Run records keep the stored field order and reject unknown fields. Their JSON writes are atomic.
`RunOrder::Descending` reads the `(started_at, id)` order backwards. A page cursor carries its order (`d` is set
only when descending), and a cursor read under the other order is refused with an error that names both orders.
`register_run_locked`, retry creation, finish and `claim_input_events_locked` require `ControlGuard`. Control facts
append through `append_control_fact` while the guard is held. A finish fact lands before its record mirror.

Registration pins `task_spec` only when it is `Some`: the body lands in `task_spec.md` before the record,
which then carries `task_spec_ref` and `task_spec_hash`. A `None` spec writes neither field. Retry creation
takes `RetryRunFields` (`kind`, `backend`, `model`, `repo_path`, `base_branch`, `branch_name`,
`worktree_path`, `sequence_ref`, `review_of_run_id`, `input_event_ids`) and goes through the same registration,
so a replay of one request returns the first record unchanged.

`RunOutcome` carries the terminal outcome, an optional input list, an optional exit code and an
optional end time. Repeated finish calls keep the first fact's payload and repair a missing record mirror.
A run's terminal outcome is its last `run_finished` fact's outcome. The display state, the blockers, the stop
requests and the finish path read it through
`terminal_outcome`; the run-identity refusal asks only whether any finish fact exists.
Stop requests append their fact under the guard. The process identity check, SIGTERM and bounded
exit wait run after the guard is released.

The store exports the legacy and v2 run-layout names. It provides
read-only path scans for legacy master logs, legacy thread event logs and v2 run raw logs, plus a
scan of all run records. `run_identity_refusal` reads the run record and full fact history before a
route accepts run-token identity. `records_generation` increments after each run-record write.
`mark_sidebar_dirty` updates the process-local sidebar dirty set and its derived generation.

## Task tree (S3, implemented)

- `Store::session_detail(&self, session_id: &str) -> StoreResult<Value>`
- `create_task(&self, request: CreateTask, caller: TaskCaller, slot_values: Map<String, Value>) -> StoreResult<SessionMetadata>`
- `runtime_gate(&self, request: &CreateTask, caller: &TaskCaller) -> StoreResult<()>` (the create-task runtime gate)
- `set_node_backend_locked(&self, guard: &ControlGuard, session_id: &str, backend: &str) -> StoreResult<()>`
- `task_tree::is_read_only_exempt(task: Option<&TaskSpec>) -> bool` (re-exported)
- `adopt_metadata_slot_locked(&self, guard: &ControlGuard, session_id: &str, old: &SessionMetadata, owner: &str, fields: Option<&[&str]>) -> StoreResult<SessionMetadata>`
- `record_native_anchor(&self, session_id: &str, prompt_hash: &str, backend: &str, model: Option<&str>) -> StoreResult<()>`; `record_native_anchor_with_reset(&self, session_id: &str, prompt_hash: &str, backend: &str, model: Option<&str>, reset_anchor: bool) -> StoreResult<()>`
- `archive_unsuccessful_worker(&self, guard: &ControlGuard, session_id: &str, run_id: &str, outcome: &str) -> StoreResult<bool>`
- `archive_subtree(&self, session_id: &str, caller: TaskCaller) -> StoreResult<Vec<String>>`
- `delete_permanently(&self, session_id: &str, caller: TaskCaller) -> StoreResult<bool>`
- `patch_task(&self, session_id: &str, patch: PatchTask, caller: TaskCaller, guards: &dyn StructuralBlockers) -> StoreResult<SessionMetadata>`
- `ensure_prompt_changed_fact(&self, session_id: &str, meta: &SessionMetadata, scope: PromptScope) -> StoreResult<()>`
- `load_meta(&self, session_id: &str) -> StoreResult<Option<SessionMetadata>>`; `load_task_meta(&self, session_id: &str) -> StoreResult<SessionMetadata>`
- `task_state(&self, session_id: &str) -> StoreResult<TaskState>`; `facts_of(&self, session_id: &str) -> StoreResult<TaskFacts>`
- `archived_of(&self, session_id: &str) -> StoreResult<bool>`; `check_task_authorization(&self, session_id: &str) -> StoreResult<String>`
- `ancestor_ids(&self, session_id: &str) -> StoreResult<Vec<String>>`; `descendant_ids(&self, session_id: &str) -> StoreResult<Vec<String>>`
- `resolve_successor_chain(&self, session_id: &str) -> StoreResult<Vec<SessionMetadata>>`; `deliver_to_successor(&self, session_id: &str, event: ChatEvent) -> StoreResult<Option<String>>`
- `invalidate_tree_index(&self)`; `require_open_ancestry(&self, session_id: &str) -> StoreResult<Vec<String>>`
- `child_ids(&self, session_id: &str) -> StoreResult<Vec<String>>`; `derived_archived_ids(&self) -> StoreResult<Vec<String>>`
- `task_tree_activity(&self, session_id: &str) -> StoreResult<TaskTreeActivity>`
- `store_prompt_body(&self, body: &str) -> StoreResult<String>`; `prompt_bodies_dir(&self) -> PathBuf`
- `rebind_elone_bindings_locked(&self, guard: &ControlGuard, source_session_id: &str, new_session_id: &str) -> StoreResult<bool>`

`TaskFacts` is the durable event fold: task state, run outcomes and first finish positions, input boundaries and
acknowledgements, close facts, delivered child reports, and the event-id index all come from `load_all_facts`.
`check_task_authorization` returns the nearest user-message node that authorizes delegation.
`archive_unsuccessful_worker` returns whether it appended the archive fact.

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
launch starts a fresh native context; the four-argument method is the continuing-context form.

## Session fork and elone (S3, implemented)

- `fork_session(&self, parent_id: &str, event_index: Option<i64>, backend: Option<&str>) -> StoreResult<SessionMetadata>`
- `elone_session(&self, parent_id: &str, event_index: i64, backend: Option<&str>, redelivery: &dyn CloseReportRedelivery) -> StoreResult<SessionMetadata>`
- `CloseReportRedelivery::redeliver_close_reports_locked(&self, guard: &ControlGuard, session_id: &str) -> StoreHookFuture<'_, ()>` (implemented by `Dispatcher`; redelivers the subtree's close reports)
- `RuntimeEventHooks::copy_on_fork(&self, parent_dir: &Path, child_dir: &Path) -> StoreResult<()>`
- `FORK_BOOTSTRAP_OPENER`, `ELONE_BOOTSTRAP_OPENER`, `HISTORY_LOCATION_NOTE`

The child's chat log is born whole: the parent's raw lines for `[0, end)`, then the `clone_start` marker and the child's
`task_created` fact, written as one atomic stream. Elone runs under the control lock: it validates the blockers, redelivers
the subtree's close reports through `CloseReportRedelivery`, moves the source's children, closes the source as archived,
and archives it. The session-api route resolves the backend before either call, and it admits the bootstrap prompt and
dispatches the successor's run after the call returns.

## Task errors (S3)

Every task-tree refusal is a `StoreError` variant. The shared route mapping (`task_errors::task_http_error`) sets the
HTTP status from the variant.

| Rust variant | HTTP | Store producers |
| --- | --- | --- |
| `StoreError::invalid` (`Invalid`) | 400 | `create_task`: a missing request id, a worker parent |
| `StoreError::not_found` (`NotFound`) | 404 | `load_task_meta` and every tree read of an absent node; `archive_subtree`, `delete_permanently`; the locked slot, backend, adopt and rebind writes of an absent session; `require_run` |
| `StoreError::forbidden` (`Forbidden`) | 403 | an agent's create outside its own open manager task (`runtime_gate`); every `check_task_authorization` refusal; `archive_subtree` and `delete_permanently` for a caller that is not the operator |
| `StoreError::archived` (`Archived`) | 409, the sentence alone | none in the store yet: machine input to an archived node is refused by the dispatcher's input admission (`admit_input`) |
| `StoreError::conflict` (`Conflict`) | 409, `{message, blockers}` | a closed parent or ancestor (including a create under an archived parent); a relation cycle or hop limit; `archive_subtree`'s unfinished runs; `delete_permanently`'s saved references and content |
| `StoreError::conflict` with one blocker | 409, `{message, blockers}` | the stop path's process identity checks (`follow_through_stop`) |
| `StoreError::new` (`Failure`) | none | every other error: I/O, serialization, corrupt state, hook and invariant errors |

`Display` gives the refusal text: a conflict joins its blockers with `; `, and an archived refusal reads
`task <id> is archived`. `blockers()` returns a conflict's list and `None` for every other variant. The archived
variant carries no list: its route answers the sentence alone. A refusal from `check_task_authorization` is
`forbidden` on every path.

`create_task` without a name takes `Session N` from `sessions/.counter`. An unreadable or unparsable counter takes
the session-directory count and is then rewritten with the next number.

Each control fact notifies the sidebar with its type after it is durable: `append_control_fact` sends
`task_tree_changed` with the fact's type, and `create_task` sends `task_created` after its publish and index
invalidation. A replayed create notifies nothing. A failed notification logs `task_tree_changed_broadcast_failed` and
the write still succeeds.

## Runtime event bus, message model, and busy state (S4, implemented)

- `prime_aggregator(&self, session_id: &str) -> StoreResult<u64>`; `announce_appended_event(&self, session_id: &str, event: &ChatEvent, epoch: u64) -> StoreResult<()>`
- `persist_and_broadcast(&self, session_id: &str, event: ChatEvent) -> StoreResult<ChatEvent>`; `drop_session_runtime_state(&self, session_id: &str)`
- `subscribe(&self, channel: &str) -> Subscription`; `unsubscribe(&self, subscription: &mut Subscription)`; `broadcast(&self, channel: &str, frame: Value) -> StoreResult<()>`
- `register_socket(&self) -> SocketHandle`; `close_subscribers(&self)` (async: closes every registered socket with code 1000 and waits for each close frame)
- `session_channel(session_id: &str) -> String`; `SIDEBAR_CHANNEL`; `STREAM_COALESCE_INTERVAL`
- `broadcast_only(&self, session_id: &str, frame: Value) -> StoreResult<()>`; `broadcast_sidebar(&self, session_id: &str, event_type: &str, fields: Map<String, Value>) -> StoreResult<()>`; `broadcast_task_tree_changed(&self, session_id: &str, event_type: Option<&str>) -> StoreResult<()>`
- `notify_task_tree_changed(&self, session_id: &str, event_type: Option<&str>)` (best effort, no result)
- `load_chat_events_tail(&self, session_id: &str, limit: usize) -> StoreResult<(Vec<ChatEvent>, usize, bool)>`; `load_chat_events_range(&self, session_id: &str, start: usize, end: usize) -> StoreResult<(Vec<ChatEvent>, bool)>`; `get_chat_event_count_sync(&self, session_id: &str) -> StoreResult<usize>`
- `get_message_projection(&self, session_id: &str) -> StoreResult<Option<Arc<MessageProjection>>>`
- `Store::new_message_aggregator(&self, event_index_offset: usize, emit_stream_deltas: bool) -> MessageAggregator`; `MessageAggregator::{new, with_renderers, feed, feed_indexed, flush_pending, pending_draft_message}`
- `MessageProjection::{new, advanced, history, committed, pending_draft, tail, slice_before}` and bounded page-body cache methods
- `RuntimeEventHooks`, `EventRenderer`, and `AfterTurnWork` supplied in `StoreHooks.runtime_events`
- `mark_busy(&self, session_id: &str, since: UtcDateTime) -> StoreResult<(UtcDateTime, bool)>`; `clear_busy(&self, session_id: &str)`; `busy_since(&self, session_id: &str) -> Option<UtcDateTime>`
- `mark_run_busy(&self, session_id: &str, run_id: &str, since: UtcDateTime)`; `clear_run_busy(&self, session_id: &str, run_id: &str)`; `note_run_backend(&self, session_id: &str, backend: &str)`; `run_backend(&self, session_id: &str) -> Option<String>`
- `build_scheduled_trigger_event(content: &str) -> ChatEvent`; `build_agent_message_event(&self, session_id: &str, text: &str, provenance: MessageProvenance) -> ChatEvent`; `master_done_input_event_ids(&self, event: &ChatEvent) -> Vec<String>`; `events_to_messages(&self, events: &[ChatEvent]) -> StoreResult<Vec<Value>>`; `events_to_view(&self, events: &[ChatEvent]) -> StoreResult<(Vec<Value>, Option<Value>)>`
- `require_session(&self, session_id: &str) -> StoreResult<SessionMetadata>`; `SESSION_NOT_FOUND_DETAIL` (404 detail `Session not found`)
- `handle_compaction_event(&self, session_id: &str, event: &Value) -> StoreResult<Option<ChatEvent>>`
- `normalize_user_message_event`, `serialize_uploaded_files`, `strip_attached_files_block`, `build_agent_input_content`, `stable_history_projection`, `stable_closed_prefix_len`

`persist_and_broadcast` feeds the aggregator after the durable append, broadcasts the event, and
starts each registered turn contribution's `after_turn` work for `master_done`. Stream deltas are
coalesced per channel over 200 ms, with a leading frame and the latest trailing frame; message and
error frames discard a pending preview. The control-sink two-phase path primes before the tree lock,
appends under the lock, then announces after release only when its epoch still matches. Feature event
renderers and after-turn work enter through `StoreHooks.runtime_events`; they are not mutable store hooks.

## Test write faults (`write-faults` feature, test-only)

The `write-faults` feature compiles `charliebot_store::write_faults`. It fails the store's own writes on demand, so a
test can drive the out-of-space landing retry through a live store write.

The feature is off by default, and no production crate enables it. The seam also compiles in this crate's own test
builds, so its unit tests cover the injectors. A crate that needs the seam in its tests adds a `[dev-dependencies]`
entry, `charliebot-store = { path = "...", features = ["write-faults"] }`. No other build compiles the seam.

- `inject_chat_append_fault(store: &Store, event_type: &str, session_ids: &[&str], times: usize, errno: i32) -> WriteFault`
- `inject_run_record_write_fault(store: &Store, only_finished: bool, times: usize, errno: i32) -> WriteFault`
- `WriteFault::hits(&self) -> Vec<Hit>` — one entry per matching write, in write order; a failed write is a failed hit.

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

## Behavior notes

- Non-UTC offsets in chat-event timestamps normalize to UTC on read and write.
- Slot values are not type-checked on write. `MetadataSlot` carries defaults only, so a typed write
  refusal lives with the caller.
- Archive rotation has no lock against a concurrent append, so an append racing
  `archive_events_before` can land in either the old or the new live file.
- The session counter parses a signed 64-bit integer. A counter with digit underscores or a larger
  value takes the session-directory count.
- One task-tree notification has no sender in this crate. A run's first launch is
  not sent by `record_launch`; it will send through `notify_task_tree_changed`. A fork sends its own
  `broadcast_task_tree_changed` for the child (`session_fork.rs:302`), and the metadata patch sends from `patch_task`.
