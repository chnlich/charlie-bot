"""Config fragments: keys may live in ``config.d/<topic>.yaml`` instead of config.yaml.

Covers the merge in :func:`load_config` and the fingerprint behind
:func:`get_config`'s reload check. The headline property is split equivalence:
any partition of the example config's top-level keys across the base file and
fragments must load to the exact same ``CharlieBotConfig``.
"""

import os
from collections.abc import Callable, Iterator
from pathlib import Path

import pytest

from src.core import config as core_config
from src.core.config import CharlieBotConfig
from src.core.yaml_utils import load_yaml, save_yaml


@pytest.fixture(autouse=True)
def profile_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[Path]:
  """Point the profile at a fresh tmp dir and clear the module caches.

  ``get_config()`` caches process-wide on a fingerprint, so an instance from an
  earlier test would answer with the wrong profile.
  """
  def _clear() -> None:
    core_config._config = None
    core_config._config_mtime = None
    core_config._cron_snapshot = core_config._CronSnapshot()

  monkeypatch.setenv(core_config.CHARLIEBOT_HOME_ENV, str(tmp_path))
  _clear()
  yield tmp_path
  _clear()


def _example_mapping() -> dict:
  """The full top-level mapping of configs/config.example.yaml."""
  repo_root = Path(__file__).resolve().parents[1]
  return load_yaml(repo_root / "configs" / "config.example.yaml")


def _write_key_set(home: Path, rel_path: str, keys: list[str], mapping: dict) -> None:
  """Write one config file holding *keys* of *mapping*; an empty set is an empty file."""
  path = home / rel_path
  path.parent.mkdir(parents=True, exist_ok=True)
  if keys:
    save_yaml(path, {key: mapping[key] for key in keys})
  else:
    path.write_text("", encoding="utf-8")


def _without_home(cfg: CharlieBotConfig) -> dict:
  return {key: value for key, value in cfg.model_dump().items() if key != "charliebot_home"}


# Each case is a partition of the example mapping's top-level keys into files.
# Fragment file names are chosen so the sort order is deterministic; a.yaml
# gets the earlier half so any accidental precedence would be observable.
_PARTITIONS: dict[str, Callable[[list[str]], dict[str, list[str]]]] = {
    "everything in config.yaml":
        lambda keys: {"config.yaml": keys},
    "everything in one fragment":
        lambda keys: {"config.yaml": [], "config.d/all.yaml": keys},
    "split across two fragments":
        lambda keys: {
            "config.yaml": [],
            "config.d/a.yaml": keys[:len(keys) // 2],
            "config.d/b.yaml": keys[len(keys) // 2:],
        },
    "interleaved split across config.yaml and a fragment":
        lambda keys: {"config.yaml": keys[1::2], "config.d/even.yaml": keys[::2]},
    "empty fragment file alongside a split":
        lambda keys: {
            "config.yaml": keys[::2],
            "config.d/empty.yaml": [],
            "config.d/z.yaml": keys[1::2],
        },
}


@pytest.mark.parametrize("partition", _PARTITIONS.values(), ids=list(_PARTITIONS))
def test_split_equivalence(profile_home: Path, partition: Callable[[list[str]], dict[str, list[str]]]) -> None:
  """Any partition of the same keys loads to the same config, field for field.

  This one property covers key arrival, type preservation, and default
  preservation together: the reference load saw every key in config.yaml, so
  any key that fails to arrive — or arrives with a mangled type — shows up as
  the pydantic default here instead of the example's value.
  """
  mapping = _example_mapping()
  keys = sorted(mapping)

  save_yaml(profile_home / "config.yaml", mapping)
  reference = core_config.load_config()

  (profile_home / "config.yaml").unlink()
  for rel_path, file_keys in partition(keys).items():
    _write_key_set(profile_home, rel_path, file_keys, mapping)
  actual = core_config.load_config()

  assert _without_home(actual) == _without_home(reference)
  assert actual.charliebot_home == profile_home


def test_key_in_both_config_yaml_and_fragment_raises(profile_home: Path) -> None:
  """There is no override precedence; the error names the key and both paths."""
  save_yaml(profile_home / "config.yaml", {"server_port": 18498})
  fragment = profile_home / "config.d" / "server.yaml"
  fragment.parent.mkdir()
  save_yaml(fragment, {"server_port": 1})
  with pytest.raises(ValueError) as exc_info:
    core_config.load_config()
  message = str(exc_info.value)
  assert "server_port" in message
  assert str(profile_home / "config.yaml") in message
  assert str(fragment) in message


def test_key_in_two_fragments_raises(profile_home: Path) -> None:
  """Fragment-versus-fragment conflicts are treated like config.yaml conflicts."""
  save_yaml(profile_home / "config.yaml", {"charliebot_access_key": "x"})
  config_d = profile_home / "config.d"
  config_d.mkdir()
  save_yaml(config_d / "a.yaml", {"public_base_url": "https://a.example.com"})
  save_yaml(config_d / "b.yaml", {"public_base_url": "https://b.example.com"})
  with pytest.raises(ValueError) as exc_info:
    core_config.load_config()
  message = str(exc_info.value)
  assert "public_base_url" in message
  assert str(config_d / "a.yaml") in message
  assert str(config_d / "b.yaml") in message


def test_fingerprint_sees_every_fragment_mutation_class(profile_home: Path) -> None:
  """Add, remove, rename, mtime change and pinned-mtime size change all re-key."""
  save_yaml(profile_home / "config.yaml", {"server_port": 18498})
  config_d = profile_home / "config.d"
  config_d.mkdir()
  fragment = config_d / "a.yaml"
  save_yaml(fragment, {"worktree_dir": "w"})

  base = core_config._config_fingerprint()
  # Unchanged inputs keep the fingerprint (the cache-hit case).
  assert core_config._config_fingerprint() == base

  # Fragment added.
  added_path = config_d / "b.yaml"
  save_yaml(added_path, {"headless_chrome_bin": "b"})
  assert core_config._config_fingerprint() != base

  # Fragment removed: not only changes from the added value, returns to base.
  added_path.unlink()
  assert core_config._config_fingerprint() == base

  # Fragment renamed, content untouched.
  os.rename(fragment, config_d / "z.yaml")
  renamed = core_config._config_fingerprint()
  assert renamed != base
  os.rename(config_d / "z.yaml", fragment)
  assert core_config._config_fingerprint() == base

  # Content edited, mtime moves (moved explicitly to stay exact on
  # coarse-resolution filesystems).
  new_mtime = fragment.stat().st_mtime + 10
  save_yaml(fragment, {"worktree_dir": "v"})
  os.utime(fragment, (new_mtime, new_mtime))
  assert core_config._config_fingerprint() != base

  # Content edited so the size changes while the mtime is pinned: an
  # mtime-only key would miss this silently. Pin to a whole second so the
  # float round-trip through os.utime stays exact.
  pinned_mtime = float(int(fragment.stat().st_mtime))
  save_yaml(fragment, {"worktree_dir": "x"})
  os.utime(fragment, (pinned_mtime, pinned_mtime))
  short = core_config._config_fingerprint()
  save_yaml(fragment, {"worktree_dir": "x with a good deal more content"})
  os.utime(fragment, (pinned_mtime, pinned_mtime))
  assert fragment.stat().st_mtime == pinned_mtime
  assert core_config._config_fingerprint() != short


def test_get_config_reloads_after_a_fragment_edit(profile_home: Path) -> None:
  """A fragment edit takes effect on the next call, on the same object."""
  save_yaml(profile_home / "config.yaml", {"server_port": 18498})
  cfg = core_config.get_config()
  assert cfg.headless_chrome_bin == ""

  # Fragment appears: the fingerprint grows a tuple, no mtime precision needed.
  fragment = profile_home / "config.d" / "chrome.yaml"
  fragment.parent.mkdir()
  save_yaml(fragment, {"headless_chrome_bin": "first"})
  assert core_config.get_config().headless_chrome_bin == "first"

  # Fragment content edited with a different size: the fingerprint differs even
  # on a filesystem with one-second mtime resolution.
  save_yaml(fragment, {"headless_chrome_bin": "second, longer than first"})
  reloaded = core_config.get_config()
  assert reloaded.headless_chrome_bin == "second, longer than first"
  # The stable-identity contract: holders never re-fetch.
  assert reloaded is cfg


def test_charliebot_home_key_in_a_fragment_names_that_fragment(profile_home: Path) -> None:
  """The rejection survives the merge and points at the offending file."""
  save_yaml(profile_home / "config.yaml", {"server_port": 18498})
  fragment = profile_home / "config.d" / "sneaky.yaml"
  fragment.parent.mkdir()
  save_yaml(fragment, {"charliebot_home": str(profile_home / "elsewhere")})
  with pytest.raises(ValueError) as exc_info:
    core_config.load_config()
  message = str(exc_info.value)
  assert "charliebot_home" in message
  assert str(fragment) in message
  assert str(profile_home / "config.yaml") not in message


def test_cron_files_are_not_config_fragments(profile_home: Path) -> None:
  """cron loading has its own reader; config.d/cron.yaml and cron.d/ stay out."""
  save_yaml(profile_home / "config.yaml", {"server_port": 18498})
  config_d = profile_home / "config.d"
  (config_d / "cron.d").mkdir(parents=True)
  save_yaml(config_d / "cron.yaml", {"bogus_legacy_key": 1})
  save_yaml(config_d / "cron.d" / "job.yaml", {"bogus_cron_job_key": 2})
  save_yaml(config_d / ".hidden.yaml", {"bogus_hidden_key": 3})

  cfg = core_config.load_config()
  dumped = cfg.model_dump()
  assert "bogus_legacy_key" not in dumped
  assert "bogus_cron_job_key" not in dumped
  assert "bogus_hidden_key" not in dumped
  assert cfg.model_extra in (None, {})
  assert cfg.server_port == 18498


@pytest.mark.parametrize("with_empty_config_d", [False, True], ids=["no-config.d", "empty-config.d"])
def test_without_fragments_matches_plain_config_yaml(profile_home: Path, with_empty_config_d: bool) -> None:
  """No config.d (or an empty one) is byte-for-byte the old single-file load."""
  if with_empty_config_d:
    (profile_home / "config.d").mkdir()
  mapping = {
      "server_port": 19999,
      "public_base_url": "https://bot.example.com",
      "slack_allowed_user_ids": ["U1", "U2"],
      "workspace_dirs": ["~/elsewhere"],
  }
  save_yaml(profile_home / "config.yaml", mapping)

  loaded = core_config.load_config()
  plain = CharlieBotConfig(**mapping)
  assert _without_home(loaded) == _without_home(plain)
