"""model_fallback_notice: turn-end served-model attribution.

Covers plan v7 §4.2: the pure family detector, the family decision table
(plus its single-suffix-strip regression guard), the aggregator render
mapping, and the two turn-end call sites (live re-read and re-attach
projection reuse).
"""

from __future__ import annotations

import json
import pathlib

import conftest
import pytest

from src.agents.backends import claude_code
from src.core import config, models, runs, sessions
from src.core import event_types as ET

CONFIGURED = "claude-fable-5-1"
FABLE_OPTION = conftest.backend_option(
    id="claude-fable-5.1", label="Fable", type="cc-claude", model=CONFIGURED, prompt_overlay="none")


def _assistant(
    model: str | None,
    text: str | None = "reply",
    *,
    blocks: list[dict] | None = None,
    parent_tool_use_id: str | None = None,
) -> dict:
  """A claude-shape assistant event; text=None builds a message with no text block."""
  content = blocks if blocks is not None else ([{"type": "text", "text": text}] if text is not None else [])
  message: dict = {"content": content}
  if model is not None:
    message["model"] = model
  event: dict = {"type": ET.ASSISTANT, "message": message}
  if parent_tool_use_id:
    event["parent_tool_use_id"] = parent_tool_use_id
  return event


def _result(**extra: object) -> dict:
  return {"type": ET.RESULT, "result": "", "usage": {"input_tokens": 10, "output_tokens": 5}, **extra}


# ---------------------------------------------------------------------------
# Detector (pure function over parsed event dicts)
# ---------------------------------------------------------------------------


def test_healthy_fable_round_detects_nothing() -> None:
  """(a) Both healthy fable spellings plus a haiku modelUsage key stay silent."""
  events = [
      _assistant("claude-fable-5-1", "Plan looks good."),
      _assistant("claude-fable-5", "Here is the summary."),
      # Subagent (background) text: excluded by parent_tool_use_id.
      _assistant("claude-haiku-4-5-20251001", "subagent reply", parent_tool_use_id="toolu_01"),
      # Healthy rounds routinely carry a haiku usage-tier key — never read here.
      _result(modelUsage={
          "claude-haiku-4-5-20251001": {
              "input_tokens": 1
          },
          "claude-fable-5-1": {
              "input_tokens": 2
          },
      }),
  ]
  assert claude_code.out_of_family_served_models(events, CONFIGURED) == []


def test_pure_and_mixed_out_of_family_rounds_detect_served_models() -> None:
  """(b) Pure opus rounds and fable/opus mixed rounds surface the served model."""
  pure = [_assistant("claude-opus-4-8", "a"), _assistant("claude-opus-4-8", "b"), _result()]
  assert claude_code.out_of_family_served_models(pure, CONFIGURED) == ["claude-opus-4-8"]

  # Cross-kind split: fable-5-1 text and opus-4-8 text both serve the visible
  # reply in one round — exactly the out-of-family model is named.
  mixed = [_assistant("claude-fable-5-1", "start"), _assistant("claude-opus-4-8", "rest"), _result()]
  assert claude_code.out_of_family_served_models(mixed, CONFIGURED) == ["claude-opus-4-8"]


# ---------------------------------------------------------------------------
# Wiring — re-attach path (reuses the whole-round projection, zero new I/O)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_resume_notice_persists_exactly_once_with_full_fields(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(d) Through the real persistence layer, the round gains exactly one
  model_fallback_notice line carrying every schema field."""
  log_dir = tmp_path / "run"
  log_dir.mkdir(parents=True)
  raw_path = log_dir / runs.RAW_LOG_NAME
  raw_path.write_text(
      json.dumps(_assistant("claude-opus-4-8", "one")) + "\n" + json.dumps(_assistant("claude-sonnet-5", "two")) +
      "\n" + json.dumps(_result()) + "\n",
      encoding="utf-8")
  (log_dir / runs.CURSOR_NAME).write_text("0", encoding="utf-8")

  record = conftest.crashed_run_record(raw_path)
  cfg = config.CharlieBotConfig(charliebot_home=tmp_path / "home", backends={"options": [FABLE_OPTION]})
  mgr = sessions.SessionManager(cfg)
  session = await mgr.create_session(models.CreateSessionRequest(name="fb-persist"))
  meta = await mgr.get_session(session.id)
  assert meta is not None

  conftest.patch_resume_seams(monkeypatch)
  await conftest.run_resume_round(cfg, meta, record, mgr.callbacks(), is_alive=lambda: False)

  events = mgr.load_chat_events_sync(session.id)
  notices = [e for e in events if e.get("type") == ET.MODEL_FALLBACK_NOTICE]
  assert len(notices) == 1
  assert notices[0]["backend"] == FABLE_OPTION.id
  assert notices[0]["configured_model"] == CONFIGURED
  assert notices[0]["served_models"] == ["claude-opus-4-8", "claude-sonnet-5"]
  types = [e.get("type") for e in events]
  assert types.index(ET.RESULT) < types.index(ET.MODEL_FALLBACK_NOTICE)
