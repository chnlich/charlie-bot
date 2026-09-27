"""Session memory-cap cgroup helpers: config schema, naming, degradation, attribution."""

from pathlib import Path

import pytest

from src.core import process as cgroup_process
from src.core.config import ServerConfig
from src.core.process import (
    classify_cgroup_exit,
    ensure_session_cgroup,
)

SESSION_ID = "abcd1234-ef56-7890-abcd-ef1234567890"

# ---------------------------------------------------------------------------
# Config schema
# ---------------------------------------------------------------------------


def test_server_config_zero_disables_cgroup() -> None:
  cfg = ServerConfig(session_memory_max_mb=0, session_swap_max_mb=0)
  assert cfg.session_memory_max_mb == 0
  assert cfg.session_swap_max_mb == 0


# ---------------------------------------------------------------------------
# Cgroup directory naming


# ---------------------------------------------------------------------------
# Ensure: creation, refresh, degradation
# ---------------------------------------------------------------------------


@pytest.fixture
def fake_app_slice(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
  """A temp dir standing in for the delegated app.slice base."""
  base = tmp_path / "app.slice"
  base.mkdir()
  monkeypatch.setattr(cgroup_process, "CGROUP_V2_APP_SLICE", str(base))
  return base


def test_ensure_session_cgroup_creates_with_limit_files(fake_app_slice: Path) -> None:
  path = ensure_session_cgroup(SESSION_ID, 64, 32)
  assert path == fake_app_slice / "charliebot-sess-abcd1234"
  assert path.is_dir()
  assert (path / "memory.max").read_text() == str(64 * 1024 * 1024)
  assert (path / "memory.swap.max").read_text() == str(32 * 1024 * 1024)


def test_ensure_session_cgroup_degrades_when_base_missing(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  monkeypatch.setattr(cgroup_process, "CGROUP_V2_APP_SLICE", str(tmp_path / "missing" / "app.slice"))
  assert ensure_session_cgroup(SESSION_ID, 64, 32) is None


# ---------------------------------------------------------------------------
# preexec construction and composition


# ---------------------------------------------------------------------------
# memory.events reading and exit attribution
# ---------------------------------------------------------------------------


def test_classify_cap_kill_on_max_growth() -> None:
  msg = classify_cgroup_exit(-9, (1, 0), (2, 0), 12288)
  assert msg is not None
  assert "session 内存上限触发" in msg
  assert "12288" in msg
  assert "gpuq" in msg


# ---------------------------------------------------------------------------
# Lifecycle: cleanup and startup sweep
