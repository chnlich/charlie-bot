"""CHARLIEBOT_HOME: the environment variable that selects a profile.

Two layers are covered here. The first is the resolver contract. The second is the
property the resolver exists for: with a profile selected, nothing writes to the
default location. That second test exercises real entry points rather than grepping
for a spelling, so any hardcoded state path it touches fails it regardless of how
the path was written.
"""

import asyncio
import pathlib

import conftest
import pytest

from src.infra import config as core_config

# Both caches are keyed on nothing but their own mtimes, so a cached instance
# from an earlier test would answer with the wrong profile.
_reset_config_caches = conftest.fresh_state_fixture(conftest.reset_config_caches)

# (env value to set — None deletes the variable, "{home}" interpolates tmp_path —
# and the directory the resolver must answer with, relative to tmp_path).
_HOME_ENV_CASES = [
    pytest.param(None, ".charliebot", id="unset-env-defaults"),
    pytest.param("   ", ".charliebot", id="blank-env-defaults"),
    pytest.param("{home}/profile", "profile", id="env-selects-the-home"),
    pytest.param("{home}/profile/", "profile", id="trailing-slash-normalized"),
]


@pytest.mark.parametrize(("env_value", "expected_name"), _HOME_ENV_CASES)
def test_env_resolves_the_home_dir(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path, env_value: str | None, expected_name: str) -> None:
  """A set CHARLIEBOT_HOME selects the profile, its trailing slash normalized;
  unset or blank falls back to the default home."""
  (tmp_path / "profile").mkdir()
  if env_value is None:
    monkeypatch.delenv("CHARLIEBOT_HOME", raising=False)
  else:
    monkeypatch.setenv("CHARLIEBOT_HOME", env_value.replace("{home}", str(tmp_path)))
  monkeypatch.setenv("HOME", str(tmp_path))
  assert core_config.charliebot_home_dir() == tmp_path / expected_name


def test_tilde_expanded(monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> None:
  monkeypatch.setenv("HOME", str(tmp_path))
  monkeypatch.setenv("CHARLIEBOT_HOME", "~/dbg")
  assert core_config.charliebot_home_dir() == (tmp_path / "dbg").resolve()


def test_relative_path_rejected(monkeypatch: pytest.MonkeyPatch) -> None:
  """A relative value would resolve against each process's own cwd."""
  monkeypatch.setenv("CHARLIEBOT_HOME", "dbg-home")
  with pytest.raises(ValueError, match="absolute path"):
    core_config.charliebot_home_dir()


def test_config_yaml_may_not_set_the_home(monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> None:
  profile = tmp_path / "profile"
  profile.mkdir()
  (profile / "config.yaml").write_text(f"charliebot_home: {tmp_path}/elsewhere\n", encoding="utf-8")
  monkeypatch.setenv("CHARLIEBOT_HOME", str(profile))
  with pytest.raises(ValueError, match="CHARLIEBOT_HOME"):
    core_config.load_config()


def test_config_loads_from_the_selected_profile(monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> None:
  profile = tmp_path / "profile"
  profile.mkdir()
  (profile / "config.yaml").write_text("server:\n  port: 19999\n", encoding="utf-8")
  monkeypatch.setenv("CHARLIEBOT_HOME", str(profile))
  cfg = core_config.load_config()
  assert cfg.charliebot_home == profile
  assert cfg.server.port == 19999
  assert cfg.sessions_dir == profile / "sessions"


def test_profile_leaves_the_default_home_untouched(monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> None:
  """The property the whole feature exists for.

  Exercise every entry point that owns a path inside the state directory, then
  assert the default location was never created. This asserts the mechanism, not a
  spelling: a hardcoded path written any other way still lands in the fake home and
  still fails here.
  """
  fake_home = tmp_path / "home"
  fake_home.mkdir()
  profile = tmp_path / "profile"
  monkeypatch.setenv("HOME", str(fake_home))
  monkeypatch.setenv("CHARLIEBOT_HOME", str(profile))

  from src.app import pages as api_pages
  from src.backends.claude_sub import claude_sub
  from src.features.backup import backup as core_backup
  from src.features.cron import api as api_cron
  from src.runtime import init as core_init

  asyncio.run(core_init.init_seed.init_charliebot_home())

  cfg = core_config.get_config()
  core_config.get_scheduled_tasks()
  api_cron.cron_dir().mkdir(parents=True, exist_ok=True)
  api_cron._write_cron_yaml("probe", {"cron": "* * * * *", "prompt": "p"})
  assert api_cron._read_cron_yaml("probe") == {"cron": "* * * * *", "prompt": "p"}

  owned = [
      cfg.charliebot_home,
      cfg.sessions_dir,
      cfg.config_file,
      cfg.config_d_dir,
      cfg.memory_dir,
      cfg.claude_md_file,
      api_cron.cron_dir(),
      api_pages._perfetto_merge_cache_dir(),
      core_backup.charliebot_dir(),
      claude_sub._session_marker_dir(),
  ]
  for path in owned:
    assert path == profile or profile in path.parents, f"{path} is outside the profile"

  assert not (fake_home / ".charliebot").exists(), "a state path escaped to the default home"
  assert not (fake_home / ".charliebot_backup").exists()
  assert (profile / "config.yaml").is_file()
  assert core_backup.backup_dir() == profile.with_name(profile.name + "_backup")


def test_no_new_hardcoded_state_paths() -> None:
  """Regression guard for code the isolation test above does not execute.

  The isolation test catches any spelling but only on paths it reaches; this catches
  any path but only the two spellings that build one from the user's home directory.
  ``src/infra/home.py`` owns the resolution and is the single exemption.
  """
  exempt = {conftest.ROOT / "src" / "infra" / "home.py"}
  offenders: list[str] = []

  python_files = [conftest.ROOT / "server.py", *sorted((conftest.ROOT / "src").rglob("*.py"))]
  for path in python_files:
    if path in exempt:
      continue
    for lineno, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
      if "Path.home()" in line and ".charliebot" in line:
        offenders.append(f"{path.relative_to(conftest.ROOT)}:{lineno}: {line.strip()}")

  web_files = [
      *sorted((conftest.ROOT / "web" / "static" / "js").rglob("*.js")),
      *sorted((conftest.ROOT / "web" / "templates").rglob("*.html")),
  ]
  for path in web_files:
    for lineno, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
      if "/.charliebot/" in line:
        offenders.append(f"{path.relative_to(conftest.ROOT)}:{lineno}: {line.strip()}")

  assert not offenders, (
      "state paths must come from CharlieBotConfig, not from the user's home directory:\n" + "\n".join(offenders))


def test_terminal_session_name_separates_profiles(monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> None:
  """The tmux server is shared, so the session name is what separates profiles."""
  from src.features.terminal import terminal

  monkeypatch.setenv("HOME", str(tmp_path))
  monkeypatch.delenv("CHARLIEBOT_HOME", raising=False)
  assert terminal.terminal_session_id() == "terminal"
  assert terminal.terminal_tmux_name() == "charliebot-terminal"

  monkeypatch.setenv("CHARLIEBOT_HOME", str(tmp_path / "a"))
  name_a = terminal.terminal_tmux_name()
  monkeypatch.setenv("CHARLIEBOT_HOME", str(tmp_path / "b"))
  name_b = terminal.terminal_tmux_name()
  assert name_a != name_b
  assert name_a != "charliebot-terminal"
  assert name_a.startswith("charliebot-terminal-")
