"""The config registry (src/infra/config_registry.py): packages own backend option models and config sections.

Deleting a package's line in ``registrations.PACKAGES`` must remove its backend from config parsing, and a
hot reload must carry a package's section into the config instance that holders already captured.
"""

import os
import pathlib
import subprocess
import sys

import conftest
import pytest

from src.infra import config as core_config
from src.infra import config_registry

CODEX_OPTION = "  - {id: codex-x, label: Codex, type: codex, model: gpt-6}\n"
GEMINI_OPTION = "  - {id: gemini-x, label: Gemini, type: gemini, model: gemini-3}\n"

# One fresh interpreter: gemini leaves PACKAGES before register_all(), then two configs load, one per argv path.
DELETED_PACKAGE_SCRIPT = "\n".join(
    [
        "import os, sys",
        "from src.app import registrations",
        "registrations.PACKAGES = tuple(p for p in registrations.PACKAGES if p != 'src.backends.gemini')",
        "registrations.register_all()",
        "from src.infra import config, config_registry",
        "from src.runtime.hooks import backend_types",
        "print('gemini' in config_registry.registered_backend_types(), 'gemini' in backend_types.registered_types())",
        "for path in sys.argv[1:]:",
        "  os.environ['CHARLIEBOT_HOME'] = path",
        "  try:",
        "    print('loaded', [o.id for o in config.load_config().backends.options])",
        "  except ValueError as exc:",
        "    print('refused', ' '.join(str(exc).split()))",
    ])


def _home(tmp_path: pathlib.Path, name: str, *options: str) -> str:
  home = tmp_path / name
  home.mkdir()
  (home / "config.yaml").write_text("backends:\n  options:\n" + "".join(options), encoding="utf-8")
  return str(home)


def test_a_config_loads_without_a_deleted_package_and_refuses_its_type(tmp_path: pathlib.Path) -> None:
  without_gemini = _home(tmp_path, "without", CODEX_OPTION)
  with_gemini = _home(tmp_path, "with", CODEX_OPTION, GEMINI_OPTION)
  env = {k: v for k, v in os.environ.items() if k != "CHARLIEBOT_HOME"}

  result = subprocess.run(
      [sys.executable, "-c", DELETED_PACKAGE_SCRIPT, without_gemini, with_gemini],
      cwd=conftest.ROOT,
      env=env,
      capture_output=True,
      text=True,
      check=True,
      timeout=60)

  registered, loaded, refused = result.stdout.splitlines()[:3]
  assert registered == "False False"
  assert loaded == "loaded ['codex-x']"
  assert refused.startswith("refused ") and "'gemini' is not registered" in refused


# One fresh interpreter in which the package in argv[1] is deleted: it leaves PACKAGES before register_all() and
# its modules no longer import. Then / renders and the page is searched for the fallback text in argv[2].
INDEX_WITHOUT_PACKAGE_SCRIPT = "\n".join(
    [
        "import sys",
        "class Deleted:",
        "  def find_spec(self, name, path=None, target=None):",
        "    if name == sys.argv[1] or name.startswith(sys.argv[1] + '.'):",
        "      raise ModuleNotFoundError(name)",
        "sys.meta_path.insert(0, Deleted())",
        "from src.app import registrations",
        "registrations.PACKAGES = tuple(p for p in registrations.PACKAGES if p != sys.argv[1])",
        "sys.path.insert(0, 'tests')",
        "import conftest",
        "from src.infra import config",
        "from src.runtime import sessions, task_sessions, threads",
        "cfg = config.get_config()",
        "cfg.sessions_dir.mkdir(parents=True)",
        "session_mgr = sessions.SessionManager(cfg)",
        "tree = task_sessions.TaskTreeManager(cfg, session_mgr)",
        "client = conftest.make_sessions_listing_page_client(cfg, session_mgr, tree, threads.ThreadManager(cfg))",
        "response = client.get('/', follow_redirects=False)",
        "print(response.status_code)",
        "print(sys.argv[2] in response.text)",
    ])


@pytest.mark.parametrize(
    "package, fallback",
    [
        ("src.features.voice", "const VOICE_DEFAULT_BACKEND = null;"),
        ("src.features.session_tree_preview", "const PREVIEW_MODE = false;"),
    ],
)
def test_the_index_page_renders_without_a_deleted_package(package: str, fallback: str) -> None:
  result = subprocess.run(
      [sys.executable, "-c", INDEX_WITHOUT_PACKAGE_SCRIPT, package, fallback],
      cwd=conftest.ROOT,
      capture_output=True,
      text=True,
      check=True,
      timeout=60)

  status, has_fallback = result.stdout.splitlines()[-2:]
  assert status == "200"
  assert has_fallback == "True"


def test_registry_refuses_a_second_registration_and_an_unregistered_type() -> None:
  with pytest.raises(ValueError, match="already registered"):
    config_registry.register_option_model("codex", "src.backends.codex.options:CodexBackend")
  with pytest.raises(ValueError, match="already registered"):
    config_registry.register_config_section("accounts", "src.backends.claude_code.claude_config:AccountsConfig")
  with pytest.raises(ValueError, match="already registered"):
    config_registry.register_config_check("src.backends.claude_code.claude_config:check_claude_pools")
  with pytest.raises(ValueError, match=r"'no-such-type' is not registered; registered types: .*codex"):
    config_registry.option_model("no-such-type")


_UTIME_TICK = [0]


def _write_config(home: pathlib.Path, relay_tokens: int, pool: str = "") -> None:
  """config.yaml with one account and the given compaction floor; a distinct forced mtime keeps the reload firing."""
  _UTIME_TICK[0] += 1
  path = home / "config.yaml"
  path.write_text(
      "accounts:\n  claude:\n    - {label: a, config_dir: /tmp/claude-a}\n"
      f"  claude_pools: {{{pool}}}\n  claude_compaction: {{relay_tokens: {relay_tokens}}}\n",
      encoding="utf-8")
  os.utime(path, (_UTIME_TICK[0], _UTIME_TICK[0]))


def test_hot_reload_updates_a_package_section_and_a_failed_check_keeps_it(profile_home: pathlib.Path) -> None:
  _write_config(profile_home, 111)
  cfg = core_config.get_config()
  assert cfg.accounts.claude_compaction.relay_tokens == 111

  _write_config(profile_home, 222)
  assert core_config.get_config() is cfg
  assert cfg.accounts.claude_compaction.relay_tokens == 222

  _write_config(profile_home, 333, pool="alpha: [ghost]")
  assert core_config.get_config() is cfg
  assert cfg.accounts.claude_compaction.relay_tokens == 222
  assert cfg.accounts.claude_pools == {}
