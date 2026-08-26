"""Backend-switch endpoint: resume-domain guard and the §4.1 API contract.

Carries two of the acceptance-tested mechanisms:
  * guard ⟷ reachability — the API's same-domain predicate is exactly the
    condition under which the runtime resume resolver can re-find the transcript;
  * the §4.1 API contract over all four rows plus the idempotent no-op.
"""

from pathlib import Path
from unittest.mock import AsyncMock

import pytest
import yaml
from conftest import CODEX_BACKEND_OPTION
from conftest import make_transcript as _make_transcript
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.agents import master_cc
from src.api.deps import get_session_manager, get_trigger_manager
from src.api.sessions import _active_backend_payload, _same_backend_domain
from src.api.sessions import router as sessions_router
from src.core.config import CharlieBotConfig, get_config
from src.core.models import (
  BackendOption,
  CreateSessionRequest,
  SessionMetadata,
  SessionStatus,
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
          CODEX_BACKEND_OPTION,
      ],
  )
  return cfg, config_a, config_b


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


def test_switchable_backend_ids_follow_uniform_domain_rule(tmp_path: Path) -> None:
  """Ordinary sessions follow the domain rule; PM dedicated sessions offer all backends."""
  cfg, _config_a, _config_b = _build_cfg(tmp_path)
  meta = SessionMetadata(id="m_id", name="t", backend="claude-opus-5")
  payload = _active_backend_payload(meta, cfg)
  assert payload["active_backend"] == "claude-opus-5"
  assert payload["switchable_backends"] == ["claude-opus-5", "claude-fable-5"]
  assert payload["backend_switch_rotates"] is False

  # A role-carrying session follows the same domain rule: outside the
  # cc-claude domain (codex here) it has no switchable targets either.
  role_session = SessionMetadata(id="role-id", name="t", backend="codex-o3", role="project")
  role_payload = _active_backend_payload(role_session, cfg)
  assert role_payload["switchable_backends"] == []
  assert role_payload["backend_switch_rotates"] is False

  # A cron-dedicated role-carrying (PM) session's switch writes through to the
  # task yaml and rotates, so the payload offers every backend option and flags
  # that switching rotates.
  dedicated = SessionMetadata(
      id="pm-id", name="pm", backend="claude-opus-5", scheduled_task="pm_x", role="project")
  dedicated_payload = _active_backend_payload(dedicated, cfg)
  assert dedicated_payload["switchable_backends"] == [
      opt.id for opt in cfg.backend_options]
  assert dedicated_payload["backend_switch_rotates"] is True

  # A cron-dedicated role-less session keeps the domain-filtered list (its
  # session-page switch stays an in-place switch).
  rl = SessionMetadata(
      id="rl-id", name="rl", backend="claude-opus-5", scheduled_task="nightly", role=None)
  rl_payload = _active_backend_payload(rl, cfg)
  assert rl_payload["switchable_backends"] == ["claude-opus-5", "claude-fable-5"]
  assert rl_payload["backend_switch_rotates"] is False


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


async def _seed_role(session_mgr: SessionManager, *, backend: str, role: str = "project") -> SessionMetadata:
  return await session_mgr.create_session(CreateSessionRequest(name="Role session", role=role), backend=backend)


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
  """A non-cron role session switches in place via the ordinary same-domain path."""
  cfg, _config_a, _config_b = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  trigger_mgr = TriggerManager(cfg, session_mgr)
  role_session = await _seed_role(session_mgr, backend="claude-opus-5")

  with _build_client(cfg, session_mgr, trigger_mgr) as client:
    response = client.post(
        f"/api/sessions/{role_session.id}/backend",
        json={"backend": "claude-fable-5"},
    )

  assert response.status_code == 200
  assert response.json()["id"] == role_session.id
  updated = await session_mgr.get_session(role_session.id)
  assert updated is not None
  assert updated.backend == "claude-fable-5"
  assert updated.role == "project"
  assert len(await session_mgr.list_sessions()) == 1


@pytest.mark.asyncio
async def test_switch_cross_domain_role_session_gets_clone_fork_400(tmp_path: Path) -> None:
  """A non-cron role-carrying session gets the uniform cross-domain 400; it is unchanged."""
  cfg, _config_a, _config_b = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  role_session = await _seed_role(session_mgr, backend="claude-opus-5")

  with _build_client(cfg, session_mgr) as client:
    for target in ("invite-opus", "codex-o3"):
      response = client.post(f"/api/sessions/{role_session.id}/backend", json={"backend": target})
      assert response.status_code == 400, f"target={target}"
      detail = response.json()["detail"]
      assert "clone" in detail.lower() or "fork" in detail.lower(), f"target={target}"

  on_disk = await session_mgr.get_session(role_session.id)
  assert on_disk is not None
  assert on_disk.backend == "claude-opus-5"
  assert on_disk.role == "project"
  assert on_disk.status == SessionStatus.ACTIVE
  assert len(await session_mgr.list_sessions()) == 1


def test_manager_resolve_route_is_gone(tmp_path: Path) -> None:
  """GET /api/sessions/manager no longer routes; "manager" can only match /{session_id} → 404."""
  cfg, _config_a, _config_b = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  with _build_client(cfg, session_mgr) as client:
    response = client.get("/api/sessions/manager")
  assert response.status_code == 404
  assert response.json() == {"detail": "Session not found"}


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


# ---------------------------------------------------------------------------
# §4.1 write-through: PM dedicated session switch updates the task yaml
# ---------------------------------------------------------------------------


def _seed_pm_task(home: Path, name: str) -> Path:
  """Write a mode: master cron yaml plus the env-rooted cron dir it loads from.

  The host file carries the path to its prompt source under ``prompt_file``;
  the pointed file owns the body and the loader reads it on every load.
  """
  cron_dir = home / "config.d" / "cron.d"
  cron_dir.mkdir(parents=True, exist_ok=True)
  prompt_path = cron_dir.parent / "prompts" / f"{name}.md"
  prompt_path.parent.mkdir(parents=True, exist_ok=True)
  prompt_path.write_text("review and report", encoding="utf-8")
  path = cron_dir / f"{name}.yaml"
  body = {
      "cron": "0 2 * * *",
      "prompt_file": str(prompt_path),
      "mode": "master",
      "project": "the-group",
      "timezone": "America/Los_Angeles",
      "enabled": True,
  }
  path.write_text(yaml.safe_dump(body, sort_keys=False), encoding="utf-8")
  return path


def _read_yaml(path: Path) -> dict:
  return yaml.safe_load(path.read_text(encoding="utf-8"))


async def _seed_pm_session(session_mgr: SessionManager) -> SessionMetadata:
  return await session_mgr.create_session(
      CreateSessionRequest(name="Scheduled: pm_x", scheduled_task="pm_x", role="project"),
      backend="claude-opus-5",
  )


@pytest.mark.asyncio
async def test_switch_pm_dedicated_session_writes_through_to_yaml_and_rotates(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg, _config_a, _config_b = _build_cfg(tmp_path)
  home = tmp_path / ".charliebot"
  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
  session_mgr = SessionManager(cfg)
  yaml_path = _seed_pm_task(home, "pm_x")
  parent = await _seed_pm_session(session_mgr)
  parent.group = "the-group"
  await session_mgr.save_metadata(parent)

  with _build_client(cfg, session_mgr) as client:
    response = client.post(f"/api/sessions/{parent.id}/backend", json={"backend": "claude-fable-5"})

  assert response.status_code == 200
  rotated = response.json()
  assert rotated is not None
  assert rotated["id"] != parent.id
  assert rotated["backend"] == "claude-fable-5"
  assert rotated["role"] == "project"
  assert rotated["group"] == "the-group"
  assert rotated["scheduled_task"] == "pm_x"

  # The yaml is the single control point: the backend key is now persisted.
  assert _read_yaml(yaml_path)["backend"] == "claude-fable-5"

  # The old session is archived, the new one is the sole active dedicated one.
  on_disk_parent = await session_mgr.get_session(parent.id)
  assert on_disk_parent is not None
  assert on_disk_parent.status == SessionStatus.ARCHIVED
  active = await session_mgr.list_sessions(status=SessionStatus.ACTIVE, scheduled=True)
  assert [s.id for s in active] == [rotated["id"]]


@pytest.mark.asyncio
async def test_switch_pm_dedicated_session_busy_is_409_without_yaml_write(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  from src.core.thinking_state import clear_busy, mark_busy

  home = tmp_path / ".charliebot"
  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
  cfg, _config_a, _config_b = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  yaml_path = _seed_pm_task(home, "pm_x")
  parent = await _seed_pm_session(session_mgr)
  before = yaml_path.read_bytes()

  mark_busy(parent.id)
  try:
    with _build_client(cfg, session_mgr) as client:
      response = client.post(f"/api/sessions/{parent.id}/backend", json={"backend": "claude-fable-5"})
  finally:
    clear_busy(parent.id)

  assert response.status_code == 409
  assert "backend switch" in response.json()["detail"]
  # Busy 409 surfaces before any yaml write.
  assert yaml_path.read_bytes() == before


@pytest.mark.asyncio
async def test_switch_pm_dedicated_session_unknown_backend_is_400(tmp_path: Path) -> None:
  cfg, _config_a, _config_b = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  parent = SessionMetadata(
      id="pm-id", name="pm", scheduled_task="pm_x", role="project", backend="claude-opus-5")
  await session_mgr.save_metadata(parent)

  with _build_client(cfg, session_mgr) as client:
    response = client.post(
        f"/api/sessions/{parent.id}/backend", json={"backend": "missing-backend"})

  assert response.status_code == 400
  assert "clone" in response.json()["detail"].lower() or "fork" in response.json()["detail"].lower()


@pytest.mark.asyncio
async def test_switch_pm_dedicated_session_missing_task_yaml_is_404(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, _config_a, _config_b = _build_cfg(tmp_path)
  home = tmp_path / ".charliebot"
  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
  session_mgr = SessionManager(cfg)
  _seed_pm_task(home, "pm_x")
  parent = await _seed_pm_session(session_mgr)
  (home / "config.d" / "cron.d" / "pm_x.yaml").unlink()

  with _build_client(cfg, session_mgr) as client:
    response = client.post(f"/api/sessions/{parent.id}/backend", json={"backend": "claude-fable-5"})

  assert response.status_code == 404
  assert 'Task "pm_x" not found' in response.json()["detail"]


@pytest.mark.asyncio
async def test_switch_role_less_dedicated_session_stays_in_place(tmp_path: Path) -> None:
  """A cron-dedicated role-less session keeps the in-place domain switch."""
  cfg, _config_a, _config_b = _build_cfg(tmp_path)
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
