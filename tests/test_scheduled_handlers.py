"""Packages register their cron handlers and the loop action; the scheduler names no package.

The registry refuses a second registration, answers None for a name no package registered,
and imports a handler's module only when the handler resolves, so the scheduler's import
carries no backup, storage or usage stack.
"""

import json
import pathlib
import subprocess
import sys

import conftest
import pytest

from src.features.backlog import backlog_loop
from src.features.backlog import config as backlog_config
from src.features.backup import backup
from src.features.storage import storage_cool
from src.features.usage import usage_ledger
from src.runtime.hooks import scheduled_handlers

HANDLER_STACKS = (
    "tarfile", "src.features.backup.backup", "src.features.storage.storage_cool", "src.features.usage.usage_ledger")


def run_probe(source: str) -> object:
  """Run source in a fresh process that has not called register_all(); return the JSON line it prints last."""
  probe = subprocess.run(
      [sys.executable, "-c", source], cwd=conftest.ROOT, capture_output=True, text=True, timeout=60, check=False)
  assert probe.returncode == 0, probe.stderr
  return json.loads(probe.stdout.splitlines()[-1])


def test_a_second_registration_raises() -> None:
  with pytest.raises(ValueError, match="backup"):
    scheduled_handlers.register_handler("backup", "src.features.backup.backup", attr="run_scheduled_backup")
  with pytest.raises(ValueError, match="loop action"):
    scheduled_handlers.register_loop_action(
        "src.features.backlog.backlog_loop",
        attr="scheduled_loop_action",
        model="src.features.backlog.config:ImprovementLoopConfig",
    )

  assert scheduled_handlers.handler("backup") is backup.run_scheduled_backup


def test_an_unregistered_name_resolves_to_none() -> None:
  assert scheduled_handlers.handler("missing") is None


def test_the_loop_action_of_an_empty_registry_raises() -> None:
  probe = """
import json
from src.runtime.hooks import scheduled_handlers
try:
  scheduled_handlers.loop_action()
except LookupError:
  print(json.dumps("LookupError"))
"""
  assert run_probe(probe) == "LookupError"


def test_the_loop_model_of_an_empty_registry_raises() -> None:
  probe = """
import json
from src.runtime.hooks import scheduled_handlers
try:
  scheduled_handlers.loop_model()
except LookupError:
  print(json.dumps("LookupError"))
"""
  assert run_probe(probe) == "LookupError"


def test_a_loop_task_loads_as_a_file_error_when_no_package_registered_the_loop_section(
    profile_home: pathlib.Path) -> None:
  prompt = profile_home / "prompt.md"
  prompt.write_text("body", encoding="utf-8")
  cron_d = profile_home / "config.d" / "cron.d"
  cron_d.mkdir(parents=True)
  (cron_d / "plain.yaml").write_text(f"cron: '* * * * *'\nprompt_file: {prompt}\n", encoding="utf-8")
  (cron_d / "looped.yaml").write_text(
      f"cron: '* * * * *'\nrepo: {profile_home}\nloop:\n  backlog: b.yaml\n  role: r\n  scope_files: [x]\n",
      encoding="utf-8")
  probe = """
import json
from src.app import registrations
registrations.PACKAGES = tuple(p for p in registrations.PACKAGES if p != "src.features.backlog")
registrations.register_all()
from src.features.cron import loader
print(json.dumps([[task.name for task in loader.get_scheduled_tasks()],
                  [[error.name, error.error] for error in loader.get_scheduled_task_errors()]]))
"""
  tasks, errors = run_probe(probe)

  assert tasks == ["plain"]
  assert [name for name, _ in errors] == ["looped"]
  assert "no package registered a loop section" in errors[0][1]


def test_the_packages_register_the_built_in_handlers_and_the_loop_action() -> None:
  assert scheduled_handlers.handler("backup") is backup.run_scheduled_backup
  assert scheduled_handlers.handler("cool_storage") is storage_cool.run_scheduled_cool_storage
  assert scheduled_handlers.handler("usage_ledger") is usage_ledger.run_scheduled_usage_ledger
  assert scheduled_handlers.loop_action() is backlog_loop.scheduled_loop_action
  assert scheduled_handlers.loop_model() is backlog_config.ImprovementLoopConfig


def test_importing_the_scheduler_loads_no_handler_stack() -> None:
  probe = f"""
import json
import sys
from src.app import registrations
registrations.register_all()
import src.features.cron.scheduler
from src.runtime.hooks import scheduled_handlers
loaded_at_import = [name for name in {HANDLER_STACKS!r} if name in sys.modules]
scheduled_handlers.handler("backup")
print(json.dumps([loaded_at_import, "src.features.backup.backup" in sys.modules]))
"""
  loaded_at_import, loaded_on_resolve = run_probe(probe)

  assert loaded_at_import == []
  assert loaded_on_resolve
