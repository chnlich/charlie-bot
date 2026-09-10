"""Backend resolution hardening: fresh config on wake, no silent substitution, safe resume."""

from __future__ import annotations

from datetime import UTC, datetime
from pathlib import Path
from unittest.mock import patch

import pytest
from conftest import (
    BUILD_BACKEND_PATCH_TARGET,
    TRIGGERS_GET_CONFIG_PATCH_TARGET,
    FakeBackend,
    backend_option,
    make_home_config,
    make_work_item,
    patch_instructions_content,
    patch_trigger_mocks,
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


def test_load_config_reads_proxy_url_per_backend(tmp_path: Path, monkeypatch) -> None:
  home = tmp_path / "charliebot"
  home.mkdir()
  (home / "config.yaml").write_text(
      """
backends:
  options:
    - id: opencode-proxied
      label: Proxied OpenCode
      type: opencode
      model: provider/model
      proxy_url: http://proxy.test:8080
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

  proxied, plain, claude = cfg.backends.options
  assert proxied.proxy_url == "http://proxy.test:8080"
  assert plain.proxy_url is None
  assert claude.id == "claude"


def test_registry_scopes_opencode_proxy_to_opencode_constructor(monkeypatch) -> None:
  captured: dict[str, dict] = {}

  class _FakeOpenCodeBackend:

    def __init__(self, **kwargs) -> None:
      captured["opencode"] = kwargs

  monkeypatch.setattr(registry, "OpenCodeBackend", _FakeOpenCodeBackend)
  cfg = core_config.CharlieBotConfig(charliebot_home=Path("/tmp/charliebot-test"))
  proxied = backend_option(
      id="opencode-proxied",
      label="Proxied OpenCode",
      type="opencode",
      model="provider/model",
      proxy_url="http://proxy.test:8080",
  )
  plain = backend_option(
      id="opencode-plain",
      label="Plain OpenCode",
      type="opencode",
      model="provider/model",
  )

  registry.build_backend(proxied, cfg)
  assert captured["opencode"]["proxy_url"] == "http://proxy.test:8080"
  registry.build_backend(plain, cfg)
  assert captured["opencode"]["proxy_url"] is None


# --------------------------------------------------------------- config reload


def test_get_config_refreshes_in_place_keeping_identity(tmp_path: Path, monkeypatch) -> None:
  """A reload must update the existing instance so earlier holders see new values."""
  home = tmp_path / "home"
  (home / ".charliebot").mkdir(parents=True)
  cfg_path = home / ".charliebot" / "config.yaml"
  cfg_path.write_text("server:\n  port: 1111\n", encoding="utf-8")
  monkeypatch.setattr(Path, "home", classmethod(lambda cls: home))
  # The test asserts default-home resolution under the patched Path.home, so the suite-wide profile variable is cleared.
  monkeypatch.delenv(core_config.CHARLIEBOT_HOME_ENV, raising=False)
  monkeypatch.setattr(core_config, "_home_cache", {})

  first = core_config.get_config()
  holder = first  # a long-lived singleton captures the object here
  assert first.server.port == 1111

  cfg_path.write_text("server:\n  port: 2222\n", encoding="utf-8")
  import os
  os.utime(cfg_path, (0, 0))  # force a different mtime

  second = core_config.get_config()
  assert second is first
  assert holder.server.port == 2222


def test_get_config_keeps_previous_value_when_reload_fails(tmp_path: Path, monkeypatch) -> None:
  home = tmp_path / "home"
  (home / ".charliebot").mkdir(parents=True)
  cfg_path = home / ".charliebot" / "config.yaml"
  cfg_path.write_text("server:\n  port: 1111\n", encoding="utf-8")
  monkeypatch.setattr(Path, "home", classmethod(lambda cls: home))
  # The test asserts default-home resolution under the patched Path.home, so the suite-wide profile variable is cleared.
  monkeypatch.delenv(core_config.CHARLIEBOT_HOME_ENV, raising=False)
  monkeypatch.setattr(core_config, "_home_cache", {})

  first = core_config.get_config()
  cfg_path.write_text("server:\n  port: 2222\n", encoding="utf-8")
  import os
  os.utime(cfg_path, (0, 0))
  monkeypatch.setattr(core_config, "load_config", lambda: (_ for _ in ()).throw(ValueError("bad yaml")))

  second = core_config.get_config()
  assert second is first
  assert second.server.port == 1111


# ------------------------------------------------------------- trigger wake-up


@pytest.mark.asyncio
async def test_trigger_wake_uses_current_config_not_construction_snapshot(tmp_path: Path) -> None:
  """A backend added after the manager was constructed must reach trigger_master."""
  stale = make_home_config(tmp_path)
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
      backends={"options": [backend_option(id="added-later", label="New", type="cc-claude", model="m")]},
  )
  with (
      patch_trigger_mocks() as mock_master,
      patch(TRIGGERS_GET_CONFIG_PATCH_TARGET, return_value=current),
  ):
    await trigger_mgr._wait_and_fire(trigger)

  passed_cfg = mock_master.await_args.args[2]
  assert passed_cfg is current
  assert passed_cfg.get_backend_option("added-later") is not None


# --------------------------------------------------------- no silent fallback

# Rows are the two ways a session backend fails to resolve: a pin naming no
# configured option, and no pin at all. Both must hard-fail identically —
# no backend started, exit code 1, one assistant error — rather than
# substitute a different backend.
_REFUSAL_ROWS = [
    pytest.param("deleted-id", ("deleted-id", "refusing to substitute"), id="unknown-pin"),
    pytest.param("", ("no backend option", "backends.options[0]"), id="no-pin"),
]


@pytest.mark.asyncio
@pytest.mark.parametrize("session_backend, error_fragments", _REFUSAL_ROWS)
async def test_run_cc_refuses_to_substitute_an_unresolvable_session_backend(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    session_backend: str,
    error_fragments: tuple[str, str],
) -> None:
  """A session backend the config cannot resolve rejects the run instead of
  substituting another option: exit code 1, no backend started, one assistant
  error naming the cause."""
  cfg = core_config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backends={"options": [backend_option(id="cc", label="CC", type="cc-claude", model="claude-fable-5")]},
  )
  session_meta = models.SessionMetadata(id="session-id", name="S", backend=session_backend)
  spawned: list[object] = []
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, lambda *a, **k: spawned.append(1) or FakeBackend())
  patch_instructions_content(monkeypatch)

  item = make_work_item(cfg, session_meta, None)
  cc_session_id, exit_code, error_msg, extras = await master_cc._run_cc(item)

  assert not spawned
  assert cc_session_id is None
  assert exit_code == 1
  assert all(fragment in error_msg for fragment in error_fragments)
  assert not extras
  events = [c.args[1] for c in item.callbacks.persist_and_broadcast.await_args_list]
  assert any(e["type"] == ET.ASSISTANT_ERROR and error_fragments[0] in e["content"] for e in events)


def test_spawner_refuses_to_substitute_an_unknown_pinned_backend() -> None:
  cfg = core_config.CharlieBotConfig(
      backends={"options": [backend_option(id="cc", label="CC", type="cc-claude", model="claude-fable-5")]})
  session_meta = models.SessionMetadata(id="s", name="S", backend="deleted-id")
  with pytest.raises(ValueError, match="refusing to substitute"):
    _resolve_session_default_backend_model(cfg, session_meta)


def test_spawner_defaults_when_session_pins_no_backend() -> None:
  cfg = core_config.CharlieBotConfig(
      backends={"options": [backend_option(id="cc", label="CC", type="cc-claude", model="claude-fable-5")]})
  session_meta = models.SessionMetadata(id="s", name="S", backend="")
  assert _resolve_session_default_backend_model(cfg, session_meta) == ("cc", "claude-fable-5")


# ------------------------------------------------------------ resume guarding


def test_cc_transcript_exists_ignores_subagent_logs(tmp_path: Path) -> None:
  cfg_dir = tmp_path / ".claude-ext-1"
  _write_transcript(cfg_dir, "conv-1")
  nested = cfg_dir / "projects" / "-slug" / "parent-uuid" / "subagents"
  nested.mkdir(parents=True)
  (nested / "agent-deep.jsonl").write_text("{}\n", encoding="utf-8")

  assert master_cc._cc_transcript_exists(cfg_dir, "conv-1") is True
  assert master_cc._cc_transcript_exists(cfg_dir, "agent-deep") is False
  assert master_cc._cc_transcript_exists(cfg_dir, "absent") is False


# Rows are the transcript-reachability gate's two outcomes for a non-pooled
# cc-claude option, whose login dir the gate reads from $CLAUDE_CONFIG_DIR.
# --resume survives only when the anchor's transcript exists in that dir; a
# transcript under any other login's dir drops the resume context instead of
# resuming a foreign session.
_TRANSCRIPT_ROWS = [
    pytest.param(False, id="transcript-in-another-login-dir"),
    pytest.param(True, id="transcript-in-configured-dir"),
]


@pytest.mark.asyncio
@pytest.mark.parametrize("transcript_in_configured_dir", _TRANSCRIPT_ROWS)
async def test_run_cc_resume_gate_by_transcript_location(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    transcript_in_configured_dir: bool,
) -> None:
  """The anchor's --resume survives only when its transcript exists in the
  configured login dir; otherwise the resume context drops with reason
  transcript_missing and the run proceeds without it."""
  configured = tmp_path / ".claude-configured"
  elsewhere = tmp_path / ".claude-elsewhere"
  _write_transcript(elsewhere, "conv-1")
  if transcript_in_configured_dir:
    _write_transcript(configured, "conv-1")
  monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(configured))

  cfg = core_config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backends={"options": [backend_option(id="cc", label="CC", type="cc-claude", model="claude-fable-5")]},
  )
  session_meta = models.SessionMetadata(id="session-id", name="S", backend="cc", cc_session_id="conv-1")
  captures: dict[str, object] = {}
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, lambda option, cfg, **k: captures.update(kwargs=k) or FakeBackend())
  patch_instructions_content(monkeypatch)

  item = make_work_item(cfg, session_meta, cfg.backends.options[0])
  _cc, exit_code, error_msg, _extras = await master_cc._run_cc(item)

  assert exit_code == 0 and error_msg is None
  extra_flags = captures["kwargs"]["extra_flags"] or []
  events = [c.args[1] for c in item.callbacks.persist_and_broadcast.await_args_list]
  dropped = [e for e in events if e["type"] == ET.RESUME_CONTEXT_DROPPED]
  if transcript_in_configured_dir:
    assert extra_flags[:2] == ["--resume", "conv-1"]
    assert dropped == []
  else:
    assert "--resume" not in extra_flags
    assert [d["reason"] for d in dropped] == ["transcript_missing"]


def test_resume_context_dropped_renders_backend_neutral_by_reason() -> None:
  from src.core.message_aggregator import _SIMPLE_HANDLERS
  anchor = _SIMPLE_HANDLERS[ET.RESUME_CONTEXT_DROPPED]({"type": ET.RESUME_CONTEXT_DROPPED, "reason": "anchor_missing"})
  assert anchor["role"] == "system"
  assert "anchor" in anchor["content"].lower()
  assert "claude" not in anchor["content"].lower()

  transcript = _SIMPLE_HANDLERS[ET.RESUME_CONTEXT_DROPPED](
      {
          "type": ET.RESUME_CONTEXT_DROPPED,
          "reason": "transcript_missing"
      })
  assert transcript["role"] == "system"
  assert "transcript" in transcript["content"].lower()
  assert "claude" not in transcript["content"].lower()
