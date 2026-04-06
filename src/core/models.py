"""All Pydantic models for CharlieBot."""

import uuid
from datetime import datetime, timezone
from enum import Enum
from typing import Annotated, Literal, Optional

from pydantic import BaseModel, BeforeValidator, Field


def _ensure_utc(v: datetime | str) -> datetime:
  """Coerce naive datetimes to UTC; pass aware datetimes through unchanged."""
  if isinstance(v, str):
    v = datetime.fromisoformat(v.replace("Z", "+00:00"))
  if isinstance(v, datetime) and v.tzinfo is None:
    return v.replace(tzinfo=timezone.utc)
  return v


UtcDatetime = Annotated[datetime, BeforeValidator(_ensure_utc)]

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


# ---------------------------------------------------------------------------
# Thread Models
# ---------------------------------------------------------------------------


class ThreadMetadata(BaseModel):
  id: str = Field(default_factory=lambda: str(uuid.uuid4()))
  session_id: str
  description: str
  status: ThreadStatus = ThreadStatus.IDLE
  created_at: UtcDatetime = Field(default_factory=lambda: datetime.now(timezone.utc))
  started_at: Optional[UtcDatetime] = None
  completed_at: Optional[UtcDatetime] = None
  pid: Optional[int] = None
  exit_code: Optional[int] = None
  cli_command: Optional[str] = None
  branch_name: Optional[str] = None
  base_branch: Optional[str] = None
  repo_path: Optional[str] = None
  worktree_path: Optional[str] = None
  review_of: Optional[str] = None
  context: Optional[str] = None
  backend: Optional[str] = None
  model: Optional[str] = None
  require_review: bool = True
  tried_backends: list[str] = Field(default_factory=list)


# ---------------------------------------------------------------------------
# Trigger Models
# ---------------------------------------------------------------------------


class PendingTrigger(BaseModel):
  id: str = Field(default_factory=lambda: str(uuid.uuid4()))
  session_id: str
  fire_at: UtcDatetime
  message: str
  created_at: UtcDatetime = Field(default_factory=lambda: datetime.now(timezone.utc))
  status: TriggerStatus = TriggerStatus.PENDING
  fired_at: Optional[UtcDatetime] = None


# ---------------------------------------------------------------------------
# Backend Models
# ---------------------------------------------------------------------------


class BackendOption(BaseModel):
  id: str
  label: str
  type: str  # 'cc-claude' | 'cc-kimi'
  model: Optional[str] = None
  effort: Optional[str] = None


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
  created_at: UtcDatetime = Field(default_factory=lambda: datetime.now(timezone.utc))
  updated_at: UtcDatetime = Field(default_factory=lambda: datetime.now(timezone.utc))
  cc_session_id: Optional[str] = None
  cc_session_started_at: Optional[UtcDatetime] = None
  backend: str = "claude-opus-4.6"  # must match an id in cfg.backend_options
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
  # Rewind fields
  parent_session_id: Optional[str] = None  # original session this was rewound from
  rewind_summary: Optional[str] = None  # context summary from parent session
  # Rating
  rating: Optional[SessionRating] = None
  # Grouping
  group: Optional[str] = None


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
  timestamp: UtcDatetime = Field(default_factory=lambda: datetime.now(timezone.utc))


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


class VoiceTranscriptionResponse(BaseModel):
  transcription: str
  disclaimer: str = (
      "This is a voice-transcribed message and may not be exactly accurate. "
      "Please ask clarifying questions if anything is unclear.")


class RenameSessionRequest(BaseModel):
  name: str


class RateSessionRequest(BaseModel):
  rating: SessionRating


class SetGroupRequest(BaseModel):
  group: Optional[str] = None


class RenameGroupRequest(BaseModel):
  old_name: str
  new_name: str


class DeleteGroupRequest(BaseModel):
  group: str


class DelegateRequest(BaseModel):
  """Request body for the internal delegation endpoint."""
  session_id: str
  description: str
  base_branch: str
  backend: Optional[str] = None
  repo_path: Optional[str] = None
  context: Optional[str] = None
  require_review: bool = True


class ImproveRequest(BaseModel):
  """Request body for the internal improve endpoint."""
  session_id: str
  repo_path: str
  base_branch: str
  backend: Optional[str] = None
  iterations: int = 3
  goal: str
  branch_prefix: Optional[str] = None


class ScheduleTriggerRequest(BaseModel):
  """Request body for the internal schedule-trigger endpoint."""
  session_id: str
  delay_seconds: int
  message: str
