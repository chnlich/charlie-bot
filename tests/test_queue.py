"""Tests for src/core/queue.py (QueueManager)."""

import asyncio
import pytest

from src.core.config import load_config
from src.core.models import Priority, Task, TaskStatus
from src.core.queue import QueueManager, _sort_tasks, _PRIORITY_ORDER
from src.core.models import TaskQueue


# ---------------------------------------------------------------------------
# _sort_tasks (pure function — no I/O)
# ---------------------------------------------------------------------------

class TestSortTasks:
    def _make_queue(self, priorities):
        tasks = [Task(priority=p, description=f"task {p}") for p in priorities]
        return TaskQueue(session_id="s", tasks=tasks)

    def test_p0_before_p1_before_p2(self):
        q = self._make_queue([Priority.P2, Priority.P0, Priority.P1])
        _sort_tasks(q)
        assert [t.priority for t in q.tasks] == [Priority.P0, Priority.P1, Priority.P2]

    def test_same_priority_ordered_by_created_at(self):
        import time
        t1 = Task(priority=Priority.P1, description="first")
        time.sleep(0.01)
        t2 = Task(priority=Priority.P1, description="second")
        q = TaskQueue(session_id="s", tasks=[t2, t1])
        _sort_tasks(q)
        assert q.tasks[0].description == "first"
        assert q.tasks[1].description == "second"

    def test_mixed_priorities_stable_within_group(self):
        t_p0a = Task(priority=Priority.P0, description="p0a")
        t_p1a = Task(priority=Priority.P1, description="p1a")
        t_p0b = Task(priority=Priority.P0, description="p0b")
        q = TaskQueue(session_id="s", tasks=[t_p1a, t_p0b, t_p0a])
        _sort_tasks(q)
        assert q.tasks[0].priority == Priority.P0
        assert q.tasks[1].priority == Priority.P0
        assert q.tasks[2].priority == Priority.P1


# ---------------------------------------------------------------------------
# QueueManager (uses tmp filesystem via tmp_home fixture)
# ---------------------------------------------------------------------------

@pytest.fixture()
def queue(tmp_home):
    cfg = load_config()
    cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
    session_dir = cfg.sessions_dir / "test-session"
    session_dir.mkdir(parents=True, exist_ok=True)
    # Pre-create an empty queue file (as the real SessionManager would)
    queue_path = session_dir / "task_queue.json"
    import json
    queue_path.write_text(json.dumps({"session_id": "test-session", "tasks": [], "updated_at": "2024-01-01T00:00:00"}))
    return QueueManager("test-session", cfg)


@pytest.mark.asyncio
class TestQueueManager:
    async def test_push_and_pop(self, queue):
        task = Task(priority=Priority.P1, description="do work")
        await queue.push(task)
        popped = await queue.pop_next()
        assert popped is not None
        assert popped.id == task.id
        assert popped.status == TaskStatus.RUNNING

    async def test_pop_returns_none_when_empty(self, queue):
        result = await queue.pop_next()
        assert result is None

    async def test_pop_skips_non_pending(self, queue):
        task = Task(priority=Priority.P1, description="work")
        await queue.push(task)
        # Manually mark as cancelled via cancel()
        await queue.cancel(task.id)
        result = await queue.pop_next()
        assert result is None

    async def test_priority_ordering(self, queue):
        t_low = Task(priority=Priority.P2, description="low")
        t_high = Task(priority=Priority.P0, description="high")
        t_mid = Task(priority=Priority.P1, description="mid")
        await queue.push(t_low)
        await queue.push(t_high)
        await queue.push(t_mid)

        first = await queue.pop_next()
        second = await queue.pop_next()
        third = await queue.pop_next()

        assert first.priority == Priority.P0
        assert second.priority == Priority.P1
        assert third.priority == Priority.P2

    async def test_reorder_changes_priority(self, queue):
        task = Task(priority=Priority.P2, description="needs upgrade")
        await queue.push(task)
        await queue.reorder(task.id, Priority.P0)

        q = await queue.get_queue()
        updated = next(t for t in q.tasks if t.id == task.id)
        assert updated.priority == Priority.P0

    async def test_reorder_does_not_affect_running_tasks(self, queue):
        task = Task(priority=Priority.P2, description="running")
        await queue.push(task)
        await queue.pop_next()  # Marks as RUNNING

        # Attempt to reorder — should silently do nothing
        await queue.reorder(task.id, Priority.P0)
        q = await queue.get_queue()
        updated = next(t for t in q.tasks if t.id == task.id)
        assert updated.priority == Priority.P2  # Unchanged

    async def test_cancel_pending_task(self, queue):
        task = Task(priority=Priority.P1, description="cancel me")
        await queue.push(task)
        await queue.cancel(task.id)

        q = await queue.get_queue()
        t = next(t for t in q.tasks if t.id == task.id)
        assert t.status == TaskStatus.CANCELLED

    async def test_cancel_running_task_is_noop(self, queue):
        task = Task(priority=Priority.P1, description="running")
        await queue.push(task)
        await queue.pop_next()  # Marks as RUNNING
        await queue.cancel(task.id)  # Should not cancel

        q = await queue.get_queue()
        t = next(t for t in q.tasks if t.id == task.id)
        assert t.status == TaskStatus.RUNNING

    async def test_mark_complete(self, queue):
        task = Task(priority=Priority.P1, description="x")
        await queue.push(task)
        await queue.pop_next()
        await queue.mark_complete(task.id)

        q = await queue.get_queue()
        t = next(t for t in q.tasks if t.id == task.id)
        assert t.status == TaskStatus.COMPLETED

    async def test_mark_failed(self, queue):
        task = Task(priority=Priority.P1, description="x")
        await queue.push(task)
        await queue.pop_next()
        await queue.mark_failed(task.id)

        q = await queue.get_queue()
        t = next(t for t in q.tasks if t.id == task.id)
        assert t.status == TaskStatus.FAILED

    async def test_mark_pending_quota_and_requeue(self, queue):
        task = Task(priority=Priority.P1, description="quota")
        await queue.push(task)
        await queue.pop_next()
        await queue.mark_pending_quota(task.id)

        q = await queue.get_queue()
        t = next(t for t in q.tasks if t.id == task.id)
        assert t.status == TaskStatus.PENDING_QUOTA

        count = await queue.requeue_pending_quota()
        assert count == 1

        q = await queue.get_queue()
        t = next(t for t in q.tasks if t.id == task.id)
        assert t.status == TaskStatus.PENDING

    async def test_requeue_returns_zero_when_none(self, queue):
        count = await queue.requeue_pending_quota()
        assert count == 0

    async def test_atomic_save_creates_no_tmp_residue(self, queue):
        """Ensure .tmp file is removed after save."""
        task = Task(priority=Priority.P1, description="atomic")
        await queue.push(task)
        tmp_path = queue._queue_path.with_suffix(".tmp")
        assert not tmp_path.exists()

    async def test_multiple_tasks_pop_order(self, queue):
        """Push 5 tasks with mixed priorities and verify pop order."""
        tasks = [
            Task(priority=Priority.P2, description="bg1"),
            Task(priority=Priority.P0, description="urgent"),
            Task(priority=Priority.P1, description="normal"),
            Task(priority=Priority.P0, description="urgent2"),
            Task(priority=Priority.P2, description="bg2"),
        ]
        for t in tasks:
            await queue.push(t)

        popped_priorities = []
        while True:
            t = await queue.pop_next()
            if t is None:
                break
            popped_priorities.append(t.priority)

        assert popped_priorities[0] == Priority.P0
        assert popped_priorities[1] == Priority.P0
        assert popped_priorities[2] == Priority.P1
        assert popped_priorities[3] == Priority.P2
        assert popped_priorities[4] == Priority.P2
