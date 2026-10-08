"""Backend resolution hardening: fresh config on wake, no silent substitution, safe resume."""

from __future__ import annotations

import pathlib

import conftest
import pytest

from src.backends.claude_code import claude_lifecycle
from src.infra import config as core_config
from src.infra import event_types as ET
from src.infra import models
from src.runtime import master_cc, spawner


def _write_transcript(config_dir: pathlib.Path, cc_session_id: str) -> None:
  project = config_dir / "projects" / "-home-user--charliebot-sessions-session-id"
  project.mkdir(parents=True, exist_ok=True)
  (project / f"{cc_session_id}.jsonl").write_text("{}\n", encoding="utf-8")


# --------------------------------------------------------------- config reload


def _reload_rig(home: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> pathlib.Path:
  """Default home (the ``path_home`` fixture's) carrying config.yaml (port 1111); returns the config path.

  Both reload tests assert default-home resolution under the fixture's patched
  Path.home, so the suite-wide profile variable that the autouse fixture sets is
  deleted here.
  """
  (home / ".charliebot").mkdir(parents=True)
  cfg_path = home / ".charliebot" / "config.yaml"
  cfg_path.write_text("server:\n  port: 1111\n", encoding="utf-8")
  monkeypatch.delenv(core_config.CHARLIEBOT_HOME_ENV, raising=False)
  # The home cache lives in src.infra.home; config re-exports the name, but the
  # resolver reads its own module's global, so the reset must target the owner.
  from src.infra import home as core_home
  monkeypatch.setattr(core_home, "_home_cache", {})
  return cfg_path


def _write_port_2222(cfg_path: pathlib.Path) -> None:
  import os

  cfg_path.write_text("server:\n  port: 2222\n", encoding="utf-8")
  os.utime(cfg_path, (0, 0))  # force a different mtime


def test_get_config_refreshes_in_place_keeping_identity(
    path_home: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A reload must update the existing instance so earlier holders see new values."""
  cfg_path = _reload_rig(path_home, monkeypatch)

  first = core_config.get_config()
  holder = first  # a long-lived singleton captures the object here
  assert first.server.port == 1111

  _write_port_2222(cfg_path)

  second = core_config.get_config()
  assert second is first
  assert holder.server.port == 2222


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
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
    session_backend: str,
    error_fragments: tuple[str, str],
) -> None:
  """A session backend the config cannot resolve rejects the run instead of
  substituting another option: exit code 1, no backend started, one assistant
  error naming the cause."""
  cfg = core_config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backends={"options": [conftest.backend_option(id="cc", label="CC", type="cc-claude", model="claude-fable-5")]},
  )
  session_meta = models.SessionMetadata(profile="manager", id="session-id", name="S", backend=session_backend)
  spawned: list[object] = []
  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, lambda *a, **k: spawned.append(1) or conftest.FakeBackend())

  item = conftest.make_work_item(cfg, session_meta, None)
  cc_session_id, exit_code, error_msg, extras = await master_cc.master_cc_run._run_cc(item)

  assert not spawned
  assert cc_session_id is None
  assert exit_code == 1
  assert all(fragment in error_msg for fragment in error_fragments)
  assert not extras
  events = [c.args[1] for c in item.callbacks.persist_and_broadcast.await_args_list]
  assert any(e["type"] == ET.ASSISTANT_ERROR and error_fragments[0] in e["content"] for e in events)


def test_spawner_refuses_to_substitute_an_unknown_pinned_backend() -> None:
  cfg = core_config.CharlieBotConfig(
      backends={"options": [conftest.backend_option(id="cc", label="CC", type="cc-claude", model="claude-fable-5")]})
  session_meta = models.SessionMetadata(profile="manager", id="s", name="S", backend="deleted-id")
  with pytest.raises(ValueError, match="refusing to substitute"):
    spawner.spawner_backends._resolve_session_default_backend_model(cfg, session_meta)


# ------------------------------------------------------------ resume guarding


def test_cc_transcript_exists_ignores_subagent_logs(tmp_path: pathlib.Path) -> None:
  cfg_dir = tmp_path / ".claude-ext-1"
  _write_transcript(cfg_dir, "conv-1")
  nested = cfg_dir / "projects" / "-slug" / "parent-uuid" / "subagents"
  nested.mkdir(parents=True)
  (nested / "agent-deep.jsonl").write_text("{}\n", encoding="utf-8")

  assert claude_lifecycle.cc_transcript_exists(cfg_dir, "conv-1") is True
  assert claude_lifecycle.cc_transcript_exists(cfg_dir, "agent-deep") is False
  assert claude_lifecycle.cc_transcript_exists(cfg_dir, "absent") is False


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
    tmp_path: pathlib.Path,
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
      backends={"options": [conftest.backend_option(id="cc", label="CC", type="cc-claude", model="claude-fable-5")]},
  )
  session_meta = models.SessionMetadata(profile="manager", id="session-id", name="S", backend="cc", cc_session_id="conv-1")
  captures: dict[str, object] = {}
  monkeypatch.setattr(
      conftest.BUILD_BACKEND_PATCH_TARGET, lambda option, cfg, **k: captures.update(kwargs=k) or conftest.FakeBackend())

  item = conftest.make_work_item(cfg, session_meta, cfg.backends.options[0])
  _cc, exit_code, error_msg, _extras = await master_cc.master_cc_run._run_cc(item)

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
