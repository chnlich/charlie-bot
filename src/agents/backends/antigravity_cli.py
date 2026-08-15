"""AntigravityCliBackend — AgentBackend wrapping `agy --print` final output."""

import asyncio
import contextlib
from collections.abc import AsyncIterator
from pathlib import Path
from typing import Optional

import aiofiles

from src.agents.backends.base import (
  AgentBackend,
  make_error_event,
  make_result_event,
  make_text_event,
  resolve_binary,
)
from src.core import runs

_PRINT_TIMEOUT = "24h"


class AntigravityCliBackend(AgentBackend):
  """Runs `agy --print` and emits the completed stdout as one assistant event."""

  def __init__(self, *, model: Optional[str] = None, **kwargs):
    if kwargs.get("resume_session_id"):
      raise ValueError("antigravity backend does not support stable session resume")
    super().__init__(model=model, **kwargs)
    self._agy_bin = resolve_binary("agy", str(Path.home() / ".local" / "bin"))

  def _build_command(self, prompt: str) -> list[str]:
    effective_prompt = self._effective_prompt(prompt)
    cmd = [
        self._agy_bin,
        f"--print={effective_prompt}",
        "--print-timeout",
        _PRINT_TIMEOUT,
        "--dangerously-skip-permissions",
    ]
    cmd.extend(self._extra_flags)
    return cmd

  def _prepare_env(self, env: dict) -> dict:
    antigravity_env = {**env}
    antigravity_env.pop("GEMINI_API_KEY", None)
    antigravity_env.pop("GOOGLE_API_KEY", None)
    local_bin = str(Path.home() / ".local" / "bin")
    current_path = antigravity_env.get("PATH", "")
    if local_bin not in current_path.split(":"):
      antigravity_env["PATH"] = f"{local_bin}:{current_path}"
    return antigravity_env

  async def run(self, prompt: str, cwd: str, env: dict) -> AsyncIterator[dict]:
    """Run the final-only CLI mode and yield stdout after the process exits."""
    await asyncio.to_thread(self._prepare_cwd, cwd)
    cmd = self._build_command(prompt)
    final_env = self._prepare_env(env)

    self._proc = await asyncio.create_subprocess_exec(
        *cmd,
        cwd=cwd,
        stdin=asyncio.subprocess.DEVNULL,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        env=final_env,
        limit=self._buffer_limit,
        start_new_session=True,
    )
    # Pin the process identity BEFORE on_spawn so the callback can persist
    # (pid, pid_start) together; a proc that exited before we could read its
    # stat simply yields None and can never be judged alive later.
    stat_pair = runs.read_pid_stat(self._proc.pid)
    self.pid_start = stat_pair[0] if stat_pair else None
    if self._on_spawn is not None:
      await self._on_spawn(self._proc.pid)

    assert self._proc.stdout is not None
    stdout_bytes = bytearray()

    if self._log_dir is not None:
      self._log_dir.mkdir(parents=True, exist_ok=True)
      stdout_log_cm = aiofiles.open(self._log_dir / "stdout.log", "wb")
      stderr_log_path: Optional[Path] = self._log_dir / "stderr.log"
    else:
      stdout_log_cm = contextlib.nullcontext(None)
      stderr_log_path = None

    self._stderr_task = asyncio.create_task(self._stream_stderr(stderr_log_path))

    async with stdout_log_cm as stdout_log:
      while True:
        chunk = await self._proc.stdout.read(8192)
        if not chunk:
          break
        if stdout_log is not None:
          await stdout_log.write(chunk)
          await stdout_log.flush()
        stdout_bytes.extend(chunk)

    await self._drain_and_cleanup(self._CLEANUP_TIMEOUT)

    stdout_text = bytes(stdout_bytes).decode("utf-8", errors="replace").strip()
    if self.exit_code == 0:
      if stdout_text:
        yield make_text_event(stdout_text)
      yield make_result_event()
      return

    message = stdout_text or f"Antigravity CLI exited with code {self.exit_code}"
    yield make_error_event(message)
