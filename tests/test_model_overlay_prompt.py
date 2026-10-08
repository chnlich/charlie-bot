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

import pathlib

import conftest
import pytest

from src.infra import config as core_config
from src.infra import event_types as ET
from src.infra import models
from src.runtime import master_cc, message_aggregator
from src.runtime.hooks import backend_types


def _wake_cfg(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> core_config.CharlieBotConfig:
  """A config whose ``charlie_bot_repo`` points at a synthetic tmp repo dir."""
  repo = tmp_path / "repo"
  (repo / "prompts").mkdir(parents=True)
  (repo / "prompts" / "master.md").write_text("BASE PROMPT", encoding="utf-8")
  (repo / "prompts" / "manager_workflows.md").write_text("MANAGER WORKFLOWS PROMPT", encoding="utf-8")
  (repo / "prompts" / "thread_session.md").write_text("THREAD SESSION PROMPT", encoding="utf-8")
  home = tmp_path / "home"
  (home / "memory" / "entries").mkdir(parents=True)
  (home / "memory" / "topics").write_text("profile resident\n", encoding="utf-8")
  cfg = core_config.CharlieBotConfig(
      charliebot_home=home,
      backends={"options": [conftest.backend_option(id="fake", label="Fake", type="codex", model="unused/model")]},
  )
  monkeypatch.setattr(core_config.CharlieBotConfig, "charlie_bot_repo", property(lambda self: repo))
  return cfg


def _overlay_dir(cfg: core_config.CharlieBotConfig) -> pathlib.Path:
  return cfg.charlie_bot_repo / "prompts" / "model_overlays"


def _rendered_overlay_alert(event: dict) -> list[dict]:
  """Feed a persisted event through the aggregator; return visible message deltas."""
  return [
      delta["message"]
      for delta in message_aggregator.MessageAggregator().feed(event)
      if delta.get("type") == "message" and delta.get("message", {}).get("role") == "system"
  ]


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("prompt_overlay", "file_exists", "expected_product", "expects_alert"),
    [
        ("synthetic_overlay", True, "BASE PROMPT\n\nMANAGER WORKFLOWS PROMPT\n\nOVERLAY BODY", False),
        ("none", False, "BASE PROMPT\n\nMANAGER WORKFLOWS PROMPT", False),
        (None, False, "BASE PROMPT\n\nMANAGER WORKFLOWS PROMPT", True),
    ],
)
async def test_wake_path_overlay_four_states(
    tmp_path: pathlib.Path,
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

  option = conftest.backend_option(
      id="fake", label="Fake", type="codex", model="ignored/model", prompt_overlay=prompt_overlay)
  captured: dict[str, object] = {}
  monkeypatch.setattr(
      backend_types, "build_backend",
      lambda *a, **kw: captured.update(instructions_content=kw.get("instructions_content")) or conftest.FakeBackend())

  item = conftest.make_work_item(cfg, models.SessionMetadata(id="s", name="S", backend="fake"), option)
  cc_session_id, exit_code, error_msg, _extras = await master_cc.master_cc_run._run_cc(item)

  assert cc_session_id is None
  assert exit_code == 0
  assert error_msg is None
  assert captured["instructions_content"] == expected_product

  if expects_alert:
    alert_events = [
        c.args[1]
        for c in item.callbacks.persist_and_broadcast.await_args_list
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
        c.args[1]
        for c in item.callbacks.persist_and_broadcast.await_args_list
        if c.args[1].get("type") == ET.BACKEND_OVERLAY_INACTIVE
    ]
