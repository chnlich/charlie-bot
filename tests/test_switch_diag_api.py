"""Tests for the session-switch telemetry endpoint (src/api/diag.py)."""

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient
from structlog.testing import capture_logs

from src.api import diag as diag_api


def _build_client() -> TestClient:
  app = FastAPI()
  app.include_router(diag_api.router, prefix="/api/diag")
  return TestClient(app, raise_server_exceptions=False)


def _payload(**overrides) -> dict:
  record = {
      "phase": "started",
      "from_session": "s-old",
      "to_session": "s-new",
      "generation": 3,
      "winner_generation": None,
      "elapsed_ms": 0,
      "error": None,
      "client_ts": "2026-09-06T12:00:00Z",
  }
  record.update(overrides)
  return record


def test_valid_payload_answers_ok_and_logs_one_diag_switch_line() -> None:
  client = _build_client()
  with capture_logs() as logs:
    resp = client.post("/api/diag/switch-events", json=_payload())

  assert resp.status_code == 200
  assert resp.json() == {"ok": True}
  switch_lines = [entry for entry in logs if entry.get("event") == "diag_switch"]
  assert len(switch_lines) == 1
  assert switch_lines[0]["phase"] == "started"
  assert switch_lines[0]["from_session"] == "s-old"
  assert switch_lines[0]["to_session"] == "s-new"
  assert switch_lines[0]["generation"] == 3
  assert switch_lines[0]["winner_generation"] is None
  assert switch_lines[0]["elapsed_ms"] == 0
  assert switch_lines[0]["client_ts"] == "2026-09-06T12:00:00Z"


def test_invalid_phase_is_rejected() -> None:
  client = _build_client()

  resp = client.post("/api/diag/switch-events", json=_payload(phase="bogus"))

  assert resp.status_code == 422


def test_extra_field_is_rejected() -> None:
  client = _build_client()

  resp = client.post("/api/diag/switch-events", json=_payload(session_name="leak"))

  assert resp.status_code == 422


def test_negative_elapsed_ms_is_rejected() -> None:
  client = _build_client()

  resp = client.post("/api/diag/switch-events", json=_payload(elapsed_ms=-1))

  assert resp.status_code == 422


def test_long_error_is_truncated_in_the_logged_record() -> None:
  client = _build_client()
  with capture_logs() as logs:
    resp = client.post("/api/diag/switch-events", json=_payload(phase="failed", error="x" * 500))

  assert resp.status_code == 200
  switch_lines = [entry for entry in logs if entry.get("event") == "diag_switch"]
  assert len(switch_lines) == 1
  assert switch_lines[0]["error"] == "x" * 300


@pytest.mark.parametrize("phase", ["started", "completed", "superseded", "failed", "render_error"])
def test_every_phase_is_accepted(phase: str) -> None:
  client = _build_client()
  with capture_logs() as logs:
    resp = client.post("/api/diag/switch-events", json=_payload(phase=phase))

  assert resp.status_code == 200
  assert [entry["phase"] for entry in logs if entry.get("event") == "diag_switch"] == [phase]
