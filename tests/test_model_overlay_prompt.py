"""Overlay declared by the backend_option, judged on the wake path.

``BackendOption.prompt_overlay`` names the fence file under
``prompts/model_overlays/`` (sans ``.md``); the literal ``none`` means
explicitly no overlay; a missing key means undeclared. Both undeclared and a
declared-but-unreadable overlay degrade to a fenceless wake plus one unified
``backend_overlay_inactive`` alert event, told apart by its ``reason`` field —
an ``OSError``/``UnicodeDecodeError`` read failure never raises through the
wake. These tests assert the mechanism at the wake-path layer with synthetic
``BackendOption``s and synthetic overlay files — never the real overlay or any
deployment name.
"""

from __future__ import annotations

import asyncio
from pathlib import Path

import pytest
from conftest import mock_session_callbacks

from src.agents import master_cc
from src.agents.backends import base as backend_base
from src.agents.backends import registry
from src.core import config as core_config
from src.core import event_types as ET
from src.core.message_aggregator import MessageAggregator
from src.core.models import BackendOption, SessionMetadata


class _FakeBackend:
  exit_code = 0
  stderr_text = ""
  terminated = False

  async def terminate(self) -> None:
    self.terminated = True

  async def run(self, prompt: str, cwd: str, env: dict):
    yield backend_base.make_result_event()


def _wake_cfg(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> core_config.CharlieBotConfig:
  """A config whose ``charlie_bot_repo`` points at a synthetic tmp repo dir."""
  repo = tmp_path / "repo"
  (repo / "prompts").mkdir(parents=True)
  (repo / "prompts" / "master.md").write_text("BASE PROMPT", encoding="utf-8")
  home = tmp_path / "home"
  (home / "memory" / "entries").mkdir(parents=True)
  (home / "memory" / "topics").write_text("profile resident\n", encoding="utf-8")
  cfg = core_config.CharlieBotConfig(
      charliebot_home=home,
      backend_options=[BackendOption(id="fake", label="Fake", type="codex")],
  )
  monkeypatch.setattr(core_config.CharlieBotConfig, "charlie_bot_repo", property(lambda self: repo))
  return cfg


def _overlay_dir(cfg: core_config.CharlieBotConfig) -> Path:
  return cfg.charlie_bot_repo / "prompts" / "model_overlays"


def _item(cfg, session_meta: SessionMetadata, backend_option: BackendOption) -> master_cc._WorkItem:
  return master_cc._WorkItem(
      cfg=cfg,
      session_meta=session_meta,
      user_content="hello",
      callbacks=mock_session_callbacks(),
      is_voice=False,
      auto_trigger=False,
      backend_option=backend_option,
      extra_claude_flags=None,
      should_check_tex=False,
      future=asyncio.get_running_loop().create_future(),
  )


def _rendered_overlay_alert(event: dict) -> list[dict]:
  """Feed a persisted event through the aggregator; return visible message deltas."""
  return [
      delta["message"] for delta in MessageAggregator().feed_all([event])
      if delta.get("type") == "message" and delta.get("message", {}).get("role") == "system"
  ]


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "prompt_overlay, file_exists, expected_product, expects_alert",
    [
        ("synthetic_overlay", True, "BASE PROMPT\n\nOVERLAY BODY", False),
        ("none", False, "BASE PROMPT", False),
        (None, False, "BASE PROMPT", True),
    ],
)
async def test_wake_path_overlay_four_states(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    prompt_overlay: str | None,
    file_exists: bool,
    expected_product: str,
    expects_alert: bool,
) -> None:
  """Acceptance A: the overlay product and alert are decided on the wake path."""
  cfg = _wake_cfg(tmp_path, monkeypatch)
  if file_exists:
    overlay_dir = _overlay_dir(cfg)
    overlay_dir.mkdir(parents=True, exist_ok=True)
    (overlay_dir / "synthetic_overlay.md").write_text("OVERLAY BODY", encoding="utf-8")

  option = BackendOption(id="fake", label="Fake", type="codex", model="ignored/model", prompt_overlay=prompt_overlay)
  captured: dict[str, object] = {}
  monkeypatch.setattr(
      registry, "build_backend",
      lambda *a, **kw: captured.update(instructions_content=kw.get("instructions_content")) or _FakeBackend())

  item = _item(cfg, SessionMetadata(id="s", name="S", backend="fake"), option)
  cc_session_id, exit_code, error_msg, _extras = await master_cc._run_cc(item)

  assert cc_session_id is None
  assert exit_code == 0
  assert error_msg is None
  assert captured["instructions_content"] == expected_product

  if expects_alert:
    alert_events = [
        c.args[1] for c in item.callbacks.persist_and_broadcast.await_args_list
        if c.args[1].get("type") == ET.BACKEND_OVERLAY_INACTIVE
    ]
    assert len(alert_events) == 1
    assert alert_events[0]["backend"] == "fake"
    assert alert_events[0]["reason"] == "undeclared"
    # Assert at the aggregated/rendered message level, not event persistence.
    rendered = _rendered_overlay_alert(alert_events[0])
    assert len(rendered) == 1
    assert "fake" in rendered[0]["content"]
    assert "prompt_overlay" in rendered[0]["content"]
  else:
    assert not [
        c.args[1] for c in item.callbacks.persist_and_broadcast.await_args_list
        if c.args[1].get("type") == ET.BACKEND_OVERLAY_INACTIVE
    ]


@pytest.mark.asyncio
async def test_declared_overlay_missing_file_degrades(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A declared overlay whose file is missing degrades to a fenceless wake.

  The wake succeeds (no raise), the instructions are the base prompt only, and
  exactly one unified backend_overlay_inactive event lands with
  reason=unreadable plus the overlay name and the exception class name.
  """
  cfg = _wake_cfg(tmp_path, monkeypatch)
  option = BackendOption(id="fake", label="Fake", type="codex", prompt_overlay="missing_overlay")
  captured: dict[str, object] = {}
  monkeypatch.setattr(
      registry, "build_backend",
      lambda *a, **kw: captured.update(instructions_content=kw.get("instructions_content")) or _FakeBackend())

  item = _item(cfg, SessionMetadata(id="s", name="S", backend="fake"), option)
  _cc_session_id, exit_code, error_msg, _extras = await master_cc._run_cc(item)

  assert exit_code == 0
  assert error_msg is None
  assert captured["instructions_content"] == "BASE PROMPT"

  alert_events = [
      c.args[1] for c in item.callbacks.persist_and_broadcast.await_args_list
      if c.args[1].get("type") == ET.BACKEND_OVERLAY_INACTIVE
  ]
  assert len(alert_events) == 1
  event = alert_events[0]
  assert event["backend"] == "fake"
  assert event["reason"] == "unreadable"
  assert event["overlay"] == "missing_overlay"
  assert event["error"] == "FileNotFoundError"

  # Assert at the aggregated/rendered message level, not event persistence.
  rendered = _rendered_overlay_alert(event)
  assert len(rendered) == 1
  assert rendered[0]["content"] == (
      "Backend fake: prompt_overlay 'missing_overlay' could not be loaded "
      "(FileNotFoundError) — running without a fence")


def test_legacy_overlay_undeclared_event_renders_as_undeclared() -> None:
  """A persisted legacy backend_overlay_undeclared event feeds through the
  aggregator and renders byte-identically to the undeclared message."""
  legacy_event = {"type": "backend_overlay_undeclared", "backend": "legacy-fake"}
  rendered = _rendered_overlay_alert(legacy_event)
  assert len(rendered) == 1
  assert rendered[0]["content"] == "Backend legacy-fake declares no prompt_overlay — running without a fence"


@pytest.mark.asyncio
async def test_model_string_has_zero_impact_on_wake_path(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Acceptance B: two options differing only in model share a byte-identical product."""
  cfg = _wake_cfg(tmp_path, monkeypatch)
  overlay_dir = _overlay_dir(cfg)
  overlay_dir.mkdir(parents=True, exist_ok=True)
  (overlay_dir / "shared.md").write_text("OVERLAY BODY", encoding="utf-8")

  products: list[str | None] = []
  monkeypatch.setattr(
      registry, "build_backend",
      lambda *a, **kw: products.append(kw.get("instructions_content")) or _FakeBackend())

  for model in ("vendor/one", "vendor/two"):
    option = BackendOption(id="opt", label="Opt", type="codex", model=model, prompt_overlay="shared")
    await master_cc._run_cc(_item(cfg, SessionMetadata(id="s", name="S"), option))

  assert products[0] is not None
  assert products[0] == products[1] == "BASE PROMPT\n\nOVERLAY BODY"