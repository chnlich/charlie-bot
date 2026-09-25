"""Backend-switch endpoint: resume-domain guard and the §4.1 API contract.

Carries two of the acceptance-tested mechanisms:
  * guard ⟷ reachability — the API's same-domain predicate is exactly the
    condition under which the runtime resume resolver can re-find the transcript;
  * the §4.1 API contract over all four rows plus the idempotent no-op.
"""

from pathlib import Path
from unittest.mock import AsyncMock

import pytest
from conftest import CODEX_BACKEND_OPTION, apply_config_overrides, backend_option
from conftest import make_sessions_client as _build_client
from conftest import make_transcript as _make_transcript
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.agents import master_cc
from src.api.deps import get_session_manager, get_thread_manager
from src.api.sessions import _active_backend_payload, _same_backend_domain
from src.api.sessions import router as sessions_router
from src.core.config import CLAUDE_CONFIG_DIR_ENV_VAR, CharlieBotConfig
from src.core.models import (
    CreateSessionRequest,
    SessionMetadata,
)
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager


def _build_cfg(tmp_path: Path) -> tuple[CharlieBotConfig, Path]:
  """cfg plus the one login directory every cc-claude option shares: entries carry
  no per-entry config dir any more — they all draw the process login, which the
  guard/reachability test pins through $CLAUDE_CONFIG_DIR."""
  config_a = tmp_path / "cfg-a"
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backends={
          "options":
              [
                  backend_option(id="claude-opus-5", label="Opus 5", type="cc-claude", model="claude-opus-5"),
                  backend_option(id="claude-fable-5", label="Fable 5", type="cc-claude", model="claude-fable-5"),
                  backend_option(id="invite-opus", label="Invite Opus", type="cc-claude", model="claude-opus-4-6"),
                  CODEX_BACKEND_OPTION,
              ]
      },
  )
  return cfg, config_a


# ---------------------------------------------------------------------------
# Acceptance #1: guard ⟷ reachability through the real production functions
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("tgt_id", ["claude-opus-5", "claude-fable-5", "codex-o3"])
def test_guard_is_exactly_transcript_reachability(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, tgt_id: str) -> None:
  """allowed(cur, tgt) holds exactly when _resolve_resume_id(tgt) re-finds the transcript.

  A transcript written under the process login dir ($CLAUDE_CONFIG_DIR) is
  reachable by any cc-claude target — every cc-claude option shares that one
  resume domain. Non-cc-claude targets are refused by the guard regardless.
  """
  cfg, config_a = _build_cfg(tmp_path)
  monkeypatch.setenv(CLAUDE_CONFIG_DIR_ENV_VAR, str(config_a))
  sid = "cc-session-uuid"
  _make_transcript(config_a, sid)

  cur_id = "claude-opus-5"  # effective current option, config dir = config_a
  meta = SessionMetadata(id="session-id", name="t", backend=cur_id, cc_session_id=sid)
  allowed = _same_backend_domain(cur_id, tgt_id, cfg)

  tgt_option = cfg.get_backend_option(tgt_id)
  assert tgt_option is not None
  resolved = master_cc._resolve_resume_id(tgt_option, meta)
  reachable = resolved == sid

  if tgt_option.type != "cc-claude":
    assert allowed is False, "non-cc-claude targets are always refused by the guard"
  else:
    assert allowed == reachable, f"cur={cur_id} tgt={tgt_id}: allowed={allowed} reachable={reachable}"


def test_switchable_backend_ids_follow_uniform_domain_rule(tmp_path: Path) -> None:
  """Every session follows the same domain rule, scheduled ones included: a
  scheduled session's switch is an in-place switch like any other session's."""
  cfg, _config_a = _build_cfg(tmp_path)
  meta = SessionMetadata(id="m_id", name="t", backend="claude-opus-5")
  payload = _active_backend_payload(meta, cfg)
  assert payload["active_backend"] == "claude-opus-5"
  assert payload["switchable_backends"] == ["claude-opus-5", "claude-fable-5", "invite-opus"]

  # A cron-dedicated scheduled session gets the same domain-filtered list an
  # ordinary session gets.
  dedicated = SessionMetadata(id="rl-id", name="rl", backend="claude-opus-5", scheduled_task="nightly")
  dedicated_payload = _active_backend_payload(dedicated, cfg)
  assert dedicated_payload["switchable_backends"] == payload["switchable_backends"]


def test_payload_resolves_default_when_backend_empty(tmp_path: Path) -> None:
  cfg, _config_a = _build_cfg(tmp_path)
  meta = SessionMetadata(id="m_id", name="t", backend="")
  payload = _active_backend_payload(meta, cfg)
  assert payload["active_backend"] == cfg.backends.options[0].id
  assert "claude-opus-5" in payload["switchable_backends"]


@pytest.mark.asyncio
async def test_session_view_ships_the_backend_payload_fields(tmp_path: Path) -> None:
  """The view route serves the same backend derivation the bootstrap and usage
  routes do: renderSessionView reads switchable_backends off the first render,
  before the idle usage poll runs."""
  cfg, _config_a = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="t"), backend="claude-opus-5")
  app = FastAPI()
  app.include_router(sessions_router, prefix="/api/sessions")
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  app.dependency_overrides[get_thread_manager] = lambda: ThreadManager(cfg)
  apply_config_overrides(app, cfg)
  with TestClient(app) as client:
    body = client.get(f"/api/sessions/{meta.id}/view").json()
  expected = _active_backend_payload(meta, cfg)
  assert body["active_backend"] == expected["active_backend"] == "claude-opus-5"
  assert body["active_backend_type"] == expected["active_backend_type"]
  assert body["switchable_backends"] == expected["switchable_backends"]


# ---------------------------------------------------------------------------
# Acceptance #3: §4.1 API contract
# ---------------------------------------------------------------------------


async def _seed(session_mgr: SessionManager, *, backend: str) -> str:
  meta = await session_mgr.create_session(CreateSessionRequest(name="t"), backend=backend)
  return meta.id


def _capture_persisted_events(monkeypatch: pytest.MonkeyPatch, session_mgr: SessionManager) -> list[dict]:
  """Swap in a capturing AsyncMock for ``persist_and_broadcast``; return the events it captured."""
  captured: list[dict] = []
  monkeypatch.setattr(
      session_mgr, "persist_and_broadcast", AsyncMock(side_effect=lambda _sid, event: captured.append(event) or None))
  return captured


@pytest.mark.asyncio
async def test_switch_same_domain_returns_updated_meta_and_persists_audit(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg, _config_a = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  sid = await _seed(session_mgr, backend="claude-opus-5")

  captured = _capture_persisted_events(monkeypatch, session_mgr)

  with _build_client(cfg, session_mgr) as client:
    response = client.post(f"/api/sessions/{sid}/backend", json={"backend": "claude-fable-5"})

  assert response.status_code == 200
  assert response.json()["backend"] == "claude-fable-5"
  assert captured == [{"type": "backend_switched", "from": "claude-opus-5", "to": "claude-fable-5"}]
  on_disk = await session_mgr.get_session(sid)
  assert on_disk.backend == "claude-fable-5"


@pytest.mark.asyncio
async def test_switch_to_effective_current_is_idempotent_noop(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, _config_a = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  sid = await _seed(session_mgr, backend="claude-opus-5")

  captured: list[dict] = []
  real_persist = session_mgr.persist_and_broadcast

  async def recording_persist(_sid: str, event: dict) -> None:
    captured.append(event)
    await real_persist(_sid, event)

  monkeypatch.setattr(session_mgr, "persist_and_broadcast", recording_persist)

  with _build_client(cfg, session_mgr) as client:
    response = client.post(f"/api/sessions/{sid}/backend", json={"backend": "claude-opus-5"})
  assert response.status_code == 200
  assert response.json()["backend"] == "claude-opus-5"
  assert not captured, "idempotent no-op must persist no audit event"


@pytest.mark.asyncio
async def test_switch_cross_domain_refuses_and_guides_clone(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, _config_a = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  sid = await _seed(session_mgr, backend="claude-opus-5")

  captured = _capture_persisted_events(monkeypatch, session_mgr)

  with _build_client(cfg, session_mgr) as client:
    for target, reason in [
        ("codex-o3", "non-cc-claude family"),
    ]:
      response = client.post(f"/api/sessions/{sid}/backend", json={"backend": target})
      assert response.status_code == 400, f"{target}: {reason}"
      detail = response.json()["detail"]
      assert "clone" in detail.lower() or "fork" in detail.lower(), f"{target}: detail must steer to clone/fork"

  assert not captured, "cross-domain refusal must not persist an event"


def test_manager_resolve_route_is_gone(tmp_path: Path) -> None:
  """GET /api/sessions/manager does not route; "manager" can only match /{session_id} → 404."""
  cfg, _config_a = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  with _build_client(cfg, session_mgr) as client:
    response = client.get("/api/sessions/manager")
  assert response.status_code == 404
  assert response.json() == {"detail": "Session not found"}


@pytest.mark.asyncio
async def test_switch_unknown_backend_is_400(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, _config_a = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  sid = await _seed(session_mgr, backend="claude-opus-5")
  captured = _capture_persisted_events(monkeypatch, session_mgr)

  with _build_client(cfg, session_mgr) as client:
    response = client.post(f"/api/sessions/{sid}/backend", json={"backend": "missing-backend"})
  assert response.status_code == 400
  assert "clone" in response.json()["detail"].lower() or "fork" in response.json()["detail"].lower()
  assert not captured


@pytest.mark.asyncio
async def test_switch_missing_session_returns_404(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, _config_a = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  captured = _capture_persisted_events(monkeypatch, session_mgr)
  with _build_client(cfg, session_mgr) as client:
    response = client.post("/api/sessions/does-not-exist/backend", json={"backend": "claude-fable-5"})
  assert response.status_code == 404
  assert not captured


@pytest.mark.asyncio
async def test_switch_scheduled_session_unknown_backend_is_400(tmp_path: Path) -> None:
  cfg, _config_a = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  parent = SessionMetadata(id="pm-id", name="pm", scheduled_task="pm_x", backend="claude-opus-5")
  await session_mgr.save_metadata(parent)

  with _build_client(cfg, session_mgr) as client:
    response = client.post(f"/api/sessions/{parent.id}/backend", json={"backend": "missing-backend"})

  assert response.status_code == 400
  assert "clone" in response.json()["detail"].lower() or "fork" in response.json()["detail"].lower()


@pytest.mark.asyncio
async def test_switch_dedicated_session_stays_in_place(tmp_path: Path) -> None:
  """A cron-dedicated scheduled session switches in place like any other session."""
  cfg, _config_a = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  rl = await session_mgr.create_session(
      CreateSessionRequest(name="Scheduled: nightly", scheduled_task="nightly"),
      backend="claude-opus-5",
  )

  with _build_client(cfg, session_mgr) as client:
    response = client.post(f"/api/sessions/{rl.id}/backend", json={"backend": "claude-fable-5"})

  assert response.status_code == 200
  assert response.json()["id"] == rl.id
  updated = await session_mgr.get_session(rl.id)
  assert updated is not None
  assert updated.backend == "claude-fable-5"
  assert len(await session_mgr.list_sessions()) == 1
