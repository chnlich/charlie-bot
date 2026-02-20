"""All Pydantic models for CharlieBot."""

import uuid
from datetime import datetime
from enum import Enum
from typing import Any, Optional

from pydantic import BaseModel, Field


# ---------------------------------------------------------------------------
# Enums
# ---------------------------------------------------------------------------


class Priority(str, Enum):
  P0 = "P0"  # Immediate
  P1 = "P1"  # Standard
  P2 = "P2"  # Background


class TaskStatus(str, Enum):
  PENDING = "pending"
  RUNNING = "running"
  COMPLETED = "completed"
  FAILED = "failed"
  PENDING_QUOTA = "pending_quota"
  CANCELLED = "cancelled"


class ThreadStatus(str, Enum):
  IDLE = "idle"
  PLANNING = "planning"
  RUNNING = "running"
  AWAITING_APPROVAL = "awaiting_approval"
  COMPLETED = "completed"
  FAILED = "failed"
  CONFLICT = "conflict"
  CANCELLED = "cancelled"


class SessionStatus(str, Enum):
  ACTIVE = "active"
  ARCHIVED = "archived"


class MessageRole(str, Enum):
  USER = "user"
  ASSISTANT = "assistant"
  SYSTEM = "system"


# ---------------------------------------------------------------------------
# Task Queue Models
# ---------------------------------------------------------------------------


class Task(BaseModel):
  id: str = Field(default_factory=lambda: str(uuid.uuid4()))
  priority: Priority
  description: str
  created_at: datetime = Field(default_factory=datetime.utcnow)
  status: TaskStatus = TaskStatus.PENDING
  thread_id: Optional[str] = None
  plan_steps: Optional[list[str]] = None
  is_plan_mode: bool = False
  context: dict[str, Any] = Field(default_factory=dict)


class TaskQueue(BaseModel):
  session_id: str
  tasks: list[Task] = Field(default_factory=list)
  updated_at: datetime = Field(default_factory=datetime.utcnow)


# ---------------------------------------------------------------------------
# Thread Models
# ---------------------------------------------------------------------------


class ThreadMetadata(BaseModel):
  id: str = Field(default_factory=lambda: str(uuid.uuid4()))
  session_id: str
  task_id: str
  description: str
  branch_name: str
  status: ThreadStatus = ThreadStatus.IDLE
  created_at: datetime = Field(default_factory=datetime.utcnow)
  started_at: Optional[datetime] = None
  completed_at: Optional[datetime] = None
  pid: Optional[int] = None
  exit_code: Optional[int] = None
  cli_command: Optional[str] = None
  worktree_path: Optional[str] = None
  base_branch: Optional[str] = None
  is_conflict_resolver: bool = False


# ---------------------------------------------------------------------------
# Session Models
# ---------------------------------------------------------------------------


class SessionMetadata(BaseModel):
  id: str = Field(default_factory=lambda: str(uuid.uuid4()))
  name: str
  repo_path: Optional[str] = None
  status: SessionStatus = SessionStatus.ACTIVE
  has_unread: bool = False
  created_at: datetime = Field(default_factory=datetime.utcnow)
  updated_at: datetime = Field(default_factory=datetime.utcnow)
  cc_session_id: Optional[str] = None


# ---------------------------------------------------------------------------
# Chat Models
# ---------------------------------------------------------------------------


class ChatMessage(BaseModel):
  id: str = Field(default_factory=lambda: str(uuid.uuid4()))
  role: MessageRole
  content: str
  timestamp: datetime = Field(default_factory=datetime.utcnow)
  is_voice: bool = False
  thread_id: Optional[str] = None


class ConversationHistory(BaseModel):
  session_id: str
  messages: list[ChatMessage] = Field(default_factory=list)
  summary: Optional[str] = None


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
  repo_path: Optional[str] = None


class SendMessageRequest(BaseModel):
  content: str


class VoiceTranscriptionResponse(BaseModel):
  transcription: str
  disclaimer: str = (
    "This is a voice-transcribed message and may not be exactly accurate. "
    "Please ask clarifying questions if anything is unclear."
  )


class PlanApprovalRequest(BaseModel):
  approved_steps: list[str]
  edited_steps: Optional[list[str]] = None


class RenameSessionRequest(BaseModel):
  name: str


class ReorderTaskRequest(BaseModel):
  task_id: str
  priority: Priority


class TaskDelegationResult(BaseModel):
  """Returned when Master Agent delegates a task to the queue."""
  task_id: str
  priority: Priority
  description: str
  plan_mode: bool
  message: str


class DelegateRequest(BaseModel):
  """Request body for the internal delegation endpoint."""
  session_id: str
  description: str
  priority: Priority = Priority.P1
  plan_mode: bool = False
