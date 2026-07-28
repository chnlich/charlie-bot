"""Acceptance tests for repo-owned default cron tasks.

Covers the seed mechanism in ``src/core/init.py::seed_default_cron_tasks``,
the loader-level ``prompt_file`` / ``timezone: local`` resolution and cache
invalidation in ``src/core/config.py::get_scheduled_tasks``, and the shipped
``configs/cron.default.yaml`` + ``prompts/cron/memory_guideline_check.md``.
"""

import asyncio
import copy
import os
from pathlib import Path
from zoneinfo import ZoneInfo

import pytest

from src.core.config import (
    ScheduledTaskConfig,
    ScheduledTaskResolutionError,
    _resolve_local_timezone,
    _resolve_prompt_file,
    get_config,
    get_scheduled_tasks,
)
from src.core.init import init_charliebot_home, seed_default_cron_tasks
from src.core.yaml_utils import load_yaml, save_yaml

REPO_ROOT = Path(__file__).resolve().parents[1]


# --- helpers -----------------------------------------------------------------


def _cron_path(home: Path) -> Path:
  return Path(home) / ".charliebot" / "config.d" / "cron.yaml"


def _write_cron(home: Path, tasks: list[dict]) -> Path:
  p = _cron_path(home)
  p.parent.mkdir(parents=True, exist_ok=True)
  save_yaml(p, {"scheduled_tasks": copy.deepcopy(tasks)})
  return p


def _read_cron(home: Path) -> dict:
  return load_yaml(_cron_path(home), default={"scheduled_tasks": []})


@pytest.fixture
def temp_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
  """Point HOME at a temp dir and reset the config/cron module-level caches."""
  monkeypatch.setenv("HOME", str(tmp_path))
  import src.core.config as cm
  cm._config = None
  cm._config_mtime = 0.0
  cm._cron_tasks = []
  cm._cron_mtime = 0.0
  cm._cron_prompt_mtimes = {}
  return tmp_path


HOST_ONLY = {"name": "host-only-task", "cron": "0 0 * * *", "prompt": "host only body"}
HOST_SAME_NAME = {
    "name": "memory-guideline-check",
    "cron": "5 5 * * *",
    "timezone": "America/New_York",
    "backend": "claude-opus-4.6",
    "enabled": False,
    "prompt": "custom inline prompt",
    "allow_failure": True,
    "project": "my-proj",
    "notify": "telegram",
    "repo": "~/workspace/foo",
}


# --- 1. seed idempotence ------------------------------------------------------


def test_seed_idempotence(temp_home: Path) -> None:
  cfg = get_config()
  cron = cfg.config_d_dir / "cron.yaml"
  assert not cron.exists()

  report1 = seed_default_cron_tasks(cfg)
  assert cron.exists()
  assert any(it["status"] == "created" for it in report1)
  bytes1 = cron.read_bytes()

  # The seeded skeleton is exactly name / cron / timezone / prompt_file: no
  # backend, no prompt body copied into the host file.
  seeded = _read_cron(temp_home)["scheduled_tasks"][0]
  assert set(seeded.keys()) == {"name", "cron", "timezone", "prompt_file"}
  assert "backend" not in seeded
  assert "prompt" not in seeded

  report2 = seed_default_cron_tasks(cfg)
  bytes2 = cron.read_bytes()

  assert bytes1 == bytes2
  assert all(it["status"] == "exists" for it in report2)


# --- 2. existing entry untouched ----------------------------------------------


@pytest.mark.parametrize("field_name", list(ScheduledTaskConfig.model_fields))
def test_existing_entry_untouched(temp_home: Path, field_name: str) -> None:
  cfg = get_config()
  _write_cron(temp_home, [HOST_ONLY, HOST_SAME_NAME])
  before_bytes = _cron_path(temp_home).read_bytes()
  before_same = next(t for t in _read_cron(temp_home)["scheduled_tasks"]
                     if t["name"] == HOST_SAME_NAME["name"])
  before_cfg = ScheduledTaskConfig(**copy.deepcopy(before_same))

  report = seed_default_cron_tasks(cfg)
  assert all(it["status"] == "exists" for it in report)

  after_bytes = _cron_path(temp_home).read_bytes()
  assert after_bytes == before_bytes  # nothing written
  after_tasks = _read_cron(temp_home)["scheduled_tasks"]
  # host-only entries and overall order are unchanged
  assert [t["name"] for t in after_tasks] == [HOST_ONLY["name"], HOST_SAME_NAME["name"]]
  assert next(t for t in after_tasks if t["name"] == HOST_ONLY["name"]) == HOST_ONLY
  # the same-name entry's field is not rewritten
  after_same = next(t for t in after_tasks if t["name"] == HOST_SAME_NAME["name"])
  after_cfg = ScheduledTaskConfig(**copy.deepcopy(after_same))
  assert getattr(before_cfg, field_name) == getattr(after_cfg, field_name)


# --- 3. startup never writes cron.yaml ---------------------------------------


def test_startup_never_writes_cron(temp_home: Path) -> None:
  cron = _write_cron(temp_home, [HOST_ONLY])
  before = cron.read_bytes()
  asyncio.run(init_charliebot_home())
  after = cron.read_bytes()
  assert before == after
  # init_charliebot_home must not call seed_default_cron_tasks, directly or via
  # a global lookup the bytecode would record in co_names.
  assert "seed_default_cron_tasks" not in init_charliebot_home.__code__.co_names


# --- 4. get_scheduled_tasks is read-only -------------------------------------


def test_get_scheduled_tasks_readonly(temp_home: Path) -> None:
  _write_cron(temp_home, [{"name": "t1", "cron": "* * * * *", "prompt": "p"}])
  cron = _cron_path(temp_home)
  before = cron.read_bytes()
  tasks = get_scheduled_tasks()
  assert len(tasks) == 1 and tasks[0].name == "t1"
  assert cron.read_bytes() == before


# --- 5. prompt body tracks the repo ------------------------------------------


def test_body_tracks_repo(temp_home: Path) -> None:
  prompt_path = temp_home / "cron-prompts" / "my-task.md"
  prompt_path.parent.mkdir(parents=True, exist_ok=True)
  prompt_path.write_text("body v1", encoding="utf-8")
  m1 = prompt_path.stat().st_mtime
  _write_cron(temp_home, [{"name": "t", "cron": "* * * * *",
                           "prompt_file": "~/cron-prompts/my-task.md"}])

  tasks = get_scheduled_tasks()
  assert tasks[0].prompt == "body v1"

  # Rewrite the prompt file only — do not touch cron.yaml, do not re-seed.
  prompt_path.write_text("body v2", encoding="utf-8")
  os.utime(prompt_path, (m1 + 5.0, m1 + 5.0))

  tasks2 = get_scheduled_tasks()
  assert tasks2[0].prompt == "body v2"


# --- 6. shipped default is loadable ------------------------------------------


def test_shipped_default_loadable(temp_home: Path) -> None:
  cfg = get_config()
  repo_root = cfg.charlie_bot_repo
  defaults = load_yaml(repo_root / "configs" / "cron.default.yaml", default={})
  entries = defaults.get("scheduled_tasks", [])
  assert entries, "configs/cron.default.yaml defines no scheduled_tasks"
  for entry in entries:
    assert isinstance(entry, dict)
    # repo default carries no backend; skeleton is name/cron/timezone/prompt_file
    assert "backend" not in entry
    resolved = copy.deepcopy(entry)
    path = _resolve_prompt_file(resolved, repo_root)
    assert path is not None and path.exists(), f"prompt_file does not resolve: {entry}"
    _resolve_local_timezone(resolved)
    ScheduledTaskConfig(**resolved)  # validates after resolution


# --- 7. boundaries -----------------------------------------------------------


def test_prompt_and_prompt_file_both_present_raises(temp_home: Path) -> None:
  _write_cron(temp_home, [{"name": "t", "cron": "* * * * *", "prompt": "inline",
                           "prompt_file": "~/no-such.md"}])
  with pytest.raises(ScheduledTaskResolutionError):
    get_scheduled_tasks()


def test_missing_prompt_file_raises_not_swallowed(temp_home: Path) -> None:
  _write_cron(temp_home, [{"name": "t", "cron": "* * * * *",
                          "prompt_file": "~/definitely-missing.md"}])
  # The error must escape the generic reload fallback rather than being caught
  # and silently serving a stale task list. Each tick re-attempts (self-healing).
  with pytest.raises(ScheduledTaskResolutionError):
    get_scheduled_tasks()
  with pytest.raises(ScheduledTaskResolutionError):
    get_scheduled_tasks()


def test_timezone_local_resolves(temp_home: Path) -> None:
  _write_cron(temp_home, [{"name": "t", "cron": "* * * * *", "timezone": "local",
                           "prompt": "p"}])
  tasks = get_scheduled_tasks()
  assert tasks[0].timezone != "local"
  ZoneInfo(tasks[0].timezone)  # valid IANA key


def test_explicit_timezone_untouched(temp_home: Path) -> None:
  _write_cron(temp_home, [{"name": "t", "cron": "* * * * *",
                           "timezone": "America/New_York", "prompt": "p"}])
  tasks = get_scheduled_tasks()
  assert tasks[0].timezone == "America/New_York"


# --- 8. prompt file carries no host paths ------------------------------------


def test_memory_guideline_check_prompt_no_host_paths() -> None:
  body = (REPO_ROOT / "prompts" / "cron" / "memory_guideline_check.md").read_text(encoding="utf-8")
  assert "/home/" not in body
  assert "chaoli" not in body
