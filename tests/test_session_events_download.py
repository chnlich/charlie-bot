"""The raw events download (``GET /api/sessions/{id}/events.jsonl``) ships its
gzip form pre-compressed so the server's gzip middleware skips its inline
per-chunk deflate — the serve the events viewer's fetch and the download link
ride.
"""

from __future__ import annotations

import pathlib

import conftest
import fastapi
from fastapi import testclient

from src.infra import config
from src.runtime.api import sessions

PROBE_EVENTS = "".join(
    f'{{"id":"e{i}","type":"user","message":{{"role":"user","content":"probe {i}"}},'
    f'"timestamp":"2026-09-01T00:00:{i % 60:02d}Z"}}\n' for i in range(64))


def _gzip_client(cfg: config.CharlieBotConfig) -> testclient.TestClient:
  """The sessions router behind the production gzip mount, so the test sees the
  skip the pre-compressed body buys."""
  app = fastapi.FastAPI()
  app.include_router(sessions.router, prefix="/api/sessions")
  conftest.mount_production_gzip(app)
  app.dependency_overrides[sessions.get_config] = lambda: cfg
  return testclient.TestClient(app)


def _session_with_events(home: pathlib.Path) -> tuple[config.CharlieBotConfig, str]:
  """One session whose live chat file carries the probe events, staged under
  *home* — the profile_home fixture points the route's direct get_config() call
  at the same tree."""
  cfg = config.CharlieBotConfig(charliebot_home=home, backends={"options": [conftest.OPUS_BACKEND_OPTION]})
  events_path = home / "sessions" / "s-probe" / "data" / "chat_events.jsonl"
  events_path.parent.mkdir(parents=True)
  events_path.write_text(PROBE_EVENTS, encoding="utf-8")
  (home / "sessions" / "s-probe" / "metadata.json").write_text('{"id": "s-probe", "name": "probe"}', encoding="utf-8")
  return cfg, "s-probe"


def test_gzip_accepted_download_ships_precompressed_body(profile_home: pathlib.Path) -> None:
  cfg, sid = _session_with_events(profile_home)
  resp = _gzip_client(cfg).get(f"/api/sessions/{sid}/events.jsonl", headers={"Accept-Encoding": "gzip"})
  assert resp.status_code == 200
  conftest.assert_gzip_served(resp)
  assert resp.text == PROBE_EVENTS
