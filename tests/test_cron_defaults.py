"""Acceptance tests for repo-owned default cron tasks and the per-job loader.

Covers the seed mechanism in ``src/core/init.py::seed_default_cron_tasks`` (the
seeded host file keeps the ``prompt_file`` pointer, never an inlined body), the
loader's acceptance of ``prompt_file``, its rejection of an inline ``prompt``
(and of a body with no prompt source at all), ``timezone: local`` resolution
plus hot-reload in ``src/core/config.py::get_scheduled_tasks`` /
``get_scheduled_task_errors``, broken-entry ``path``/``enabled`` carrying, the
per-file failure isolation of ``config.d/cron.d/<name>.yaml``, and the shipped
``configs/cron.default.yaml`` + ``prompts/cron/memory_curator/memory_selector.md`` /
``memory_reviewer.md``.
"""

import asyncio
import copy
import os
from pathlib import Path
from zoneinfo import ZoneInfo

import pytest
from conftest import cron_d_dir as _cron_d_dir
from conftest import dump_yaml as _dump
from conftest import write_cron_task as _write_task_text
from pydantic import ValidationError

from src.core.config import (
    ScheduledTaskConfig,
    _load_cron_file,
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


def _write_healthy(home: Path, name: str, cron: str, prompt_body: str) -> Path:
  """Write a pointer-backed healthy task: a ``prompt_file`` host file whose
  pointed file exists in the repo, exactly as production host files look."""
  repo = home / "repo"
  repo.mkdir(parents=True, exist_ok=True)
  pf = repo / f"{name}.md"
  pf.write_text(prompt_body, encoding="utf-8")
  _write_task_text(home, name, _dump({"cron": cron, "prompt_file": str(pf)}))
  return pf


def _write_legacy_cron(home: Path) -> Path:
  p = Path(home) / ".charliebot" / "config.d" / "cron.yaml"
  p.parent.mkdir(parents=True, exist_ok=True)
  p.write_text("scheduled_tasks: []\n", encoding="utf-8")
  return p


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
  # The seeded host file keeps the repo pointers: the pointed files own the
  # prompt bodies, and the host file carries only the paths to them.
  assert body1 == {
      "cron":
          "27 6 * * *",
      "timezone":
          "local",
      "steps":
          [
              {
                  "name": "selector",
                  "prompt_file": "prompts/cron/memory_curator/memory_selector.md",
              },
              {
                  "name": "reviewer",
                  "prompt_file": "prompts/cron/memory_curator/memory_reviewer.md",
              },
          ],
  }
  assert "prompt" not in body1
  assert "backend" not in body1
  assert "name" not in body1

  report2 = seed_default_cron_tasks(cfg)
  bytes2 = seeded_path.read_bytes()
  assert bytes1 == bytes2
  assert all(it["status"] == "exists" for it in report2)


# --- 2. existing job file untouched ------------------------------------------


def test_existing_entry_untouched(temp_home: Path) -> None:
  cfg = get_config()
  _write_task_text(temp_home, "memory-curator", _dump({"cron": "5 5 * * *", "prompt_file": "prompts/whatever.md"}))
  path = _cron_d_dir(temp_home) / "memory-curator.yaml"
  before_bytes = path.read_bytes()

  report = seed_default_cron_tasks(cfg)
  assert all(it["status"] == "exists" for it in report)
  assert path.read_bytes() == before_bytes  # nothing written


# --- 3. startup never writes cron config -------------------------------------


def test_startup_never_writes_cron(temp_home: Path) -> None:
  _write_healthy(temp_home, "task-a", "0 0 * * *", "a body")
  path = _cron_d_dir(temp_home) / "task-a.yaml"
  before = path.read_bytes()
  asyncio.run(init_charliebot_home())
  after = path.read_bytes()
  assert before == after
  assert "seed_default_cron_tasks" not in init_charliebot_home.__code__.co_names


# --- 4. get_scheduled_tasks is read-only -------------------------------------


def test_get_scheduled_tasks_readonly(temp_home: Path) -> None:
  _write_healthy(temp_home, "t1", "* * * * *", "p")
  path = _cron_d_dir(temp_home) / "t1.yaml"
  before = path.read_bytes()
  tasks = get_scheduled_tasks()
  assert len(tasks) == 1 and tasks[0].name == "t1"
  assert path.read_bytes() == before


# --- 5. the pointed file owns the prompt body; the host file carries its path -
#
# A cron.d/* host file holds the path to its prompt source under prompt_file;
# the pointed file owns the body and the loader reads it on every load. A file
# that instead carries the body itself under `prompt` holds a second source, so
# it is a per-file load error; a file that carries neither a prompt source nor a
# handler/loop is likewise an error whose message names prompt_file.


def test_loader_loads_prompt_file_pointer(temp_home: Path) -> None:
  prompt_path = _write_healthy(temp_home, "t", "* * * * *", "body v1")
  tasks = get_scheduled_tasks()
  assert len(tasks) == 1
  assert tasks[0].name == "t"
  assert tasks[0].prompt == "body v1"
  # the raw pointer is preserved on the runtime model for transport to the API/UI
  assert tasks[0].prompt_file == str(prompt_path)


def test_loader_rejects_inline_prompt(temp_home: Path) -> None:
  _write_task_text(temp_home, "t", _dump({"cron": "* * * * *", "prompt": "body v1"}))

  assert not get_scheduled_tasks()
  errors = get_scheduled_task_errors()
  assert len(errors) == 1 and errors[0].name == "t"
  assert "prompt_file" in errors[0].error


def test_loader_rejects_promptless_with_no_handler_or_loop(temp_home: Path) -> None:
  _write_task_text(temp_home, "t", _dump({"cron": "* * * * *"}))

  assert not get_scheduled_tasks()
  errors = get_scheduled_task_errors()
  assert len(errors) == 1 and errors[0].name == "t"
  # the message the operator reads names prompt_file as the missing source
  assert "prompt_file" in errors[0].error


def test_promptless_handler_task_loads(temp_home: Path) -> None:
  """A handler task carries no prompt source of any kind and still loads."""
  _write_task_text(temp_home, "backup", _dump({"cron": "0 3 * * *", "handler": "backup"}))
  tasks = get_scheduled_tasks()
  assert len(tasks) == 1
  assert tasks[0].name == "backup"
  assert tasks[0].handler == "backup"
  assert tasks[0].prompt is None and tasks[0].prompt_file is None


def test_missing_pointer_target_recovers_when_restored(temp_home: Path) -> None:
  """A pointer whose target is missing is a loud per-file error; restoring the
  target flips the file back to healthy on the next load, without a restart."""
  prompt_path = temp_home / "prompt.md"
  _write_task_text(temp_home, "task-a", _dump({"cron": "0 0 * * *", "prompt_file": str(prompt_path)}))
  assert [e.name for e in get_scheduled_task_errors()] == ["task-a"]

  prompt_path.write_text("v1", encoding="utf-8")
  assert not get_scheduled_task_errors()
  assert get_scheduled_tasks()[0].prompt == "v1"


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
    if resolved.get("prompt_file"):
      path = _resolve_prompt_file(resolved, repo_root)
      assert path is not None and path.exists(), f"prompt_file does not resolve: {entry}"
    for step in resolved.get("steps") or []:
      step_path = _resolve_prompt_file(step, repo_root)
      assert step_path is not None and step_path.exists(), f"step prompt_file does not resolve: {entry}"
    _resolve_local_timezone(resolved)
    ScheduledTaskConfig(name=entry.get("name"), **resolved)

  report = seed_default_cron_tasks(cfg)
  assert any(it["status"] == "created" for it in report)
  assert [t.name for t in get_scheduled_tasks()] == [e["name"] for e in entries]
  for entry in entries:
    seeded = load_yaml(cfg.config_d_dir / "cron.d" / f"{entry['name']}.yaml", default={})
    # seeded host files keep the pointer, never an inlined body
    assert "prompt" not in seeded
    if "prompt_file" in entry:
      assert seeded["prompt_file"] == entry["prompt_file"]
    if "steps" in entry:
      assert seeded["steps"] == entry["steps"]


def test_seed_fails_loud_on_legacy_cron(temp_home: Path) -> None:
  cfg = get_config()
  _write_legacy_cron(temp_home)
  assert not (cfg.config_d_dir / "cron.d").exists()
  with pytest.raises(ValueError, match="legacy"):
    seed_default_cron_tasks(cfg)
  assert not (cfg.config_d_dir / "cron.d").exists(), "nothing written on legacy tripwire"


# --- timezone boundaries -----------------------------------------------------


def test_timezone_local_resolves(temp_home: Path) -> None:
  prompt_path = _write_healthy(temp_home, "t", "* * * * *", "p")
  _write_task_text(temp_home, "t", _dump({"cron": "* * * * *", "timezone": "local", "prompt_file": str(prompt_path)}))
  tasks = get_scheduled_tasks()
  assert tasks[0].timezone != "local"
  ZoneInfo(tasks[0].timezone)


def test_explicit_timezone_untouched(temp_home: Path) -> None:
  prompt_path = _write_healthy(temp_home, "t", "* * * * *", "p")
  _write_task_text(
      temp_home, "t", _dump({
          "cron": "* * * * *",
          "timezone": "America/New_York",
          "prompt_file": str(prompt_path)
      }))
  tasks = get_scheduled_tasks()
  assert tasks[0].timezone == "America/New_York"


# --- 8. prompt file carries no host paths ------------------------------------


def test_memory_curator_prompts_no_host_paths() -> None:
  for filename in ("memory_selector.md", "memory_reviewer.md"):
    body = (REPO_ROOT / "prompts" / "cron" / "memory_curator" / filename).read_text(encoding="utf-8")
    assert "/home/" not in body
    assert "chaoli" not in body


# --- 9. failure isolation: one broken file never aborts another ---------------
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
  return _write_task_text(
      h, "broken-3", _dump({
          "name": "should-not-survive",
          "cron": "0 0 * * *",
          "prompt_file": "p.md"
      }))


def _inject_unknown_key(h: Path) -> Path:
  return _write_task_text(h, "broken-4", _dump({"cron": "0 0 * * *", "prompt_file": "p.md", "promt_file": "typo.md"}))


def _inject_missing_source(h: Path) -> Path:
  return _write_task_text(h, "broken-5", _dump({"cron": "0 0 * * *"}))


def _inject_missing_prompt_file(h: Path) -> Path:
  return _write_task_text(h, "broken-6", _dump({"cron": "0 0 * * *", "prompt_file": "~/definitely-missing.md"}))


def _inject_inline_prompt(h: Path) -> Path:
  return _write_task_text(
      h, "broken-7", _dump({
          "cron": "0 0 * * *",
          "prompt": "inline",
          "prompt_file": "~/also-missing.md"
      }))


def _inject_legacy(h: Path) -> Path:
  return _write_legacy_cron(h)


# The broken-file taxonomy is defined once, here: both parametrized consumers
# below (loader isolation and the API total test) read this list, so adding a
# case happens in this one place.
_BROKEN_CASES = [
    (_inject_syntax, "broken-1", "broken-1.yaml"),
    (_inject_not_mapping, "broken-2", "broken-2.yaml"),
    (_inject_name_key, "broken-3", "broken-3.yaml"),
    (_inject_unknown_key, "broken-4", "broken-4.yaml"),
    (_inject_missing_source, "broken-5", "broken-5.yaml"),
    (_inject_missing_prompt_file, "broken-6", "broken-6.yaml"),
    (_inject_inline_prompt, "broken-7", "broken-7.yaml"),
    (_inject_legacy, "cron.yaml (legacy)", "cron.yaml"),
]
_BROKEN_CASE_IDS = [
    "whole-file-syntax",
    "not-mapping",
    "name-key",
    "unknown-key",
    "missing-required-source",
    "missing-prompt-file",
    "inline-prompt",
    "legacy-cron",
]


@pytest.mark.parametrize(("inject", "expected_name", "expected_path"), _BROKEN_CASES, ids=_BROKEN_CASE_IDS)
def test_single_broken_file_isolated(
    temp_home: Path,
    inject,
    expected_name: str,
    expected_path: str,
) -> None:
  _write_healthy(temp_home, "task-a", "0 0 * * *", "a body")
  _write_healthy(temp_home, "task-b", "0 1 * * *", "b body")
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


# --- hot reload: add / edit / delete / pointed-file change --------------------


def test_hot_reload_add_delete_file(temp_home: Path) -> None:
  _write_healthy(temp_home, "task-a", "0 0 * * *", "a body")
  assert [t.name for t in get_scheduled_tasks()] == ["task-a"]

  _write_healthy(temp_home, "task-c", "0 2 * * *", "c body")
  assert [t.name for t in get_scheduled_tasks()] == ["task-a", "task-c"]

  (_cron_d_dir(temp_home) / "task-c.yaml").unlink()
  assert [t.name for t in get_scheduled_tasks()] == ["task-a"]


def test_hot_reload_edit_existing_file(temp_home: Path) -> None:
  _write_healthy(temp_home, "task-a", "0 0 * * *", "a body")
  assert get_scheduled_tasks()[0].cron == "0 0 * * *"

  _write_task_text(
      temp_home, "task-a", _dump({
          "cron": "0 9 * * *",
          "prompt_file": str(temp_home / "repo" / "task-a.md")
      }))
  tasks = get_scheduled_tasks()
  assert tasks[0].cron == "0 9 * * *" and tasks[0].prompt == "a body"


def test_hot_reload_pointed_file_change_live_body(temp_home: Path) -> None:
  """Rewriting the pointed file (and advancing its mtime) is picked up on the
  next call: the yaml is untouched and no restart is involved."""
  prompt_path = _write_healthy(temp_home, "task-a", "0 0 * * *", "body v1")
  assert get_scheduled_tasks()[0].prompt == "body v1"
  yaml_path = _cron_d_dir(temp_home) / "task-a.yaml"
  yaml_before = yaml_path.read_bytes()

  prompt_path.write_text("body v2", encoding="utf-8")
  m = prompt_path.stat().st_mtime
  os.utime(prompt_path, (m + 5.0, m + 5.0))

  assert get_scheduled_tasks()[0].prompt == "body v2"
  assert yaml_path.read_bytes() == yaml_before


# --- broken entries carry the failing file's path and raw enabled value ------
#
# The UI modal renders a broken task from {"name", "error", "path", "enabled"}:
# `path` lets the maintainer locate the file, `enabled` renders the file's own
# raw value (None — a greyed box — when the body cannot be parsed at all).


def test_broken_entry_carries_absolute_path_and_enabled(temp_home: Path) -> None:
  injected = _write_task_text(
      temp_home, "broken", _dump({
          "cron": "0 0 * * *",
          "prompt_file": "~/typo-missing.md",
          "enabled": False
      }))
  errors = get_scheduled_task_errors()
  assert len(errors) == 1
  assert errors[0].path == str(injected)
  assert Path(errors[0].path).is_absolute()
  assert errors[0].enabled is False


def test_broken_entry_enabled_null_on_unparseable_yaml(temp_home: Path) -> None:
  _write_task_text(temp_home, "broken", "cron: [unbalanced\n")
  errors = get_scheduled_task_errors()
  assert len(errors) == 1
  assert errors[0].enabled is None


# --- a prompt_file pointing at a path that no longer exists ------------------
#
# The loud-failure fixture: the broken entry's message carries the target's
# absolute path so the operator can locate the missing file.


def test_broken_prompt_file_missing_path(temp_home: Path) -> None:
  missing = temp_home / "prompts" / "cron" / "memory_curator" / "memory_selector.md"
  injected = _write_task_text(
      temp_home, "memory-curator",
      _dump({
          "cron": "27 6 * * *",
          "timezone": "local",
          "prompt_file": str(missing),
          "enabled": True
      }))

  assert not get_scheduled_tasks()
  errors = get_scheduled_task_errors()
  assert len(errors) == 1
  err = errors[0]
  assert err.name == "memory-curator"
  assert err.path == str(injected)
  assert err.enabled is True
  assert str(missing) in err.error  # the loud message carries the absolute path


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
    ("inject", "expected_name"),
    [(inject, name) for inject, name, _ in _BROKEN_CASES],
    ids=_BROKEN_CASE_IDS,
)
def test_list_tasks_never_500_with_broken(temp_home: Path, inject, expected_name: str) -> None:
  _write_healthy(temp_home, "task-a", "0 0 * * *", "a body")
  _write_healthy(temp_home, "task-b", "0 1 * * *", "b body")
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
  # The modal's broken error view consumes exactly these keys.
  assert set(broken) == {"name", "error", "broken", "path", "enabled"}
  assert Path(broken["path"]).is_absolute()


def test_list_tasks_omits_resolved_prompt(temp_home: Path) -> None:
  """GET /tasks rows omit prompt: the list payload served whole resolved bodies
  (~90 KB on the seed host) that no consumer reads; the UI edits prompt_file."""
  _write_healthy(temp_home, "task-a", "0 0 * * *", "a resolved body")
  repo = temp_home / "repo"
  step_pf = repo / "step-one.md"
  step_pf.write_text("resolved step body", encoding="utf-8")
  _write_task_text(
      temp_home, "task-chain", _dump({
          "cron": "0 3 * * *",
          "steps": [{
              "name": "one",
              "prompt_file": str(step_pf)
          }],
      }))
  cfg = get_config()

  with _client(cfg) as client:
    response = client.get("/api/cron/tasks")
    assert response.status_code == 200
    data = response.json()
  row = next(d for d in data if d["name"] == "task-a")
  assert "prompt" not in row
  assert "prompt_file" in row
  chain = next(d for d in data if d["name"] == "task-chain")
  assert "prompt" not in chain["steps"][0]
  assert "prompt_file" in chain["steps"][0]


# --- API: create persists the pointer, and the file reloads (round-trip) -----


def _assert_pointer_round_trip(home: Path, prompt_path: Path, name: str, cron: str) -> None:
  """Both halves of the round-trip regression: the persisted file still carries
  prompt_file (and no prompt), AND it reloads through the loader into the
  resolved body. Asserting only the reload would pass the old inlining behavior
  too, so both halves are required."""
  cron_dir = _cron_d_dir(home)
  stored = load_yaml(cron_dir / f"{name}.yaml", default={})
  assert stored["prompt_file"] == str(prompt_path)
  assert "prompt" not in stored
  cfg = get_config()
  task, _ = _load_cron_file(cron_dir / f"{name}.yaml", cfg.charlie_bot_repo, name)
  assert task.prompt == prompt_path.read_text(encoding="utf-8")
  assert task.prompt_file == str(prompt_path)


def test_api_create_round_trips_prompt_file(temp_home: Path) -> None:
  """POST /tasks with prompt_file persists the pointer, never the body; the
  persisted file reloads through the loader into the resolved body."""
  prompt_path = temp_home / "prompts" / "nightly.md"
  prompt_path.parent.mkdir(parents=True, exist_ok=True)
  body = "Rebase omni main and report status.\n"
  prompt_path.write_text(body, encoding="utf-8")
  cfg = get_config()
  with _client(cfg) as client:
    response = client.post(
        "/api/cron/tasks", json={
            "name": "nightly",
            "cron": "0 2 * * *",
            "prompt_file": str(prompt_path)
        })
    assert response.status_code == 200
    assert response.json()["prompt_file"] == str(prompt_path)
    assert "prompt" not in response.json()
  _assert_pointer_round_trip(temp_home, prompt_path, "nightly", "0 2 * * *")


def test_api_put_round_trips_prompt_file(temp_home: Path) -> None:
  """PUT /tasks/<name> with prompt_file persists the pointer, not a body, and
  the persisted file reloads into the resolved body."""
  prompt_path = temp_home / "prompts" / "nightly.md"
  prompt_path.parent.mkdir(parents=True, exist_ok=True)
  body = "Rebase nightly orchestrator and report status.\n"
  prompt_path.write_text(body, encoding="utf-8")
  cfg = get_config()
  with _client(cfg) as client:
    created = client.post(
        "/api/cron/tasks", json={
            "name": "nightly",
            "cron": "0 2 * * *",
            "prompt_file": str(prompt_path)
        })
    assert created.status_code == 200
    response = client.put("/api/cron/tasks/nightly", json={"cron": "0 4 * * *", "prompt_file": str(prompt_path)})
    assert response.status_code == 200
  _assert_pointer_round_trip(temp_home, prompt_path, "nightly", "0 4 * * *")


# --- single source: an inline-prompt file is the only error, the pointer loads -


def test_single_source_inline_and_pointer(temp_home: Path) -> None:
  """A cron.d directory holding one inline-prompt file and one pointer file:
  only the inline file appears in the error list, and the pointer file loads
  normally."""
  _write_task_text(temp_home, "inline", _dump({"cron": "0 1 * * *", "prompt": "inline body"}))
  prompt_path = _write_healthy(temp_home, "pointer", "0 2 * * *", "pointed body")

  tasks = get_scheduled_tasks()
  errors = get_scheduled_task_errors()
  assert [t.name for t in tasks] == ["pointer"]
  assert tasks[0].prompt == "pointed body"
  assert tasks[0].prompt_file == str(prompt_path)
  assert [e.name for e in errors] == ["inline"]
  assert "prompt_file" in errors[0].error


# --- API: CRUD writes single files -------------------------------------------


def test_api_create_writes_single_file(temp_home: Path) -> None:
  cfg = get_config()
  prompt_path = temp_home / "prompts" / "nightly.md"
  prompt_path.parent.mkdir(parents=True, exist_ok=True)
  prompt_path.write_text("run nightly", encoding="utf-8")
  with _client(cfg) as client:
    response = client.post(
        "/api/cron/tasks", json={
            "name": "nightly",
            "cron": "0 2 * * *",
            "prompt_file": str(prompt_path)
        })
    assert response.status_code == 200
    path = _cron_d_dir(temp_home) / "nightly.yaml"
    assert path.exists()
    stored = load_yaml(path, default={})
    assert "name" not in stored, "persisted body must not carry a name key"
    assert stored["cron"] == "0 2 * * *" and stored["prompt_file"] == str(prompt_path)
    assert "prompt" not in stored

    # 409 on an existing job
    dup = client.post("/api/cron/tasks", json={"name": "nightly", "cron": "0 3 * * *", "prompt_file": str(prompt_path)})
    assert dup.status_code == 409


def test_api_put_round_trips_single_file(temp_home: Path) -> None:
  cfg = get_config()
  prompt_path = temp_home / "prompts" / "nightly.md"
  prompt_path.parent.mkdir(parents=True, exist_ok=True)
  prompt_path.write_text("run nightly", encoding="utf-8")
  with _client(cfg) as client:
    client.post("/api/cron/tasks", json={"name": "nightly", "cron": "0 2 * * *", "prompt_file": str(prompt_path)})
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
  _write_healthy(temp_home, "task-a", "0 0 * * *", "a body")
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
    assert not get_scheduled_tasks()
    assert not get_scheduled_task_errors()


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
    r_post = client.post("/api/cron/tasks", json={"name": bad, "cron": "0 1 * * *", "prompt_file": "p.md"})
  # Every malicious name must be rejected before reaching the filesystem. A
  # single-segment bad name (e.g. ``a..``, ``.lead``) hits the handler's regex
  # check (400); path-shaped names (``..``, ``a/b``, ``/abs``) are blocked even
  # earlier by the URL normalizer/router (404). Either way nothing is written.
  assert r_put.status_code in (400, 404)
  assert r_del.status_code in (400, 404)
  assert r_post.status_code in (400, 404)
  # no files escaped cron.d/ (a ".." or "a/b" name must never reach the FS)
  assert not (temp_home / ".charliebot" / "config.d" / bad).exists()
  assert not list(_cron_d_dir(temp_home).glob("*.yaml"))


# --- config surface: no base-branch key ----------------------------------------


def test_scheduled_task_config_has_no_base_branch_field() -> None:
  """Branch policy must not migrate back into per-task YAML: the launch fallback
  resolves a base-less task's base from the remote's default branch, and
  ``extra='forbid'`` keeps a stray ``base_branch:`` key an error, not a silent drop."""
  assert "base_branch" not in ScheduledTaskConfig.model_fields
  with pytest.raises(ValidationError, match="base_branch"):
    ScheduledTaskConfig(name="t", cron="0 0 * * *", prompt="p", base_branch="main")  # type: ignore[call-arg]
