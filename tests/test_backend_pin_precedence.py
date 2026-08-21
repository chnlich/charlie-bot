"""Run path never adopts a backend the session did not select.

Covers both `backend_option is None` substitution sites (chat.py's message
path, master_trigger.py's wake path) and the run_cc guard's precedence over
`backend_options[0]` for callers (replay) that pass no option at all.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from src.agents import master_cc, master_cc_run
from src.agents.backends import base as backend_base
from src.api.chat import run_and_finalize
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.master_trigger import trigger_master
from src.core.models import BackendOption, CreateSessionRequest
from src.core.sessions import SessionManager


class _FakeBackend:
  exit_code = 0
  stderr_text = ""

  async def run(self, prompt: str, cwd: str, env: dict):
    yield backend_base.make_result_event()


def _assistant_errors(session_mgr: SessionManager, session_id: str) -> list[dict]:
  events = session_mgr.load_chat_events_sync(session_id)
  return [e for e in events if e["type"] == ET.ASSISTANT_ERROR]


# --------------------------------------------------- unresolvable codex pin


@pytest.mark.asyncio
async def test_message_path_unresolvable_codex_pin_hard_fails(tmp_path: Path, monkeypatch) -> None:
  """No backend_options entry starts with codex — the pin still must not be
  substituted onto anything."""
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6")],
  )
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Test Session"), backend="codex-ghost-9")

  spawned: list[object] = []
  monkeypatch.setattr("src.agents.backends.registry.build_backend",
                      lambda *a, **k: spawned.append(1) or _FakeBackend())
  monkeypatch.setattr(master_cc_run, "_build_instructions_content", lambda session_meta, cfg, model=None: "instructions")

  await run_and_finalize(cfg, session, "hello", session_mgr)

  assert spawned == []
  errors = _assistant_errors(session_mgr, session.id)
  assert errors
  assert any("codex-ghost-9" in e["content"] for e in errors)


@pytest.mark.asyncio
async def test_wake_path_unresolvable_codex_pin_hard_fails(tmp_path: Path, monkeypatch) -> None:
  """Same session/pin as above, driven through the async-wake entry instead
  of the message path — guards delegation merge / improve completion /
  schedule triggers / review wakes, which all funnel through trigger_master."""
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6")],
  )
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Test Session"), backend="codex-ghost-9")

  spawned: list[object] = []
  monkeypatch.setattr("src.agents.backends.registry.build_backend",
                      lambda *a, **k: spawned.append(1) or _FakeBackend())
  monkeypatch.setattr(master_cc_run, "_build_instructions_content", lambda session_meta, cfg, model=None: "instructions")

  await trigger_master(session.id, "worker summary", cfg, session_mgr)

  assert spawned == []
  errors = _assistant_errors(session_mgr, session.id)
  assert errors
  assert any("codex-ghost-9" in e["content"] for e in errors)


@pytest.mark.asyncio
async def test_unresolvable_codex_pin_lands_on_none_of_several_codex_options(tmp_path: Path, monkeypatch) -> None:
  """Discriminates the deleted `next(... type == "codex" ...)` substitution:
  with several codex-type entries configured, an unresolvable codex-prefixed
  pin must still hard-fail rather than land on any of them (the arbitrary
  first match `next()` used to pick)."""
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
          BackendOption(id="codex-alpha", label="Codex Alpha", type="codex", model="alpha"),
          BackendOption(id="codex-beta", label="Codex Beta", type="codex", model="beta"),
      ],
  )
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Test Session"), backend="codex-ghost-9")

  spawned_option_ids: list[str] = []
  monkeypatch.setattr(
      "src.agents.backends.registry.build_backend",
      lambda option, cfg, **k: spawned_option_ids.append(option.id) or _FakeBackend())
  monkeypatch.setattr(master_cc_run, "_build_instructions_content", lambda session_meta, cfg, model=None: "instructions")

  await run_and_finalize(cfg, session, "hello", session_mgr)

  assert spawned_option_ids == []
  assert "codex-alpha" not in spawned_option_ids
  assert "codex-beta" not in spawned_option_ids
  errors = _assistant_errors(session_mgr, session.id)
  assert any("codex-ghost-9" in e["content"] for e in errors)


# --------------------------------------------------------------- replay path


@pytest.mark.asyncio
async def test_replay_runs_on_the_sessions_pinned_backend_not_backend_options_zero(
    tmp_path: Path, monkeypatch) -> None:
  """replay_user_message never passes backend_option — the only caller that
  doesn't. A resolvable pin that isn't backend_options[0] must still win."""
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
          BackendOption(id="codex-o3", label="Codex", type="codex", model="o3"),
      ],
  )
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Test Session"), backend="codex-o3")
  assert session.backend != cfg.backend_options[0].id

  captured: dict[str, object] = {}
  monkeypatch.setattr(
      "src.agents.backends.registry.build_backend",
      lambda option, cfg, **k: captured.update(option=option) or _FakeBackend())
  monkeypatch.setattr(master_cc_run, "_build_instructions_content", lambda session_meta, cfg, model=None: "instructions")

  user_event = {"id": "u1", "type": "user", "content": "unanswered message"}
  await master_cc.replay_user_message(cfg, session, user_event, session_mgr.callbacks())

  assert captured["option"].id == "codex-o3"


@pytest.mark.asyncio
async def test_replay_unresolvable_pin_hard_fails_not_substituted(tmp_path: Path, monkeypatch) -> None:
  """Replay of an unresolvable pin must reach the hard fail, same as the
  message and wake paths — never a substitution onto backend_options[0]."""
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
          BackendOption(id="codex-o3", label="Codex", type="codex", model="o3"),
      ],
  )
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Test Session"), backend="codex-ghost-9")

  spawned: list[object] = []
  monkeypatch.setattr("src.agents.backends.registry.build_backend",
                      lambda *a, **k: spawned.append(1) or _FakeBackend())
  monkeypatch.setattr(master_cc_run, "_build_instructions_content", lambda session_meta, cfg, model=None: "instructions")

  user_event = {"id": "u1", "type": "user", "content": "unanswered message"}
  await master_cc.replay_user_message(cfg, session, user_event, session_mgr.callbacks())

  assert spawned == []
  errors = _assistant_errors(session_mgr, session.id)
  assert any("codex-ghost-9" in e["content"] for e in errors)


# ------------------------------------------------------- factory default


@pytest.mark.asyncio
async def test_empty_pin_no_option_rejects_not_backend_options_zero(tmp_path: Path, monkeypatch) -> None:
  """No pin at all and no explicit option must hard-fail, not fall back to
  backend_options[0] (the wake-path fallback was removed)."""
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
          BackendOption(id="codex-o3", label="Codex", type="codex", model="o3"),
      ],
  )
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Test Session"))
  session.backend = ""

  spawned: list[object] = []
  monkeypatch.setattr(
      "src.agents.backends.registry.build_backend",
      lambda *a, **k: spawned.append(1) or _FakeBackend())
  monkeypatch.setattr(master_cc_run, "_build_instructions_content", lambda session_meta, cfg, model=None: "instructions")

  await run_and_finalize(cfg, session, "hello", session_mgr)

  assert spawned == []
  errors = _assistant_errors(session_mgr, session.id)
  assert errors
  assert any("no backend option" in e["content"] for e in errors)
