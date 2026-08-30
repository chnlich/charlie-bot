"""Tests for the backlog read endpoints' empty-state contract (src/api/backlog.py)."""

from pathlib import Path

import pytest
import yaml
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api import backlog as backlog_api
from src.core.config import BacklogRepoConfig, CharlieBotConfig


def _build_client(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, cfg: CharlieBotConfig
) -> TestClient:
  monkeypatch.setattr("src.core.config.get_config", lambda: cfg)
  app = FastAPI()
  app.include_router(backlog_api.router, prefix="/api/backlog")
  return TestClient(app, raise_server_exceptions=False)


def test_reads_return_empty_lists_when_unconfigured(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  client = _build_client(monkeypatch, tmp_path, CharlieBotConfig(charliebot_home=tmp_path / "home"))

  for path in ("/api/backlog", "/api/backlog/history"):
    resp = client.get(path)
    assert resp.status_code == 200, path
    assert resp.json() == [], path


def test_reads_serve_configured_repo(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  repo = tmp_path / "repo"
  (repo / "backlog").mkdir(parents=True)
  (repo / "backlog" / "backlog.yaml").write_text(yaml.dump([{"id": "a1", "title": "item"}]))
  (repo / "backlog" / "history-2026-01-01.yaml").write_text(yaml.dump([{"timestamp": "2026-01-01", "action": "add"}]))
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      backlog_repos=[BacklogRepoConfig(label="main", path=str(repo))],
  )
  client = _build_client(monkeypatch, tmp_path, cfg)

  items = client.get("/api/backlog").json()
  assert [i["id"] for i in items] == ["a1"]
  history = client.get("/api/backlog/history").json()
  assert [h["action"] for h in history] == ["add"]


def test_patch_stays_loud_when_unconfigured(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  client = _build_client(monkeypatch, tmp_path, CharlieBotConfig(charliebot_home=tmp_path / "home"))

  resp = client.patch("/api/backlog/a1", json={"status": "approved"})
  assert resp.status_code == 500
