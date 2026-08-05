"""Backend-switch endpoint: resume-domain guard and the §4.1 API contract.

Carries two of the acceptance-tested mechanisms:
  * guard ⟷ reachability — the API's same-domain predicate is exactly the
    condition under which the runtime resume resolver can re-find the transcript;
  * the §4.1 API contract over all four rows plus the idempotent no-op.
"""

from pathlib import Path
from unittest.mock import AsyncMock

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.agents import master_cc
from src.api.deps import get_session_manager
from src.api.sessions import _active_backend_payload, _same_backend_domain
from src.api.sessions import router as sessions_router
from src.core.config import CharlieBotConfig, get_config
from src.core.models import BackendOption, CreateSessionRequest, SessionMetadata
from src.core.sessions import SessionManager


def _build_cfg(tmp_path: Path) -> tuple[CharlieBotConfig, Path, Path]:
  config_a = tmp_path / "cfg-a"
  config_b = tmp_path / "cfg-b"
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          BackendOption(
              id="claude-opus-5", label="Opus 5", type="cc-claude", model="claude-opus-5",
              claude_config_dir=str(config_a)),
          BackendOption(
              id="claude-fable-5", label="Fable 5", type="cc-claude", model="claude-fable-5",
              claude_config_dir=str(config_a)),
          BackendOption(
              id="invite-opus", label="Invite Opus", type="cc-claude", model="claude-opus-4-6",
              claude_config_dir=str(config_b)),
          BackendOption(id="codex-o3", label="Codex", type="codex", model="o3"),
      ],
  )
  return cfg, config_a, config_b


def _make_transcript(config_dir: Path, cc_session_id: str) -> Path:
  transcript = config_dir / "projects" / "slug" / f"{cc_session_id}.jsonl"
  transcript.parent.mkdir(parents=True, exist_ok=True)
  transcript.write_text("[]", encoding="utf-8")
  return transcript


def _build_client(cfg: CharlieBotConfig, session_mgr: SessionManager) -> TestClient:
  app = FastAPI()
  app.include_router(sessions_router, prefix="/api/sessions")
  app.dependency_overrides[get_config] = lambda: cfg
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  return TestClient(app)


# ---------------------------------------------------------------------------
# Acceptance #1: guard ⟷ reachability through the real production functions
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("tgt_id", ["claude-opus-5", "claude-fable-5", "invite-opus", "codex-o3"])
def test_guard_is_exactly_transcript_reachability(tmp_path: Path, tgt_id: str) -> None:
  """allowed(cur, tgt) holds exactly when _resolve_resume_id(tgt) re-finds the transcript.

  A transcript written under the *current* option's resolved config dir is
  reachable by a cc-claude target iff that target shares the current option's
  resume domain. Non-cc-claude targets are refused by the guard regardless.
  """
  cfg, config_a, _config_b = _build_cfg(tmp_path)
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


def test_switchable_backend_ids_only_same_domain_and_include_current(tmp_path: Path) -> None:
  """The payload's switchable list is server-computed and domain-scoped."""
  cfg, _config_a, _config_b = _build_cfg(tmp_path)
  meta = SessionMetadata(id="m_id", name="t", backend="claude-opus-5")
  payload = _active_backend_payload(meta, cfg)
  assert payload["active_backend"] == "claude-opus-5"
  assert payload["switchable_backends"] == ["claude-opus-5", "claude-fable-5"]


def test_payload_resolves_default_when_backend_empty(tmp_path: Path) -> None:
  cfg, _config_a, _config_b = _build_cfg(tmp_path)
  meta = SessionMetadata(id="m_id", name="t", backend="")
  payload = _active_backend_payload(meta, cfg)
  assert payload["active_backend"] == cfg.backend_options[0].id
  assert "claude-opus-5" in payload["switchable_backends"]


# ---------------------------------------------------------------------------
# Acceptance #3: §4.1 API contract
# ---------------------------------------------------------------------------


async def _seed(session_mgr: SessionManager, *, backend: str) -> str:
  meta = await session_mgr.create_session(CreateSessionRequest(name="t"), backend=backend)
  return meta.id


@pytest.mark.asyncio
async def test_switch_same_domain_returns_updated_meta_and_persists_audit(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg, _config_a, _config_b = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  sid = await _seed(session_mgr, backend="claude-opus-5")

  captured: list[dict] = []
  fake = AsyncMock(side_effect=lambda _sid, event: captured.append(event) or None)
  monkeypatch.setattr(session_mgr, "persist_and_broadcast", fake)

  with _build_client(cfg, session_mgr) as client:
    response = client.post(f"/api/sessions/{sid}/backend", json={"backend": "claude-fable-5"})

  assert response.status_code == 200
  assert response.json()["backend"] == "claude-fable-5"
  assert captured == [{"type": "backend_switched", "from": "claude-opus-5", "to": "claude-fable-5"}]
  on_disk = await session_mgr.get_session(sid)
  assert on_disk.backend == "claude-fable-5"


@pytest.mark.asyncio
async def test_switch_to_effective_current_is_idempotent_noop(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, _config_a, _config_b = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  sid = await _seed(session_mgr, backend="claude-opus-5")

  captured: list[dict] = []
  real_persist = session_mgr.persist_and_broadcast
  async def recording_persist(_sid, event):
    captured.append(event)
    await real_persist(_sid, event)
  monkeypatch.setattr(session_mgr, "persist_and_broadcast", recording_persist)

  with _build_client(cfg, session_mgr) as client:
    response = client.post(f"/api/sessions/{sid}/backend", json={"backend": "claude-opus-5"})
  assert response.status_code == 200
  assert response.json()["backend"] == "claude-opus-5"
  assert captured == [], "idempotent no-op must persist no audit event"


@pytest.mark.asyncio
async def test_switch_cross_domain_refuses_and_guides_clone(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, _config_a, _config_b = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  sid = await _seed(session_mgr, backend="claude-opus-5")

  captured: list[dict] = []
  monkeypatch.setattr(
      session_mgr, "persist_and_broadcast",
      AsyncMock(side_effect=lambda _sid, event: captured.append(event) or None))

  with _build_client(cfg, session_mgr) as client:
    for target, reason in [
        ("invite-opus", "different config dir"),
        ("codex-o3", "non-cc-claude family"),
    ]:
      response = client.post(f"/api/sessions/{sid}/backend", json={"backend": target})
      assert response.status_code == 400, f"{target}: {reason}"
      detail = response.json()["detail"]
      assert "clone" in detail.lower() or "fork" in detail.lower(), f"{target}: detail must steer to clone/fork"

  assert captured == [], "cross-domain refusal must not persist an event"


@pytest.mark.asyncio
async def test_switch_unknown_backend_is_400(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, _config_a, _config_b = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  sid = await _seed(session_mgr, backend="claude-opus-5")
  captured: list[dict] = []
  monkeypatch.setattr(
      session_mgr, "persist_and_broadcast",
      AsyncMock(side_effect=lambda _sid, event: captured.append(event) or None))

  with _build_client(cfg, session_mgr) as client:
    response = client.post(f"/api/sessions/{sid}/backend", json={"backend": "missing-backend"})
  assert response.status_code == 400
  assert "clone" in response.json()["detail"].lower() or "fork" in response.json()["detail"].lower()
  assert captured == []


@pytest.mark.asyncio
async def test_switch_missing_session_returns_404(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, _config_a, _config_b = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  captured: list[dict] = []
  monkeypatch.setattr(
      session_mgr, "persist_and_broadcast",
      AsyncMock(side_effect=lambda _sid, event: captured.append(event) or None))
  with _build_client(cfg, session_mgr) as client:
    response = client.post("/api/sessions/does-not-exist/backend", json={"backend": "claude-fable-5"})
  assert response.status_code == 404
  assert captured == []