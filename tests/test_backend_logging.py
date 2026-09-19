"""Tests for AgentBackend stdout/stderr capture and hang_diagnostics."""
from __future__ import annotations

import asyncio
import os
from collections.abc import Awaitable, Callable
from pathlib import Path

import pytest
from conftest import cancel_and_drain

from src.agents.backends.base import AgentBackend


class _ScriptedBackend(AgentBackend):
  """Minimal AgentBackend that runs an arbitrary bash -c script as the subprocess."""

  def __init__(self, script: str, **kwargs: object) -> None:
    super().__init__(**kwargs)
    self._script = script

  def _build_command(self, prompt: str) -> list[str]:
    return ["bash", "-c", self._script]


async def _consume(backend: AgentBackend, cwd: Path) -> list[dict]:
  return [evt async for evt in backend.run("ignored prompt", str(cwd), {"PATH": "/usr/bin:/bin"})]


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
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
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
  assert diag.get("process_tree")
  assert diag.get("status")
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
      'echo "err line $i" 1>&2; sleep 0.1; '
      "done\n"
      'printf \'{"type": "result", "result": "", "usage": {}}\\n\'\n'
      "exit 0\n")

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
  await cancel_and_drain(poll_task)

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
  script = ('echo "on stderr" 1>&2\n'
            'printf \'{"type": "result", "result": "", "usage": {}}\\n\'\n'
            "exit 0\n")
  backend = _ScriptedBackend(script)
  await _consume(backend, tmp_path)
  assert "on stderr" in backend.stderr_text
  assert backend.exit_code == 0
  assert backend.hang_diagnostics is None


@pytest.mark.asyncio
async def test_write_chunk_lands_every_byte_in_order(tmp_path: Path) -> None:
  """The per-chunk write-all contract shared by the stderr tee and the stdout
  pumps: a short write keeps going, chunk order holds."""
  from src.agents.backends.base import _write_chunk

  path = tmp_path / "chunk.log"
  fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o666)
  try:
    chunks = [bytes([i % 256]) * (8192 + i) for i in range(9)]
    for chunk in chunks:
      await _write_chunk(fd, chunk)
  finally:
    os.close(fd)
  assert path.read_bytes() == b"".join(chunks)


class _StubStream:
  """A subprocess stream standing in for StreamReader.read: scripted chunks, then EOF."""

  def __init__(self, chunks: list[bytes]) -> None:
    self._chunks = list(chunks)

  async def read(self, size: int) -> bytes:
    if not self._chunks:
      return b""
    return self._chunks.pop(0)


class _CountingWrites:
  """A _write_chunk stand-in that records each flushed window's size."""

  def __init__(self, real: Callable[[int, bytes], Awaitable[None]]) -> None:
    self.real = real
    self.flushes: list[int] = []

  async def __call__(self, fd: int, chunk: bytes) -> None:
    self.flushes.append(len(chunk))
    await self.real(fd, chunk)


@pytest.mark.asyncio
async def test_tee_stream_batches_flushes_and_lands_every_byte(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Full chunks batch to one executor hop per _LOG_FLUSH_BYTES window; a short
  read flushes its window; the file lands byte-identical to the stream."""
  from src.agents.backends import base as base_mod

  counter = _CountingWrites(base_mod._write_chunk)
  monkeypatch.setattr(base_mod, "_write_chunk", counter)
  chunks = [bytes([i % 256]) * base_mod._PUMP_CHUNK_BYTES for i in range(20)]
  chunks.append(b"tail")  # a short read: the stream drained below one chunk
  expected = b"".join(chunks)

  path = tmp_path / "pump.log"
  fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o666)
  await base_mod._tee_stream(_StubStream(chunks).read, fd)
  assert path.read_bytes() == expected
  # 20 full chunks = 2.5 windows: two fills, then the short read's flush of
  # the 32 KB remainder; the end flush finds an empty buffer.
  assert counter.flushes == [base_mod._LOG_FLUSH_BYTES, base_mod._LOG_FLUSH_BYTES, 32772]


@pytest.mark.asyncio
async def test_tee_stream_flushes_a_sparse_line_immediately(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The tail -f contract: a lone short chunk (one sparse line) flushes during
  the loop, not at stream end, so the log stays live between bursts."""
  from src.agents.backends import base as base_mod

  counter = _CountingWrites(base_mod._write_chunk)
  monkeypatch.setattr(base_mod, "_write_chunk", counter)
  chunks = [b"one sparse line\n", b"x" * base_mod._PUMP_CHUNK_BYTES]

  path = tmp_path / "pump.log"
  fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o666)
  await base_mod._tee_stream(_StubStream(chunks).read, fd)
  assert path.read_bytes() == b"".join(chunks)
  assert counter.flushes == [16, base_mod._PUMP_CHUNK_BYTES]


@pytest.mark.asyncio
async def test_stream_stderr_batches_writes_and_keeps_tail_current(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The stderr pump batches its log writes while the in-memory tail stays
  per chunk: after the pump, the tail holds every byte the stream carried."""
  from src.agents.backends import base as base_mod

  counter = _CountingWrites(base_mod._write_chunk)
  monkeypatch.setattr(base_mod, "_write_chunk", counter)
  chunks = [bytes([i % 256]) * base_mod._PUMP_CHUNK_BYTES for i in range(20)]
  chunks.append(b"tail")

  backend = _ScriptedBackend("true", log_dir=tmp_path / "logs")
  backend._proc = type("P", (), {"stderr": _StubStream(chunks)})()
  await backend._stream_stderr(tmp_path / "stderr.log")
  # The tail is the stream's last _STDERR_TAIL_BYTES, kept current per chunk
  # while the log file's writes batched underneath it.
  assert bytes(backend._stderr_tail) == b"".join(chunks)[-base_mod._STDERR_TAIL_BYTES:]
  assert len(counter.flushes) == 3


@pytest.mark.asyncio
async def test_stream_stdout_batches_writes(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The opencode stdout pump rides the same batching: one hop per window,
  byte parity at the log."""
  from src.agents.backends import base as base_mod
  from src.agents.backends.opencode import OpenCodeBackend

  counter = _CountingWrites(base_mod._write_chunk)
  monkeypatch.setattr(base_mod, "_write_chunk", counter)
  chunks = [bytes([i % 256]) * base_mod._PUMP_CHUNK_BYTES for i in range(20)]
  chunks.append(b"tail")

  class _StubBackend:
    _proc = None
    _stdout_fd = None

  stub = _StubBackend()
  stub._proc = type("P", (), {"stdout": _StubStream(chunks)})()
  fd = os.open(tmp_path / "stdout.log", os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o666)
  stub._stdout_fd = fd
  await OpenCodeBackend._stream_stdout(stub)  # type: ignore[arg-type]
  assert (tmp_path / "stdout.log").read_bytes() == b"".join(chunks)
  assert len(counter.flushes) == 3
