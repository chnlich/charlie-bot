"""Tests for src/core/models.py."""

import pytest
from datetime import datetime

from src.core.models import (
    Priority,
    TaskStatus,
    ThreadStatus,
    SessionStatus,
    MessageRole,
    Task,
    TaskQueue,
    ThreadMetadata,
    SessionMetadata,
    ChatMessage,
    ConversationHistory,
    WorkerEvent,
    CreateSessionRequest,
    SendMessageRequest,
    PlanApprovalRequest,
    ReorderTaskRequest,
    TaskDelegationResult,
)


# ---------------------------------------------------------------------------
# Enum sanity checks
# ---------------------------------------------------------------------------

class TestEnums:
    def test_priority_values(self):
        assert Priority.P0 == "P0"
        assert Priority.P1 == "P1"
        assert Priority.P2 == "P2"

    def test_task_status_values(self):
        assert TaskStatus.PENDING == "pending"
        assert TaskStatus.RUNNING == "running"
        assert TaskStatus.COMPLETED == "completed"
        assert TaskStatus.FAILED == "failed"
        assert TaskStatus.CANCELLED == "cancelled"
        assert TaskStatus.PENDING_QUOTA == "pending_quota"

    def test_thread_status_values(self):
        assert ThreadStatus.IDLE == "idle"
        assert ThreadStatus.COMPLETED == "completed"
        assert ThreadStatus.FAILED == "failed"
        assert ThreadStatus.CONFLICT == "conflict"

    def test_message_role_values(self):
        assert MessageRole.USER == "user"
        assert MessageRole.ASSISTANT == "assistant"
        assert MessageRole.SYSTEM == "system"


# ---------------------------------------------------------------------------
# Task model
# ---------------------------------------------------------------------------

class TestTask:
    def test_default_values(self):
        task = Task(priority=Priority.P1, description="do something")
        assert task.status == TaskStatus.PENDING
        assert task.is_plan_mode is False
        assert task.context == {}
        assert task.plan_steps is None
        assert task.thread_id is None
        assert isinstance(task.id, str) and len(task.id) > 0
        assert isinstance(task.created_at, datetime)

    def test_unique_ids(self):
        t1 = Task(priority=Priority.P0, description="a")
        t2 = Task(priority=Priority.P0, description="b")
        assert t1.id != t2.id

    def test_plan_mode(self):
        task = Task(priority=Priority.P1, description="plan this", is_plan_mode=True)
        assert task.is_plan_mode is True

    def test_context_stored(self):
        task = Task(priority=Priority.P2, description="bg", context={"key": "value"})
        assert task.context["key"] == "value"

    def test_roundtrip_json(self):
        task = Task(priority=Priority.P0, description="json test")
        restored = Task.model_validate_json(task.model_dump_json())
        assert restored.id == task.id
        assert restored.priority == task.priority
        assert restored.status == task.status


# ---------------------------------------------------------------------------
# TaskQueue model
# ---------------------------------------------------------------------------

class TestTaskQueue:
    def test_empty_queue(self):
        q = TaskQueue(session_id="sess-1")
        assert q.tasks == []

    def test_with_tasks(self):
        t = Task(priority=Priority.P1, description="x")
        q = TaskQueue(session_id="sess-2", tasks=[t])
        assert len(q.tasks) == 1
        assert q.tasks[0].id == t.id


# ---------------------------------------------------------------------------
# ThreadMetadata
# ---------------------------------------------------------------------------

class TestThreadMetadata:
    def test_defaults(self):
        thread = ThreadMetadata(session_id="s", task_id="t", description="desc", branch_name="br")
        assert thread.status == ThreadStatus.IDLE
        assert thread.pid is None
        assert thread.exit_code is None
        assert thread.started_at is None
        assert thread.completed_at is None
        assert thread.is_conflict_resolver is False

    def test_roundtrip_json(self):
        thread = ThreadMetadata(session_id="s", task_id="t", description="d", branch_name="b")
        restored = ThreadMetadata.model_validate_json(thread.model_dump_json())
        assert restored.id == thread.id


# ---------------------------------------------------------------------------
# SessionMetadata
# ---------------------------------------------------------------------------

class TestSessionMetadata:
    def test_defaults(self):
        s = SessionMetadata(name="my-session")
        assert s.status == SessionStatus.ACTIVE
        assert s.base_branch == "main"
        assert s.repo_url is None
        assert s.repo_path is None

    def test_roundtrip_json(self):
        s = SessionMetadata(name="test", repo_url="https://example.com/repo.git")
        restored = SessionMetadata.model_validate_json(s.model_dump_json())
        assert restored.name == "test"
        assert restored.repo_url == "https://example.com/repo.git"


# ---------------------------------------------------------------------------
# ChatMessage & ConversationHistory
# ---------------------------------------------------------------------------

class TestChatMessage:
    def test_defaults(self):
        msg = ChatMessage(role=MessageRole.USER, content="hello")
        assert msg.is_voice is False
        assert msg.thread_id is None

    def test_voice_flag(self):
        msg = ChatMessage(role=MessageRole.ASSISTANT, content="hi", is_voice=True)
        assert msg.is_voice is True


class TestConversationHistory:
    def test_empty_history(self):
        h = ConversationHistory(session_id="s")
        assert h.messages == []
        assert h.summary is None

    def test_append_message(self):
        h = ConversationHistory(session_id="s")
        h.messages.append(ChatMessage(role=MessageRole.USER, content="hello"))
        assert len(h.messages) == 1


# ---------------------------------------------------------------------------
# WorkerEvent
# ---------------------------------------------------------------------------

class TestWorkerEvent:
    def test_minimal(self):
        e = WorkerEvent(type="output")
        assert e.content is None
        assert e.path is None

    def test_with_fields(self):
        e = WorkerEvent(type="file_edit", path="/foo/bar.py", lines_added=5)
        assert e.path == "/foo/bar.py"
        assert e.lines_added == 5


# ---------------------------------------------------------------------------
# API Request/Response models
# ---------------------------------------------------------------------------

class TestApiModels:
    def test_create_session_request_defaults(self):
        req = CreateSessionRequest(name="my-session")
        assert req.base_branch == "main"
        assert req.repo_url is None

    def test_send_message_request(self):
        req = SendMessageRequest(content="do the thing")
        assert req.content == "do the thing"

    def test_plan_approval_request(self):
        req = PlanApprovalRequest(approved_steps=["step1", "step2"])
        assert len(req.approved_steps) == 2
        assert req.edited_steps is None

    def test_reorder_task_request(self):
        req = ReorderTaskRequest(task_id="abc", priority=Priority.P0)
        assert req.priority == Priority.P0

    def test_task_delegation_result(self):
        r = TaskDelegationResult(
            task_id="tid",
            priority=Priority.P1,
            description="do something",
            plan_mode=False,
            message="queued",
        )
        assert r.task_id == "tid"
        assert r.plan_mode is False
