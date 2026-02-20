"""Thread-safe memory file management for CharlieBot."""

import asyncio

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

  async def read_progress(self) -> str:
    async with self._progress_lock:
      return self._cfg.progress_file.read_text(encoding="utf-8")
