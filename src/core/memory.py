"""Thread-safe memory file management for CharlieBot."""

import asyncio
from pathlib import Path

import aiofiles

from src.core.config import CharliBotConfig


class MemoryManager:
  """Manages MEMORY.md, PAST_TASKS.md, and PROGRESS.md with concurrency guards."""

  def __init__(self, cfg: CharliBotConfig):
    self._cfg = cfg
    self._memory_lock = asyncio.Lock()
    self._past_tasks_lock = asyncio.Lock()
    self._progress_lock = asyncio.Lock()

  async def read_memory(self) -> str:
    async with self._memory_lock:
      return self._cfg.memory_file.read_text(encoding="utf-8")

  async def append_memory(self, content: str) -> None:
    async with self._memory_lock:
      async with aiofiles.open(self._cfg.memory_file, "a", encoding="utf-8") as f:
        await f.write(f"\n{content.strip()}\n")

  async def read_past_tasks(self) -> str:
    async with self._past_tasks_lock:
      return self._cfg.past_tasks_file.read_text(encoding="utf-8")

  async def append_past_task(self, task_summary: str) -> None:
    """Append a completed task record to PAST_TASKS.md."""
    async with self._past_tasks_lock:
      async with aiofiles.open(self._cfg.past_tasks_file, "a", encoding="utf-8") as f:
        await f.write(f"\n---\n\n{task_summary.strip()}\n")

  async def search_past_tasks(self, query: str, max_results: int = 5) -> list[str]:
    """
    Keyword-based search over PAST_TASKS.md.
    Each task record is delimited by '---'.
    Returns up to max_results best-matching chunks.
    """
    async with self._past_tasks_lock:
      content = self._cfg.past_tasks_file.read_text(encoding="utf-8")

    chunks = [c.strip() for c in content.split("---") if c.strip()]
    if not query.strip():
      return chunks[:max_results]

    query_words = set(query.lower().split())
    scored: list[tuple[int, str]] = []
    for chunk in chunks:
      words = set(chunk.lower().split())
      score = len(query_words & words)
      if score > 0:
        scored.append((score, chunk))

    scored.sort(key=lambda x: x[0], reverse=True)
    return [chunk for _, chunk in scored[:max_results]]

  async def read_progress(self) -> str:
    async with self._progress_lock:
      return self._cfg.progress_file.read_text(encoding="utf-8")

  async def append_progress(self, insight: str) -> None:
    async with self._progress_lock:
      async with aiofiles.open(self._cfg.progress_file, "a", encoding="utf-8") as f:
        await f.write(f"\n- {insight.strip()}\n")
