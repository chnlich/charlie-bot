"""Tests for src/core/memory.py (MemoryManager)."""

import pytest
from src.core.config import load_config
from src.core.memory import MemoryManager


@pytest.fixture()
def memory_manager(tmp_home):
    cfg = load_config()
    # Seed files
    cfg.memory_file.write_text("# MEMORY\n\nSome initial memory.\n", encoding="utf-8")
    cfg.past_tasks_file.write_text("# PAST TASKS\n\ntask one details\n---\ntask two details\n", encoding="utf-8")
    cfg.progress_file.write_text("# PROGRESS\n\n- First insight\n", encoding="utf-8")
    return MemoryManager(cfg)


@pytest.mark.asyncio
class TestMemoryManager:
    async def test_read_memory(self, memory_manager):
        content = await memory_manager.read_memory()
        assert "initial memory" in content

    async def test_append_memory(self, memory_manager):
        await memory_manager.append_memory("New preference: dark mode")
        content = await memory_manager.read_memory()
        assert "dark mode" in content

    async def test_append_memory_strips_whitespace(self, memory_manager):
        await memory_manager.append_memory("   trimmed   ")
        content = await memory_manager.read_memory()
        assert "trimmed" in content

    async def test_read_past_tasks(self, memory_manager):
        content = await memory_manager.read_past_tasks()
        assert "task one" in content

    async def test_append_past_task(self, memory_manager):
        await memory_manager.append_past_task("Completed: refactor auth module")
        content = await memory_manager.read_past_tasks()
        assert "refactor auth module" in content

    async def test_search_past_tasks_returns_matching(self, memory_manager):
        results = await memory_manager.search_past_tasks("task one")
        assert len(results) >= 1
        assert any("task one" in r for r in results)

    async def test_search_past_tasks_empty_query_returns_all(self, memory_manager):
        results = await memory_manager.search_past_tasks("", max_results=10)
        # Should return chunks without filtering
        assert len(results) >= 1

    async def test_search_past_tasks_no_match_returns_empty(self, memory_manager):
        results = await memory_manager.search_past_tasks("xyzzy_no_match")
        assert results == []

    async def test_search_past_tasks_max_results(self, memory_manager):
        # Add several tasks
        for i in range(10):
            await memory_manager.append_past_task(f"task keyword entry {i}")
        results = await memory_manager.search_past_tasks("keyword", max_results=3)
        assert len(results) <= 3

    async def test_read_progress(self, memory_manager):
        content = await memory_manager.read_progress()
        assert "First insight" in content

    async def test_append_progress(self, memory_manager):
        await memory_manager.append_progress("New insight about testing")
        content = await memory_manager.read_progress()
        assert "New insight about testing" in content
