"""All Pydantic models for CharlieBot."""

import uuid
from datetime import datetime, timezone
from enum import Enum
from typing import Optional

from pydantic import BaseModel, Field

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
  WAITING = "waiting"


# ---------------------------------------------------------------------------
# Thread Models
# ---------------------------------------------------------------------------


class ThreadMetadata(BaseModel):
  id: str = Field(default_factory=lambda: str(uuid.uuid4()))
  session_id: str
  description: str
  status: ThreadStatus = ThreadStatus.IDLE
  created_at: datetime = Field(default_factory=lambda: datetime.now(timezone.utc))
  started_at: Optional[datetime] = None
  completed_at: Optional[datetime] = None
  pid: Optional[int] = None
  exit_code: Optional[int] = None
  cli_command: Optional[str] = None


# ---------------------------------------------------------------------------
# Backend Models
# ---------------------------------------------------------------------------


class BackendOption(BaseModel):
  id: str
  label: str
  type: str  # 'cc-claude' | 'cc-kimi'
  model: Optional[str] = None


# ---------------------------------------------------------------------------
# Session Models
# ---------------------------------------------------------------------------


class SessionMetadata(BaseModel):
  id: str = Field(default_factory=lambda: str(uuid.uuid4()))
  name: str
  status: SessionStatus = SessionStatus.ACTIVE
  has_unread: bool = False
  has_running_tasks: bool = False
  starred: bool = False
  thinking_since: Optional[datetime] = None
  created_at: datetime = Field(default_factory=lambda: datetime.now(timezone.utc))
  updated_at: datetime = Field(default_factory=lambda: datetime.now(timezone.utc))
  cc_session_id: Optional[str] = None
  backend: str = "claude-opus-4.6"  # must match an id in cfg.backend_options
  scheduled_task: Optional[str] = None  # task name; None = regular session
  last_scheduled_run: Optional[str] = None  # ISO datetime of last scheduler execution
  # Transient fields, populated by API layer for scheduled sessions only
  schedule_cron: Optional[str] = None
  schedule_enabled: Optional[bool] = None
  schedule_next_run: Optional[str] = None
  schedule_timezone: Optional[str] = None
  # Rewind fields
  parent_session_id: Optional[str] = None  # original session this was rewound from
  rewind_summary: Optional[str] = None  # context summary from parent session


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
  timestamp: datetime = Field(default_factory=lambda: datetime.now(timezone.utc))


# ---------------------------------------------------------------------------
# API Request / Response Models
# ---------------------------------------------------------------------------


class CreateSessionRequest(BaseModel):
  name: Optional[str] = None
  scheduled_task: Optional[str] = None


class SendMessageRequest(BaseModel):
  content: str
  uploaded_files: list[str] = []


class VoiceTranscriptionResponse(BaseModel):
  transcription: str
  disclaimer: str = (
      "This is a voice-transcribed message and may not be exactly accurate. "
      "Please ask clarifying questions if anything is unclear.")


class RenameSessionRequest(BaseModel):
  name: str


class DelegateRequest(BaseModel):
  """Request body for the internal delegation endpoint."""
  session_id: str
  description: str
  repo_path: Optional[str] = None
