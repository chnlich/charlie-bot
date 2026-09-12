"""Session memory-cap cgroup helpers (plan_01 v3): config schema, naming, degradation, attribution."""

from pathlib import Path

import pytest
from pydantic import ValidationError

from src.core import process as cgroup_process
from src.core.config import ServerConfig
from src.core.process import (
    classify_cgroup_exit,
    cleanup_session_cgroup,
    compose_preexec,
    ensure_session_cgroup,
    log_session_cgroup_startup,
    make_session_cgroup_preexec,
    prepare_session_cgroup,
    session_cgroup_name,
    session_cgroup_path,
    sweep_stale_session_cgroups,
)

SESSION_ID = "abcd1234-ef56-7890-abcd-ef1234567890"

# ---------------------------------------------------------------------------
# Config schema (plan_01 v3 §4.1)
# ---------------------------------------------------------------------------


def test_server_config_session_memory_defaults():
  cfg = ServerConfig()
  assert cfg.session_memory_max_mb == 12288
  assert cfg.session_swap_max_mb == 2048


def test_server_config_zero_disables_cgroup():
  cfg = ServerConfig(session_memory_max_mb=0, session_swap_max_mb=0)
  assert cfg.session_memory_max_mb == 0
  assert cfg.session_swap_max_mb == 0


def test_server_config_rejects_non_integer():
  with pytest.raises(ValidationError):
    ServerConfig(session_memory_max_mb="12GB")
  with pytest.raises(ValidationError):
    ServerConfig(session_swap_max_mb=1.5)


def test_server_config_rejects_unknown_keys():
  with pytest.raises(ValidationError):
    ServerConfig(session_memory_limit_mb=1)


# ---------------------------------------------------------------------------
# Cgroup directory naming
# ---------------------------------------------------------------------------


def test_session_cgroup_name_uses_first_eight_chars():
  assert session_cgroup_name(SESSION_ID) == "charliebot-sess-abcd1234"


def test_session_cgroup_name_keeps_short_ids_whole():
  assert session_cgroup_name("short") == "charliebot-sess-short"


def test_session_cgroup_path_sits_under_app_slice():
  assert session_cgroup_path(SESSION_ID).parent == Path(cgroup_process.CGROUP_V2_APP_SLICE)


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


def test_ensure_session_cgroup_creates_with_limit_files(fake_app_slice: Path):
  path = ensure_session_cgroup(SESSION_ID, 64, 32)
  assert path == fake_app_slice / "charliebot-sess-abcd1234"
  assert path.is_dir()
  assert (path / "memory.max").read_text() == str(64 * 1024 * 1024)
  assert (path / "memory.swap.max").read_text() == str(32 * 1024 * 1024)


def test_ensure_session_cgroup_refreshes_existing_limits(fake_app_slice: Path):
  path = ensure_session_cgroup(SESSION_ID, 64, 32)
  ensure_session_cgroup(SESSION_ID, 128, 64)
  assert (path / "memory.max").read_text() == str(128 * 1024 * 1024)
  assert (path / "memory.swap.max").read_text() == str(64 * 1024 * 1024)


def test_ensure_session_cgroup_degrades_when_base_missing(monkeypatch: pytest.MonkeyPatch, tmp_path: Path):
  monkeypatch.setattr(cgroup_process, "CGROUP_V2_APP_SLICE", str(tmp_path / "missing" / "app.slice"))
  assert ensure_session_cgroup(SESSION_ID, 64, 32) is None


def test_ensure_session_cgroup_degrades_when_dir_path_is_a_file(fake_app_slice: Path):
  # The cgroup directory name taken by a regular file: mkdir reports
  # FileExistsError (treated as refresh), then the limit write fails loud as
  # an OSError — degraded to None, never raised.
  (fake_app_slice / "charliebot-sess-abcd1234").write_text("not a dir")
  assert ensure_session_cgroup(SESSION_ID, 64, 32) is None


def test_prepare_session_cgroup_disabled_by_zero_cap(fake_app_slice: Path):
  assert prepare_session_cgroup(SESSION_ID, memory_max_mb=0, swap_max_mb=64) is None
  assert not (fake_app_slice / "charliebot-sess-abcd1234").exists()


def test_prepare_session_cgroup_none_without_session():
  assert prepare_session_cgroup(None, memory_max_mb=64, swap_max_mb=64) is None
  assert prepare_session_cgroup("", memory_max_mb=64, swap_max_mb=64) is None


# ---------------------------------------------------------------------------
# preexec construction and composition
# ---------------------------------------------------------------------------


def test_make_session_cgroup_preexec_none_when_off():
  assert make_session_cgroup_preexec(None) is None


def test_compose_preexec_runs_all_in_order():
  calls: list[str] = []

  def first() -> None:
    calls.append("a")

  def second() -> None:
    calls.append("b")

  preexec = compose_preexec(None, first, None, second)
  assert preexec is not None
  preexec()
  assert calls == ["a", "b"]


def test_compose_preexec_all_none_gives_none():
  assert compose_preexec(None, None) is None


def test_compose_preexec_single_fn_returned_directly():

  def only() -> None:
    pass

  assert compose_preexec(None, only) is only


# ---------------------------------------------------------------------------
# memory.events reading and exit attribution (plan_01 v3 §4.2)
# ---------------------------------------------------------------------------


def test_prepare_session_cgroup_snapshots_events_before(fake_app_slice: Path):
  cgroup = prepare_session_cgroup(SESSION_ID, memory_max_mb=64, swap_max_mb=32)
  assert cgroup is not None
  assert cgroup.events_before is None  # no memory.events yet on a plain fs
  (cgroup.path / "memory.events").write_text("low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\n")
  again = prepare_session_cgroup(SESSION_ID, memory_max_mb=64, swap_max_mb=32)
  assert again is not None
  assert again.events_before == (0, 0)


def test_classify_cap_kill_on_max_growth():
  msg = classify_cgroup_exit(-9, (1, 0), (2, 0), 12288)
  assert msg is not None
  assert "session 内存上限触发" in msg
  assert "12288" in msg
  assert "gpuq" in msg


def test_classify_global_oom_on_oom_kill_growth_only():
  msg = classify_cgroup_exit(-9, (1, 5), (1, 6), 12288)
  assert msg is not None
  assert "全局 OOM" in msg
  assert "集群" not in msg


def test_classify_ignores_non_sigkill_exits_and_static_counters():
  assert classify_cgroup_exit(-15, (1, 0), (2, 0), 12288) is None
  assert classify_cgroup_exit(0, (1, 0), (2, 0), 12288) is None
  assert classify_cgroup_exit(1, (1, 0), (2, 0), 12288) is None
  assert classify_cgroup_exit(-9, (1, 0), (1, 0), 12288) is None
  assert classify_cgroup_exit(-9, None, (2, 0), 12288) is None
  assert classify_cgroup_exit(-9, (1, 0), None, 12288) is None


def test_session_cgroup_classify_exit_reads_events_file(fake_app_slice: Path):
  prepared = prepare_session_cgroup(SESSION_ID, memory_max_mb=64, swap_max_mb=32)
  assert prepared is not None
  (prepared.path / "memory.events").write_text("max 0\noom_kill 0\n")
  spawned = prepare_session_cgroup(SESSION_ID, memory_max_mb=64, swap_max_mb=32)
  assert spawned is not None
  assert spawned.events_before == (0, 0)
  (prepared.path / "memory.events").write_text("max 1\noom_kill 0\n")
  assert spawned.classify_exit(-9) is not None
  assert spawned.classify_exit(0) is None


# ---------------------------------------------------------------------------
# Lifecycle: cleanup and startup sweep
# ---------------------------------------------------------------------------


def test_cleanup_session_cgroup_removes_empty_dir(fake_app_slice: Path):
  path = fake_app_slice / "charliebot-sess-abcd1234"
  path.mkdir()
  assert cleanup_session_cgroup(SESSION_ID) is True
  assert not path.exists()


def test_cleanup_session_cgroup_keeps_non_empty_dir(fake_app_slice: Path):
  path = fake_app_slice / "charliebot-sess-abcd1234"
  path.mkdir()
  (path / "memory.max").write_text("1")  # regular file blocks rmdir on a plain fs
  assert cleanup_session_cgroup(SESSION_ID) is False
  assert path.exists()


def test_cleanup_session_cgroup_missing_dir_is_false(fake_app_slice: Path):
  assert cleanup_session_cgroup(SESSION_ID) is False


def test_sweep_stale_session_cgroups_removes_empty_keeps_nonempty(fake_app_slice: Path):
  empty = fake_app_slice / "charliebot-sess-deadbeef"
  empty.mkdir()
  busy = fake_app_slice / "charliebot-sess-livebeef"
  busy.mkdir()
  (busy / "memory.events").write_text("max 1\noom_kill 0\n")
  unrelated = fake_app_slice / "unrelated"
  unrelated.mkdir()
  assert sweep_stale_session_cgroups() == 1
  assert not empty.exists()
  assert busy.exists()
  assert unrelated.exists()


def test_sweep_stale_session_cgroups_degrades_on_missing_base(monkeypatch: pytest.MonkeyPatch, tmp_path: Path):
  monkeypatch.setattr(cgroup_process, "CGROUP_V2_APP_SLICE", str(tmp_path / "missing"))
  assert sweep_stale_session_cgroups() == 0


def test_log_session_cgroup_startup_disabled_by_zero_no_raise():
  log_session_cgroup_startup(0, 2048, uncovered_backends=False)


def test_log_session_cgroup_startup_missing_base_no_raise(monkeypatch: pytest.MonkeyPatch, tmp_path: Path):
  monkeypatch.setattr(cgroup_process, "CGROUP_V2_APP_SLICE", str(tmp_path / "missing"))
  log_session_cgroup_startup(12288, 2048, uncovered_backends=True)
