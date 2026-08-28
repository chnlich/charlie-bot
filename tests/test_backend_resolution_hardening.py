"""Backend resolution hardening: fresh config on wake, no silent substitution, safe resume."""

from __future__ import annotations

from datetime import UTC, datetime
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest
from conftest import (
  BROADCAST_PATCH_TARGET,
  BUILD_BACKEND_PATCH_TARGET,
  TRIGGER_MASTER_PATCH_TARGET,
  TRIGGERS_GET_CONFIG_PATCH_TARGET,
  FakeBackend,
  make_work_item,
  patch_instructions_content,
)

from src.agents import master_cc
from src.agents.backends import registry
from src.core import config as core_config
from src.core import event_types as ET
from src.core import models
from src.core.models import CreateSessionRequest, PendingTrigger
from src.core.sessions import SessionManager
from src.core.spawner import _resolve_session_default_backend_model
from src.core.triggers import TriggerManager


def _write_transcript(config_dir: Path, cc_session_id: str) -> None:
  project = config_dir / "projects" / "-home-user--charliebot-sessions-session-id"
  project.mkdir(parents=True, exist_ok=True)
  (project / f"{cc_session_id}.jsonl").write_text("{}\n", encoding="utf-8")


def test_load_config_reads_opencode_proxy_url_per_backend(tmp_path: Path, monkeypatch) -> None:
  home = tmp_path / "charliebot"
  home.mkdir()
  (home / "config.yaml").write_text(
      """
backend_options:
  - id: opencode-proxied
    label: Proxied OpenCode
    type: opencode
    model: provider/model
    opencode_proxy_url: http://proxy.test:8080
  - id: opencode-plain
    label: Plain OpenCode
    type: opencode
    model: provider/model
  - id: claude
    label: Claude
    type: cc-claude
    model: model
""",
      encoding="utf-8")
  monkeypatch.setenv(core_config.CHARLIEBOT_HOME_ENV, str(home))

  cfg = core_config.load_config()

  assert [option.opencode_proxy_url for option in cfg.backend_options] == ["http://proxy.test:8080", None, None]


def test_registry_scopes_opencode_proxy_to_opencode_constructor(monkeypatch) -> None:
  captured: dict[str, dict] = {}

  class _FakeOpenCodeBackend:

    def __init__(self, **kwargs):
      captured["opencode"] = kwargs

  class _FakeClaudeBackend:

    def __init__(self, **kwargs):
      captured["claude"] = kwargs

  monkeypatch.setattr(registry, "OpenCodeBackend", _FakeOpenCodeBackend)
  monkeypatch.setattr(registry, "ClaudeCodeBackend", _FakeClaudeBackend)
  cfg = core_config.CharlieBotConfig(charliebot_home=Path("/tmp/charliebot-test"))
  proxied = models.BackendOption(
      id="opencode-proxied",
      label="Proxied OpenCode",
      type="opencode",
      model="provider/model",
      opencode_proxy_url="http://proxy.test:8080",
  )
  plain = models.BackendOption(
      id="opencode-plain",
      label="Plain OpenCode",
      type="opencode",
      model="provider/model",
  )
  non_opencode = models.BackendOption(
      id="claude",
      label="Claude",
      type="cc-claude",
      model="model",
      opencode_proxy_url="http://proxy.test:8080",
  )

  registry.build_backend(proxied, cfg)
  assert captured["opencode"]["opencode_proxy_url"] == "http://proxy.test:8080"
  registry.build_backend(plain, cfg)
  assert captured["opencode"]["opencode_proxy_url"] is None
  registry.build_backend(non_opencode, cfg)
  assert "opencode_proxy_url" not in captured["claude"]


# --------------------------------------------------------------- config reload

def test_get_config_refreshes_in_place_keeping_identity(tmp_path: Path, monkeypatch) -> None:
  """A reload must update the existing instance so earlier holders see new values."""
  home = tmp_path / "home"
  (home / ".charliebot").mkdir(parents=True)
  cfg_path = home / ".charliebot" / "config.yaml"
  cfg_path.write_text("server_port: 1111\n", encoding="utf-8")
  monkeypatch.setattr(Path, "home", classmethod(lambda cls: home))
  monkeypatch.setattr(core_config, "_config", None)
  monkeypatch.setattr(core_config, "_config_mtime", 0.0)

  first = core_config.get_config()
  holder = first  # a long-lived singleton captures the object here
  assert first.server_port == 1111

  cfg_path.write_text("server_port: 2222\n", encoding="utf-8")
  import os
  os.utime(cfg_path, (0, 0))  # force a different mtime

  second = core_config.get_config()
  assert second is first
  assert holder.server_port == 2222


def test_get_config_keeps_previous_value_when_reload_fails(tmp_path: Path, monkeypatch) -> None:
  home = tmp_path / "home"
  (home / ".charliebot").mkdir(parents=True)
  cfg_path = home / ".charliebot" / "config.yaml"
  cfg_path.write_text("server_port: 1111\n", encoding="utf-8")
  monkeypatch.setattr(Path, "home", classmethod(lambda cls: home))
  monkeypatch.setattr(core_config, "_config", None)
  monkeypatch.setattr(core_config, "_config_mtime", 0.0)

  first = core_config.get_config()
  cfg_path.write_text("server_port: 2222\n", encoding="utf-8")
  import os
  os.utime(cfg_path, (0, 0))
  monkeypatch.setattr(core_config, "load_config", lambda: (_ for _ in ()).throw(ValueError("bad yaml")))

  second = core_config.get_config()
  assert second is first
  assert second.server_port == 1111


# ------------------------------------------------------------- trigger wake-up

@pytest.mark.asyncio
async def test_trigger_wake_uses_current_config_not_construction_snapshot(tmp_path: Path) -> None:
  """A backend added after the manager was constructed must reach trigger_master."""
  stale = core_config.CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  session_mgr = SessionManager(stale)
  session = await session_mgr.create_session(CreateSessionRequest(name="Trigger"))
  trigger_mgr = TriggerManager(stale, session_mgr)
  trigger = PendingTrigger(
      id="trigger-1",
      session_id=session.id,
      fire_at=datetime.now(UTC),
      message="wake",
  )
  await trigger_mgr._save_trigger(trigger)

  current = core_config.CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      backend_options=[models.BackendOption(id="added-later", label="New", type="cc-claude", model="m")],
  )
  with (
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()),
      patch(TRIGGER_MASTER_PATCH_TARGET, new=AsyncMock()) as mock_master,
      patch(TRIGGERS_GET_CONFIG_PATCH_TARGET, return_value=current),
  ):
    await trigger_mgr._wait_and_fire(trigger)

  passed_cfg = mock_master.await_args.args[2]
  assert passed_cfg is current
  assert passed_cfg.get_backend_option("added-later") is not None


# --------------------------------------------------------- no silent fallback

@pytest.mark.asyncio
async def test_run_cc_refuses_to_substitute_an_unknown_pinned_backend(tmp_path: Path, monkeypatch) -> None:
  cfg = core_config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[models.BackendOption(id="cc", label="CC", type="cc-claude", model="claude-fable-5")],
  )
  session_meta = models.SessionMetadata(id="session-id", name="S", backend="deleted-id",
                                        cc_session_id="conv-1")
  spawned: list[object] = []
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, lambda *a, **k: spawned.append(1) or FakeBackend())
  patch_instructions_content(monkeypatch)

  item = make_work_item(cfg, session_meta, None)
  cc_session_id, exit_code, error_msg, extras = await master_cc._run_cc(item)

  assert not spawned
  assert cc_session_id is None
  assert exit_code == 1
  assert "deleted-id" in error_msg and "refusing to substitute" in error_msg
  assert not extras
  events = [c.args[1] for c in item.callbacks.persist_and_broadcast.await_args_list]
  assert any(e["type"] == ET.ASSISTANT_ERROR and "deleted-id" in e["content"] for e in events)


@pytest.mark.asyncio
async def test_run_cc_refuses_no_option_and_no_pin(tmp_path: Path, monkeypatch) -> None:
  """Neither an explicit per-run option nor a session pin is a documented rejection:
  exit code 1, no backend started, an assistant error written."""
  cfg = core_config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[models.BackendOption(id="cc", label="CC", type="cc-claude", model="claude-fable-5")],
  )
  session_meta = models.SessionMetadata(id="session-id", name="S", backend="")
  spawned: list[object] = []
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, lambda *a, **k: spawned.append(1) or FakeBackend())
  patch_instructions_content(monkeypatch)

  item = make_work_item(cfg, session_meta, None)
  cc_session_id, exit_code, error_msg, extras = await master_cc._run_cc(item)

  assert not spawned
  assert cc_session_id is None
  assert exit_code == 1
  assert "no backend option" in error_msg and "backend_options[0]" in error_msg
  assert not extras
  events = [c.args[1] for c in item.callbacks.persist_and_broadcast.await_args_list]
  assert any(e["type"] == ET.ASSISTANT_ERROR and "no backend option" in e["content"] for e in events)


def test_spawner_refuses_to_substitute_an_unknown_pinned_backend() -> None:
  cfg = core_config.CharlieBotConfig(
      backend_options=[models.BackendOption(id="cc", label="CC", type="cc-claude", model="claude-fable-5")])
  session_meta = models.SessionMetadata(id="s", name="S", backend="deleted-id")
  with pytest.raises(ValueError, match="refusing to substitute"):
    _resolve_session_default_backend_model(cfg, session_meta)


def test_spawner_defaults_when_session_pins_no_backend() -> None:
  cfg = core_config.CharlieBotConfig(
      backend_options=[models.BackendOption(id="cc", label="CC", type="cc-claude", model="claude-fable-5")])
  session_meta = models.SessionMetadata(id="s", name="S", backend="")
  assert _resolve_session_default_backend_model(cfg, session_meta) == ("cc", "claude-fable-5")


# ------------------------------------------------------------ resume guarding

def test_claude_config_dir_prefers_option_then_env_then_home(monkeypatch) -> None:
  opt = models.BackendOption(id="a", label="A", type="cc-claude", model="m",
                             claude_config_dir="~/.claude-ext-1")
  assert core_config.claude_config_dir(opt) == Path.home() / ".claude-ext-1"

  bare = models.BackendOption(id="b", label="B", type="cc-claude", model="m")
  monkeypatch.setenv("CLAUDE_CONFIG_DIR", "/tmp/env-claude")
  assert core_config.claude_config_dir(bare) == Path("/tmp/env-claude")

  monkeypatch.delenv("CLAUDE_CONFIG_DIR", raising=False)
  assert core_config.claude_config_dir(bare) == Path.home() / ".claude"


def test_cc_transcript_exists_ignores_subagent_logs(tmp_path: Path) -> None:
  cfg_dir = tmp_path / ".claude-ext-1"
  _write_transcript(cfg_dir, "conv-1")
  nested = cfg_dir / "projects" / "-slug" / "parent-uuid" / "subagents"
  nested.mkdir(parents=True)
  (nested / "agent-deep.jsonl").write_text("{}\n", encoding="utf-8")

  assert master_cc._cc_transcript_exists(cfg_dir, "conv-1") is True
  assert master_cc._cc_transcript_exists(cfg_dir, "agent-deep") is False
  assert master_cc._cc_transcript_exists(cfg_dir, "absent") is False


@pytest.mark.asyncio
async def test_run_cc_drops_resume_when_transcript_is_in_another_account_dir(
    tmp_path: Path, monkeypatch) -> None:
  other = tmp_path / ".claude-ext-1"
  _write_transcript(other, "conv-1")
  target = tmp_path / ".claude"
  (target / "projects").mkdir(parents=True)

  cfg = core_config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[models.BackendOption(id="cc", label="CC", type="cc-claude",
                                            model="claude-fable-5", claude_config_dir=str(target))],
  )
  session_meta = models.SessionMetadata(id="session-id", name="S", backend="cc", cc_session_id="conv-1")
  captures: dict[str, object] = {}
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, lambda option, cfg, **k: captures.update(kwargs=k) or FakeBackend())
  patch_instructions_content(monkeypatch)

  item = make_work_item(cfg, session_meta, cfg.backend_options[0])
  _cc, exit_code, error_msg, _extras = await master_cc._run_cc(item)

  assert exit_code == 0 and error_msg is None
  assert "--resume" not in (captures["kwargs"]["extra_flags"] or [])
  events = [c.args[1] for c in item.callbacks.persist_and_broadcast.await_args_list]
  dropped = [e for e in events if e["type"] == ET.RESUME_CONTEXT_DROPPED]
  assert len(dropped) == 1
  assert dropped[0]["reason"] == "transcript_missing"


@pytest.mark.asyncio
async def test_run_cc_keeps_resume_when_transcript_is_present(tmp_path: Path, monkeypatch) -> None:
  target = tmp_path / ".claude-ext-1"
  _write_transcript(target, "conv-1")

  cfg = core_config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[models.BackendOption(id="cc", label="CC", type="cc-claude",
                                            model="claude-fable-5", claude_config_dir=str(target))],
  )
  session_meta = models.SessionMetadata(id="session-id", name="S", backend="cc", cc_session_id="conv-1")
  captures: dict[str, object] = {}
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, lambda option, cfg, **k: captures.update(kwargs=k) or FakeBackend())
  patch_instructions_content(monkeypatch)

  item = make_work_item(cfg, session_meta, cfg.backend_options[0])
  await master_cc._run_cc(item)

  extra_flags = captures["kwargs"]["extra_flags"] or []
  assert extra_flags[:2] == ["--resume", "conv-1"]
  events = [c.args[1] for c in item.callbacks.persist_and_broadcast.await_args_list]
  assert not [e for e in events if e["type"] == ET.RESUME_CONTEXT_DROPPED]


def test_resume_context_dropped_renders_backend_neutral_by_reason() -> None:
  from src.core.message_aggregator import _SIMPLE_HANDLERS
  anchor = _SIMPLE_HANDLERS[ET.RESUME_CONTEXT_DROPPED](
      {"type": ET.RESUME_CONTEXT_DROPPED, "reason": "anchor_missing"})
  assert anchor["role"] == "system"
  assert "anchor" in anchor["content"].lower()
  assert "claude" not in anchor["content"].lower()

  transcript = _SIMPLE_HANDLERS[ET.RESUME_CONTEXT_DROPPED](
      {"type": ET.RESUME_CONTEXT_DROPPED, "reason": "transcript_missing"})
  assert transcript["role"] == "system"
  assert "transcript" in transcript["content"].lower()
  assert "claude" not in transcript["content"].lower()
