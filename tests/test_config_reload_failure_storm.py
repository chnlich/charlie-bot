"""get_config's failed-reload path: one parse per fingerprint, one warning per error.

A reload that raises keeps the last-good config but must not re-run the full
parse and re-fire the warning on every subsequent call — the auth middleware
calls get_config() per request, so an unchanged broken corpus would otherwise
pay a full parse and a log line per request (the live burst: 4431 lines in a
2 h window). The fingerprint that produced the failure keys the skip, exactly
as the successful path keys its cache.
"""

import os

import pytest

from src.core import config as core_config


@pytest.fixture
def reload_log(monkeypatch: pytest.MonkeyPatch) -> list[dict]:
  """Capture config-module warnings while the real log stays quiet."""
  records: list[dict] = []
  monkeypatch.setattr(core_config.log, "warning", lambda event, **kw: records.append({"event": event, **kw}))
  return records


@pytest.fixture
def counted_loads(monkeypatch: pytest.MonkeyPatch) -> list[int]:
  """Count load_config calls without changing what a load does."""
  calls: list[int] = []
  real_load = core_config.load_config
  monkeypatch.setattr(core_config, "load_config", lambda: (calls.append(1), real_load())[1])
  return calls


def _seed_good_config() -> None:
  """Cache a good config: an empty profile home loads clean on defaults."""
  core_config._config = None
  core_config._config_mtime = 0.0
  core_config._config_failed_mtime = None
  core_config._reset_config_reload_failures_for_tests()
  core_config.get_config()


_UTIME_TICK = [0]


def _write_broken(home, name: str, key: str) -> None:
  """One fragment declaring a key the model does not declare — the observed
  burst's error shape (unknown config key(s) ...). Each write takes a distinct
  forced mtime: same-size rewrites land inside one float-mtime tick otherwise,
  the fingerprint's documented blind spot, and the reload the test intends to
  trigger never runs."""
  _UTIME_TICK[0] += 1
  fragment = home / "config.d" / name
  fragment.parent.mkdir(parents=True, exist_ok=True)
  fragment.write_text(f"{key}: 1\n", encoding="utf-8")
  os.utime(fragment, (_UTIME_TICK[0], _UTIME_TICK[0]))


def test_broken_steady_state_parses_once_and_warns_once(profile_home, reload_log, counted_loads) -> None:
  """With the corpus broken and unchanged, 60 calls pay one parse and one line."""
  _seed_good_config()
  _write_broken(profile_home, "broken.yaml", "unknown_m53_key")

  cached = core_config.get_config()
  assert len(counted_loads) == 2  # the seed load plus the onset parse
  for _ in range(60):
    assert core_config.get_config() is cached
  assert len(counted_loads) == 2
  assert [r["event"] for r in reload_log] == ["config_reload_failed"]


def test_same_error_across_a_fingerprint_move_stays_one_line(profile_home, reload_log, counted_loads) -> None:
  """A rewritten fragment with the same unknown key moves the fingerprint —
  the reload must re-run (freshness) while the alarm it re-fires stays one
  line: the burst's exact shape."""
  _seed_good_config()
  _write_broken(profile_home, "broken.yaml", "unknown_m53_key")
  core_config.get_config()
  assert len(counted_loads) == 2 and len(reload_log) == 1  # seed load + onset parse

  _write_broken(profile_home, "broken.yaml", "unknown_m53_key")  # same key, moved mtime
  core_config.get_config()
  assert len(counted_loads) == 3
  assert [r["event"] for r in reload_log] == ["config_reload_failed"]

  for _ in range(10):  # moved corpus, unchanged since: no parse, no line
    core_config.get_config()
  assert len(counted_loads) == 3 and len(reload_log) == 1


def test_changed_error_earns_a_new_line(profile_home, reload_log, counted_loads) -> None:
  """A reload that fails differently reports the new failure."""
  _seed_good_config()
  _write_broken(profile_home, "broken.yaml", "unknown_m53_key")
  core_config.get_config()
  assert len(counted_loads) == 2 and len(reload_log) == 1

  _write_broken(profile_home, "other.yaml", "another_unknown_key")  # fingerprint moves, still broken
  core_config.get_config()
  assert len(counted_loads) == 3
  assert [r["event"] for r in reload_log] == ["config_reload_failed"] * 2
  assert reload_log[1]["error"] != reload_log[0]["error"]

  for _ in range(10):  # new broken corpus, unchanged: no parse, no line
    core_config.get_config()
  assert len(counted_loads) == 3 and len(reload_log) == 2


def _touch_config(home) -> None:
  """Give the home a config.yaml with a fresh forced mtime so the next
  fingerprint differs from the cached one — deleting a fragment alone can
  return the corpus to the exact fingerprint the cached config was loaded
  under, which is no reload at all. An empty file loads clean on defaults."""
  _UTIME_TICK[0] += 1
  path = home / "config.yaml"
  path.write_text("", encoding="utf-8")
  os.utime(path, (_UTIME_TICK[0], _UTIME_TICK[0]))


def test_recovery_rearms_the_warning(profile_home, reload_log, counted_loads) -> None:
  """A load that succeeds clears the registry: a later relapse is a new onset
  and earns one new line (the M50 recovery rule)."""
  _seed_good_config()
  _write_broken(profile_home, "broken.yaml", "unknown_m53_key")
  core_config.get_config()
  assert len(reload_log) == 1

  (profile_home / "config.d" / "broken.yaml").unlink()  # recovery: success logs nothing
  _touch_config(profile_home)
  recovered = core_config.get_config()
  assert len(reload_log) == 1
  assert core_config.get_config() is recovered

  _write_broken(profile_home, "broken.yaml", "unknown_m53_key")  # relapse
  core_config.get_config()
  assert len(reload_log) == 2


def test_startup_with_broken_config_still_raises(profile_home, reload_log) -> None:
  """No cached config means nothing to fall back to: the raise survives."""
  _write_broken(profile_home, "broken.yaml", "unknown_m53_key")
  core_config._config = None
  core_config._config_mtime = 0.0
  core_config._config_failed_mtime = None
  with pytest.raises(Exception):
    core_config.get_config()


def test_reset_config_caches_clears_the_failed_state(profile_home, reload_log) -> None:
  """The conftest reset covers both new module states."""
  from tests import conftest as conftest_mod

  _seed_good_config()
  _write_broken(profile_home, "broken.yaml", "unknown_m53_key")
  core_config.get_config()
  assert core_config._config_failed_mtime is not None
  assert core_config._config_reload_errors_seen

  conftest_mod.reset_config_caches()
  assert core_config._config_failed_mtime is None
  assert not core_config._config_reload_errors_seen
