from pathlib import Path

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.agents.backends.pty_common import _TMUX_SOCKET, tmux_session_name
from src.api import threads as threads_api
from src.api.deps import get_config_on_loop, get_thread_manager
from src.api.threads import build_attach_command
from src.api.threads import router as threads_router
from src.core.config import CharlieBotConfig, get_config
from src.core.models import BackendOption, ThreadMetadata
from src.core.threads import ThreadManager


def _thread(**overrides) -> ThreadMetadata:
  base = {
      "id": "thread-id",
      "session_id": "session-id",
      "description": "task",
      "worktree_path": "/tmp/worktree",
      "claude_session_id": "claude-session-id",
  }
  base.update(overrides)
  return ThreadMetadata(**base)


def _claude_sub_cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      backend_options=[
          BackendOption(
              id="claude-sub",
              label="Claude Sub",
              type="cc-claude",
              model="claude-opus-4-8",
              cli_binary="claude-sub",
          ),
      ],
  )


@pytest.mark.parametrize(
    ("backend_type", "expected_kind"),
    [
        ("cc-claude", "claude-resume"),
        ("cc-kimi", None),
        ("cc-openai-compatible", None),
        ("codex", None),
        ("charlie-code", None),
        ("gemini", None),
        ("opencode", None),
        ("antigravity", None),
        ("tui-cli", "tmux"),
        ("unknown", None),
    ],
)
def test_build_attach_command_dispatches_known_backends(backend_type: str, expected_kind: str | None) -> None:
  command = build_attach_command(_thread(backend=backend_type))

  if expected_kind == "claude-resume":
    assert command == "cd /tmp/worktree && claude --resume claude-session-id"
  elif expected_kind == "tmux":
    assert command == f"tmux -L {_TMUX_SOCKET} attach -t {tmux_session_name('session-id')}"
  else:
    assert command is None


def test_build_attach_command_uses_tmux_for_claude_sub_config(tmp_path: Path) -> None:
  cfg = _claude_sub_cfg(tmp_path)

  command = build_attach_command(_thread(backend="claude-sub"), cfg)

  assert command == f"tmux -L {_TMUX_SOCKET} attach -r -t {tmux_session_name('claude-session-id')}"


@pytest.mark.asyncio
async def test_attach_available_uses_tmux_for_claude_sub_config(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  cfg = _claude_sub_cfg(tmp_path)
  seen: list[str] = []

  async def fake_tmux_session_exists(session_id: str) -> bool:
    seen.append(session_id)
    return True

  monkeypatch.setattr(threads_api, "tmux_session_exists", fake_tmux_session_exists)

  available = await threads_api._attach_available(_thread(backend="claude-sub"), cfg)

  assert available is True
  assert seen == ["claude-session-id"]


def _build_client(cfg: CharlieBotConfig, thread_mgr: ThreadManager) -> TestClient:
  app = FastAPI()
  app.include_router(threads_router, prefix="/api/threads")
  app.dependency_overrides[get_config] = lambda: cfg
  app.dependency_overrides[get_config_on_loop] = lambda: cfg
  app.dependency_overrides[get_thread_manager] = lambda: thread_mgr
  return TestClient(app)


@pytest.mark.asyncio
async def test_thread_metadata_endpoint_exposes_derived_attach_fields(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      backend_options=[
          BackendOption(id="claude-opus", label="Claude", type="cc-claude", model="claude-opus-4-8"),
      ],
  )
  thread_mgr = ThreadManager(cfg)
  worktree = tmp_path / "worktree"
  worktree.mkdir()
  thread = _thread(
      backend="claude-opus",
      session_id="session-id",
      worktree_path=str(worktree),
      claude_session_id="session-123",
  )
  await thread_mgr.save_metadata(thread)

  with _build_client(cfg, thread_mgr) as client:
    response = client.get(f"/api/threads/{thread.session_id}/threads/{thread.id}")
    assert response.status_code == 200
    data = response.json()
    assert data["attach_command"] == f"cd {worktree} && claude --resume session-123"
    assert data["attach_available"] is True

    worktree.rmdir()
    response = client.get(f"/api/threads/{thread.session_id}/threads/{thread.id}")
    assert response.status_code == 200
    data = response.json()
    assert data["attach_command"] == f"cd {worktree} && claude --resume session-123"
    assert data["attach_available"] is False

  raw_metadata = (cfg.sessions_dir / thread.session_id / "threads" / thread.id /
                  "metadata.json").read_text(encoding="utf-8")
  assert "attach_command" not in raw_metadata
  assert "attach_available" not in raw_metadata


@pytest.mark.asyncio
async def test_thread_metadata_endpoint_attach_mode_serves_only_the_pair(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      backend_options=[
          BackendOption(id="claude-opus", label="Claude", type="cc-claude", model="claude-opus-4-8"),
      ],
  )
  thread_mgr = ThreadManager(cfg)
  worktree = tmp_path / "worktree"
  worktree.mkdir()
  thread = _thread(
      backend="claude-opus",
      session_id="session-id",
      worktree_path=str(worktree),
      claude_session_id="session-123",
      description="x" * 5000,
  )
  await thread_mgr.save_metadata(thread)

  with _build_client(cfg, thread_mgr) as client:
    attach_data = client.get(f"/api/threads/{thread.session_id}/threads/{thread.id}?attach=1").json()
    assert set(attach_data) == {"attach_command", "attach_available"}
    assert attach_data["attach_command"] == f"cd {worktree} && claude --resume session-123"
    assert attach_data["attach_available"] is True

    full_data = client.get(f"/api/threads/{thread.session_id}/threads/{thread.id}").json()
    assert full_data["attach_command"] == attach_data["attach_command"]
    assert full_data["attach_available"] == attach_data["attach_available"]
    # The task-spec body has no reader on this endpoint; the modal reads
    # description once per click.
    assert "context" not in full_data
    assert full_data["description"] == "x" * 5000


@pytest.mark.asyncio
async def test_thread_metadata_endpoint_serves_a_rewritten_row(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  thread_mgr = ThreadManager(cfg)
  thread = _thread(session_id="session-id", description="before")
  await thread_mgr.save_metadata(thread)

  with _build_client(cfg, thread_mgr) as client:
    url = f"/api/threads/{thread.session_id}/threads/{thread.id}"
    assert client.get(url).json()["description"] == "before"

    thread.description = "after"
    await thread_mgr.save_metadata(thread)
    assert client.get(url).json()["description"] == "after"
