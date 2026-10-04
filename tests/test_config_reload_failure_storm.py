"""get_config's failed-reload path: one parse per fingerprint, one warning per error.

A reload that raises keeps the last-good config but must not re-run the full
parse and re-fire the warning on every subsequent call — the auth middleware
calls get_config() per request, so an unchanged broken corpus would otherwise
pay a full parse and a log line per request (the live burst: 4431 lines in a
2 h window). The fingerprint that produced the failure keys the skip, exactly
as the successful path keys its cache.
"""

import os
import pathlib

import pytest

from src.core import config as core_config


@pytest.fixture
def reload_log(monkeypatch: pytest.MonkeyPatch) -> list[dict]:
  """Capture hot-reload warnings while the real log stays quiet.

  The reload caches warn through their owner modules' loggers: the cache class
  lives in src.core.credentials (the light split) and config's own paths warn
  through config's, so both land in the one records list.
  """
  from src.core import credentials as core_credentials

  records: list[dict] = []

  def _record(event: str, **kw: object) -> None:
    records.append({"event": event, **kw})

  monkeypatch.setattr(core_config.log, "warning", _record)
  monkeypatch.setattr(core_credentials.log, "warning", _record)
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
  core_config._config_cache.reset()
  core_config.get_config()


_UTIME_TICK = [0]


def _write_broken(home: pathlib.Path, key: str) -> None:
  """config.yaml declaring a key the model does not declare — the observed
  burst's error shape (unknown config key(s) ...). Each write takes a distinct
  forced mtime: same-size rewrites land inside one float-mtime tick otherwise,
  the fingerprint's documented blind spot, and the reload the test intends to
  trigger never runs."""
  _UTIME_TICK[0] += 1
  path = home / "config.yaml"
  path.write_text(f"{key}: 1\n", encoding="utf-8")
  os.utime(path, (_UTIME_TICK[0], _UTIME_TICK[0]))


def test_broken_steady_state_parses_once_and_warns_once(
    profile_home: pathlib.Path, reload_log: list[dict], counted_loads: list[int]) -> None:
  """With the corpus broken and unchanged, 60 calls pay one parse and one line."""
  _seed_good_config()
  _write_broken(profile_home, "unknown_m53_key")

  cached = core_config.get_config()
  assert len(counted_loads) == 2  # the seed load plus the onset parse
  for _ in range(60):
    assert core_config.get_config() is cached
  assert len(counted_loads) == 2
  assert [r["event"] for r in reload_log] == ["config_reload_failed"]


def test_startup_with_broken_config_still_raises(profile_home: pathlib.Path, reload_log: list[dict]) -> None:
  """No cached config means nothing to fall back to: the raise survives."""
  _write_broken(profile_home, "unknown_m53_key")
  core_config._config_cache.reset()
  with pytest.raises(ValueError, match="unknown config key"):
    core_config.get_config()
