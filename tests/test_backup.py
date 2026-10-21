"""Unit tests for the profile backup's exclusion contract.

The backup walks the whole profile home and the retention policy keeps
archives for up to 15 days, so the exclusion list is what keeps the secrets
file (credentials.yaml) out of the archives it never deletes early. The
profile home under test comes from the conftest autouse `_isolate_profile`
fixture, which pins CHARLIEBOT_HOME at a fresh directory per test.
"""
import datetime
import pathlib
import tarfile
import types

import pytest

from src.features.backup import backup
from src.infra import config


def test_exclusions_cover_the_secrets_file_and_noise() -> None:
  assert backup._should_exclude("credentials.yaml")
  assert backup._should_exclude(".git/HEAD")
  assert backup._should_exclude("__pycache__/x.pyc")
  assert backup._should_exclude("cache/x.pyc")
  assert backup._should_exclude("sessions/s1/threads/t1/metadata.json")
  assert not backup._should_exclude("config.yaml")
  assert not backup._should_exclude("sessions/s1/triggers/t1.json")


def test_create_backup_omits_the_secrets_file() -> None:
  home = config.charliebot_home_dir()
  (home / "credentials.yaml").write_text("credentials:\n  access_key: secret\n", encoding="utf-8")
  (home / "config.yaml").write_text("server: {}\n", encoding="utf-8")

  archive = backup.create_backup()
  members = [m.name for m in tarfile.open(archive).getmembers()]

  assert "config.yaml" in members
  assert not any("credentials.yaml" in name for name in members)


def test_create_backup_archive_round_trips_bytes() -> None:
  home = config.charliebot_home_dir()
  payload = "key: value\nlist:\n  - a\n  - b\n"
  (home / "config.yaml").write_text(payload, encoding="utf-8")

  archive = backup.create_backup()
  with tarfile.open(archive, "r:gz") as tar:
    extracted = tar.extractfile("config.yaml")
    assert extracted is not None
    assert extracted.read().decode("utf-8") == payload


def test_create_backup_leaves_out_only_the_top_level_opencode_store(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The profile's opencode/ store is left out at the top level only; a deeper path part of the same name stays."""
  monkeypatch.setenv(config.CHARLIEBOT_HOME_ENV, str(tmp_path / "profile"))
  profile = config.charliebot_home_dir()
  (profile / "opencode" / "data" / "opencode").mkdir(parents=True)
  (profile / "opencode" / "data" / "opencode" / "opencode.db").write_bytes(b"db")
  (profile / "sessions" / "s1" / "opencode").mkdir(parents=True)
  (profile / "sessions" / "s1" / "opencode" / "note.json").write_text("{}", encoding="utf-8")

  archive = backup.create_backup()
  with tarfile.open(archive, "r:gz") as tar:
    members = tar.getnames()

  assert not any(name == "opencode" or name.startswith("opencode/") for name in members)
  assert "sessions/s1/opencode/note.json" in members


_FIXED_NOW = datetime.datetime(2026, 11, 4, 4, 50)


class _FixedClockDatetime(datetime.datetime):
  """The datetime class with now() pinned to _FIXED_NOW; parsing and arithmetic stay real."""

  @classmethod
  def now(cls, tz: datetime.tzinfo | None = None) -> datetime.datetime:
    return _FIXED_NOW


def test_retention_keeps_last_three_days_and_recent_sundays(monkeypatch: pytest.MonkeyPatch) -> None:
  # The scheduled run stamps each day's archive at 04:45 and applies retention at 04:50 on the newest day.
  monkeypatch.setattr(backup, "datetime", types.SimpleNamespace(datetime=_FixedClockDatetime))
  target = backup.backup_dir()
  target.mkdir()
  scheduled_run = _FIXED_NOW.replace(hour=4, minute=45, second=0)
  for days_ago in range(40):
    stamp = scheduled_run - datetime.timedelta(days=days_ago)
    (target / f"charliebot-{stamp:%Y%m%d-%H%M%S}.tar.gz").write_bytes(b"")

  backup.apply_retention()

  kept = {path.name for path in target.iterdir()}
  kept_days = ("20261025", "20261101", "20261102", "20261103", "20261104")
  assert kept == {f"charliebot-{day}-044500.tar.gz" for day in kept_days}
