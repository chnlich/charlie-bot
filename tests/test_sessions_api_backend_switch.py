"""Backend-switch endpoint: the continuation-domain rule and the API contract.

Carries two of the acceptance-tested mechanisms:
  * domain ⟷ reachability — ``same_continuation_domain`` is exactly the
    condition under which the runtime resume resolver can re-find a cc-claude
    target's transcript (a cross-family target is its own domain and starts
    its own conversation, which the endpoint accepts for an ordinary session);
  * the API contract over the four configured rows plus the idempotent no-op:
    an ordinary session switches in place across families, a cron-dedicated
    session keeps the in-domain rule, and the pre-rule backfill.
"""

from pathlib import Path
from unittest.mock import AsyncMock

import pytest
from conftest import CODEX_BACKEND_OPTION, backend_option, bind_deps_managers
from conftest import make_sessions_client as _build_client

from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest
from src.core.sessions import SessionManager


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
  assert captured == [
      {
          "type": "backend_switched",
          "from": "claude-opus-5",
          "to": "claude-fable-5",
          "previous_native_backend": None,
          "previous_native_session_id": None,
      }
  ]
  on_disk = await session_mgr.get_session(sid)
  assert on_disk.backend == "claude-fable-5"


@pytest.mark.asyncio
async def test_switch_cross_family_switches_in_place(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """An ordinary session accepts a cross-family target: 200, backend updated on
  disk, and the audit event carries exactly the two previous-native fields (a
  session holding no native id records none)."""
  cfg, _config_a = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  sid = await _seed(session_mgr, backend="claude-opus-5")

  captured = _capture_persisted_events(monkeypatch, session_mgr)

  with _build_client(cfg, session_mgr) as client:
    response = client.post(f"/api/sessions/{sid}/backend", json={"backend": "codex-o3"})

  assert response.status_code == 200
  assert response.json()["backend"] == "codex-o3"
  assert captured == [
      {
          "type": "backend_switched",
          "from": "claude-opus-5",
          "to": "codex-o3",
          "previous_native_backend": None,
          "previous_native_session_id": None,
      }
  ]
  on_disk = await session_mgr.get_session(sid)
  assert on_disk is not None
  assert on_disk.backend == "codex-o3"


@pytest.mark.asyncio
async def test_switch_bound_node_cross_family_is_400(
    tmp_path: Path, temp_home: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A task-bound node keeps the in-domain rule: a cross-family target earns
  the cron-config 400 pointing at the task's config, no event, backend
  unchanged. The judgment reads the binding (the loaded task configs'
  session_id), never a scheduled_task stamp."""
  cfg, _config_a = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  from src.core.task_sessions import TaskTreeManager
  tree = TaskTreeManager(cfg, session_mgr)
  bind_deps_managers(monkeypatch, tree, session_mgr)
  rl = await tree.create_task(
      request_id="scheduled-node:nightly",
      task_parent_id=None,
      profile="manager",
      task=None,
      name="nightly",
      backend="claude-opus-5",
      caller="system")
  # temp_home points HOME (and so cron_dir()) at tmp_path: the binding file the
  # loaded task configs read lives under the same synthetic home.
  assert cfg.charliebot_home == Path(temp_home) / ".charliebot"
  # A host file carries the path to its prompt source, never the inline body.
  prompts = cfg.charliebot_home / "prompts"
  prompts.mkdir(parents=True, exist_ok=True)
  prompt_path = prompts / "nightly.md"
  prompt_path.write_text("run nightly", encoding="utf-8")
  cron_d = cfg.charliebot_home / "config.d" / "cron.d"
  cron_d.mkdir(parents=True, exist_ok=True)
  import yaml as yaml_mod
  (cron_d / "nightly.yaml").write_text(
      yaml_mod.safe_dump(
          {
              "cron": "0 3 * * *",
              "prompt_file": str(prompt_path),
              "session_id": rl.id,
              "backend": "claude-opus-5",
          }),
      encoding="utf-8")

  captured = _capture_persisted_events(monkeypatch, session_mgr)

  with _build_client(cfg, session_mgr) as client:
    response = client.post(f"/api/sessions/{rl.id}/backend", json={"backend": "codex-o3"})

  assert response.status_code == 400
  detail = response.json()["detail"]
  assert "cron" in detail
  assert "codex-o3" in detail
  assert not captured
  on_disk = await session_mgr.get_session(rl.id)
  assert on_disk is not None
  assert on_disk.backend == "claude-opus-5"


@pytest.mark.asyncio
async def test_switch_missing_session_returns_404(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, _config_a = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  captured = _capture_persisted_events(monkeypatch, session_mgr)
  with _build_client(cfg, session_mgr) as client:
    response = client.post("/api/sessions/does-not-exist/backend", json={"backend": "claude-fable-5"})
  assert response.status_code == 404
  assert not captured
