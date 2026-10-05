"""The migration script's three subcommands: the raw-JSON snapshot over a real
home (the fold reads files, never the model module), the apply verification
over the real archive routes, and the rollback walk. The script's server calls
ride a double with the script's own ``_request`` signature and (status, body)
shape, backed by the real routers over the real managers."""

from __future__ import annotations

import json
import sys
from pathlib import Path

import pytest
from conftest import OPERATOR, build_env, create_task, stub_credentials

import scripts.archive_unify_migration as migration
from src.core import event_types as ET

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

    from src.api import sessions as sessions_api
    from src.api.deps import get_run_store, get_session_manager, get_task_manager
    from src.core import config

    app = FastAPI()
    app.include_router(sessions_api.router, prefix="/api/sessions")

    @app.get("/api/internal/version")
    async def version() -> dict:
      return {"sha": "test", "started_at": "test"}

    app.dependency_overrides[config.get_config] = lambda: cfg
    app.dependency_overrides[config.configured_access_key] = lambda: KEY
    app.dependency_overrides[get_session_manager] = lambda: session_mgr
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
  legacy = await session_mgr.create_session(
      __import__("src.core.models", fromlist=["CreateSessionRequest"]).CreateSessionRequest(name="Legacy"),
      backend="claude-opus-4.6")
  assert legacy.profile is None  # not a task node: absent from both maps
  hide_like_the_old_server(cfg, mid.id)

  out = tmp_path / "snapshot.json"
  monkeypatch.setattr(migration, "_request", RouterDouble(cfg, session_mgr, tree))
  migration.cmd_snapshot(out)

  doc = json.loads(out.read_text())
  assert doc["kind"] == migration.SNAPSHOT_KIND
  assert set(doc["migrate"]) == {mid.id}
  assert doc["task_parents"] == {root.id: None, mid.id: root.id, closed.id: root.id}


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
  await tree.archive_subtree(root.id, caller=OPERATOR)
  assert tree.task_state(mid.id) == "archived"

  monkeypatch.setattr(migration, "_request", RouterDouble(cfg, session_mgr, tree))
  migration.cmd_rollback()

  assert tree.task_state(root.id) == "open"
  assert tree.task_state(mid.id) == "open"
  assert tree.task_state(keep.id) == "open"
  for node in (root.id, mid.id):
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
  src.core.models (which drops the retired field the snapshot has to read)."""
  import subprocess

  result = subprocess.run(
      [
          sys.executable, "-c",
          (
              "import sys; sys.path.insert(0, '.'); "
              "import scripts.archive_unify_migration; "
              "assert 'src.core.models' not in sys.modules, 'the script imported the model module'")
      ],
      cwd=Path(__file__).resolve().parents[1],
      capture_output=True,
      text=True,
      timeout=60)
  assert result.returncode == 0, result.stderr
