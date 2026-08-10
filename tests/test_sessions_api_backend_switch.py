"""Backend-switch endpoint: resume-domain guard and the §4.1 API contract.

Carries two of the acceptance-tested mechanisms:
  * guard ⟷ reachability — the API's same-domain predicate is exactly the
    condition under which the runtime resume resolver can re-find the transcript;
  * the §4.1 API contract over all four rows plus the idempotent no-op.
"""

import asyncio
from datetime import datetime, timedelta, timezone
from pathlib import Path
from unittest.mock import AsyncMock

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.agents import master_cc
from src.api import sessions as sessions_api
from src.api.deps import get_session_manager, get_trigger_manager
from src.api.sessions import _active_backend_payload, _same_backend_domain
from src.api.sessions import router as sessions_router
from src.core.config import CharlieBotConfig, get_config
from src.core.models import (
  BackendOption,
  CreateSessionRequest,
  PendingTrigger,
  SessionMetadata,
  SessionStatus,
  TriggerStatus,
)
from src.core.sessions import SessionManager
from src.core.triggers import TriggerManager


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


def _build_client(
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    trigger_mgr: TriggerManager | None = None,
) -> TestClient:
  app = FastAPI()
  app.include_router(sessions_router, prefix="/api/sessions")
  app.dependency_overrides[get_config] = lambda: cfg
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  if trigger_mgr is not None:
    app.dependency_overrides[get_trigger_manager] = lambda: trigger_mgr
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


def test_switchable_backend_ids_are_role_aware(tmp_path: Path) -> None:
  """Role sessions list all configured targets; ordinary sessions stay domain-scoped."""
  cfg, _config_a, _config_b = _build_cfg(tmp_path)
  meta = SessionMetadata(id="m_id", name="t", backend="claude-opus-5")
  payload = _active_backend_payload(meta, cfg)
  assert payload["active_backend"] == "claude-opus-5"
  assert payload["switchable_backends"] == ["claude-opus-5", "claude-fable-5"]

  manager = SessionMetadata(id="manager-id", name="manager", backend="codex-o3", role="manager")
  manager_payload = _active_backend_payload(manager, cfg)
  assert manager_payload["switchable_backends"] == [option.id for option in cfg.backend_options]


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


async def _seed_role(session_mgr: SessionManager, *, backend: str, role: str = "manager") -> SessionMetadata:
  return await session_mgr.create_session(CreateSessionRequest(name="Session Manager", role=role), backend=backend)


def _write_trigger(cfg: CharlieBotConfig, trigger: PendingTrigger) -> None:
  path = cfg.sessions_dir / trigger.session_id / "triggers" / f"{trigger.id}.json"
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(trigger.model_dump_json(indent=2), encoding="utf-8")


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
async def test_switch_same_domain_role_session_stays_in_place(tmp_path: Path) -> None:
  cfg, _config_a, _config_b = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  trigger_mgr = TriggerManager(cfg, session_mgr)
  manager = await _seed_role(session_mgr, backend="claude-opus-5")

  with _build_client(cfg, session_mgr, trigger_mgr) as client:
    response = client.post(
        f"/api/sessions/{manager.id}/backend",
        json={"backend": "claude-fable-5"},
    )

  assert response.status_code == 200
  assert response.json()["id"] == manager.id
  updated = await session_mgr.get_session(manager.id)
  assert updated is not None
  assert updated.backend == "claude-fable-5"
  assert updated.role == "manager"
  assert len(await session_mgr.list_sessions()) == 1


@pytest.mark.asyncio
async def test_switch_cross_domain_role_session_migrates_with_triggers_and_orientation(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg, _config_a, _config_b = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  trigger_mgr = TriggerManager(cfg, session_mgr)
  manager = await _seed_role(session_mgr, backend="claude-opus-5")
  trigger = PendingTrigger(
      id="manager-patrol",
      session_id=manager.id,
      fire_at=datetime.now(timezone.utc) + timedelta(hours=1),
      message="[patrol] patrol",
  )
  _write_trigger(cfg, trigger)

  orientation_calls: list[tuple[str, str]] = []
  monkeypatch.setattr(
      sessions_api,
      "_send_manager_orientation",
      lambda _cfg, meta, _session_mgr: orientation_calls.append((meta.id, meta.role or "")),
  )
  audit_events: list[tuple[str, dict]] = []
  monkeypatch.setattr(
      session_mgr,
      "persist_and_broadcast",
      AsyncMock(side_effect=lambda sid, event: audit_events.append((sid, event))),
  )

  with _build_client(cfg, session_mgr, trigger_mgr) as client:
    response = client.post(
        f"/api/sessions/{manager.id}/backend",
        json={"backend": "invite-opus"},
    )

  assert response.status_code == 200
  new_id = response.json()["id"]
  assert new_id != manager.id

  old = await session_mgr.get_session(manager.id)
  new = await session_mgr.get_session(new_id)
  assert old is not None and new is not None
  assert old.status == SessionStatus.ARCHIVED
  assert old.role is None
  assert new.status == SessionStatus.ACTIVE
  assert new.role == "manager"
  assert new.name == manager.name
  assert new.backend == "invite-opus"
  assert orientation_calls == [(new_id, "manager")]

  old_triggers = await trigger_mgr.list_triggers(manager.id)
  assert [item.status for item in old_triggers] == [TriggerStatus.CANCELLED]
  assert audit_events == [
      (
          manager.id,
          {
              "type": "backend_switched",
              "from": "claude-opus-5",
              "to": "invite-opus",
              "old_session_id": manager.id,
              "new_session_id": new_id,
          },
      )
  ]


@pytest.mark.asyncio
async def test_migration_and_manager_resolution_share_lock(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, _config_a, _config_b = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  trigger_mgr = TriggerManager(cfg, session_mgr)
  manager = await _seed_role(session_mgr, backend="claude-opus-5")
  monkeypatch.setattr(sessions_api, "_manager_session_lock", asyncio.Lock())
  monkeypatch.setattr(sessions_api, "_send_manager_orientation", lambda *_args: None)

  migration, resolution = await asyncio.gather(
      sessions_api.switch_session_backend(
          manager.id,
          sessions_api.SwitchBackendRequest(backend="invite-opus"),
          parent=manager,
          session_mgr=session_mgr,
          trigger_mgr=trigger_mgr,
          cfg=cfg,
      ),
      sessions_api.get_manager_session(session_mgr=session_mgr, trigger_mgr=trigger_mgr, cfg=cfg),
  )

  assert migration.id != manager.id
  assert resolution["id"]
  active_managers = [
      session for session in await session_mgr.list_sessions(status=SessionStatus.ACTIVE)
      if session.role == "manager"
  ]
  assert len(active_managers) == 1


@pytest.mark.asyncio
async def test_failed_role_migration_restores_original_and_cleans_partial_replacement(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg, _config_a, _config_b = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  trigger_mgr = TriggerManager(cfg, session_mgr)
  manager = await _seed_role(session_mgr, backend="claude-opus-5")
  real_create_session = session_mgr.create_session

  async def create_then_fail(req, backend=None):
    await real_create_session(req, backend=backend)
    raise RuntimeError("backend setup failed")

  monkeypatch.setattr(session_mgr, "create_session", create_then_fail)

  with pytest.raises(RuntimeError, match="backend setup failed"):
    await sessions_api.switch_session_backend(
        manager.id,
        sessions_api.SwitchBackendRequest(backend="invite-opus"),
        parent=manager,
        session_mgr=session_mgr,
        trigger_mgr=trigger_mgr,
        cfg=cfg,
    )

  restored = await session_mgr.get_session(manager.id)
  assert restored is not None
  assert restored.status == SessionStatus.ACTIVE
  assert restored.role == "manager"
  active_managers = [
      session for session in await session_mgr.list_sessions(status=SessionStatus.ACTIVE)
      if session.role == "manager"
  ]
  assert [session.id for session in active_managers] == [manager.id]


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
