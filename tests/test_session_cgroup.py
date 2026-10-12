"""Process helpers: session memory-cap cgroups (config schema, naming, degradation, attribution) and
wait_or_kill_group's no-limit path."""

import asyncio
import pathlib

import pytest

from src.infra import config, process

SESSION_ID = "abcd1234-ef56-7890-abcd-ef1234567890"

# ---------------------------------------------------------------------------
# Config schema
# ---------------------------------------------------------------------------


def test_server_config_zero_disables_cgroup() -> None:
  cfg = config.ServerConfig(session_memory_max_mb=0, session_swap_max_mb=0)
  assert cfg.session_memory_max_mb == 0
  assert cfg.session_swap_max_mb == 0


# ---------------------------------------------------------------------------
# Ensure: creation, refresh, degradation
# ---------------------------------------------------------------------------


@pytest.fixture
def fake_app_slice(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> pathlib.Path:
  """A temp dir standing in for the delegated app.slice base."""
  base = tmp_path / "app.slice"
  base.mkdir()
  monkeypatch.setattr(process, "CGROUP_V2_APP_SLICE", str(base))
  return base


def test_ensure_session_cgroup_creates_with_limit_files(fake_app_slice: pathlib.Path) -> None:
  path = process.ensure_session_cgroup(SESSION_ID, 64, 32)
  assert path == fake_app_slice / "charliebot-sess-abcd1234"
  assert path.is_dir()
  assert (path / "memory.max").read_text() == str(64 * 1024 * 1024)
  assert (path / "memory.swap.max").read_text() == str(32 * 1024 * 1024)


def test_ensure_session_cgroup_degrades_when_base_missing(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> None:
  monkeypatch.setattr(process, "CGROUP_V2_APP_SLICE", str(tmp_path / "missing" / "app.slice"))
  assert process.ensure_session_cgroup(SESSION_ID, 64, 32) is None


# ---------------------------------------------------------------------------
# memory.events reading and exit attribution
# ---------------------------------------------------------------------------


def test_classify_cap_kill_on_max_growth() -> None:
  msg = process.classify_cgroup_exit(-9, (1, 0), (2, 0), 12288)
  assert msg is not None
  assert "session 内存上限触发" in msg
  assert "12288" in msg
  assert "gpuq" in msg


# ---------------------------------------------------------------------------
# wait_or_kill_group: timeout=None waits without a limit
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_wait_or_kill_group_without_a_limit_returns_the_result() -> None:

  async def answer() -> str:
    await asyncio.sleep(0.05)
    return "done"

  stderr_task = asyncio.create_task(asyncio.sleep(3600))
  assert await process.wait_or_kill_group(answer(), None, -1, stderr_task) == "done"
  assert stderr_task.cancelled()
