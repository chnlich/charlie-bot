"""The archive API surface: DELETE archives the subtree, the message routes
enforce the input table (the user's message restores; an agent's is 409 with
the one archived sentence), and unarchive restores the chain — all through the
real TaskTreeManager and SessionManager behind the real routers."""

from __future__ import annotations

from pathlib import Path

import pytest
from conftest import OPERATOR, build_env, create_task

from src.infra import event_types as ET
from src.infra import models
from src.infra.models import RunRecord
from src.runtime.run_token import RunTokenClaims, sign_run_token


@pytest.mark.asyncio
async def test_archive_route_returns_the_archived_ids_and_409_detail_reaches_the_client(tmp_path: Path) -> None:
  from tests.test_task_execution import make_api_client

  cfg, session_mgr, tree = build_env(tmp_path)
  root = await create_task(tree, parent=None, request_id="root")
  mid = await create_task(tree, parent=root.id, request_id="mid")
  key = "op-secret"
  from conftest import stub_credentials
  stub_credentials({"charliebot": {"access_key": key}})
  with make_api_client(cfg, session_mgr, tree) as client:
    archived = client.delete(f"/api/sessions/{root.id}")
    assert archived.status_code == 200
    assert archived.json() == {"archived": [root.id, mid.id]}

    # An already-archived target: 200 with an empty list.
    again = client.delete(f"/api/sessions/{root.id}")
    assert again.status_code == 200 and again.json() == {"archived": []}

    # The tree detail reads both nodes as archived with no open node below an
    # archived ancestor.
    for sid in (root.id, mid.id):
      detail = client.get(f"/api/sessions/{sid}").json()
      assert detail["task_state"] == "archived" and detail["archived"] is True

    # An unfinished run refuses the archive with the node list, and the 409
    # detail carries the blockers the sidebar shows.
    leaf = await create_task(tree, parent=None, request_id="leaf")
    await tree.runs.register_run(
        RunRecord(id="run-live", session_id=leaf.id, kind="work", pid=424242, pid_start="ps-live"))
    refused = client.delete(f"/api/sessions/{leaf.id}")
    assert refused.status_code == 409
    assert "run run-live" in refused.json()["detail"]["blockers"][0]


@pytest.mark.asyncio
async def test_user_message_restores_through_the_chat_route_and_agent_is_409(tmp_path: Path) -> None:
  from fastapi import FastAPI
  from fastapi.testclient import TestClient

  from src.infra import config
  from src.runtime.api import chat as chat_api
  from src.runtime.api import internal as internal_api
  from src.runtime.api.deps import get_config_on_loop, get_run_store, get_session_manager, get_task_manager

  cfg, session_mgr, tree = build_env(tmp_path)
  root = await create_task(tree, parent=None, request_id="root")
  mid = await create_task(tree, parent=root.id, request_id="mid")
  leaf = await create_task(tree, parent=mid.id, request_id="leaf", profile="worker")
  await tree.archive_subtree(root.id, caller=OPERATOR)
  await tree.runs.register_run(RunRecord(id="run-agent", session_id=mid.id, kind="manager_turn"))
  await tree.runs.record_launch(mid.id, "run-agent", pid=424242, pid_start="ps-1")

  app = FastAPI()
  app.include_router(chat_api.router, prefix="/api/chat")
  app.include_router(internal_api.router, prefix="/api/internal")
  key = "op-secret"
  from conftest import stub_credentials
  stub_credentials({"charliebot": {"access_key": key}})
  app.dependency_overrides[config.get_config] = lambda: cfg
  app.dependency_overrides[get_config_on_loop] = lambda: cfg
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  app.dependency_overrides[get_task_manager] = lambda: tree
  app.dependency_overrides[get_run_store] = lambda: tree.runs
  agent_headers = {
      "Authorization": "Bearer " +
                       sign_run_token(RunTokenClaims(run_id="run-agent", session_id=mid.id, agent="m"), key)
  }
  with TestClient(app) as client:
    # Agent message via the chat route: 409 with the one archived sentence.
    agent_msg = client.post(f"/api/chat/{leaf.id}/message", json={"content": "keep going"}, headers=agent_headers)
    assert agent_msg.status_code == 409
    assert agent_msg.json()["detail"] == f"task {leaf.id} is archived"

    # Agent message via the internal relay: the same refusal.
    relay = client.post(
        "/api/internal/session-message", json={
            "session_id": mid.id,
            "target_session_id": leaf.id,
            "content": "go"
        })
    assert relay.status_code == 409
    assert relay.json()["detail"] == f"task {leaf.id} is archived"
    # Nothing landed on the leaf.
    assert not [e for e in tree.events.load_events(leaf.id) if e["type"] == ET.AGENT_MESSAGE]

    # The user's own message restores the chain and is the round's only input
    # (the executor is unregistered, so the decision records the pending batch
    # instead of launching — the restore itself is the behavior under test).
    sent = client.post(f"/api/chat/{leaf.id}/message", json={"content": "continue from here"})
    assert sent.status_code == 202
    body = sent.json()
    assert body["status"] == "accepted"
    for node in (root.id, mid.id, leaf.id):
      assert tree.task_state(node) == "open"
      reopens = [e for e in tree.events.load_events(node) if e["type"] == ET.TASK_REOPENED]
      assert [e["reason"] for e in reopens] == ["user message"]
    pending = tree.dispatch.pending_inputs(leaf.id)
    assert [str(e["id"]) for e in pending] == [body["input_event_id"]]
    users = [e for e in tree.events.load_events(leaf.id) if e["type"] == ET.USER]
    assert len(users) == 1


@pytest.mark.asyncio
async def test_unarchive_route_restores_the_chain_and_reports_the_ids(tmp_path: Path) -> None:
  from tests.test_task_execution import make_api_client

  cfg, session_mgr, tree = build_env(tmp_path)
  root = await create_task(tree, parent=None, request_id="root")
  mid = await create_task(tree, parent=root.id, request_id="mid")
  leaf = await create_task(tree, parent=root.id, request_id="leaf")
  await tree.archive_subtree(root.id, caller=OPERATOR)
  key = "op-secret"
  from conftest import stub_credentials
  stub_credentials({"charliebot": {"access_key": key}})
  with make_api_client(cfg, session_mgr, tree) as client:
    restored = client.post(f"/api/sessions/{mid.id}/unarchive")
    assert restored.status_code == 200
    assert restored.json() == {"restored": [root.id, mid.id]}
    # The sibling descendant stays archived; the chain is open.
    assert tree.task_state(root.id) == "open"
    assert tree.task_state(mid.id) == "open"
    assert tree.task_state(leaf.id) == "archived"
    for node in (root.id, mid.id):
      reopens = [e for e in tree.events.load_events(node) if e["type"] == ET.TASK_REOPENED]
      assert [e["reason"] for e in reopens] == ["sidebar unarchive"]
