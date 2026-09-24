"""All Pydantic models for CharlieBot."""

import uuid
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from datetime import UTC, datetime
from enum import StrEnum
from typing import Annotated, Literal

from pydantic import BaseModel, BeforeValidator, ConfigDict, Field

# The cross-layer constants single-home in src/core.constants (stdlib-only, the
# CLI import floor's contract).
# The config-field models live in backend_models so the config chain (every CLI
# invocation's get_config) skips constructing the session/API models; these
# re-exports keep the established src.core.models import path working.
from src.core.backend_models import (  # noqa: F401  (re-export)
    BACKEND_CLASSES,
    BACKEND_OPTION_ADAPTER,
    MODEL_OPTIONAL_ROUTING_BACKEND_TYPES,
    BackendBase,
    BackendOption,
    CcClaudeBackend,
    CharlieCodeBackend,
    ClaudeAccount,
    ClaudeCompactionConfig,
    CodexBackend,
    OpencodeBackend,
    TuiCliBackend,
    backend_type_allows_missing_model,
    option_default_model,
)
from src.core.constants import MAX_TRIGGER_MESSAGE_CHARS, WatchKind


def ensure_utc(v: datetime | str) -> datetime:
  """Coerce naive datetimes to UTC; pass aware datetimes through unchanged."""
  if isinstance(v, str):
    v = datetime.fromisoformat(v)
  if isinstance(v, datetime) and v.tzinfo is None:
    return v.replace(tzinfo=UTC)
  return v


def parse_utc_datetime(v: str) -> datetime:
  """Parse an ISO 8601 string and normalize naive datetimes to UTC."""
  return ensure_utc(v)


def utc_now() -> datetime:
  """Return the current UTC datetime as a tz-aware value."""
  return datetime.now(UTC)


UtcDatetime = Annotated[datetime, BeforeValidator(ensure_utc)]

# ---------------------------------------------------------------------------
# Aliased types
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# Enums
# ---------------------------------------------------------------------------


class ThreadStatus(StrEnum):
  IDLE = "idle"
  RUNNING = "running"
  COMPLETED = "completed"
  FAILED = "failed"
  CANCELLED = "cancelled"


# Terminal statuses: the worker will produce no more output. Callers compare
# raw JSON strings against this set, valid because ThreadStatus is a str-enum.
TERMINAL_THREAD_STATUSES: frozenset[ThreadStatus] = frozenset(
    {ThreadStatus.COMPLETED, ThreadStatus.FAILED, ThreadStatus.CANCELLED})


class SessionStatus(StrEnum):
  ACTIVE = "active"
  ARCHIVED = "archived"


class TriggerStatus(StrEnum):
  PENDING = "pending"
  FIRED = "fired"
  CANCELLED = "cancelled"


class TaskType(StrEnum):
  IMPLEMENT = "implement"
  QUICK_EDIT = "quick-edit"
  SCRIPT_RUN = "script-run"
  VERIFY = "verify"


class LastRunStatus(StrEnum):
  """Outcome of a scheduled task's most recent run; every ``SessionMetadata.last_run_status`` is one of these."""
  RUNNING = "running"
  SUCCESS = "success"
  FAILED = "failed"
  SKIPPED = "skipped"



# ---------------------------------------------------------------------------
# Task-tree record types (schema_version=2)
# ---------------------------------------------------------------------------

# The only execution roles a v2 task carries; every manager depth shares one
# role and a worker is always a leaf. None on a legacy (v1) session, which is
# not a task-tree node.
TaskProfile = Literal["manager", "worker"]

# Tree display preference: shown pins the row visible, hidden pins it collapsed
# (the legacy archive entry maps here), auto derives from task facts.
PresentationMode = Literal["auto", "shown", "hidden"]

# What one Run actually executed. review is the same worker node's review pass;
# iteration is one round of an improve loop; scheduled_step is one cron-chain
# step.
RunKind = Literal["manager_turn", "work", "review", "iteration", "scheduled_step"]

# Derived task lifecycle, rebuilt from task_closed/task_reopened facts - never
# persisted as a state machine.
TaskState = Literal["open", "completed", "cancelled"]

# Derived per-node work state, rebuilt from run and input facts.
WorkState = Literal["idle", "running", "waiting", "attention"]

# Outcome carried by a run_finished fact.
RunOutcomeValue = Literal["success", "failed", "interrupted"]


class TaskSpec(BaseModel):
  # The task record a v2 session carries: goal, acceptance, and execution bounds.
  model_config = ConfigDict(extra="forbid")

  goal: str = ""
  acceptance: list[str] = Field(default_factory=list)
  context_refs: list[str] = Field(default_factory=list)
  repo_path: str | None = None
  base_branch: str | None = None
  task_type: TaskType | None = None
  keep_worktree: bool = False


class EventRef(BaseModel):
  # Pointer to the source chat event behind a metadata fact (provenance, not identity).
  model_config = ConfigDict(extra="forbid")

  session_id: str
  # origin_ref allows a null event id (the copy source predates event ids);
  # created_by_event always carries one and is validated at its write site.
  event_id: str | None = None


class SequenceRef(BaseModel):
  # Execution-sequence association: an improve loop or a cron-steps chain.
  model_config = ConfigDict(extra="forbid")

  kind: Literal["improve", "cron_steps"]
  owner_ref: str
  position: int


class RunRecord(BaseModel):
  # One actual execution and its evidence (schema_version=2 Run schema).
  #
  # Owned by src.core.runs; lives at sessions/<id>/data/runs/<run_id>/metadata.json.
  # Identity and evidence fields pin the execution; activity is always re-read
  # from process identity plus terminal facts, never from a persisted status.
  model_config = ConfigDict(extra="forbid")

  id: str
  session_id: str
  kind: RunKind = "work"
  # The exact input batch this run was dispatched against.
  input_event_ids: list[str] = Field(default_factory=list)
  # The task-spec body pinned at launch: ref points at the immutable pinned
  # text, hash is its SHA-256 (execution evidence binding).
  task_spec_hash: str | None = None
  task_spec_ref: str | None = None
  # Reference to the launch instruction snapshot (context stage writes it).
  prompt_snapshot_ref: str | None = None
  # The run this one retries; the retried run's evidence is preserved.
  retry_of_run_id: str | None = None
  # The work Run a review Run judges (plan 4.2: a review is the same worker
  # node's review pass, chained back to the delivered work). None on every
  # non-review Run.
  review_of_run_id: str | None = None
  backend: str | None = None
  model: str | None = None
  native_session_id: str | None = None
  pid: int | None = None
  pid_start: str | None = None
  started_at: UtcDatetime | None = None
  ended_at: UtcDatetime | None = None
  exit_code: int | None = None
  repo_path: str | None = None
  base_branch: str | None = None
  branch_name: str | None = None
  worktree_path: str | None = None
  sequence_ref: SequenceRef | None = None
  raw_log_ref: str | None = None
  events_ref: str | None = None
  result_ref: str | None = None


# ---------------------------------------------------------------------------
# Thread Models
# ---------------------------------------------------------------------------


class ThreadMetadata(BaseModel):
  id: str = Field(default_factory=lambda: str(uuid.uuid4()))
  session_id: str
  description: str
  status: ThreadStatus = ThreadStatus.IDLE
  created_at: UtcDatetime = Field(default_factory=utc_now)
  started_at: UtcDatetime | None = None
  completed_at: UtcDatetime | None = None
  pid: int | None = None
  # Field 22 of /proc/<pid>/stat (process start time in clock ticks since host
  # boot). Together with pid it pins a run to one process instance, so pid
  # reuse after a crash never fakes liveness. None for threads recorded by
  # older builds — such threads can never be judged alive.
  pid_start: str | None = None
  exit_code: int | None = None
  claude_session_id: str | None = None
  branch_name: str | None = None
  base_branch: str | None = None
  repo_path: str | None = None
  worktree_path: str | None = None
  review_of: str | None = None
  context: str | None = None
  backend: str | None = None
  model: str | None = None
  require_review: bool = True
  skip_cleanup: bool = False
  keep_worktree: bool = False
  tried_backends: list[str] = Field(default_factory=list)
  task_type: TaskType | None = None
  # Cron steps chain position (src/core/task_chain.py): chain_root is the
  # thread id of the chain's first step (the first step points at itself);
  # step_index is this thread's index into the task's steps list. None on every
  # non-chain thread.
  chain_root: str | None = None
  step_index: int | None = None


# ---------------------------------------------------------------------------
# Trigger Models
# ---------------------------------------------------------------------------


class LocalPid(BaseModel):
  """A process on the trigger-server host, watched via pidfd."""
  kind: Literal[WatchKind.LOCAL_PID] = WatchKind.LOCAL_PID
  pid: int


class RemotePid(BaseModel):
  """A process on another host, watched via periodic ssh `kill -0` probes."""
  kind: Literal[WatchKind.REMOTE_PID] = WatchKind.REMOTE_PID
  host: str
  pid: int


class SlurmJob(BaseModel):
  """A SLURM job, watched via `sacct` for its authoritative terminal state.

  `host` routes the probe: None runs `sacct` on the trigger-server host, a value
  runs it over `ssh <host>` against that cluster's login node.
  """
  kind: Literal[WatchKind.SLURM_JOB] = WatchKind.SLURM_JOB
  host: str | None = None
  job_id: int


# Discriminated union on `kind`: each variant carries only its own fields, so
# illegal combinations (a local pid with a host, a slurm job with a pid) are
# unconstructable.
WatchTarget = Annotated[LocalPid | RemotePid | SlurmJob, Field(discriminator="kind")]


class PendingTrigger(BaseModel):
  id: str = Field(default_factory=lambda: str(uuid.uuid4()))
  session_id: str
  fire_at: UtcDatetime
  message: str
  created_at: UtcDatetime = Field(default_factory=utc_now)
  status: TriggerStatus = TriggerStatus.PENDING
  fired_at: UtcDatetime | None = None
  watch_targets: list[WatchTarget] = Field(default_factory=list)
  fire_reason: str | None = None  # one of 'completed', 'timeout', populated when fired


# ---------------------------------------------------------------------------
# Session Models
# ---------------------------------------------------------------------------

# Role carried by the dedicated session of a type: pm cron task — the
# Project Manager for the task's ``project`` (group) value.
PROJECT_ROLE = "project"


class MasterRunRecord(BaseModel):
  """Identity of one in-flight master turn, persisted for restart reconciliation.

  Written when the turn's backend process spawns, cleared when the turn's
  MASTER_DONE lands. A record still present at server start means the turn's
  outcome is unresolved: startup reconcile resolves it through
  ``runs.resolve_run``'s outcome table (re-attach, drain, or clear); only a
  cleared record keeps the turn's user message (user_event_id) eligible for
  replay.
  """
  pid: int | None = None
  pid_start: str | None = None  # /proc/<pid>/stat field 22 at spawn time
  started_at: UtcDatetime
  raw_log: str  # absolute path to this turn's raw NDJSON transport file
  user_event_id: str | None = None  # chat event this turn answers


class SlackOrigin(BaseModel):
  """Slack thread a session was summoned from; set at creation, never mutated."""
  team_id: str
  channel_id: str
  thread_ts: str


class SessionMetadata(BaseModel):
  id: str = Field(default_factory=lambda: str(uuid.uuid4()))
  name: str
  status: SessionStatus = SessionStatus.ACTIVE
  has_unread: bool = False
  has_running_tasks: bool = False
  has_pending_trigger: bool = False
  pending_trigger_count: int = 0
  next_trigger_at: datetime | None = None
  has_pending_plan_approval: bool = False
  starred: bool = False
  # Transient runtime fact derived from src.core.thinking_state at read time;
  # never persisted (excluded by _TRANSIENT_METADATA_FIELDS).
  thinking_since: UtcDatetime | None = None
  created_at: UtcDatetime = Field(default_factory=utc_now)
  updated_at: UtcDatetime = Field(default_factory=utc_now)
  cc_session_id: str | None = None
  cc_session_started_at: UtcDatetime | None = None
  # Label (claude_accounts[].label) of the pool account whose transcript store
  # holds this session's Claude Code conversation. None until the pool assigns
  # one, and always None for a pinned or non-cc-claude backend.
  claude_account: str | None = None
  # In-flight master turn identity for restart reconcile; None when idle.
  master_run: MasterRunRecord | None = None
  backend: str = ""  # empty default; create_session always provides the real value
  scheduled_task: str | None = None  # task name; None = regular session
  role: str | None = None  # role ("project" from the scheduler; arbitrary via create API); None = regular session
  last_scheduled_run: str | None = None  # ISO datetime of last scheduler execution
  last_run_status: LastRunStatus | None = None
  last_scheduled_cron: str | None = None  # cron expr at last run; detects changes
  # Transient fields, populated by API layer for scheduled sessions only
  schedule_cron: str | None = None
  schedule_enabled: bool | None = None
  schedule_next_run: str | None = None
  schedule_timezone: str | None = None
  schedule_project: str | None = None
  schedule_allow_failure: bool | None = None
  # Parent session for clone/elone-derived sessions
  parent_session_id: str | None = None
  # Elone successor pointer: id of the session that took over from this one via
  # elone. Latest-wins for ordinary sessions — each elone overwrites the pointer
  # to name the parent's most recent elone child. Scheduler-owned sessions keep
  # a single succession. Ordinary fork/archive/delete leave it None.
  successor_session_id: str | None = None
  # Slack thread this session was summoned from; set at creation, never mutated.
  slack_origin: SlackOrigin | None = None
  # Newest consumed thread ts for a followed Slack thread; None = nothing
  # consumed yet. Advanced by summon creation (mention ts) and ack only.
  slack_watermark_ts: str | None = None
  # ------------------------------------------------------------------
  # Task-tree fields (schema_version=2). All default to their v1 absence so
  # existing metadata.json files keep parsing; a v2 task sets profile=manager
  # or worker and schema_version=2 at creation.
  # ------------------------------------------------------------------
  schema_version: int = 1
  # Parent task (decomposition edge). Null on an independent root. History
  # copying (parent_session_id/origin_ref) never becomes a task parent.
  task_parent_id: str | None = None
  profile: TaskProfile | None = None
  task: TaskSpec | None = None
  # Compatibility label for project/group grouping; effective rules come from
  # prompt references and task ancestry, not from this value.
  project_key: str | None = None
  presentation: PresentationMode = "auto"
  # Pausing blocks new automatic execution only; it never terminates a live run.
  automation_paused: bool = False
  # The create operation's source event (provenance and retry dedup).
  created_by_event: EventRef | None = None
  # History-copy source (fork/elone), kept separate from task_parent_id.
  origin_ref: EventRef | None = None
  # SHA-256 body references: subtree_prompt applies to this node and every
  # descendant, node_prompt to this node only. Bodies live immutable under
  # prompt_bodies/<sha256>.md in the selected home.
  subtree_prompt_ref: str | None = None
  node_prompt_ref: str | None = None
  # The native-backend continuation anchor's provenance: the instruction-hash
  # and backend identity the current cc_session_id conversation was continued
  # under. A manager turn resumes the native conversation only when all three
  # match the launch's own snapshot and backend; any change starts a fresh
  # native context (earlier history stays on disk, never rewritten).
  native_prompt_hash: str | None = None
  native_backend: str | None = None
  native_model: str | None = None

  # Key is the round event id (UUID generated at event write time, or
  # "legacy:<event_index>" for events predating the UUID migration).
  round_ratings: dict[str, Literal['thumbs_up', 'thumbs_down']] = Field(default_factory=dict)
  # Grouping
  group: str | None = None
  # Number of chat events that have been moved out of the live chat_events.jsonl
  # into archive files. All event_index values seen by the UI/API are GLOBAL =
  # archive_offset + line_number_in_live_file.
  archive_offset: int = 0


# ---------------------------------------------------------------------------
# Chat Models
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# Worker Output Models
# ---------------------------------------------------------------------------


class WorkerEvent(BaseModel):
  type: str
  content: str | None = None
  path: str | None = None
  lines_added: int | None = None
  message: str | None = None
  status: str | None = None
  tool_name: str | None = None
  input: dict | None = None
  # Set only when the projection trimmed this row's output to the
  # TOOL_PREVIEW_CHARS wire bound; the persisted events log keeps the full text.
  output_truncated: bool | None = None
  timestamp: UtcDatetime = Field(default_factory=utc_now)


# ---------------------------------------------------------------------------
# API Request / Response Models
# ---------------------------------------------------------------------------


class CreateSessionRequest(BaseModel):
  name: str | None = None
  scheduled_task: str | None = None
  backend: str | None = None
  role: str | None = None
  session_id: str | None = None
  slack_origin: SlackOrigin | None = None
  # ---- v2 task create: any of these set routes POST /api/sessions/ through the
  # task-tree owner; request_id is required there and binds the stable node id.
  request_id: str | None = None
  task_parent_id: str | None = None
  profile: TaskProfile | None = None
  task: TaskSpec | None = None


class PatchSessionTaskRequest(BaseModel):
  # v2 PATCH /api/sessions/{id} body: task metadata mutations.
  #
  # Field absence (None + not in model_fields_set) means "no change"; an
  # explicit null clears where clearing is legal (task_parent_id -> root,
  # prompts -> no local rule). Name rides the same PATCH as the rename route.
  model_config = ConfigDict(extra="forbid")

  name: str | None = None
  task_parent_id: str | None = None
  profile: TaskProfile | None = None
  task: TaskSpec | None = None
  presentation: PresentationMode | None = None
  automation_paused: bool | None = None
  # New rule bodies (or null to clear); the server stores the body immutable
  # and swaps the reference atomically, recording prompt_changed.
  subtree_prompt: str | None = None
  node_prompt: str | None = None


class AncestorRef(BaseModel):
  # One entry of a session detail's ancestors path (the task-parent chain).
  model_config = ConfigDict(extra="forbid")

  id: str
  name: str


class SessionRow(BaseModel):
  # One task summary row of GET /api/sessions/tree (and the archive/search projection).
  model_config = ConfigDict(extra="forbid")

  id: str
  name: str
  profile: TaskProfile | None = None
  task_parent_id: str | None = None
  task_state: TaskState
  work_state: WorkState
  archived: bool
  child_count: int
  open_descendant_count: int
  attention_descendant_count: int
  # Descendants whose work_state is currently running — the collapsed-row
  # delegated-work cue (a manager with only active descendants must not hide
  # ongoing work). Derived in the same projection pass as the other counts.
  running_descendant_count: int
  # The SessionManager-owned unread-reply flag (mark_unread on a delivered
  # reply, mark_read on opening). Independent of work_state: an idle task can
  # carry an unread reply, and a running one hides the dot without discarding
  # the flag.
  has_unread: bool


class RunRow(RunRecord):
  # One /runs row: the record plus the fact-derived display state
  # (queued|running|attention|stopped|success|failed|interrupted) and whether a
  # durable stop request stands. Derived per request by the run owner.
  state: str = "queued"
  stop_requested: bool = False


class RunPage(BaseModel):
  # GET /api/sessions/{id}/runs response: one keyset page plus its cursor.
  model_config = ConfigDict(extra="forbid")

  items: list[RunRow]
  next_cursor: str | None = None


class RetryRunRequest(BaseModel):
  # POST /api/sessions/{id}/retry body.
  model_config = ConfigDict(extra="forbid")

  request_id: str
  run_id: str


class CompleteTaskRequest(BaseModel):
  # POST /api/sessions/{id}/complete body: the completion claim (plan 4.1).
  # owner_run_id is NEVER accepted here: an own-run close request takes the
  # verified caller's bound run id, never a spoofable payload field.
  model_config = ConfigDict(extra="forbid")

  request_id: str
  summary: str = ""
  result_refs: list[str] = Field(default_factory=list)
  run_ids: list[str] = Field(default_factory=list)


class CompleteTaskPendingResponse(BaseModel):
  # The 202 body of an own-run closure request: re-evaluated after that Run succeeds.
  model_config = ConfigDict(extra="forbid")

  session_id: str
  request_id: str
  status: Literal["pending_run_finish"]


class CancelTaskRequest(BaseModel):
  # POST /api/sessions/{id}/cancel body: explicit operator cancellation.
  model_config = ConfigDict(extra="forbid")

  request_id: str
  reason: str


class AcknowledgeTaskInputsRequest(BaseModel):
  # POST /api/sessions/{id}/task-inputs/acknowledge body: the operator's
  # durable confirmation that exact task inputs were handled out-of-band
  # (the terminal-driven node's normal case). Never a skip-all guard: every
  # id is acknowledged individually, later arrivals still block closure.
  model_config = ConfigDict(extra="forbid")

  request_id: str
  input_ids: list[str]
  note: str = ""


class ReopenTaskRequest(BaseModel):
  # POST /api/sessions/{id}/reopen body: explicit operator reopen.
  model_config = ConfigDict(extra="forbid")

  request_id: str
  reason: str
  closed_event_id: str | None = None


class CancelRunRequest(BaseModel):
  # POST /api/sessions/{id}/runs/{run_id}/cancel body.
  model_config = ConfigDict(extra="forbid")

  request_id: str


class RunCancelResponse(BaseModel):
  # Run cancel response: outcome is null while a stop is only requested, not observed.
  model_config = ConfigDict(extra="forbid")

  run_id: str
  stop_requested: bool
  outcome: RunOutcomeValue | None = None



class ForkSessionRequest(BaseModel):
  event_index: int | None = None
  backend: str | None = None


class EloneSessionRequest(BaseModel):
  event_index: int
  backend: str | None = None


class ExplainRequest(BaseModel):
  """One explain (btw-style) request for a divider: the chosen backend is required."""
  event_index: int
  backend: str


class UploadedFileRef(BaseModel):
  filename: str
  path: str
  size: int | None = None


class SendMessageRequest(BaseModel):
  content: str
  uploaded_files: list[UploadedFileRef] = Field(default_factory=list)
  is_voice: bool = False


class RenameSessionRequest(BaseModel):
  name: str


class SwitchBackendRequest(BaseModel):
  backend: str


class RateRoundRequest(BaseModel):
  rating: Literal['thumbs_up', 'thumbs_down'] | None


class SetGroupRequest(BaseModel):
  group: str | None = None


class RenameGroupRequest(BaseModel):
  old_name: str
  new_name: str


class DeleteGroupRequest(BaseModel):
  group: str


class DelegateInvocationMetadata(BaseModel):
  """CLI invocation metadata to render delegate handoffs without inlining task specs."""
  model_config = ConfigDict(extra="forbid")

  task_type: TaskType
  repo_path: str | None = None
  base_branch: str | None = None
  task_spec_file: str | None = None
  reviewer_context_file: str | None = None
  keep_worktree: bool
  backend: str | None = None


class DelegateRequest(BaseModel):
  """Request body for the internal delegation endpoint.

  ``request_id`` is the v2 operation identity: on a task-tree manager it binds
  (parent, request_id) to one stable child task, so a replayed create/delegate
  returns the original child and Run instead of a second process. The CLI
  derives a stable default from the request content; an explicit value names
  intentional same-spec siblings.
  """
  session_id: str
  description: str
  base_branch: str | None = None
  backend: str | None = None
  repo_path: str | None = None
  context: str | None = None
  task_type: TaskType = TaskType.IMPLEMENT
  keep_worktree: bool = False
  delegate_invocation: DelegateInvocationMetadata | None = None
  request_id: str | None = None


class ImproveRequest(BaseModel):
  """Request body for the internal improve endpoint."""
  model_config = ConfigDict(extra="forbid")

  session_id: str
  repo_path: str
  base_branch: str
  backend: str | None = None
  iterations: int = 3
  goal: str
  plan: str | None = None
  work_branch: str | None = None
  merge_back: bool = False


class ScheduleTriggerRequest(BaseModel):
  """Request body for the internal schedule-trigger endpoint."""
  model_config = ConfigDict(extra="forbid")

  session_id: str
  delay_seconds: int
  message: str = Field(max_length=MAX_TRIGGER_MESSAGE_CHARS)
  watch_targets: list[WatchTarget] | None = None


class SessionMessageRequest(BaseModel):
  """Request body for the internal session-message (agent relay) endpoint."""
  model_config = ConfigDict(extra="forbid")

  session_id: str  # caller session (provenance)
  target_session_id: str
  content: str


class SlackReplyRequest(BaseModel):
  """Request body for the internal slack/reply endpoint: the calling session posts *text* to its own thread."""
  model_config = ConfigDict(extra="forbid")

  session_id: str
  text: str


class SlackAckRequest(BaseModel):
  """Request body for the internal slack/ack endpoint: the calling session marks *message_ids* (Slack ts) as read."""
  model_config = ConfigDict(extra="forbid")

  session_id: str
  message_ids: list[str]


# ---------------------------------------------------------------------------
# Plan Registry Request Models
# ---------------------------------------------------------------------------


class PlanPresentRequest(BaseModel):
  """Request body for the internal plan/present endpoint."""
  model_config = ConfigDict(extra="forbid")

  session_id: str
  file: str
  title: str
  base_repo: str | None = None
  base_branch: str | None = None
  base_sha: str | None = None


PlanAmendTrigger = Literal["auto_amend", "feedback"]
PlanCloseMode = Literal["superseded", "abandoned", "completed"]


class PlanAmendRequest(BaseModel):
  """Request body for the internal plan/amend endpoint."""
  model_config = ConfigDict(extra="forbid")

  session_id: str
  file: str
  plan_id: int | None = None
  trigger: PlanAmendTrigger = "feedback"
  # Why this version differs from its predecessor; rides on the version record,
  # never in the page body. Required: the author is an agent absent at read time.
  note: str
  base_repo: str | None = None
  base_branch: str | None = None
  base_sha: str | None = None


class PlanApproveRequest(BaseModel):
  """Request body for the internal plan/approve endpoint."""
  model_config = ConfigDict(extra="forbid")

  session_id: str
  plan_id: int | None = None


class PlanCloseRequest(BaseModel):
  """Request body for the internal plan/close endpoint."""
  model_config = ConfigDict(extra="forbid")

  session_id: str
  plan_id: int
  close_as: PlanCloseMode


# ---------------------------------------------------------------------------
# Session Callbacks (internal DTO bundling SessionManager hooks for run_message)
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class SessionCallbacks:
  """Bundle of SessionManager hooks passed to run_message as a single unit."""
  persist_and_broadcast: Callable[[str, dict], Awaitable[None]]
  update_thinking_state: Callable[..., Awaitable[None]]
  mark_unread: Callable[[str], Awaitable[None]]
  # Returns the cc_session_id read back from disk after persisting.
  persist_cc_session_id: Callable[[str, str], Awaitable[str | None]]
  has_completed_round: Callable[[str], Awaitable[bool]]
  # Sets (or clears, on None) the session's in-flight master-turn record.
  persist_master_run: Callable[[str, MasterRunRecord | None], Awaitable[None]]
  # Persists the pool account holding the session's transcript and returns the
  # label read back from disk. Optional so callback bundles built before the
  # account pool existed (tests) stay valid; the live bundle always sets it.
  persist_claude_account: Callable[[str, str], Awaitable[str | None]] | None = None
  # (context_tokens, last_request_at) for the account pool's cold-cache rule;
  # None when the caller wired no pool (tests).
  claude_context_state: Callable[[str, SessionMetadata], Awaitable[tuple[int | None, datetime | None]]] | None = None


# ---------------------------------------------------------------------------
# Spawn Request (internal DTO for spawn_worker)
# ---------------------------------------------------------------------------


@dataclass
class SpawnRequest:
  """Worker configuration parameters that travel as a unit through spawn_worker."""
  repo_path: str | None = None
  context: str | None = None
  prompt_override: str | None = None
  resolved_backend: str = ""
  resolved_model: str | None = None
  base_branch: str | None = None
  branch_name_override: str | None = None
  loop_dir: str | None = None
  iteration_number: int | None = None
  worktree_path_override: str | None = None
  skip_cleanup: bool = False
  skip_notify: bool = False
  is_continuation: bool = False
  keep_worktree: bool = False
  task_type: TaskType = TaskType.IMPLEMENT
