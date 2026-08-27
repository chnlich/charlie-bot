"""CHARLIEBOT_HOME: the environment variable that selects a profile.

Two layers are covered here. The first is the resolver contract. The second is the
property the resolver exists for: with a profile selected, nothing writes to the
default location. That second test exercises real entry points rather than grepping
for a spelling, so any hardcoded state path it touches fails it regardless of how
the path was written.
"""

import asyncio
import pathlib

import pytest
from conftest import reset_config_caches

from src.core import config as core_config


@pytest.fixture(autouse=True)
def _reset_config_caches():
  """Clear the process-wide config and cron caches around every test.

  Both are keyed on nothing but their own mtimes, so a cached instance from an
  earlier test would answer with the wrong profile.
  """
  reset_config_caches()
  yield
  reset_config_caches()


def test_unset_env_gives_the_default_home(monkeypatch, tmp_path):
  monkeypatch.delenv("CHARLIEBOT_HOME", raising=False)
  monkeypatch.setenv("HOME", str(tmp_path))
  assert core_config.charliebot_home_dir() == tmp_path / ".charliebot"


def test_empty_env_gives_the_default_home(monkeypatch, tmp_path):
  monkeypatch.setenv("CHARLIEBOT_HOME", "   ")
  monkeypatch.setenv("HOME", str(tmp_path))
  assert core_config.charliebot_home_dir() == tmp_path / ".charliebot"


def test_env_selects_the_home(monkeypatch, tmp_path):
  profile = tmp_path / "profile"
  profile.mkdir()
  monkeypatch.setenv("CHARLIEBOT_HOME", str(profile))
  assert core_config.charliebot_home_dir() == profile


def test_trailing_slash_normalized(monkeypatch, tmp_path):
  profile = tmp_path / "profile"
  profile.mkdir()
  monkeypatch.setenv("CHARLIEBOT_HOME", f"{profile}/")
  assert core_config.charliebot_home_dir() == profile


def test_tilde_expanded(monkeypatch, tmp_path):
  monkeypatch.setenv("HOME", str(tmp_path))
  monkeypatch.setenv("CHARLIEBOT_HOME", "~/dbg")
  assert core_config.charliebot_home_dir() == (tmp_path / "dbg").resolve()


def test_relative_path_rejected(monkeypatch):
  """A relative value would resolve against each process's own cwd."""
  monkeypatch.setenv("CHARLIEBOT_HOME", "dbg-home")
  with pytest.raises(ValueError, match="absolute path"):
    core_config.charliebot_home_dir()


def test_config_yaml_may_not_set_the_home(monkeypatch, tmp_path):
  profile = tmp_path / "profile"
  profile.mkdir()
  (profile / "config.yaml").write_text(f"charliebot_home: {tmp_path}/elsewhere\n", encoding="utf-8")
  monkeypatch.setenv("CHARLIEBOT_HOME", str(profile))
  with pytest.raises(ValueError, match="CHARLIEBOT_HOME"):
    core_config.load_config()


def test_config_loads_from_the_selected_profile(monkeypatch, tmp_path):
  profile = tmp_path / "profile"
  profile.mkdir()
  (profile / "config.yaml").write_text("server_port: 19999\n", encoding="utf-8")
  monkeypatch.setenv("CHARLIEBOT_HOME", str(profile))
  cfg = core_config.load_config()
  assert cfg.charliebot_home == profile
  assert cfg.server_port == 19999
  assert cfg.sessions_dir == profile / "sessions"


def test_profile_leaves_the_default_home_untouched(monkeypatch, tmp_path):
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

  from src.api import cron as api_cron
  from src.api import pages as api_pages
  from src.cli import claude_sub
  from src.core import backup as core_backup
  from src.core import init as core_init
  from src.core import slash_commands

  asyncio.run(core_init.init_charliebot_home())

  cfg = core_config.get_config()
  core_config.get_scheduled_tasks()
  api_cron.cron_dir().mkdir(parents=True, exist_ok=True)
  api_cron._write_cron_yaml("probe", {"cron": "* * * * *", "prompt": "p"})
  assert api_cron._read_cron_yaml("probe") == {"cron": "* * * * *", "prompt": "p"}
  slash_commands.load_slash_commands()

  owned = [
      cfg.charliebot_home,
      cfg.sessions_dir,
      cfg.config_file,
      cfg.config_d_dir,
      cfg.memory_dir,
      cfg.claude_md_file,
      api_cron.cron_dir(),
      slash_commands._slash_commands_file(),
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


def test_default_home_backup_dir_is_unchanged(monkeypatch, tmp_path):
  """The no-env path keeps the historical ~/.charliebot_backup."""
  monkeypatch.delenv("CHARLIEBOT_HOME", raising=False)
  monkeypatch.setenv("HOME", str(tmp_path))
  from src.core import backup as core_backup
  assert core_backup.backup_dir() == tmp_path / ".charliebot_backup"


def test_no_new_hardcoded_state_paths():
  """Regression guard for code the isolation test above does not execute.

  The isolation test catches any spelling but only on paths it reaches; this catches
  any path but only the two spellings that build one from the user's home directory.
  ``src/core/config.py`` owns the resolution and is the single exemption.
  """
  repo_root = pathlib.Path(__file__).resolve().parents[1]
  exempt = {repo_root / "src" / "core" / "config.py"}
  offenders: list[str] = []

  python_files = [repo_root / "server.py", *sorted((repo_root / "src").rglob("*.py"))]
  for path in python_files:
    if path in exempt:
      continue
    for lineno, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
      if "Path.home()" in line and ".charliebot" in line:
        offenders.append(f"{path.relative_to(repo_root)}:{lineno}: {line.strip()}")

  web_files = [
      *sorted((repo_root / "web" / "static" / "js").rglob("*.js")),
      *sorted((repo_root / "web" / "templates").rglob("*.html")),
  ]
  for path in web_files:
    for lineno, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
      if "/.charliebot/" in line:
        offenders.append(f"{path.relative_to(repo_root)}:{lineno}: {line.strip()}")

  assert not offenders, (
      "state paths must come from CharlieBotConfig, not from the user's home directory:\n"
      + "\n".join(offenders))


def test_terminal_session_name_separates_profiles(monkeypatch, tmp_path):
  """The tmux server is shared, so the session name is what separates profiles."""
  from src.agents.backends import terminal

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
