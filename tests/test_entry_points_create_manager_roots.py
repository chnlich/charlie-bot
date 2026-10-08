"""Every session-creation entry point opens a manager root: schema 2, profile manager, a task_created fact.

The four entry points are the Slack/Discord summon, fork, elone, and the operator's legacy create shape.
"""

from __future__ import annotations

import asyncio
import json
import pathlib
from unittest.mock import patch

import conftest
import pytest

from src.features.slack.slack_listener import handle_app_mention, summon_session_id
from src.infra import metadata_slots
from src.infra import config
from src.infra import event_types as ET
from src.runtime import sessions, task_sessions


def _read_events(path: pathlib.Path) -> list[dict]:
  return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


def _assert_manager_root(cfg: config.CharlieBotConfig, session_id: str) -> list[dict]:
  """The on-disk node is a parentless manager root whose created_by_event names its one task_created fact.

  Returns the node's chat events.
  """
  node_dir = cfg.sessions_dir / session_id
  stored = json.loads((node_dir / "metadata.json").read_text(encoding="utf-8"))
  assert (stored["schema_version"], stored["profile"], stored["task_parent_id"],
          stored["task"]) == (2, "manager", None, None)
  events = _read_events(node_dir / "data" / "chat_events.jsonl")
  created = [e for e in events if e["type"] == ET.TASK_CREATED and e["source_session_id"] == session_id]
  assert len(created) == 1
  assert stored["created_by_event"] == {"session_id": session_id, "event_id": created[0]["id"]}
  return events


@pytest.mark.asyncio
async def test_summon_creates_a_manager_root_under_its_thread_id_and_origin(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = conftest.build_slack_cfg(tmp_path)
  session_mgr = sessions.SessionManager(cfg)
  tree = task_sessions.TaskTreeManager(cfg, session_mgr)
  conftest.bind_deps_managers(monkeypatch, tree, session_mgr)
  launches: list[tuple[str, int]] = []

  async def executor(session_id: str, pending: list[dict], launch_run_id: str | None = None) -> str:
    launches.append((session_id, len(pending)))
    return "run-1"

  tree.dispatch.executor = executor
  event = {
      "type": "app_mention",
      "user": "U_ALLOWED",
      "team": "T_TEST",
      "channel": "C_TEST",
      "ts": "1700000000.000100",
      "text": "hey",
      "channel_type": "channel",
  }
  tasks: list[asyncio.Task] = []

  # The wake runs for real: the summon's round must start on the new root.
  with patch(conftest.THREAD_ENTRY_CREATE_LOGGED_TASK_PATCH_TARGET, side_effect=conftest.make_task_spawner(tasks)):
    session_id = await handle_app_mention(event, cfg, session_mgr, conftest.FakeSlackClient())
    await asyncio.gather(*tasks)

  assert session_id == summon_session_id("T_TEST", "C_TEST", "1700000000.000100")
  assert launches == [(session_id, 1)]
  _assert_manager_root(cfg, session_id)
  meta = await session_mgr.get_session(session_id)
  assert meta is not None
  origin = metadata_slots.fields_of(meta, "slack").slack_origin
  assert origin is not None
  assert (origin.team_id, origin.channel_id) == ("T_TEST", "C_TEST")
  assert meta.name.startswith("Slack #") and meta.backend == cfg.backends.options[0].id


@pytest.mark.asyncio
@pytest.mark.parametrize("spawn", ["fork", "elone"])
async def test_fork_and_elone_birth_one_stream_without_syncing_the_copy(tmp_path: pathlib.Path, spawn: str) -> None:
  cfg, mgr, _ = conftest.build_env(tmp_path)
  parent = await conftest.make_parent(mgr)  # two seed events: e0, e1
  parent_events = _read_events(mgr.get_chat_events_path(parent))

  with patch("os.fdatasync") as fdatasync:
    if spawn == "fork":
      child = await mgr.fork_session(parent, event_index=len(parent_events) - 1)
    else:
      child = await mgr.elone_session(parent, event_index=1)
    born_syncs = fdatasync.call_count
    # The patch must see the durable append funnel, or the zero above proves nothing.
    await mgr.save_chat_event(child.id, conftest.user_event("first turn"))
    assert fdatasync.call_count == born_syncs + 1

  assert born_syncs == 0
  events = _assert_manager_root(cfg, child.id)
  copied_count = len(parent_events) if spawn == "fork" else 2
  assert events[:copied_count] == parent_events[:copied_count]
  parent_event_count = copied_count
  assert [e["type"] for e in events[parent_event_count:parent_event_count + 3]] == [
      ET.CLONE_START, ET.TASK_CREATED, ET.USER]
  assert (child.parent_session_id, child.origin_ref, child.task_parent_id) == (parent, None, None)
  if spawn == "elone":
    fresh_parent = await mgr.get_session(parent)
    assert fresh_parent is not None and fresh_parent.successor_session_id == child.id


@pytest.mark.asyncio
async def test_operator_create_opens_a_manager_root_with_name_group_and_backend(tmp_path: pathlib.Path) -> None:
  cfg, session_mgr, tree = conftest.build_env(tmp_path)

  with conftest.make_cron_sessions_client(cfg, session_mgr, tree) as client:
    response = client.post("/api/sessions/", json={"name": "From the sidebar", "group": "Pinned"})

  assert response.status_code == 200
  body = response.json()
  assert (body["name"], body["group"], body["backend"]) == ("From the sidebar", "Pinned", conftest.OPUS_BACKEND_ID)
  assert (body["profile"], body["schema_version"]) == ("manager", 2)
  _assert_manager_root(cfg, body["id"])
