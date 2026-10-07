"""The scheduler-side restores: cron auto-bind brings an archived reattached
node back (covered end-to-end in test_cron_auto_bind), and the cron editor's
enable restores the bound archived node. Real managers drive the tree."""

from __future__ import annotations

from pathlib import Path

import pytest
import yaml
from conftest import (
    OPERATOR,
    OPUS_BACKEND_ID,
    create_scheduled_node,
    write_nightly_task,
)

from src.features.cron.scheduler import Scheduler
from src.infra import event_types as ET
from src.infra.config import CharlieBotConfig
from src.infra.models import SessionStatus
from tests.test_cron_auto_bind import _read_task_yaml
from tests.test_cron_backend import _patch_cron_d


@pytest.fixture()
def cron_env(tmp_path: Path, temp_home: Path, monkeypatch: pytest.MonkeyPatch):
  """The cron editor's env: synthetic home for cron.d, real managers bound as
  the deps singleton, the cron router mounted with the tree override."""
  from conftest import OPUS_BACKEND_OPTION
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      backends={"options": [OPUS_BACKEND_OPTION]},
      paths={"worktree_dir": str(tmp_path / "worktrees")})
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  from src.runtime.sessions import SessionManager
  from src.runtime.task_sessions import TaskTreeManager
  session_mgr = SessionManager(cfg)
  tree = TaskTreeManager(cfg, session_mgr)
  from conftest import bind_deps_managers
  bind_deps_managers(monkeypatch, tree, session_mgr)
  from tests.test_cron_backend import make_cron_client
  _patch_cron_d(monkeypatch, temp_home / ".charliebot" / "config.d" / "cron.d")
  return cfg, session_mgr, tree, temp_home, make_cron_client


@pytest.mark.asyncio
async def test_cron_editor_enable_restores_the_bound_archived_node(cron_env) -> None:
  _cfg, _session_mgr, tree, home, make_cron_client = cron_env
  write_nightly_task(home, backend=OPUS_BACKEND_ID)
  node = await create_scheduled_node(tree, name="nightly", backend=OPUS_BACKEND_ID)
  # Bind, then let the user archive the node (its cron task stops with it).
  from src.runtime.scheduled_sessions import write_cron_key
  write_cron_key("nightly", "session_id", node.id)
  await tree.archive_subtree(node.id, caller=OPERATOR)
  assert tree.task_state(node.id) == "archived"
  # The archive disabled the bound task.
  body = _read_task_yaml(home)
  assert body["session_id"] == node.id and body["enabled"] is False

  with make_cron_client(_cfg_of(tree), _session_mgr_of(tree)) as client:
    enabled = client.put("/api/cron/tasks/nightly", json={"enabled": True})
    assert enabled.status_code == 200
    assert enabled.json()["enabled"] is True

  # The enable restored the node: open, with the cron enable as the named source.
  assert tree.task_state(node.id) == "open"
  reopens = [e for e in tree.events.load_events(node.id) if e["type"] == ET.TASK_REOPENED]
  assert [e["reason"] for e in reopens] == ["cron enable"]

  # Disabling again (and archiving again) writes no restore.
  write_cron_key("nightly", "enabled", value=False)
  await tree.archive_subtree(node.id, caller=OPERATOR)
  with make_cron_client(_cfg_of(tree), _session_mgr_of(tree)) as client:
    disabled = client.put("/api/cron/tasks/nightly", json={"enabled": False})
    assert disabled.status_code == 200
  assert tree.task_state(node.id) == "archived"
  assert len([e for e in tree.events.load_events(node.id) if e["type"] == ET.TASK_REOPENED]) == 1


def _cfg_of(tree) -> CharlieBotConfig:
  return tree._cfg


def _session_mgr_of(tree):
  return tree.sessions


@pytest.mark.asyncio
async def test_auto_bind_restore_leaves_the_legacy_status_branch_alone(cron_env) -> None:
  """The legacy stored-status branch of auto-bind still unarchives a
  status-archived node without any task fact."""
  cfg, session_mgr, tree, home = cron_env[0], cron_env[1], cron_env[2], cron_env[3]
  write_nightly_task(home, backend=OPUS_BACKEND_ID)
  node = await create_scheduled_node(tree, name="nightly", backend=OPUS_BACKEND_ID)
  node = await session_mgr.archive_session(node.id)
  assert node is not None and node.status == SessionStatus.ARCHIVED
  # A legacy-status archived node's task state is still open (no close fact).
  assert tree.task_state(node.id) == "open"

  scheduler = Scheduler(cfg, session_mgr)
  from src.infra.config import ScheduledTaskConfig

  task_cfg = ScheduledTaskConfig(
      name="nightly", cron="0 3 * * *", prompt="run nightly", enabled=True, timezone="America/Los_Angeles")
  session_cache: dict[str, list] = {}
  await scheduler._auto_bind(task_cfg, cfg, session_cache)
  assert task_cfg.session_id == node.id
  restored = await session_mgr.get_session(node.id)
  assert restored is not None and restored.status == SessionStatus.ACTIVE
  # No task_reopened fact: the legacy branch never wrote task facts.
  assert [e for e in tree.events.load_events(node.id) if e["type"] == ET.TASK_REOPENED] == []
  _ = yaml
