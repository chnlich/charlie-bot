"""Acceptance tests for repo-owned default cron tasks and the per-job loader.

Covers the seed mechanism in ``src/core/init.py::seed_default_cron_tasks``, the
loader-level ``prompt_file`` / ``timezone: local`` resolution and hot-reload in
``src/core/config.py::get_scheduled_tasks`` / ``get_scheduled_task_errors``, the
per-file failure isolation of ``config.d/cron.d/<name>.yaml``, and the shipped
``configs/cron.default.yaml`` + ``prompts/cron/memory_curator.md``.
"""

import asyncio
import copy
import os
from pathlib import Path
from zoneinfo import ZoneInfo

import pytest
import yaml
from pydantic import ValidationError

from src.core.config import (
    ScheduledTaskConfig,
    _resolve_local_timezone,
    _resolve_prompt_file,
    get_config,
    get_scheduled_task_errors,
    get_scheduled_tasks,
)
from src.core.init import init_charliebot_home, seed_default_cron_tasks
from src.core.yaml_utils import load_yaml

REPO_ROOT = Path(__file__).resolve().parents[1]

# --- helpers -----------------------------------------------------------------


def _dump(body) -> str:
  return yaml.safe_dump(body, default_flow_style=False, sort_keys=False)


def _cron_d_dir(home: Path) -> Path:
  return Path(home) / ".charliebot" / "config.d" / "cron.d"


def _write_task_text(home: Path, name: str, text: str) -> Path:
  p = _cron_d_dir(home) / f"{name}.yaml"
  p.parent.mkdir(parents=True, exist_ok=True)
  p.write_text(text, encoding="utf-8")
  return p


def _write_legacy_cron(home: Path) -> Path:
  p = Path(home) / ".charliebot" / "config.d" / "cron.yaml"
  p.parent.mkdir(parents=True, exist_ok=True)
  p.write_text("scheduled_tasks: []\n", encoding="utf-8")
  return p


@pytest.fixture
def temp_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
  """Point HOME at a temp dir and reset the config/cron module-level caches."""
  monkeypatch.setenv("HOME", str(tmp_path))
  import src.core.config as cm
  cm._config = None
  cm._config_mtime = 0.0
  cm._cron_snapshot = cm._CronSnapshot()
  return tmp_path


HEALTHY_A = {"cron": "0 0 * * *", "prompt": "a body"}
HEALTHY_B = {"cron": "0 1 * * *", "prompt": "b body"}


# --- 1. seed idempotence (per-job files) -------------------------------------


def test_seed_idempotence(temp_home: Path) -> None:
  cfg = get_config()
  cron_d = cfg.config_d_dir / "cron.d"
  assert not cron_d.exists()

  report1 = seed_default_cron_tasks(cfg)
  created = next(it for it in report1 if it["status"] == "created")
  seeded_path = cron_d / f"{created['name']}.yaml"
  assert seeded_path.exists()
  bytes1 = seeded_path.read_bytes()

  body1 = load_yaml(seeded_path, default={})
  assert body1 == {
      "cron": "27 6 * * *",
      "timezone": "local",
      "prompt_file": "prompts/cron/memory_curator.md",
  }
  assert "backend" not in body1
  assert "name" not in body1

  report2 = seed_default_cron_tasks(cfg)
  bytes2 = seeded_path.read_bytes()
  assert bytes1 == bytes2
  assert all(it["status"] == "exists" for it in report2)


# --- 2. existing job file untouched ------------------------------------------


def test_existing_entry_untouched(temp_home: Path) -> None:
  cfg = get_config()
  _write_task_text(temp_home, "memory-curator", _dump(
      {"cron": "5 5 * * *", "prompt": "custom inline prompt"}))
  path = _cron_d_dir(temp_home) / "memory-curator.yaml"
  before_bytes = path.read_bytes()

  report = seed_default_cron_tasks(cfg)
  assert all(it["status"] == "exists" for it in report)
  assert path.read_bytes() == before_bytes  # nothing written


# --- 3. startup never writes cron config -------------------------------------


def test_startup_never_writes_cron(temp_home: Path) -> None:
  _write_task_text(temp_home, "task-a", _dump(HEALTHY_A))
  path = _cron_d_dir(temp_home) / "task-a.yaml"
  before = path.read_bytes()
  asyncio.run(init_charliebot_home())
  after = path.read_bytes()
  assert before == after
  assert "seed_default_cron_tasks" not in init_charliebot_home.__code__.co_names


# --- 4. get_scheduled_tasks is read-only -------------------------------------


def test_get_scheduled_tasks_readonly(temp_home: Path) -> None:
  _write_task_text(temp_home, "t1", _dump({"cron": "* * * * *", "prompt": "p"}))
  path = _cron_d_dir(temp_home) / "t1.yaml"
  before = path.read_bytes()
  tasks = get_scheduled_tasks()
  assert len(tasks) == 1 and tasks[0].name == "t1"
  assert path.read_bytes() == before


# --- 5. prompt body tracks the repo (hot reload without restart) -------------


def test_body_tracks_repo(temp_home: Path) -> None:
  prompt_path = temp_home / "cron-prompts" / "my-task.md"
  prompt_path.parent.mkdir(parents=True, exist_ok=True)
  prompt_path.write_text("body v1", encoding="utf-8")
  m1 = prompt_path.stat().st_mtime
  _write_task_text(temp_home, "t", _dump({"cron": "* * * * *", "prompt_file": str(prompt_path)}))

  tasks = get_scheduled_tasks()
  assert tasks[0].prompt == "body v1"

  prompt_path.write_text("body v2", encoding="utf-8")
  os.utime(prompt_path, (m1 + 5.0, m1 + 5.0))

  tasks2 = get_scheduled_tasks()
  assert tasks2[0].prompt == "body v2"


# --- 6. shipped default is loadable and seeds per-job files ------------------


def test_shipped_default_loadable(temp_home: Path) -> None:
  cfg = get_config()
  repo_root = cfg.charlie_bot_repo
  defaults = load_yaml(repo_root / "configs" / "cron.default.yaml", default={})
  entries = defaults.get("scheduled_tasks", [])
  assert entries, "configs/cron.default.yaml defines no scheduled_tasks"
  for entry in entries:
    assert isinstance(entry, dict)
    assert "backend" not in entry
    resolved = copy.deepcopy(entry)
    resolved.pop("name", None)
    path = _resolve_prompt_file(resolved, repo_root)
    assert path is not None and path.exists(), f"prompt_file does not resolve: {entry}"
    _resolve_local_timezone(resolved)
    ScheduledTaskConfig(name=entry.get("name"), **resolved)

  report = seed_default_cron_tasks(cfg)
  assert any(it["status"] == "created" for it in report)
  assert [t.name for t in get_scheduled_tasks()] == [e["name"] for e in entries]


def test_seed_fails_loud_on_legacy_cron(temp_home: Path) -> None:
  cfg = get_config()
  _write_legacy_cron(temp_home)
  assert not (cfg.config_d_dir / "cron.d").exists()
  with pytest.raises(ValueError, match="legacy"):
    seed_default_cron_tasks(cfg)
  assert not (cfg.config_d_dir / "cron.d").exists(), "nothing written on legacy tripwire"


# --- timezone boundaries -----------------------------------------------------


def test_timezone_local_resolves(temp_home: Path) -> None:
  _write_task_text(temp_home, "t", _dump({"cron": "* * * * *", "timezone": "local", "prompt": "p"}))
  tasks = get_scheduled_tasks()
  assert tasks[0].timezone != "local"
  ZoneInfo(tasks[0].timezone)


def test_explicit_timezone_untouched(temp_home: Path) -> None:
  _write_task_text(temp_home, "t", _dump({"cron": "* * * * *", "timezone": "America/New_York", "prompt": "p"}))
  tasks = get_scheduled_tasks()
  assert tasks[0].timezone == "America/New_York"


# --- 8. prompt file carries no host paths ------------------------------------


def test_memory_curator_prompt_no_host_paths() -> None:
  body = (REPO_ROOT / "prompts" / "cron" / "memory_curator.md").read_text(encoding="utf-8")
  assert "/home/" not in body
  assert "chaoli" not in body


# --- 9. failure isolation: one injected file never aborts another -------------
#
# Each parametrized case injects a single broken file (or a legacy cron.yaml)
# alongside two healthy jobs, then asserts the loader is total (never raises),
# produces exactly one error record attributed to the injected file / legacy
# name, and still loads both healthy jobs fully.
#


def _inject_syntax(h: Path) -> Path:
  return _write_task_text(h, "broken-1", "cron: [unbalanced\n")


def _inject_not_mapping(h: Path) -> Path:
  return _write_task_text(h, "broken-2", "- just\n- a\n- list\n")


def _inject_name_key(h: Path) -> Path:
  return _write_task_text(h, "broken-3", _dump({"name": "should-not-survive", "cron": "0 0 * * *", "prompt": "p"}))


def _inject_unknown_key(h: Path) -> Path:
  return _write_task_text(h, "broken-4", _dump({"cron": "0 0 * * *", "prompt": "p", "promt_file": "typo.md"}))


def _inject_missing_field(h: Path) -> Path:
  return _write_task_text(h, "broken-5", _dump({"cron": "0 0 * * *"}))


def _inject_missing_prompt_file(h: Path) -> Path:
  return _write_task_text(h, "broken-6", _dump(
      {"cron": "0 0 * * *", "prompt_file": "~/definitely-missing.md"}))


def _inject_both_prompt_sources(h: Path) -> Path:
  return _write_task_text(h, "broken-7", _dump(
      {"cron": "0 0 * * *", "prompt": "p", "prompt_file": "~/also-missing.md"}))


def _inject_legacy(h: Path) -> Path:
  return _write_legacy_cron(h)


@pytest.mark.parametrize(
    "inject, expected_name, expected_path",
    [
        (_inject_syntax, "broken-1", "broken-1.yaml"),
        (_inject_not_mapping, "broken-2", "broken-2.yaml"),
        (_inject_name_key, "broken-3", "broken-3.yaml"),
        (_inject_unknown_key, "broken-4", "broken-4.yaml"),
        (_inject_missing_field, "broken-5", "broken-5.yaml"),
        (_inject_missing_prompt_file, "broken-6", "broken-6.yaml"),
        (_inject_both_prompt_sources, "broken-7", "broken-7.yaml"),
        (_inject_legacy, "cron.yaml (legacy)", "cron.yaml"),
    ],
    ids=[
        "whole-file-syntax", "not-mapping", "name-key", "unknown-key",
        "missing-required", "missing-prompt-file", "both-prompt-sources", "legacy-cron",
    ],
)
def test_single_broken_file_isolated(
    temp_home: Path,
    inject,
    expected_name: str,
    expected_path: str,
) -> None:
  _write_task_text(temp_home, "task-a", _dump(HEALTHY_A))
  _write_task_text(temp_home, "task-b", _dump(HEALTHY_B))
  inject(temp_home)

  tasks = get_scheduled_tasks()
  errors = get_scheduled_task_errors()

  assert len(tasks) == 2
  assert [t.name for t in tasks] == ["task-a", "task-b"]
  assert all(t.prompt in ("a body", "b body") for t in tasks)

  assert len(errors) == 1
  assert errors[0].name == expected_name
  assert errors[0].path.endswith(expected_path)
  assert errors[0].error


# --- hot reload: add / edit / delete / prompt-file change --------------------


def test_hot_reload_add_delete_file(temp_home: Path) -> None:
  _write_task_text(temp_home, "task-a", _dump(HEALTHY_A))
  assert [t.name for t in get_scheduled_tasks()] == ["task-a"]

  _write_task_text(temp_home, "task-c", _dump({"cron": "0 2 * * *", "prompt": "c"}))
  assert [t.name for t in get_scheduled_tasks()] == ["task-a", "task-c"]

  (_cron_d_dir(temp_home) / "task-c.yaml").unlink()
  assert [t.name for t in get_scheduled_tasks()] == ["task-a"]


def test_hot_reload_edit_existing_file(temp_home: Path) -> None:
  _write_task_text(temp_home, "task-a", _dump(HEALTHY_A))
  assert get_scheduled_tasks()[0].cron == "0 0 * * *"

  _write_task_text(temp_home, "task-a", _dump({"cron": "0 9 * * *", "prompt": "changed"}))
  tasks = get_scheduled_tasks()
  assert tasks[0].cron == "0 9 * * *" and tasks[0].prompt == "changed"


def test_hot_reload_prompt_file_edited(temp_home: Path) -> None:
  prompt_path = temp_home / "shared.md"
  prompt_path.write_text("v1", encoding="utf-8")
  m1 = prompt_path.stat().st_mtime
  _write_task_text(temp_home, "task-a", _dump({"cron": "0 0 * * *", "prompt_file": str(prompt_path)}))
  assert get_scheduled_tasks()[0].prompt == "v1"

  prompt_path.write_text("v2", encoding="utf-8")
  os.utime(prompt_path, (m1 + 9.0, m1 + 9.0))
  assert get_scheduled_tasks()[0].prompt == "v2"


def test_missing_prompt_file_after_load_becomes_error(temp_home: Path) -> None:
  prompt_path = temp_home / "gone.md"
  prompt_path.write_text("v1", encoding="utf-8")
  _write_task_text(temp_home, "task-a", _dump({"cron": "0 0 * * *", "prompt_file": str(prompt_path)}))
  assert get_scheduled_tasks()[0].prompt == "v1"

  # Deleting a referenced prompt_file must surface as an error for that job,
  # not keep serving the cached body.
  prompt_path.unlink()
  assert get_scheduled_tasks() == []
  errors = get_scheduled_task_errors()
  assert len(errors) == 1 and errors[0].name == "task-a"


# --- API: GET /tasks is total and includes broken entries --------------------


def _client(cfg):
  from fastapi import FastAPI
  from fastapi.testclient import TestClient

  from src.api import cron as api_cron
  from src.api.deps import get_session_manager
  from src.core.sessions import SessionManager
  session_mgr = SessionManager(cfg)
  app = FastAPI()
  app.include_router(api_cron.router, prefix="/api/cron")
  app.dependency_overrides[get_config] = lambda: cfg
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  return TestClient(app)


@pytest.mark.parametrize(
    "inject, expected_name",
    [
        (_inject_syntax, "broken-1"),
        (_inject_not_mapping, "broken-2"),
        (_inject_name_key, "broken-3"),
        (_inject_unknown_key, "broken-4"),
        (_inject_missing_field, "broken-5"),
        (_inject_missing_prompt_file, "broken-6"),
        (_inject_both_prompt_sources, "broken-7"),
        (_inject_legacy, "cron.yaml (legacy)"),
    ],
    ids=[
        "whole-file-syntax", "not-mapping", "name-key", "unknown-key",
        "missing-required", "missing-prompt-file", "both-prompt-sources", "legacy-cron",
    ],
)
def test_list_tasks_never_500_with_broken(temp_home: Path, inject, expected_name: str) -> None:
  _write_task_text(temp_home, "task-a", _dump(HEALTHY_A))
  _write_task_text(temp_home, "task-b", _dump(HEALTHY_B))
  inject(temp_home)
  cfg = get_config()

  with _client(cfg) as client:
    response = client.get("/api/cron/tasks")
    assert response.status_code == 200
    data = response.json()
  names = [d["name"] for d in data]
  # healthy jobs present, broken entry flagged
  assert "task-a" in names and "task-b" in names
  broken = next(d for d in data if d.get("broken") is True)
  assert broken["name"] == expected_name
  assert broken["error"]


# --- API: CRUD writes single files -------------------------------------------


def test_api_create_writes_single_file(temp_home: Path) -> None:
  cfg = get_config()
  with _client(cfg) as client:
    response = client.post("/api/cron/tasks", json={"name": "nightly", "cron": "0 2 * * *", "prompt": "run nightly"})
    assert response.status_code == 200
    path = _cron_d_dir(temp_home) / "nightly.yaml"
    assert path.exists()
    stored = load_yaml(path, default={})
    assert "name" not in stored, "persisted body must not carry a name key"
    assert stored["cron"] == "0 2 * * *" and stored["prompt"] == "run nightly"

    # 409 on an existing job
    dup = client.post("/api/cron/tasks", json={"name": "nightly", "cron": "0 3 * * *", "prompt": "dup"})
    assert dup.status_code == 409


def test_api_put_round_trips_single_file(temp_home: Path) -> None:
  cfg = get_config()
  with _client(cfg) as client:
    client.post("/api/cron/tasks", json={"name": "nightly", "cron": "0 2 * * *", "prompt": "run nightly"})
    response = client.put("/api/cron/tasks/nightly", json={"cron": "0 4 * * *"})
    assert response.status_code == 200
    path = _cron_d_dir(temp_home) / "nightly.yaml"
    assert load_yaml(path, default={})["cron"] == "0 4 * * *"
    assert "name" not in load_yaml(path, default={})

    missing = client.put("/api/cron/tasks/absent", json={"cron": "0 5 * * *"})
    assert missing.status_code == 404


def test_api_put_409_on_broken_file(temp_home: Path) -> None:
  cfg = get_config()
  _write_task_text(temp_home, "broken", "cron: [unbalanced\n")
  with _client(cfg) as client:
    response = client.put("/api/cron/tasks/broken", json={"cron": "0 5 * * *"})
    assert response.status_code == 409
    assert response.json()["detail"]


def test_api_delete_removes_file(temp_home: Path) -> None:
  cfg = get_config()
  _write_task_text(temp_home, "task-a", _dump(HEALTHY_A))
  path = _cron_d_dir(temp_home) / "task-a.yaml"
  with _client(cfg) as client:
    response = client.delete("/api/cron/tasks/task-a")
    assert response.status_code == 200
    assert not path.exists()
    missing = client.delete("/api/cron/tasks/task-a")
    assert missing.status_code == 404


def test_api_delete_broken_file_repairs(temp_home: Path) -> None:
  cfg = get_config()
  _write_task_text(temp_home, "broken", "cron: [unbalanced\n")
  path = _cron_d_dir(temp_home) / "broken.yaml"
  assert get_scheduled_task_errors()
  with _client(cfg) as client:
    response = client.delete("/api/cron/tasks/broken")
    assert response.status_code == 200
    assert not path.exists()
    # loader surface is clean after the delete
    assert get_scheduled_tasks() == []
    assert get_scheduled_task_errors() == []


# --- API: name traversal is rejected with 400 --------------------------------


@pytest.mark.parametrize(
    "bad",
    ["..", "a/b", ".lead", "/abs"],
    ids=["dotdot", "slash", "leading-dot", "leading-slash"],
)
def test_api_rejects_path_traversal_name(temp_home: Path, bad: str) -> None:
  cfg = get_config()
  with _client(cfg) as client:
    r_put = client.put(f"/api/cron/tasks/{bad}", json={"cron": "0 1 * * *"})
    r_del = client.delete(f"/api/cron/tasks/{bad}")
    r_post = client.post("/api/cron/tasks", json={"name": bad, "cron": "0 1 * * *", "prompt": "p"})
  # Every malicious name must be rejected before reaching the filesystem. A
  # single-segment bad name (e.g. ``a..``, ``.lead``) hits the handler's regex
  # check (400); path-shaped names (``..``, ``a/b``, ``/abs``) are blocked even
  # earlier by the URL normalizer/router (404). Either way nothing is written.
  assert r_put.status_code in (400, 404)
  assert r_del.status_code in (400, 404)
  assert r_post.status_code in (400, 404)
  # no files escaped cron.d/ (a ".." or "a/b" name must never reach the FS)
  assert not (temp_home / ".charliebot" / "config.d" / bad).exists()
  assert list(_cron_d_dir(temp_home).glob("*.yaml")) == []


# --- config surface: no base-branch key ----------------------------------------


def test_scheduled_task_config_has_no_base_branch_field() -> None:
  """Branch policy must not migrate back into per-task YAML: the launch fallback
  resolves a base-less task's base from the remote's default branch, and
  ``extra='forbid'`` keeps a stray ``base_branch:`` key an error, not a silent drop."""
  assert "base_branch" not in ScheduledTaskConfig.model_fields
  with pytest.raises(ValidationError, match="base_branch"):
    ScheduledTaskConfig(name="t", cron="0 0 * * *", prompt="p", base_branch="main")  # type: ignore[call-arg]
