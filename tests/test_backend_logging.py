"""Tests for AgentBackend stdout/stderr capture and hang_diagnostics."""
from __future__ import annotations

import asyncio
from pathlib import Path

import pytest

from src.agents.backends.base import AgentBackend


class _ScriptedBackend(AgentBackend):
  """Minimal AgentBackend that runs an arbitrary bash -c script as the subprocess."""

  def __init__(self, script: str, **kwargs):
    super().__init__(**kwargs)
    self._script = script

  def _build_command(self, prompt: str) -> list[str]:
    return ["bash", "-c", self._script]


async def _consume(backend: AgentBackend, cwd: Path) -> list[dict]:
  events: list[dict] = []
  async for evt in backend.run("ignored prompt", str(cwd), {"PATH": "/usr/bin:/bin"}):
    events.append(evt)
  return events


@pytest.mark.asyncio
async def test_normal_completion_writes_raw_log_no_diagnostics(tmp_path: Path) -> None:
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
async def test_subprocess_hang_after_result_captures_diagnostics(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  """Subprocess writes result then hangs without closing stdout — diagnostics captured + SIGTERM."""
  monkeypatch.setattr(AgentBackend, "_POST_RESULT_TIMEOUT", 1.0)
  monkeypatch.setattr(AgentBackend, "_CLEANUP_TIMEOUT", 1.0)

  log_dir = tmp_path / "logs"
  result_line = '{"type": "result", "result": "", "usage": {}}'
  # printf the result line, then sleep 200s without closing stdout.
  script = f"printf '{result_line}\\n'\nsleep 200\n"

  backend = _ScriptedBackend(script, log_dir=log_dir)
  events = await _consume(backend, tmp_path)

  assert backend.hang_diagnostics is not None
  diag = backend.hang_diagnostics
  assert "captured_at" in diag
  assert "process_tree" in diag and diag["process_tree"]
  assert "status" in diag and diag["status"]
  assert "fds" in diag
  assert "children" in diag
  assert (log_dir / "agent.raw.ndjson").read_bytes() == (result_line + "\n").encode("utf-8")
  assert backend.exit_code != 0
  assert any(e.get("type") == "result" for e in events)


@pytest.mark.asyncio
async def test_stderr_streams_live(tmp_path: Path) -> None:
  """Subprocess writes to stderr periodically — agent.stderr.log mtime advances during the run."""
  log_dir = tmp_path / "logs"
  # Write 5 stderr lines spaced 0.1s apart, then exit. Total ~0.5s.
  script = (
      "for i in 1 2 3 4 5; do "
      "echo \"err line $i\" 1>&2; sleep 0.1; "
      "done\n"
      'printf \'{"type": "result", "result": "", "usage": {}}\\n\'\n'
      "exit 0\n"
  )

  backend = _ScriptedBackend(script, log_dir=log_dir)
  stderr_log = log_dir / "agent.stderr.log"
  mtimes: list[float] = []

  async def _poll_mtime() -> None:
    for _ in range(20):
      await asyncio.sleep(0.1)
      if stderr_log.exists():
        mtimes.append(stderr_log.stat().st_mtime_ns)

  poll_task = asyncio.create_task(_poll_mtime())
  await _consume(backend, tmp_path)
  poll_task.cancel()
  try:
    await poll_task
  except asyncio.CancelledError:
    pass

  assert stderr_log.exists()
  contents = stderr_log.read_text(encoding="utf-8")
  assert "err line 1" in contents
  assert "err line 5" in contents
  # mtime should have advanced at least twice during the run (proves live streaming).
  unique_mtimes = sorted(set(mtimes))
  assert len(unique_mtimes) >= 2, f"stderr.log mtime did not advance during run: {mtimes}"
  assert backend.exit_code == 0


@pytest.mark.asyncio
async def test_no_log_dir_still_populates_stderr_text(tmp_path: Path) -> None:
  """Construct backend without log_dir; stderr_text still populated, no crash."""
  script = (
      "echo \"on stderr\" 1>&2\n"
      'printf \'{"type": "result", "result": "", "usage": {}}\\n\'\n'
      "exit 0\n"
  )
  backend = _ScriptedBackend(script)
  await _consume(backend, tmp_path)
  assert "on stderr" in backend.stderr_text
  assert backend.exit_code == 0
  assert backend.hang_diagnostics is None
