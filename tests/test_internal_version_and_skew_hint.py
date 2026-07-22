"""Tests for A7: /api/internal/version endpoint and the version-skew hint pure function."""

from pathlib import Path

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api.internal import router as internal_router
from src.cli.common import compose_version_skew_hint
from src.core import buildinfo


def test_version_endpoint_returns_sha_and_started_at(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Acceptance #9: GET /api/internal/version returns sha and started_at."""
  monkeypatch.setattr(buildinfo, "_sha", "abc1234", raising=False)
  monkeypatch.setattr(buildinfo, "_started_at", "2026-07-21T00:00:00+00:00", raising=False)
  app = FastAPI()
  app.include_router(internal_router, prefix="/api/internal")
  with TestClient(app) as client:
    resp = client.get("/api/internal/version")
  assert resp.status_code == 200
  body = resp.json()
  assert body["sha"] == "abc1234"
  assert body["started_at"] == "2026-07-21T00:00:00+00:00"


def test_init_build_info_populates_sha_and_started_at(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """init_build_info captures a real SHA and a non-empty started_at."""
  captured: dict[str, str] = {}

  def fake_read_git_sha() -> str:
    return "deadbee"

  monkeypatch.setattr(buildinfo, "_read_git_sha", fake_read_git_sha)
  buildinfo.init_build_info()
  captured["sha"] = buildinfo.get_sha()
  captured["started_at"] = buildinfo.get_started_at()
  assert captured["sha"] == "deadbee"
  assert captured["started_at"]  # non-empty ISO string


# ---------------------------------------------------------------------------
# compose_version_skew_hint — pure function
# ---------------------------------------------------------------------------


def test_compose_version_skew_hint_equal_shas_returns_none() -> None:
  """Acceptance #9: equal SHAs → no hint."""
  assert compose_version_skew_hint("abc", "2026-07-21", "abc") is None


def test_compose_version_skew_hint_differing_shas_returns_hint_with_both() -> None:
  """Acceptance #9: differing SHAs → hint string contains both SHAs."""
  hint = compose_version_skew_hint("abc", "2026-07-21T00:00:00+00:00", "def")
  assert hint is not None
  assert "abc" in hint
  assert "def" in hint
  assert "server restart may be required" in hint


def test_compose_version_skew_hint_missing_server_sha_returns_none() -> None:
  assert compose_version_skew_hint(None, "2026-07-21", "def") is None
  assert compose_version_skew_hint("", "2026-07-21", "def") is None


def test_compose_version_skew_hint_missing_local_sha_returns_none() -> None:
  assert compose_version_skew_hint("abc", "2026-07-21", None) is None
  assert compose_version_skew_hint("abc", "2026-07-21", "") is None


def test_compose_version_skew_hint_missing_started_at_still_emits_hint() -> None:
  """started_at is optional in the pure function; the hint still emits with both SHAs."""
  hint = compose_version_skew_hint("abc", None, "def")
  assert hint is not None
  assert "abc" in hint
  assert "def" in hint
  assert "started" not in hint
