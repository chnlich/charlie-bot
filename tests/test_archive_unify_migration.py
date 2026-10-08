"""The migration script's three subcommands: the raw-JSON snapshot over a real
home (the fold reads files, never the model module), the apply verification
over the real archive routes, and the rollback walk. The script's server calls
ride a double with the script's own ``_request`` signature and (status, body)
shape, backed by the real routers over the real managers."""

from __future__ import annotations

import json
import sys
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from conftest import OPERATOR, build_env, create_task, stub_credentials

import scripts.archive_unify_migration as migration
from src.infra import event_types as ET
from src.infra.models import RunRecord

KEY = "op-secret"
ORIGINAL_REQUEST = migration._request


@pytest.fixture()
def script_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
  """A real (cfg, session_mgr, tree) home with the script's IO seams pointed at it."""
  cfg, session_mgr, tree = build_env(tmp_path)
  stub_credentials({"charliebot": {"access_key": KEY}})
  monkeypatch.setattr(migration, "_sessions_dir", lambda: cfg.sessions_dir)
  monkeypatch.setattr(migration, "_access_key", lambda: KEY)
  return cfg, session_mgr, tree


class RouterDouble:
  """The script's ``_request`` double: same signature, same (status, body)
  shape, backed by the real sessions router over the real managers. A plain
  /api/internal/version answers the preflight's reachability check."""

  def __init__(self, cfg, session_mgr, tree) -> None:
    from fastapi import FastAPI
    from fastapi.testclient import TestClient

    from src.infra import config
    from src.runtime.api import sessions as sessions_api
    from src.runtime.api.deps import get_run_store, get_session_manager, get_session_store, get_task_manager

    app = FastAPI()
    app.include_router(sessions_api.router, prefix="/api/sessions")

    @app.get("/api/internal/version")
    async def version() -> dict:
      return {"sha": "test", "started_at": "test"}

    app.dependency_overrides[config.get_config] = lambda: cfg
    app.dependency_overrides[config.configured_access_key] = lambda: KEY
    app.dependency_overrides[get_session_manager] = lambda: session_mgr
    app.dependency_overrides[get_session_store] = lambda: session_mgr.store
    app.dependency_overrides[get_task_manager] = lambda: tree
    app.dependency_overrides[get_run_store] = lambda: tree.runs
    self.client = TestClient(app)

  def __call__(self, base: str, key: str, method: str, path: str, payload: dict | None = None):
    headers = {"Authorization": f"Bearer {key}"}
    resp = self.client.request(method, path, json=payload, headers=headers)
    try:
      body = resp.json()
    except ValueError:
      body = {}
    return resp.status_code, body


def hide_like_the_old_server(cfg, session_id: str) -> None:
  """Write the retired display preference the OLD server left behind: raw JSON,
  exactly the state a pre-restart snapshot reads."""
  path = cfg.sessions_dir / session_id / "metadata.json"
  raw = json.loads(path.read_text())
  raw["presentation"] = "hidden"
  path.write_text(json.dumps(raw))


@pytest.mark.asyncio
async def test_snapshot_lists_open_hidden_nodes_and_records_parents(
    script_env, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  cfg, session_mgr, tree = script_env
  root = await create_task(tree, parent=None, request_id="root")
  mid = await create_task(tree, parent=root.id, request_id="mid")
  closed = await create_task(tree, parent=root.id, request_id="closed")
  await tree.archive_subtree(closed.id, caller=OPERATOR)  # not open: never listed
  hide_like_the_old_server(cfg, mid.id)

  out = tmp_path / "snapshot.json"
  monkeypatch.setattr(migration, "_request", RouterDouble(cfg, session_mgr, tree))
  migration.cmd_snapshot(out)

  doc = json.loads(out.read_text())
  assert doc["kind"] == migration.SNAPSHOT_KIND
  assert set(doc["migrate"]) == {mid.id}
  assert doc["task_parents"] == {root.id: None, mid.id: root.id, closed.id: root.id}


@pytest.mark.asyncio
async def test_snapshot_folds_segments_before_the_live_file(
    script_env, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  """A close fact the rotation moved into a weekly segment and a reopen fact
  in the live file fold chronologically: the reopen (the last lifecycle fact)
  wins, so the hidden node reads open and the snapshot lists it."""
  cfg, session_mgr, tree = script_env
  node = await create_task(tree, parent=None, request_id="seg")
  await tree.archive_subtree(node.id, caller=OPERATOR)  # the close fact, live file
  # The real segment writer: the rotation moves the closed-period events into
  # data/archives/chat_events.<iso-year>-W<week>.jsonl.
  await session_mgr.recycle_history_before(node.id, datetime.now(UTC) + timedelta(seconds=1))
  segments = sorted((cfg.sessions_dir / node.id / "data" / "archives").glob("chat_events.*.jsonl"))
  assert segments, "the rotation wrote no segment"
  segment_types = [
      e["type"] for f in segments for e in (json.loads(line) for line in f.read_text().splitlines() if line)
  ]
  assert "task_closed" in segment_types and "task_reopened" not in segment_types
  await tree.completion.restore_chain(node.id, request_id="r-1", reason="sidebar unarchive")  # reopen, live file
  assert tree.task_state(node.id) == "open"
  hide_like_the_old_server(cfg, node.id)

  out = tmp_path / "snapshot.json"
  monkeypatch.setattr(migration, "_request", RouterDouble(cfg, session_mgr, tree))
  migration.cmd_snapshot(out)

  doc = json.loads(out.read_text())
  assert node.id in doc["migrate"]


@pytest.mark.asyncio
async def test_apply_archives_verifies_and_reports_mismatches(
    script_env, monkeypatch: pytest.MonkeyPatch, tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
  cfg, session_mgr, tree = script_env
  root = await create_task(tree, parent=None, request_id="root")
  mid = await create_task(tree, parent=root.id, request_id="mid")
  source = tmp_path / "snapshot.json"
  source.write_text(
      json.dumps(
          {
              "kind": migration.SNAPSHOT_KIND,
              "migrate": [root.id, mid.id],
              "task_parents": {
                  root.id: None,
                  mid.id: root.id
              },
          }))

  monkeypatch.setattr(migration, "_request", RouterDouble(cfg, session_mgr, tree))
  migration.cmd_apply(source)
  # Everything archived; a second apply of the same snapshot still verifies
  # (each node reads back archived, the parents are unchanged).
  assert tree.task_state(root.id) == "archived"
  assert tree.task_state(mid.id) == "archived"
  migration.cmd_apply(source)

  # A moved (or wrong) recorded parent fails the parent check loudly.
  bad = tmp_path / "bad.json"
  bad.write_text(
      json.dumps({
          "kind": migration.SNAPSHOT_KIND,
          "migrate": [],
          "task_parents": {
              mid.id: "some-other-parent"
          },
      }))
  with pytest.raises(SystemExit) as excinfo:
    migration.cmd_apply(bad)
  assert excinfo.value.code != 0
  printed = capsys.readouterr().out
  assert "parent moved" in printed


@pytest.mark.asyncio
async def test_rollback_unarchives_every_archived_task_node(script_env, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_mgr, tree = script_env
  root = await create_task(tree, parent=None, request_id="root")
  mid = await create_task(tree, parent=root.id, request_id="mid")
  keep = await create_task(tree, parent=None, request_id="keep")
  # A lone archived root with no archived descendant in the tree rows below
  # it: the walk's own root page must name it (its only child completed before
  # the archive, so nothing under it is an "archived" row).
  lone = await create_task(tree, parent=None, request_id="lone")
  done = await create_task(tree, parent=lone.id, request_id="done", profile="worker")
  await tree.runs.register_run(RunRecord(id="run-done", session_id=done.id, kind="work"))
  await tree.dispatch.finish_run(done.id, "run-done", outcome="success")
  await tree.archive_subtree(lone.id, caller=OPERATOR)
  await tree.archive_subtree(root.id, caller=OPERATOR)
  assert tree.task_state(mid.id) == "archived"
  assert tree.task_state(done.id) == "completed"

  monkeypatch.setattr(migration, "_request", RouterDouble(cfg, session_mgr, tree))
  migration.cmd_rollback()

  assert tree.task_state(root.id) == "open"
  assert tree.task_state(mid.id) == "open"
  assert tree.task_state(keep.id) == "open"
  # The lone root restores through its own row; its completed child is a
  # descendant the restore deliberately leaves at its end state.
  assert tree.task_state(lone.id) == "open"
  assert tree.task_state(done.id) == "completed"
  for node in (root.id, mid.id, lone.id):
    reopens = [e for e in tree.events.load_events(node) if e["type"] == ET.TASK_REOPENED]
    assert len(reopens) >= 1


def test_preflight_refuses_without_an_access_key(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr(migration, "_access_key", lambda: "")
  with pytest.raises(SystemExit, match="preflight failed"):
    migration.preflight(require_server=True)


def test_preflight_refuses_when_the_server_does_not_answer(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr(migration, "_access_key", lambda: KEY)
  monkeypatch.setattr(migration, "_base_url", lambda: "http://127.0.0.1:1")
  with pytest.raises(SystemExit, match="preflight failed"):
    migration.preflight(require_server=True)


def test_importing_the_script_module_never_imports_the_retired_field_model() -> None:
  """The snapshot must run on raw JSON: importing the script alone pulls in no
  src.infra.models (which drops the retired field the snapshot has to read)."""
  import subprocess

  result = subprocess.run(
      [
          sys.executable, "-c",
          (
              "import sys; sys.path.insert(0, '.'); "
              "import scripts.archive_unify_migration; "
              "assert 'src.infra.models' not in sys.modules, 'the script imported the model module'")
      ],
      cwd=Path(__file__).resolve().parents[1],
      capture_output=True,
      text=True,
      timeout=60)
  assert result.returncode == 0, result.stderr
