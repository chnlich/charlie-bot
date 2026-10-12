"""The session-tree preview entry point: preparation, isolation, lifetime and gates.

The unit layer exercises the pure preparation/seed/gate mechanics in-process over
synthetic homes. The CLI layer invokes the actual ``session-tree preview`` parser
in a fresh process and asserts refusals happen before any side effect; the full
fresh-start behavioral case (real server, real fence, real HTTP, independent
second instance, restart) runs with the installed charlie-code launcher and is
marked local_only.
"""

from __future__ import annotations

import json
import os
import pathlib
import socket
import subprocess
import sys

import fastapi
import pytest
import yaml

import src.features.session_tree_preview.session_tree_preview as preview_module
from tools import browser_harness_session_tree

REPO_ROOT = pathlib.Path(__file__).resolve().parents[1]

OPERATOR_KEY = "source-operator-key-0000"
PROVIDER_KEY = "provider-key-1111"


def write_source_home(home: pathlib.Path, *, backend: dict | None = None, port: int = 18498) -> dict:
  """One synthetic source (production-like) profile: minimal config + operator key."""
  home.mkdir(parents=True, exist_ok=True)
  if backend is None:
    backend = {
        "id": "clc-test",
        "label": "CLC Test",
        "type": "charlie-code",
        "model": "openai/fake-model",
        "api_base": "http://127.0.0.1:9/v1",
    }
  config = {
      "server": {
          "host": "127.0.0.1",
          "port": port
      },
      "paths": {
          "workspace_dirs": [str(home / "workspaces")],
          "worktree_dir": str(home / "worktrees")
      },
      "backends": {
          "options": [backend],
          "preference": [backend["id"]]
      },
  }
  (home / "config.yaml").write_text(yaml.safe_dump(config), encoding="utf-8")
  creds = f"charliebot:\n  access_key: {OPERATOR_KEY}\n"
  if backend.get("credential"):
    creds += f"{backend['credential']}:\n  api_key: {PROVIDER_KEY}\n"
    if backend["credential"] == "missing-section":
      creds = f"charliebot:\n  access_key: {OPERATOR_KEY}\n"
  (home / "credentials.yaml").write_text(creds, encoding="utf-8")
  return config


@pytest.fixture
def source_home(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> pathlib.Path:
  """A synthetic source profile selected by the environment for this test."""
  home = tmp_path / "source-home"
  write_source_home(home)
  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
  monkeypatch.setattr(preview_module, "check_launcher", lambda: None)
  return home


# ---------------------------------------------------------------------------
# Pure path/ownership checks
# ---------------------------------------------------------------------------


def test_check_home_location_refuses_overlaps(tmp_path: pathlib.Path, source_home: pathlib.Path) -> None:
  inside = source_home / "nested" / "preview"
  with pytest.raises(preview_module.PreviewRefusedError, match="overlaps the production home"):
    preview_module.check_home_location(inside, source_home=source_home, source_workspace_dirs=[])
  container = tmp_path / "container"
  prod_inside = container / ".charliebot"
  prod_inside.mkdir(parents=True)
  with pytest.raises(preview_module.PreviewRefusedError, match="overlaps the production home"):
    preview_module.check_home_location(container, source_home=prod_inside, source_workspace_dirs=[])
  workspace_inside = tmp_path / "workspaces" / "preview"
  with pytest.raises(preview_module.PreviewRefusedError, match="overlaps the production workspace"):
    preview_module.check_home_location(
        workspace_inside, source_home=tmp_path / "elsewhere", source_workspace_dirs=[str(tmp_path / "workspaces")])
  with pytest.raises(preview_module.PreviewRefusedError, match="overlaps the running checkout"):
    preview_module.check_home_location(
        REPO_ROOT / "sub" / "dir", source_home=tmp_path / "elsewhere", source_workspace_dirs=[])
  sibling = tmp_path / "sibling-preview"
  preview_module.check_home_location(
      sibling, source_home=source_home, source_workspace_dirs=[str(tmp_path / "workspaces")])


def test_check_port_refuses_source_port_and_occupied(source_home: pathlib.Path) -> None:
  with pytest.raises(preview_module.PreviewRefusedError, match="between 1 and 65535"):
    preview_module.check_port(0, source_server_port=18498)
  with pytest.raises(preview_module.PreviewRefusedError, match="source profile's server port"):
    preview_module.check_port(18498, source_server_port=18498)
  with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
    sock.bind(("127.0.0.1", 0))
    sock.listen(1)
    occupied = sock.getsockname()[1]
    with pytest.raises(preview_module.PreviewRefusedError, match="not free"):
      preview_module.check_port(occupied, source_server_port=18498)
  free = browser_harness_session_tree.pick_free_port()
  preview_module.check_port(free, source_server_port=18498)


def _clear_singletons(monkeypatch: pytest.MonkeyPatch) -> None:
  from src.runtime import (
      session_anchors,
      session_events,
      session_fork,
      session_lifecycle,
      session_listing,
      session_search,
      session_sidebar,
      session_store,
      session_successor,
      task_execution,
      triggers,
  )

  monkeypatch.setattr(session_store, "_store", None)
  monkeypatch.setattr(session_events, "_events", None)
  monkeypatch.setattr(session_sidebar, "_sidebar", None)
  monkeypatch.setattr(session_listing, "_listing", None)
  monkeypatch.setattr(session_search, "_search", None)
  monkeypatch.setattr(session_lifecycle, "_lifecycle", None)
  monkeypatch.setattr(session_fork, "_fork", None)
  monkeypatch.setattr(session_anchors, "_anchors", None)
  monkeypatch.setattr(session_successor, "_successor", None)
  monkeypatch.setattr(triggers, "_trigger_manager", None)
  monkeypatch.setattr(task_execution, "_task_manager", None)


def test_assert_no_bound_singletons_passes_when_none_is_bound(monkeypatch: pytest.MonkeyPatch) -> None:
  _clear_singletons(monkeypatch)
  preview_module.assert_no_bound_singletons()


@pytest.mark.parametrize(
    ("module_name", "attr"), [
        ("src.runtime.session_store", "_store"),
        ("src.runtime.session_events", "_events"),
        ("src.runtime.session_sidebar", "_sidebar"),
        ("src.runtime.session_listing", "_listing"),
        ("src.runtime.session_search", "_search"),
        ("src.runtime.session_lifecycle", "_lifecycle"),
        ("src.runtime.session_fork", "_fork"),
        ("src.runtime.session_anchors", "_anchors"),
        ("src.runtime.session_successor", "_successor"),
        ("src.runtime.triggers", "_trigger_manager"),
        ("src.runtime.task_execution", "_task_manager"),
    ])
def test_assert_no_bound_singletons_refuses_each_bound_singleton(
    module_name: str, attr: str, monkeypatch: pytest.MonkeyPatch) -> None:
  import importlib

  _clear_singletons(monkeypatch)
  monkeypatch.setattr(importlib.import_module(module_name), attr, object())
  with pytest.raises(preview_module.PreviewRefusedError, match=f"switched: {attr}$"):
    preview_module.assert_no_bound_singletons()


# ---------------------------------------------------------------------------
# Backend selection and launcher preflight
# ---------------------------------------------------------------------------


def test_read_source_backend_refuses_unisolated_backend_types(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  home = tmp_path / "source"
  write_source_home(home, backend={"id": "cc-claude-entry", "label": "CC", "type": "cc-claude", "model": "claude-x"})
  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
  with pytest.raises(preview_module.PreviewRefusedError, match="only for charlie-code"):
    preview_module.read_source_backend("cc-claude-entry")


# ---------------------------------------------------------------------------
# Launcher workspace boundary
# ---------------------------------------------------------------------------


def test_workspace_guard_refuses_outside_and_accepts_inside(tmp_path: pathlib.Path) -> None:
  from src.infra import config

  home = tmp_path / "trial-home"
  cfg = config.CharlieBotConfig(
      charliebot_home=home,
      paths={
          "workspace_dirs": [str(home / "workspaces")],
          "worktree_dir": str(home / "worktrees")
      })
  guard = preview_module.make_workspace_guard(cfg)
  with pytest.raises(preview_module.PreviewWorkspaceError, match="workspace boundary"):
    guard(REPO_ROOT)
  inside = home / "workspaces" / "synthetic-repo"
  inside.mkdir(parents=True)
  guard(inside)
  guard(home / "workspaces")


# ---------------------------------------------------------------------------
# Reachable-mechanism gate
# ---------------------------------------------------------------------------


def _gated_probe_app():
  from fastapi import routing

  probe = routing.APIRouter()

  @probe.get("/api/cron/tasks")
  async def list_tasks():
    return []

  @probe.post("/api/cron/tasks")
  async def create_task():
    return {"ok": True}

  @probe.put("/api/cron/tasks/{name}")
  async def update_task(name: str):
    return {"ok": True}

  @probe.delete("/api/cron/tasks/{name}")
  async def delete_task(name: str):
    return {"ok": True}

  @probe.post("/api/internal/schedule-trigger")
  async def schedule_trigger():
    return {"ok": True}

  @probe.post("/api/internal/slack/reply")
  async def slack_reply():
    return {"ok": True}

  @probe.post("/api/internal/slack/ack")
  async def slack_ack():
    return {"ok": True}

  @probe.post("/api/sessions/")
  async def create_session():
    return {"ok": True}

  @probe.websocket("/ws/terminal")
  async def terminal(websocket: fastapi.WebSocket):
    await websocket.accept()
    await websocket.send_text("should never be reached")

  @probe.websocket("/ws/sessions/{session_id}")
  async def session_ws(websocket: fastapi.WebSocket, session_id: str):
    await websocket.accept()
    await websocket.send_text("hello " + session_id)
    await websocket.close()

  app = fastapi.FastAPI()
  app.include_router(probe)
  app.add_middleware(preview_module.PreviewUnavailableGate)
  return app


def test_preview_gate_refuses_disabled_mutations_and_passes_the_rest() -> None:
  from fastapi import testclient

  client = testclient.TestClient(_gated_probe_app())
  assert client.get("/api/cron/tasks").status_code == 200
  refused = client.post("/api/cron/tasks", json={})
  assert refused.status_code == 403
  assert "Scheduled task management is disabled" in refused.json()["detail"]
  assert client.put("/api/cron/tasks/x", json={}).status_code == 403
  assert client.delete("/api/cron/tasks/x").status_code == 403
  for path in ("/api/internal/schedule-trigger", "/api/internal/slack/reply", "/api/internal/slack/ack"):
    response = client.post(path, json={})
    assert response.status_code == 403, path
    assert "External messaging and delayed triggers are disabled" in response.json()["detail"]
  assert client.get("/api/unknown").status_code == 404  # read paths pass through
  assert client.post("/api/sessions/", json={}).status_code == 200
  assert preview_module.refusal_reason("/api/sessions/", "POST") is None
  assert preview_module.refusal_reason("/api/cron/tasks", "GET") is None


# ---------------------------------------------------------------------------
# The real CLI in a fresh process: refusals happen before any side effect
# ---------------------------------------------------------------------------


def _cli_env(source: pathlib.Path, *, strip_launcher: bool = False, launcher_dir: pathlib.Path | None = None) -> dict:
  env = dict(os.environ.items())
  env["CHARLIEBOT_HOME"] = str(source)
  env["PYTHONUNBUFFERED"] = "1"
  if strip_launcher:
    env["PATH"] = "/usr/bin:/bin"
  if launcher_dir is not None:
    env["PATH"] = f"{launcher_dir}:{env['PATH']}"
  return env


def _run_cli(
    args: list[str],
    source: pathlib.Path,
    *,
    strip_launcher: bool = False,
    launcher_dir: pathlib.Path | None = None) -> subprocess.CompletedProcess:
  return subprocess.run(
      [sys.executable, "-m", "src.app.main", "session-tree", "preview", *args],
      cwd=str(REPO_ROOT),
      env=_cli_env(source, strip_launcher=strip_launcher, launcher_dir=launcher_dir),
      capture_output=True,
      text=True,
      timeout=120)


def _refusal(
    tmp_path: pathlib.Path,
    source: pathlib.Path,
    args: list[str],
    match: str,
    *,
    strip_launcher: bool = False) -> subprocess.CompletedProcess:
  proc = _run_cli(args, source, strip_launcher=strip_launcher)
  assert proc.returncode == 1, f"expected refusal, got {proc.returncode}: {proc.stdout} {proc.stderr}"
  diagnostic = proc.stderr.strip()
  assert match in diagnostic
  if diagnostic.startswith("{"):
    # The refusal diagnostic is one structured JSON document on stderr.
    assert match in json.dumps(json.loads(diagnostic))
  return proc


def test_cli_refuses_symlinked_home_resolving_into_production(
    tmp_path: pathlib.Path, source_home: pathlib.Path) -> None:
  link = tmp_path / "innocent-name"
  link.symlink_to(source_home / "nested-deeper")
  _refusal(
      tmp_path, source_home,
      ["--home",
       str(link), "--port",
       str(browser_harness_session_tree.pick_free_port()), "--backend", "clc-test"], "overlaps the production home")
  assert not (source_home / "nested-deeper").exists()
