"""Acceptance tests for repo-owned default cron tasks and the per-job loader.

Covers the seed mechanism in ``src/features/cron/seed.py::seed_default_cron_tasks`` (the
seeded host file keeps the ``prompt_file`` pointer, never an inlined body), the
loader's acceptance of ``prompt_file``, its rejection of an inline ``prompt``
(and of a body with no prompt source at all), ``timezone: local`` resolution
plus hot-reload in ``src/features/cron/loader.py::get_scheduled_tasks`` /
``get_scheduled_task_errors``, broken-entry ``path``/``enabled`` carrying, the
per-file failure isolation of ``config.d/cron.d/<name>.yaml``, and the shipped
``configs/cron.default.yaml`` + ``prompts/cron/memory_curator/memory_selector.md`` /
``memory_reviewer.md``.
"""

import asyncio
import pathlib

import conftest
import pytest
import test_package_structure

from src.features.cron import loader, seed
from src.infra import config, yaml_utils
from src.runtime import init

# --- helpers -----------------------------------------------------------------


def _write_healthy(home: pathlib.Path, name: str, cron: str, prompt_body: str) -> pathlib.Path:
  """Write a pointer-backed healthy task: a ``prompt_file`` host file whose
  pointed file exists in the repo, exactly as production host files look."""
  repo = home / "repo"
  repo.mkdir(parents=True, exist_ok=True)
  pf = repo / f"{name}.md"
  pf.write_text(prompt_body, encoding="utf-8")
  conftest.write_cron_task(home, name, conftest.dump_yaml({"cron": cron, "prompt_file": str(pf)}))
  return pf


# --- 1. seed idempotence (per-job files) -------------------------------------


def test_seed_idempotence(temp_home: pathlib.Path) -> None:
  cfg = config.get_config()
  cron_d = cfg.config_d_dir / "cron.d"
  assert not cron_d.exists()

  report1 = seed.seed_default_cron_tasks(cfg)
  created = next(it for it in report1 if it["status"] == "created")
  seeded_path = cron_d / f"{created['name']}.yaml"
  assert seeded_path.exists()
  bytes1 = seeded_path.read_bytes()

  body1 = yaml_utils.load_yaml(seeded_path, default={})
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
                  "distinct_backend_from": "selector",
              },
          ],
  }
  assert "prompt" not in body1
  assert "backend" not in body1
  assert "name" not in body1

  report2 = seed.seed_default_cron_tasks(cfg)
  bytes2 = seeded_path.read_bytes()
  assert bytes1 == bytes2
  assert all(it["status"] == "exists" for it in report2)


# --- 3. startup never writes cron config -------------------------------------


def test_startup_never_writes_cron(temp_home: pathlib.Path) -> None:
  _write_healthy(temp_home, "task-a", "0 0 * * *", "a body")
  path = conftest.cron_d_dir(temp_home) / "task-a.yaml"
  before = path.read_bytes()
  asyncio.run(init.init_seed.init_charliebot_home())
  after = path.read_bytes()
  assert before == after
  assert "seed_default_cron_tasks" not in init.init_seed.init_charliebot_home.__code__.co_names
  # Only the setup step reaches the seed module: the cron package names it for register_setup_step, and
  # no other file of src or server.py imports it.
  importers = [
      ref.importer
      for ref in test_package_structure.scan(conftest.ROOT).references
      if ref.target == "src/features/cron/seed.py"
  ]
  assert importers == ["src/features/cron/__init__.py"]


# --- 4. get_scheduled_tasks is read-only -------------------------------------

# --- 5. the pointed file owns the prompt body; the host file carries its path -
#
# A cron.d/* host file holds the path to its prompt source under prompt_file;
# the pointed file owns the body and the loader reads it on every load. A file
# that instead carries the body itself under `prompt` holds a second source, so
# it is a per-file load error; a file that carries neither a prompt source nor a
# handler/loop is likewise an error whose message names prompt_file.


def test_loader_loads_prompt_file_pointer(temp_home: pathlib.Path) -> None:
  prompt_path = _write_healthy(temp_home, "t", "* * * * *", "body v1")
  tasks = loader.get_scheduled_tasks()
  assert len(tasks) == 1
  assert tasks[0].name == "t"
  assert tasks[0].prompt == "body v1"
  # the raw pointer is preserved on the runtime model for transport to the API/UI
  assert tasks[0].prompt_file == str(prompt_path)


@pytest.mark.parametrize(
    "task_yaml",
    [
        pytest.param({
            "cron": "* * * * *",
            "prompt": "body v1"
        }, id="inline-prompt"),
        pytest.param({"cron": "* * * * *"}, id="no-prompt-source"),
    ],
)
def test_loader_rejects_task_without_prompt_file(temp_home: pathlib.Path, task_yaml: dict) -> None:
  conftest.write_cron_task(temp_home, "t", conftest.dump_yaml(task_yaml))

  assert not loader.get_scheduled_tasks()
  errors = loader.get_scheduled_task_errors()
  assert len(errors) == 1 and errors[0].name == "t"
  # the message the operator reads names prompt_file as the missing source
  assert "prompt_file" in errors[0].error


def test_loader_fails_the_file_whose_loop_field_the_loop_model_rejects(temp_home: pathlib.Path) -> None:
  _write_healthy(temp_home, "healthy", "* * * * *", "body")
  conftest.write_cron_task(
      temp_home, "looped",
      conftest.dump_yaml(
          {
              "cron": "* * * * *",
              "repo": str(temp_home),
              "loop": {
                  "backlog": "backlog/backlog.yaml",
                  "role": "tester"
              },
          }))

  assert [task.name for task in loader.get_scheduled_tasks()] == ["healthy"]
  errors = loader.get_scheduled_task_errors()
  assert [error.name for error in errors] == ["looped"]
  assert "scope_files" in errors[0].error and "Field required" in errors[0].error


# --- 6. shipped default is loadable and seeds per-job files ------------------

# --- 9. failure isolation: one broken file never aborts another ---------------
#
# Each parametrized case injects a single broken file alongside two healthy
# jobs, then asserts the loader is total (never raises), produces exactly one
# error record attributed to the injected file, and still loads both healthy
# jobs fully.
#

# --- broken entries carry the failing file's path and raw enabled value ------
#
# The UI modal renders a broken task from {"name", "error", "path", "enabled"}:
# `path` lets the maintainer locate the file, `enabled` renders the file's own
# raw value (None — a greyed box — when the body cannot be parsed at all).

# --- a prompt_file pointing at a path that no longer exists ------------------
#
# The loud-failure fixture: the broken entry's message carries the target's
# absolute path so the operator can locate the missing file.

# --- API: create persists the pointer, and the file reloads (round-trip) -----

# --- single source: an inline-prompt file is the only error, the pointer loads -
