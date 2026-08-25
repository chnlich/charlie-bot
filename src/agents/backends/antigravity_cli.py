"""AntigravityCliBackend — AgentBackend wrapping `agy --print` final output."""

import asyncio
import contextlib
import json
from collections.abc import AsyncIterator
from pathlib import Path
from typing import Optional

import aiofiles

from src.agents.backends.base import (
  AgentBackend,
  make_error_event,
  make_result_event,
  make_text_event,
  prepend_path_dir,
  resolve_binary,
)

_PRINT_TIMEOUT = "24h"


class AgentGuard(ValueError):
  """Raised to fail the round loudly when the agy envelope violates the resume contract."""


class AntigravityCliBackend(AgentBackend):
  """Runs `agy --print` and translates the JSON envelope into CC events."""

  def __init__(self, *, model: Optional[str] = None, **kwargs):
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
        "--output-format",
        "json",
    ]
    if self._resume_session_id:
      cmd.extend(["--conversation", self._resume_session_id])
    cmd.extend(self._extra_flags)
    return cmd

  def _prepare_env(self, env: dict) -> dict:
    antigravity_env = {**env}
    antigravity_env.pop("GEMINI_API_KEY", None)
    antigravity_env.pop("GOOGLE_API_KEY", None)
    prepend_path_dir(antigravity_env, str(Path.home() / ".local" / "bin"))
    return antigravity_env

  def _parse_envelope(self, stdout_text: str) -> Optional[dict]:
    """Parse stdout as a JSON envelope dict, or None when it is not one."""
    try:
      data = json.loads(stdout_text)
    except (json.JSONDecodeError, ValueError):
      return None
    if not isinstance(data, dict) or not isinstance(data.get("status"), str):
      return None
    return data

  async def run(self, prompt: str, cwd: str, env: dict) -> AsyncIterator[dict]:
    """Run the final-only CLI mode and translate the JSON envelope into CC events."""
    await asyncio.to_thread(self._prepare_cwd, cwd)
    cmd = self._build_command(prompt)
    final_env = self._prepare_env(env)

    await self._spawn_piped_and_pin_identity(cmd, cwd, final_env)

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
    if self.exit_code != 0:
      message = stdout_text or f"Antigravity CLI exited with code {self.exit_code}"
      yield make_error_event(message)
      return

    # exit 0: the JSON envelope is the only machine-readable source of the
    # conversation id and usage, so a non-envelope stdout is a contract breach.
    envelope = self._parse_envelope(stdout_text)
    if envelope is None:
      yield make_error_event(f"agy exited 0 with non-envelope stdout: {stdout_text[:200]}")
      raise AgentGuard(f"antigravity envelope guard: non-json stdout (exit 0): {stdout_text[:200]}")

    status = envelope.get("status")
    if status != "SUCCESS":
      message = envelope.get("error") or envelope.get("response") or f"Antigravity status {status}"
      yield make_error_event(str(message))
      return

    conversation_id = envelope.get("conversation_id")
    if not conversation_id:
      yield make_error_event("agy SUCCESS envelope missing conversation_id")
      raise AgentGuard("antigravity envelope guard: SUCCESS envelope missing conversation_id")

    if self._resume_session_id and conversation_id != self._resume_session_id:
      yield make_error_event(
          f"agy resume envelope id {conversation_id} does not match anchor {self._resume_session_id}")
      raise AgentGuard(
          f"antigravity envelope guard: resume envelope id {conversation_id} does not match "
          f"anchor {self._resume_session_id}")

    # Bare session_id event first so the master adopts it as the frozen anchor,
    # then assistant text, then usage.
    yield {"session_id": conversation_id}
    yield make_text_event(envelope.get("response", ""))
    usage = envelope.get("usage", {}) or {}
    yield make_result_event(
        input_tokens=usage.get("input_tokens", 0),
        output_tokens=usage.get("output_tokens", 0) + usage.get("thinking_tokens", 0),
        cache_read=usage.get("cache_read_tokens", 0),
    )
