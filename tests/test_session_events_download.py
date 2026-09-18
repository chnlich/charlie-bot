"""The raw events download (``GET /api/sessions/{id}/events.jsonl``) ships its
gzip form pre-compressed so the server's gzip middleware skips its inline
per-chunk deflate — the serve the events viewer's fetch and the download link
ride.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from conftest import (
    OPUS_BACKEND_OPTION,
    assert_gzip_served,
    gzip_counting_compress,
    gzip_explode_compress,
    mount_production_gzip,
)
from fastapi import FastAPI
from fastapi.testclient import TestClient

import src.api.sessions as sessions_api
from src.api.sessions import router as sessions_router
from src.core.config import CharlieBotConfig

PROBE_EVENTS = "".join(
    f'{{"id":"e{i}","type":"user","message":{{"role":"user","content":"probe {i}"}},'
    f'"timestamp":"2026-09-01T00:00:{i % 60:02d}Z"}}\n' for i in range(64))


def _client(cfg: CharlieBotConfig) -> TestClient:
  app = FastAPI()
  app.include_router(sessions_router, prefix="/api/sessions")
  app.dependency_overrides[sessions_api.get_config] = lambda: cfg
  return TestClient(app)


def _gzip_client(cfg: CharlieBotConfig) -> TestClient:
  """The sessions router behind the production gzip mount, so the test sees the
  skip the pre-compressed body buys."""
  app = FastAPI()
  app.include_router(sessions_router, prefix="/api/sessions")
  mount_production_gzip(app)
  app.dependency_overrides[sessions_api.get_config] = lambda: cfg
  return TestClient(app)


def _session_with_events(home: Path) -> tuple[CharlieBotConfig, str]:
  """One session whose live chat file carries the probe events, staged under
  *home* — the profile_home fixture points the route's direct get_config() call
  at the same tree."""
  cfg = CharlieBotConfig(charliebot_home=home, backends={"options": [OPUS_BACKEND_OPTION]})
  events_path = home / "sessions" / "s-probe" / "data" / "chat_events.jsonl"
  events_path.parent.mkdir(parents=True)
  events_path.write_text(PROBE_EVENTS, encoding="utf-8")
  (home / "sessions" / "s-probe" / "metadata.json").write_text('{"id": "s-probe", "name": "probe"}', encoding="utf-8")
  return cfg, "s-probe"


def test_gzip_accepted_download_ships_precompressed_body(profile_home: Path) -> None:
  cfg, sid = _session_with_events(profile_home)
  resp = _gzip_client(cfg).get(f"/api/sessions/{sid}/events.jsonl", headers={"Accept-Encoding": "gzip"})
  assert resp.status_code == 200
  assert_gzip_served(resp)
  assert resp.text == PROBE_EVENTS


def test_download_without_gzip_accept_serves_plain(profile_home: Path) -> None:
  """A client whose Accept-Encoding names no gzip reads the plain streaming
  body, no encoding set — the FileResponse arm, unchanged."""
  cfg, sid = _session_with_events(profile_home)
  resp = _gzip_client(cfg).get(f"/api/sessions/{sid}/events.jsonl", headers={"Accept-Encoding": "br"})
  assert resp.status_code == 200
  assert "content-encoding" not in resp.headers
  assert resp.text == PROBE_EVENTS


def test_missing_session_is_404(profile_home: Path) -> None:
  cfg, _ = _session_with_events(profile_home)
  resp = _client(cfg).get("/api/sessions/s-absent/events.jsonl")
  assert resp.status_code == 404


def test_repeat_gzip_download_recompresses_nothing(profile_home: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A repeat gzip download of an unchanged file must serve the stored
  compressed body with zero deflate calls."""
  cfg, sid = _session_with_events(profile_home)
  client = _gzip_client(cfg)
  url = f"/api/sessions/{sid}/events.jsonl"
  first = client.get(url, headers={"Accept-Encoding": "gzip"})
  assert first.status_code == 200

  monkeypatch.setattr(sessions_api, "gzip_level1", gzip_explode_compress("repeat gzip download re-ran the deflate"))
  resp = client.get(url, headers={"Accept-Encoding": "gzip"})
  assert resp.status_code == 200
  assert resp.headers["content-encoding"] == "gzip"
  assert resp.content == first.content


def test_gzip_download_recompresses_when_file_appends(profile_home: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """An append moves the stat pair the memo keys on — the move must re-run the
  deflate over the fresh bytes, never serve the old form."""
  cfg, sid = _session_with_events(profile_home)
  client = _gzip_client(cfg)
  url = f"/api/sessions/{sid}/events.jsonl"
  first = client.get(url, headers={"Accept-Encoding": "gzip"})
  assert first.status_code == 200

  calls: list[bytes] = []
  monkeypatch.setattr(sessions_api, "gzip_level1", gzip_counting_compress(sessions_api.gzip_level1, calls))
  events_path = profile_home / "sessions" / sid / "data" / "chat_events.jsonl"
  with events_path.open("a", encoding="utf-8") as stream:
    stream.write(
        '{"id":"e-late","type":"user","message":{"role":"user","content":"late"},"timestamp":"2026-09-02T00:00:00Z"}\n')
  resp = client.get(url, headers={"Accept-Encoding": "gzip"})
  assert resp.status_code == 200
  assert resp.headers["content-encoding"] == "gzip"
  # The append ran the deflate once, over the fresh file: the served form is
  # the new bytes', not the previous entry's.
  assert len(calls) == 1
  assert resp.text == events_path.read_text(encoding="utf-8")
