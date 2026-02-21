"""All Pydantic models for CharlieBot."""

import uuid
from datetime import datetime
from enum import Enum
from typing import Any, Optional

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



# ---------------------------------------------------------------------------
# Thread Models
# ---------------------------------------------------------------------------


class ThreadMetadata(BaseModel):
  id: str = Field(default_factory=lambda: str(uuid.uuid4()))
  session_id: str
  description: str
  status: ThreadStatus = ThreadStatus.IDLE
  created_at: datetime = Field(default_factory=datetime.utcnow)
  started_at: Optional[datetime] = None
  completed_at: Optional[datetime] = None
  pid: Optional[int] = None
  exit_code: Optional[int] = None
  cli_command: Optional[str] = None


# ---------------------------------------------------------------------------
# Session Models
# ---------------------------------------------------------------------------


class SessionMetadata(BaseModel):
  id: str = Field(default_factory=lambda: str(uuid.uuid4()))
  name: str
  status: SessionStatus = SessionStatus.ACTIVE
  has_unread: bool = False
  has_running_tasks: bool = False
  thinking_since: Optional[datetime] = None
  created_at: datetime = Field(default_factory=datetime.utcnow)
  updated_at: datetime = Field(default_factory=datetime.utcnow)
  cc_session_id: Optional[str] = None


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
  timestamp: datetime = Field(default_factory=datetime.utcnow)


# ---------------------------------------------------------------------------
# API Request / Response Models
# ---------------------------------------------------------------------------


class CreateSessionRequest(BaseModel):
  name: Optional[str] = None


class SendMessageRequest(BaseModel):
  content: str


class VoiceTranscriptionResponse(BaseModel):
  transcription: str
  disclaimer: str = (
    "This is a voice-transcribed message and may not be exactly accurate. "
    "Please ask clarifying questions if anything is unclear."
  )


class RenameSessionRequest(BaseModel):
  name: str


class DelegateRequest(BaseModel):
  """Request body for the internal delegation endpoint."""
  session_id: str
  description: str
  repo_path: Optional[str] = None
