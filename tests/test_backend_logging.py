"""Tests for AgentBackend stdout/stderr capture and hang_diagnostics."""
from __future__ import annotations

import os
import pathlib

import pytest

from src.agents.backends import base


class _ScriptedBackend(base.AgentBackend):
  """Minimal AgentBackend that runs an arbitrary bash -c script as the subprocess."""

  def __init__(self, script: str, **kwargs: object) -> None:
    super().__init__(**kwargs)
    self._script = script

  def _build_command(self, prompt: str) -> list[str]:
    return ["bash", "-c", self._script]


async def _consume(backend: base.AgentBackend, cwd: pathlib.Path) -> list[dict]:
  return [evt async for evt in backend.run("ignored prompt", str(cwd), {"PATH": "/usr/bin:/bin"})]


@pytest.mark.asyncio
async def test_normal_completion_writes_raw_log_no_diagnostics(tmp_path: pathlib.Path) -> None:
  """Subprocess emits NDJSON including a result event then exits cleanly."""
  log_dir = tmp_path / "logs"
  payload_lines = [
      '{"type": "assistant", "message": {"content": [{"type": "text", "text": "hi"}]}}',
      '{"type": "result", "result": "", "usage": {}}',
  ]
  joined = "\\n".join(payload_lines)
  script = f"printf '{joined}\\n'\nexit 0\n"

  backend = _ScriptedBackend(script, log_dir=log_dir)
  events = await _consume(backend, tmp_path)

  raw_log = log_dir / "agent.raw.ndjson"
  stderr_log = log_dir / "agent.stderr.log"
  assert raw_log.exists()
  assert stderr_log.exists()
  assert raw_log.read_bytes() == ("\n".join(payload_lines) + "\n").encode("utf-8")
  assert backend.exit_code == 0
  assert backend.hang_diagnostics is None
  assert not (log_dir / "hang_diagnostics.json").exists()
  assert any(e.get("type") == "result" for e in events)


@pytest.mark.asyncio
@pytest.mark.integration  # a real hung subprocess; the hang deadline is injected, not real
async def test_subprocess_hang_after_result_captures_diagnostics(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Subprocess writes result then hangs without closing stdout — diagnostics captured + SIGTERM."""
  monkeypatch.setattr(base.AgentBackend, "_POST_RESULT_TIMEOUT", 0.25)
  monkeypatch.setattr(base.AgentBackend, "_CLEANUP_TIMEOUT", 0.25)

  log_dir = tmp_path / "logs"
  result_line = '{"type": "result", "result": "", "usage": {}}'
  # printf the result line, then sleep 200s without closing stdout.
  script = f"printf '{result_line}\\n'\nsleep 200\n"

  backend = _ScriptedBackend(script, log_dir=log_dir)
  events = await _consume(backend, tmp_path)

  assert backend.hang_diagnostics is not None
  diag = backend.hang_diagnostics
  assert "captured_at" in diag
  assert diag.get("process_tree")
  assert diag.get("status")
  assert "fds" in diag
  assert "children" in diag
  assert (log_dir / "agent.raw.ndjson").read_bytes() == (result_line + "\n").encode("utf-8")
  assert backend.exit_code != 0
  assert any(e.get("type") == "result" for e in events)


@pytest.mark.asyncio
async def test_write_chunk_lands_every_byte_in_order(tmp_path: pathlib.Path) -> None:
  """The per-chunk write-all contract shared by the stderr tee and the stdout
  pumps: a short write keeps going, chunk order holds."""
  path = tmp_path / "chunk.log"
  fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o666)
  try:
    chunks = [bytes([i % 256]) * (8192 + i) for i in range(9)]
    for chunk in chunks:
      await base._write_chunk(fd, chunk)
  finally:
    os.close(fd)
  assert path.read_bytes() == b"".join(chunks)
