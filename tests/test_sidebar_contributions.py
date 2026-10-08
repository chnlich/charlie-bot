"""The sidebar-contributions hook (src/runtime/hooks/sidebar_contributions.py) and the packages that fill it.

The registry tests run against an empty registry swapped in for the process-wide one. The deletion test
runs a fresh interpreter that drops one package line from PACKAGES: the session store must keep working
without the recap memo hook and without the Threads view.
"""

import json
import os
import subprocess
import sys
import types
from pathlib import Path

import conftest
import pytest

from src.infra import models
from src.runtime import scheduled_sessions
from src.runtime.hooks import sidebar_contributions as hook


@pytest.fixture
def empty_registry(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr(hook, "_registered", {})
  monkeypatch.setattr(hook, "_resolved", None)


class Roots(hook.SidebarContribution):
  """Answers the view named by a session's name prefix: "alpha-..." roots view alpha, "beta-..." roots view beta."""

  def view_member(self, meta: models.SessionMetadata) -> str | None:
    return meta.name.split("-")[0] if meta.name.startswith(("alpha-", "beta-")) else None


def stub_module(monkeypatch: pytest.MonkeyPatch, name: str, **attrs: object) -> None:
  monkeypatch.setitem(sys.modules, name, types.SimpleNamespace(**attrs))


def test_the_base_class_adds_nothing(tmp_path: Path) -> None:
  contribution = hook.SidebarContribution()

  assert contribution.watched_files == ()
  assert contribution.row_flags(tmp_path, "sid") == {}
  assert contribution.view_member(models.SessionMetadata(profile="manager", name="any")) is None
  assert contribution.copy_on_fork(tmp_path, tmp_path) is None
  assert contribution.drop_runtime_state("sid") is None


@pytest.mark.usefixtures("empty_registry")
def test_a_contribution_imports_on_first_use_and_comes_back_in_registration_order(
    monkeypatch: pytest.MonkeyPatch) -> None:
  second, first = hook.SidebarContribution(), hook.SidebarContribution()
  hook.register_sidebar_contribution("second", "not_imported_yet_a:contribution")
  hook.register_sidebar_contribution("first", "not_imported_yet_b:contribution")
  assert "not_imported_yet_a" not in sys.modules

  stub_module(monkeypatch, "not_imported_yet_a", contribution=second)
  stub_module(monkeypatch, "not_imported_yet_b", contribution=first)

  assert hook.sidebar_contributions() == (second, first)


@pytest.mark.usefixtures("empty_registry")
def test_a_contribution_instance_can_be_registered_directly() -> None:
  contribution = hook.SidebarContribution()

  hook.register_sidebar_contribution("inline", contribution)

  assert hook.sidebar_contributions() == (contribution,)


@pytest.mark.usefixtures("empty_registry")
def test_a_second_registration_of_one_name_raises_and_leaves_the_registry_alone() -> None:
  hook.register_sidebar_contribution("alpha", "module_a:contribution")

  with pytest.raises(ValueError, match="alpha"):
    hook.register_sidebar_contribution("alpha", "module_b:contribution")

  assert hook._registered == {"alpha": "module_a:contribution"}


@pytest.mark.usefixtures("empty_registry")
def test_every_view_a_contribution_answers_maps_its_own_subtree(monkeypatch: pytest.MonkeyPatch) -> None:
  stub_module(monkeypatch, "roots_module", contribution=Roots())
  hook.register_sidebar_contribution("roots", "roots_module:contribution")
  alpha_root = models.SessionMetadata(profile="manager", name="alpha-root")
  alpha_child = models.SessionMetadata(profile="manager", name="child", task_parent_id=alpha_root.id)
  beta_root = models.SessionMetadata(profile="manager", name="beta-root")
  plain = models.SessionMetadata(profile="manager", name="plain")

  views = scheduled_sessions.view_subtree_roots([alpha_child, plain, alpha_root, beta_root])

  assert views == {
      "alpha": {
          alpha_root.id: alpha_root.id,
          alpha_child.id: alpha_root.id
      },
      "beta": {
          beta_root.id: beta_root.id
      },
  }


@pytest.mark.usefixtures("empty_registry")
def test_a_view_no_session_roots_has_no_entry() -> None:
  assert scheduled_sessions.view_subtree_roots([models.SessionMetadata(profile="manager", name="plain")]) == {}


# The probe process drops one package line, registers, imports the server, then drops a session's runtime
# state and asks for the views over a Slack thread session.
PROBE = """
import asyncio
import json
import sys
from fastapi.testclient import TestClient
from src.app import registrations
registrations.PACKAGES = tuple(p for p in registrations.PACKAGES if p != {deleted!r})
registrations.register_all()
import server
sys.path.insert(0, {tests!r})
import conftest
from pathlib import Path
from src.features.slack.metadata import SlackOrigin
from src.infra import backend_models, models
from src.infra.config import CharlieBotConfig
from src.runtime.api.deps import get_config_on_loop, get_session_manager, get_session_store


async def main():
  option = backend_models.parse_option({{"id": "b", "label": "B", "type": "cc-claude", "model": "m"}})
  cfg = CharlieBotConfig(charliebot_home=Path({home!r}), backends={{"options": [option]}})
  mgr = conftest.build_session_manager(cfg)
  thread = await conftest.create_root_session(
      mgr,
      models.CreateSessionRequest(
          name="thread", slack_origin=SlackOrigin(team_id="T", channel_id="C", thread_ts="1.0")),
      backend="b")
  mgr.events.drop_session_runtime_state(thread.id)
  views = await mgr.view_subtree_roots()
  server.app.dependency_overrides[get_config_on_loop] = lambda: cfg
  server.app.dependency_overrides[get_session_manager] = lambda: mgr
  server.app.dependency_overrides[get_session_store] = lambda: mgr.store
  workspace = TestClient(server.app).get("/api/sessions/").json()
  print(json.dumps({{"thread": thread.id, "views": views, "workspace_ids": [row["id"] for row in workspace]}}))


asyncio.run(main())
"""


def probe_without(package: str, home: Path) -> dict:
  probe = subprocess.run(
      [
          sys.executable, "-c",
          PROBE.format(deleted=package, home=str(home / "profile"), tests=str(conftest.ROOT / "tests"))
      ],
      cwd=conftest.ROOT,
      capture_output=True,
      text=True,
      timeout=60,
      check=False,
      env={
          **os.environ, "HOME": str(home),
          "CHARLIEBOT_HOME": str(home / "profile")
      })
  assert probe.returncode == 0, probe.stderr
  return json.loads(probe.stdout.splitlines()[-1])


def test_the_session_store_runs_without_the_recap_package(tmp_path: Path) -> None:
  found = probe_without("src.features.recap", tmp_path)

  assert found["views"] == {"threads": {found["thread"]: found["thread"]}}
  assert found["thread"] not in found["workspace_ids"]


def test_a_thread_session_roots_no_view_without_the_chat_threads_package(tmp_path: Path) -> None:
  found = probe_without("src.features.chat_threads", tmp_path)

  assert found["views"] == {}
  assert found["thread"] in found["workspace_ids"]
