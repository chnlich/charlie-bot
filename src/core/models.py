"""All Pydantic models for CharlieBot."""

import uuid
from dataclasses import dataclass
from datetime import datetime, timezone
from enum import Enum
from typing import Annotated, Awaitable, Callable, Dict, Literal, Optional, Union

from pydantic import BaseModel, BeforeValidator, ConfigDict, Field


def ensure_utc(v: datetime | str) -> datetime:
  """Coerce naive datetimes to UTC; pass aware datetimes through unchanged."""
  if isinstance(v, str):
    v = datetime.fromisoformat(v.replace("Z", "+00:00"))
  if isinstance(v, datetime) and v.tzinfo is None:
    return v.replace(tzinfo=timezone.utc)
  return v


def parse_utc_datetime(v: str) -> datetime:
  """Parse an ISO 8601 string and normalize naive datetimes to UTC."""
  return ensure_utc(v)


def utc_now() -> datetime:
  """Return the current UTC datetime as a tz-aware value."""
  return datetime.now(timezone.utc)


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
  started_at: Optional[UtcDatetime] = None
  completed_at: Optional[UtcDatetime] = None
  pid: Optional[int] = None
  exit_code: Optional[int] = None
  cli_command: Optional[str] = None
  claude_session_id: Optional[str] = None
  branch_name: Optional[str] = None
  base_branch: Optional[str] = None
  repo_path: Optional[str] = None
  worktree_path: Optional[str] = None
  review_of: Optional[str] = None
  context: Optional[str] = None
  backend: Optional[str] = None
  model: Optional[str] = None
  require_review: bool = True
  skip_cleanup: bool = False
  keep_worktree: bool = False
  tried_backends: list[str] = Field(default_factory=list)


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
  """A SLURM job, watched via `sacct` for its authoritative terminal state."""
  kind: Literal[WatchKind.SLURM_JOB] = WatchKind.SLURM_JOB
  job_id: int


# Discriminated union on `kind`: each variant carries only its own fields, so
# illegal combinations (a local pid with a host, a slurm job with a pid) are
# unconstructable.
WatchTarget = Annotated[Union[LocalPid, RemotePid, SlurmJob], Field(discriminator="kind")]


class PendingTrigger(BaseModel):
  id: str = Field(default_factory=lambda: str(uuid.uuid4()))
  session_id: str
  fire_at: UtcDatetime
  message: str
  created_at: UtcDatetime = Field(default_factory=utc_now)
  status: TriggerStatus = TriggerStatus.PENDING
  fired_at: Optional[UtcDatetime] = None
  watch_targets: list[WatchTarget] = Field(default_factory=list)
  fire_reason: Optional[str] = None  # one of 'completed', 'timeout', populated when fired


# ---------------------------------------------------------------------------
# Backend Models
# ---------------------------------------------------------------------------


class BackendOption(BaseModel):
  id: str
  label: str
  type: str  # 'cc-claude' | 'cc-kimi' | 'cc-openai-compatible' | 'codex' | 'charlie-code' | 'gemini' | 'opencode' | 'antigravity' | 'tui-cli'
  model: Optional[str] = None
  effort: Optional[str] = None
  cli_binary: Optional[str] = None
  codex_home: Optional[str] = None  # codex backend only: per-account $CODEX_HOME
  model_reasoning_effort: Optional[str] = None  # codex backend only: per-backend reasoning effort override
  model_auto_compact_token_limit: Optional[int] = Field(default=None, gt=0)  # codex backend only: per-backend auto-compact token limit
  api_base: Optional[str] = None  # OpenAI-compatible base URL (charlie-code, cc-openai-compatible)
  api_key_env: Optional[str] = None  # cc-openai-compatible: env var holding the upstream API key
  fast_mode: bool = False  # cc-claude only: enable Claude Code fast mode via --settings '{"fastMode":true}'


MODEL_REQUIRED_BACKEND_TYPES = frozenset(
    {
        "cc-claude",
        "cc-kimi",
        "cc-openai-compatible",
        "codex",
        "charlie-code",
        "gemini",
        "opencode",
    })
MODEL_OPTIONAL_ROUTING_BACKEND_TYPES = frozenset({"antigravity"})


def backend_type_requires_model(backend_type: str) -> bool:
  return backend_type in MODEL_REQUIRED_BACKEND_TYPES


def backend_type_allows_missing_model(backend_type: str) -> bool:
  return backend_type in MODEL_OPTIONAL_ROUTING_BACKEND_TYPES


# ---------------------------------------------------------------------------
# Session Models
# ---------------------------------------------------------------------------


class SessionMetadata(BaseModel):
  id: str = Field(default_factory=lambda: str(uuid.uuid4()))
  name: str
  status: SessionStatus = SessionStatus.ACTIVE
  has_unread: bool = False
  has_running_tasks: bool = False
  has_pending_trigger: bool = False
  pending_trigger_count: int = 0
  next_trigger_at: Optional[datetime] = None
  starred: bool = False
  thinking_since: Optional[UtcDatetime] = None
  created_at: UtcDatetime = Field(default_factory=utc_now)
  updated_at: UtcDatetime = Field(default_factory=utc_now)
  cc_session_id: Optional[str] = None
  cc_session_started_at: Optional[UtcDatetime] = None
  backend: str = ""  # empty default; create_session always provides the real value
  scheduled_task: Optional[str] = None  # task name; None = regular session
  last_scheduled_run: Optional[str] = None  # ISO datetime of last scheduler execution
  last_run_status: Optional[str] = None  # "running" / "success" / "failed"
  last_scheduled_cron: Optional[str] = None  # cron expr at last run; detects changes
  # Transient fields, populated by API layer for scheduled sessions only
  schedule_cron: Optional[str] = None
  schedule_enabled: Optional[bool] = None
  schedule_next_run: Optional[str] = None
  schedule_timezone: Optional[str] = None
  schedule_project: Optional[str] = None
  schedule_allow_failure: Optional[bool] = None
  # Parent session for clone/elone-derived sessions
  parent_session_id: Optional[str] = None
  # Rating
  rating: Optional[SessionRating] = None
  # Key is the round event id (UUID generated at event write time, or
  # "legacy:<event_index>" for events predating the UUID migration).
  round_ratings: Dict[str, Literal['thumbs_up', 'thumbs_down']] = Field(default_factory=dict)
  # Grouping
  group: Optional[str] = None
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
  content: Optional[str] = None
  path: Optional[str] = None
  lines_added: Optional[int] = None
  message: Optional[str] = None
  status: Optional[str] = None
  tool_name: Optional[str] = None
  input: Optional[dict] = None
  timestamp: UtcDatetime = Field(default_factory=utc_now)


# ---------------------------------------------------------------------------
# API Request / Response Models
# ---------------------------------------------------------------------------


class CreateSessionRequest(BaseModel):
  name: Optional[str] = None
  scheduled_task: Optional[str] = None
  backend: Optional[str] = None


class ForkSessionRequest(BaseModel):
  event_index: Optional[int] = None
  backend: Optional[str] = None


class EloneSessionRequest(BaseModel):
  event_index: int
  backend: Optional[str] = None


class UploadedFileRef(BaseModel):
  filename: str
  path: str
  size: Optional[int] = None


class SendMessageRequest(BaseModel):
  content: str
  uploaded_files: list[UploadedFileRef] = Field(default_factory=list)


class RenameSessionRequest(BaseModel):
  name: str


class RateRoundRequest(BaseModel):
  rating: Optional[Literal['thumbs_up', 'thumbs_down']]


class SetGroupRequest(BaseModel):
  group: Optional[str] = None


class RenameGroupRequest(BaseModel):
  old_name: str
  new_name: str


class DeleteGroupRequest(BaseModel):
  group: str


class DelegateInvocationMetadata(BaseModel):
  """CLI invocation metadata to render delegate handoffs without inlining task specs."""
  model_config = ConfigDict(extra="forbid")

  task_type: TaskType
  repo_path: Optional[str] = None
  base_branch: Optional[str] = None
  task_spec_file: Optional[str] = None
  reviewer_context_file: Optional[str] = None
  keep_worktree: bool
  backend: Optional[str] = None


class DelegateRequest(BaseModel):
  """Request body for the internal delegation endpoint."""
  session_id: str
  description: str
  base_branch: Optional[str] = None
  backend: Optional[str] = None
  repo_path: Optional[str] = None
  context: Optional[str] = None
  task_type: TaskType = TaskType.IMPLEMENT
  keep_worktree: bool = False
  delegate_invocation: Optional[DelegateInvocationMetadata] = None


class ImproveRequest(BaseModel):
  """Request body for the internal improve endpoint."""
  model_config = ConfigDict(extra="forbid")

  session_id: str
  repo_path: str
  base_branch: str
  backend: Optional[str] = None
  iterations: int = 3
  goal: str
  plan: Optional[str] = None
  work_branch: Optional[str] = None
  merge_back: bool = False


class ScheduleTriggerRequest(BaseModel):
  """Request body for the internal schedule-trigger endpoint."""
  model_config = ConfigDict(extra="forbid")

  session_id: str
  delay_seconds: int
  message: str
  watch_targets: Optional[list[WatchTarget]] = None


# ---------------------------------------------------------------------------
# Session Callbacks (internal DTO bundling SessionManager hooks for run_message)
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class SessionCallbacks:
  """Bundle of SessionManager hooks passed to run_message as a single unit."""
  persist_and_broadcast: Callable[[str, dict], Awaitable[None]]
  update_thinking_state: Callable[..., Awaitable[None]]
  mark_unread: Callable[[str], Awaitable[None]]
  clear_thinking_since: Callable[..., Awaitable[None]]


# ---------------------------------------------------------------------------
# Spawn Request (internal DTO for spawn_worker)
# ---------------------------------------------------------------------------


@dataclass
class SpawnRequest:
  """Worker configuration parameters that travel as a unit through spawn_worker."""
  repo_path: Optional[str] = None
  context: Optional[str] = None
  prompt_override: Optional[str] = None
  resolved_backend: str = ""
  resolved_model: Optional[str] = None
  base_branch: Optional[str] = None
  branch_name_override: Optional[str] = None
  loop_dir: Optional[str] = None
  iteration_number: Optional[int] = None
  require_takeoff: bool = False
  worktree_path_override: Optional[str] = None
  skip_cleanup: bool = False
  skip_notify: bool = False
  is_continuation: bool = False
  keep_worktree: bool = False
  task_type: TaskType = TaskType.IMPLEMENT
