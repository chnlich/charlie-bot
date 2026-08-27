"""All Pydantic models for CharlieBot."""

import uuid
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from datetime import UTC, datetime
from enum import Enum
from typing import Annotated, Literal

from pydantic import BaseModel, BeforeValidator, ConfigDict, Field


def ensure_utc(v: datetime | str) -> datetime:
  """Coerce naive datetimes to UTC; pass aware datetimes through unchanged."""
  if isinstance(v, str):
    v = datetime.fromisoformat(v.replace("Z", "+00:00"))
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
# Type aliases
# ---------------------------------------------------------------------------

SessionRating = Literal['thumbs_up', 'neutral', 'thumbs_down']

# ---------------------------------------------------------------------------
# Enums
# ---------------------------------------------------------------------------


class ThreadStatus(str, Enum):
  IDLE = "idle"
  RUNNING = "running"
  COMPLETED = "completed"
  FAILED = "failed"
  CANCELLED = "cancelled"


# Terminal statuses: the worker will produce no more output. Callers compare
# raw JSON strings against this set, valid because ThreadStatus is a str-enum.
TERMINAL_THREAD_STATUSES: frozenset[ThreadStatus] = frozenset(
    {ThreadStatus.COMPLETED, ThreadStatus.FAILED, ThreadStatus.CANCELLED})


class SessionStatus(str, Enum):
  ACTIVE = "active"
  ARCHIVED = "archived"


class TriggerStatus(str, Enum):
  PENDING = "pending"
  FIRED = "fired"
  CANCELLED = "cancelled"


class TaskType(str, Enum):
  IMPLEMENT = "implement"
  QUICK_EDIT = "quick-edit"
  SCRIPT_RUN = "script-run"
  VERIFY = "verify"


class WatchKind(str, Enum):
  UNKNOWN = "unknown"  # fail-loud sentinel; never a valid target, no default
  LOCAL_PID = "local_pid"
  REMOTE_PID = "remote_pid"
  SLURM_JOB = "slurm_job"


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
# Backend Models
# ---------------------------------------------------------------------------


class BackendOption(BaseModel):
  id: str
  label: str
  type: str  # 'cc-claude' | 'cc-kimi' | 'cc-openai-compatible' | 'codex' | 'charlie-code' | 'gemini' | 'opencode' | 'antigravity' | 'tui-cli'
  model: str | None = None
  effort: str | None = None
  cli_binary: str | None = None
  codex_home: str | None = None  # codex backend only: per-account $CODEX_HOME
  claude_config_dir: str | None = None  # cc-claude backend only: per-account CLAUDE_CONFIG_DIR
  model_reasoning_effort: str | None = None  # codex backend only: per-backend reasoning effort override
  model_auto_compact_token_limit: int | None = Field(
      default=None, gt=0)  # codex backend only: per-backend auto-compact token limit
  # Overlay filename (no .md) under prompts/model_overlays/. Literal "none" =
  # explicitly fenceless (silent); None = undeclared; a declared-but-unreadable
  # file degrades the wake to a fenceless run. The two latter cases emit one
  # unified backend_overlay_inactive alert, told apart by its reason field —
  # the read failure never raises.
  prompt_overlay: str | None = None
  api_base: str | None = None  # OpenAI-compatible base URL (charlie-code, cc-openai-compatible)
  api_key_env: str | None = None  # cc-openai-compatible: env var holding the upstream API key
  fast_mode: bool = False  # cc-claude only: enable Claude Code fast mode via --settings '{"fastMode":true}'
  opencode_proxy_url: str | None = None  # opencode only: per-backend HTTP/HTTPS proxy URL


MODEL_OPTIONAL_ROUTING_BACKEND_TYPES = frozenset({"antigravity"})


def backend_type_allows_missing_model(backend_type: str) -> bool:
  return backend_type in MODEL_OPTIONAL_ROUTING_BACKEND_TYPES


# ---------------------------------------------------------------------------
# Session Models
# ---------------------------------------------------------------------------


# Role carried by the dedicated session of a mode: master cron task — the
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
  # In-flight master turn identity for restart reconcile; None when idle.
  master_run: MasterRunRecord | None = None
  backend: str = ""  # empty default; create_session always provides the real value
  scheduled_task: str | None = None  # task name; None = regular session
  role: str | None = None  # role ("project" from the scheduler; arbitrary via create API); None = regular session
  last_scheduled_run: str | None = None  # ISO datetime of last scheduler execution
  last_run_status: str | None = None  # "running" / "success" / "failed" / "skipped"
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
  # Rating
  rating: SessionRating | None = None
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


class ForkSessionRequest(BaseModel):
  event_index: int | None = None
  backend: str | None = None


class EloneSessionRequest(BaseModel):
  event_index: int
  backend: str | None = None


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
  """Request body for the internal delegation endpoint."""
  session_id: str
  description: str
  base_branch: str | None = None
  backend: str | None = None
  repo_path: str | None = None
  context: str | None = None
  task_type: TaskType = TaskType.IMPLEMENT
  keep_worktree: bool = False
  delegate_invocation: DelegateInvocationMetadata | None = None


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


# Upper bound on trigger --message length. The message is a short label naming
# which watch fired (runbook steps and readback commands live in session
# artifacts), so the CLI argparse precheck and --help text share this constant.
MAX_TRIGGER_MESSAGE_CHARS = 200


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


class PlanAmendRequest(BaseModel):
  """Request body for the internal plan/amend endpoint."""
  model_config = ConfigDict(extra="forbid")

  session_id: str
  file: str
  plan_id: int | None = None
  trigger: Literal["auto_amend", "feedback"] = "feedback"
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
  close_as: Literal["superseded", "abandoned", "completed"]


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
