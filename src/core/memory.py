"""Thread-safe memory file management for CharlieBot."""

import asyncio

import aiofiles

from src.core.config import CharliBotConfig


class MemoryManager:
  """Manages MEMORY.md and PROGRESS.md with concurrency guards."""

  def __init__(self, cfg: CharliBotConfig):
    self._cfg = cfg
    self._memory_lock = asyncio.Lock()
    self._progress_lock = asyncio.Lock()

  async def read_memory(self) -> str:
    async with self._memory_lock:
      return self._cfg.memory_file.read_text(encoding="utf-8")

  async def append_memory(self, content: str) -> None:
    async with self._memory_lock:
      async with aiofiles.open(self._cfg.memory_file, "a", encoding="utf-8") as f:
        await f.write(f"\n{content.strip()}\n")

  async def read_progress(self) -> str:
    async with self._progress_lock:
      return self._cfg.progress_file.read_text(encoding="utf-8")

  async def append_progress(self, insight: str) -> None:
    async with self._progress_lock:
      async with aiofiles.open(self._cfg.progress_file, "a", encoding="utf-8") as f:
        await f.write(f"\n- {insight.strip()}\n")
