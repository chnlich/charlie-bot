"""Unit tests for the profile backup's exclusion contract.

The backup walks the whole profile home and the retention policy keeps
archives for up to 90 days, so the exclusion list is what keeps the secrets
file (credentials.yaml) out of the archives it never deletes early. The
profile home under test comes from the conftest autouse `_isolate_profile`
fixture, which pins CHARLIEBOT_HOME at a fresh directory per test.
"""
import tarfile

from src.core.backup import _should_exclude, create_backup
from src.core.config import charliebot_home_dir


def test_exclusions_cover_the_secrets_file_and_noise() -> None:
  assert _should_exclude("credentials.yaml")
  assert _should_exclude(".git/HEAD")
  assert _should_exclude("__pycache__/x.pyc")
  assert _should_exclude("cache/x.pyc")
  assert _should_exclude("sessions/s1/threads/t1/metadata.json")
  assert not _should_exclude("config.yaml")
  assert not _should_exclude("sessions/s1/triggers/t1.json")


def test_create_backup_omits_the_secrets_file() -> None:
  home = charliebot_home_dir()
  (home / "credentials.yaml").write_text("credentials:\n  access_key: secret\n", encoding="utf-8")
  (home / "config.yaml").write_text("server: {}\n", encoding="utf-8")

  archive = create_backup()
  members = [m.name for m in tarfile.open(archive).getmembers()]

  assert "config.yaml" in members
  assert not any("credentials.yaml" in name for name in members)
