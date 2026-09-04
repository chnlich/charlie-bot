"""model_fallback_notice: turn-end served-model attribution.

Covers plan v7 §4.2: the pure family detector, the family decision table
(plus its single-suffix-strip regression guard), the aggregator render
mapping, and the two turn-end call sites (live re-read and re-attach
projection reuse).
"""

from __future__ import annotations

import asyncio
import json
import re
from datetime import UTC, datetime, timedelta
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock

import pytest
from conftest import (
    BROADCAST_PATCH_TARGET,
    BUILD_BACKEND_PATCH_TARGET,
    SESSIONS_SESSION_MANAGER_PATCH_TARGET,
    drain_session_consumer,
    fresh_master_state,
    make_work_item,
    mock_session_callbacks,
    patch_instructions_content,
)

from src.agents import master_cc, master_cc_queue, master_cc_run
from src.agents.backends import claude_code
from src.agents.backends.claude_code import (
    _family_membership,
    out_of_family_served_models,
)
from src.core import event_types as ET
from src.core import runs
from src.core.config import CharlieBotConfig
from src.core.message_aggregator import (
    MessageAggregator,
    _fallback_notice_text,
    _model_fallback_notice_msg,
)
from src.core.models import (
    BackendOption,
    CreateSessionRequest,
    MasterRunRecord,
    SessionMetadata,
)
from src.core.sessions import SessionManager

CONFIGURED = "claude-fable-5-1"
FABLE_OPTION = BackendOption(
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


def _result(**extra) -> dict:
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
  assert out_of_family_served_models(events, CONFIGURED) == []


def test_pure_and_mixed_out_of_family_rounds_detect_served_models() -> None:
  """(b) Pure opus rounds and fable/opus mixed rounds surface the served model."""
  pure = [_assistant("claude-opus-4-8", "a"), _assistant("claude-opus-4-8", "b"), _result()]
  assert out_of_family_served_models(pure, CONFIGURED) == ["claude-opus-4-8"]

  # Cross-kind split: fable-5-1 text and opus-4-8 text both serve the visible
  # reply in one round — exactly the out-of-family model is named.
  mixed = [_assistant("claude-fable-5-1", "start"), _assistant("claude-opus-4-8", "rest"), _result()]
  assert out_of_family_served_models(mixed, CONFIGURED) == ["claude-opus-4-8"]


def test_served_models_first_appearance_order_and_dedup() -> None:
  """Raw names, first-appearance order, deduplicated."""
  events = [
      _assistant("claude-opus-4-8", "one"),
      _assistant("claude-fable-5-1", "two"),
      _assistant("claude-opus-4-8", "three"),
      _assistant("claude-sonnet-5", "four"),
  ]
  assert out_of_family_served_models(events, CONFIGURED) == ["claude-opus-4-8", "claude-sonnet-5"]


def test_dated_haiku_basename_and_sonnet_are_out_of_family() -> None:
  """A dated haiku basename and sonnet-5 authoring the visible reply are named raw."""
  haiku_round = [_assistant("claude-haiku-4-5-20251001", "served"), _result()]
  assert out_of_family_served_models(haiku_round, CONFIGURED) == ["claude-haiku-4-5-20251001"]

  sonnet_round = [_assistant("claude-sonnet-5", "served"), _result()]
  assert out_of_family_served_models(sonnet_round, CONFIGURED) == ["claude-sonnet-5"]


def test_fable_thinking_only_init_does_not_mask_out_of_family_text() -> None:
  """A text-less fable init is not a visible-reply author; the opus text still fires."""
  events = [
      _assistant("claude-fable-5-1", blocks=[{
          "type": "thinking",
          "thinking": "fable thinks"
      }]),
      _assistant("claude-opus-4-8", "the visible reply"),
  ]
  assert out_of_family_served_models(events, CONFIGURED) == ["claude-opus-4-8"]


def test_out_of_family_thinking_and_tool_blocks_never_count() -> None:
  """(f) All visible text fable; out-of-family thinking/tool_use and an opus
  modelUsage key exist but no out-of-family text block — stays silent."""
  events = [
      _assistant("claude-fable-5-1", "Visible fable reply."),
      _assistant("claude-opus-4-8", blocks=[{
          "type": "thinking",
          "thinking": "opus thinks"
      }]),
      _assistant("claude-opus-4-8", blocks=[{
          "type": "tool_use",
          "id": "t1",
          "name": "Bash",
          "input": {}
      }]),
      _result(modelUsage={"claude-opus-4-8": {
          "input_tokens": 3
      }}),
  ]
  assert out_of_family_served_models(events, CONFIGURED) == []


def test_background_subagent_text_is_excluded() -> None:
  """Out-of-family text under parent_tool_use_id never counts as served."""
  events = [
      _assistant("claude-opus-4-8", "subagent reply", parent_tool_use_id="toolu_1"),
      _assistant("claude-fable-5-1", "main reply"),
  ]
  assert out_of_family_served_models(events, CONFIGURED) == []


def test_ending_event_with_out_of_family_text_detects() -> None:
  """(g) Mixed family round whose ending event carries the out-of-family text block."""
  events = [_assistant("claude-fable-5-1", "start"), _assistant("claude-opus-4-8", "ending text"), _result()]
  assert out_of_family_served_models(events, CONFIGURED) == ["claude-opus-4-8"]


def test_no_pin_synthetic_and_missing_model_stay_silent() -> None:
  """(c) No model pin, the CLI's <synthetic> sentinel, and missing models never fire."""
  opus_text_round = [_assistant("claude-opus-4-8", "served")]
  assert out_of_family_served_models(opus_text_round, None) == []
  assert out_of_family_served_models(opus_text_round, "") == []

  synthetic_round = [_assistant("<synthetic>", "synthetic text")]
  assert out_of_family_served_models(synthetic_round, CONFIGURED) == []

  missing_model_round = [{"type": ET.ASSISTANT, "message": {"content": [{"type": "text", "text": "hi"}]}}]
  assert out_of_family_served_models(missing_model_round, CONFIGURED) == []

  tool_only_round = [
      {
          "type": ET.ASSISTANT,
          "message":
              {
                  "model": "claude-opus-4-8",
                  "content": [{
                      "type": "tool_use",
                      "id": "t1",
                      "name": "Bash",
                      "input": {},
                  }],
              },
      },
  ]
  assert out_of_family_served_models(tool_only_round, CONFIGURED) == []


# ---------------------------------------------------------------------------
# (e2) Family decision table + single-suffix-strip regression guard
# ---------------------------------------------------------------------------


def test_family_decision_table_against_configured_fable_5_1() -> None:
  """Every observed model string classified against configured claude-fable-5-1."""
  same_family = ["claude-fable-5", "claude-fable-5-1"]
  other_family = ["claude-opus-5", "claude-opus-4-8", "claude-sonnet-5", "claude-haiku-4-5-20251001"]

  for model in same_family:
    assert _family_membership(model, CONFIGURED) is True, model
    assert out_of_family_served_models([_assistant(model, "hi")], CONFIGURED) == [], model
  for model in other_family:
    assert _family_membership(model, CONFIGURED) is False, model
    assert out_of_family_served_models([_assistant(model, "hi")], CONFIGURED) == [model], model


def test_single_suffix_strip_regression_flips_exactly_the_fable_5_row(monkeypatch: pytest.MonkeyPatch) -> None:
  """Substituting the refuted rule — strip any trailing numeric segment
  (re.sub(r"-\\d+$", "")) then compare — must flip the claude-fable-5 row:
  the main healthy spelling would be misjudged out-of-family (verify r2).
  The green table above fails the moment the real rule regresses to this."""

  def single_suffix_strip_family(served: str, configured: str) -> bool:
    return re.sub(r"-\d+$", "", served) == re.sub(r"-\d+$", "", configured)

  monkeypatch.setattr(claude_code, "_family_membership", single_suffix_strip_family)

  observed = ["claude-fable-5", "claude-fable-5-1", "claude-opus-5", "claude-opus-4-8", "claude-sonnet-5"]
  classified = {model: out_of_family_served_models([_assistant(model, "hi")], CONFIGURED) for model in observed}
  # The regression the table guards: claude-fable-5 flips to out-of-family...
  assert classified["claude-fable-5"] == ["claude-fable-5"]
  # ...while every other row keeps its verdict, isolating the guarded row.
  assert classified["claude-fable-5-1"] == []
  assert classified["claude-opus-5"] == ["claude-opus-5"]
  assert classified["claude-opus-4-8"] == ["claude-opus-4-8"]
  assert classified["claude-sonnet-5"] == ["claude-sonnet-5"]


# ---------------------------------------------------------------------------
# (e) Render mapping
# ---------------------------------------------------------------------------


def test_aggregator_renders_system_notice_naming_models() -> None:
  event = {
      "type": ET.MODEL_FALLBACK_NOTICE,
      "backend": FABLE_OPTION.id,
      "configured_model": CONFIGURED,
      "served_models": ["claude-opus-4-8"],
      "timestamp": "t",
  }
  deltas = list(MessageAggregator().feed(event))
  committed = [d["message"] for d in deltas if d["type"] == "message"]
  assert len(committed) == 1
  assert committed[0]["role"] == "system"
  assert committed[0]["content"] == "Served by claude-opus-4-8 (configured claude-fable-5-1)"


def test_fallback_notice_text_names_every_served_model() -> None:
  text = _fallback_notice_text(CONFIGURED, ["claude-opus-4-8", "claude-sonnet-5"])
  assert text == "Served by claude-opus-4-8, claude-sonnet-5 (configured claude-fable-5-1)"
  assert _model_fallback_notice_msg(
      {
          "type": ET.MODEL_FALLBACK_NOTICE,
          "configured_model": CONFIGURED,
          "served_models": ["claude-opus-4-8", "claude-sonnet-5"],
      }) == {
          "role": "system",
          "content": text
      }


# ---------------------------------------------------------------------------
# Wiring — live path (re-reads this invocation's own raw log)
# ---------------------------------------------------------------------------


class _RawLogBackend:
  """Backend double mirroring the real transport: pins this turn's events to
  the per-turn raw NDJSON log the turn-end detector re-reads, then yields them."""

  terminated = False
  exit_code = 0
  stderr_text = ""

  def __init__(self, events: list[dict], log_dir: Path | None) -> None:
    self._events = events
    self._log_dir = log_dir

  def translate_event(self, event: dict) -> list[dict]:
    return [event]

  async def terminate(self) -> None:
    self.terminated = True

  async def run(self, prompt: str, cwd: str, env: dict):
    if self._log_dir is not None:
      raw_path = self._log_dir / runs.RAW_LOG_NAME
      raw_path.parent.mkdir(parents=True, exist_ok=True)
      raw_path.write_text("".join(json.dumps(event) + "\n" for event in self._events), encoding="utf-8")
    for event in self._events:
      yield event


async def _run_live_round(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    session_id: str,
    option: BackendOption,
    events: list[dict],
) -> list[dict]:
  """Drive the real _run_cc with a raw-log-pinning backend double; return the persisted events."""
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", backend_options=[option])
  meta = SessionMetadata(id=session_id, name="t", backend=option.id)
  cb = mock_session_callbacks()

  def fake_build_backend(_option, _cfg, **kwargs):
    return _RawLogBackend(events, kwargs.get("log_dir"))

  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, fake_build_backend)
  patch_instructions_content(monkeypatch)

  item = make_work_item(cfg, meta, option, callbacks=cb)
  await master_cc._run_cc(item)

  return [c.args[1] for c in cb.persist_and_broadcast.await_args_list]


@pytest.mark.asyncio
async def test_live_round_emits_notice_after_result(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(d) The persisted stream gains exactly one model_fallback_notice with all
  fields, placed after the round's result event."""
  events = [_assistant("claude-opus-4-8", "This round's answer."), _result()]
  persisted = await _run_live_round(tmp_path, monkeypatch, "fb-notice-live", FABLE_OPTION, events)

  notices = [e for e in persisted if e.get("type") == ET.MODEL_FALLBACK_NOTICE]
  assert notices == [
      {
          "type": ET.MODEL_FALLBACK_NOTICE,
          "backend": "claude-fable-5.1",
          "configured_model": CONFIGURED,
          "served_models": ["claude-opus-4-8"],
      }
  ]
  types = [e.get("type") for e in persisted]
  assert types.index(ET.RESULT) < types.index(ET.MODEL_FALLBACK_NOTICE)


@pytest.mark.asyncio
async def test_live_in_family_round_emits_nothing(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Zero-notice control: visible reply authored by the configured model (either
  healthy fable spelling) emits no event of any new type."""
  events = [
      _assistant("claude-fable-5-1", "first half"),
      _assistant("claude-fable-5", "second half"),
      _result(modelUsage={"claude-haiku-4-5-20251001": {
          "input_tokens": 1
      }}),
  ]
  persisted = await _run_live_round(tmp_path, monkeypatch, "fb-notice-live-quiet", FABLE_OPTION, events)

  assert [e for e in persisted if e.get("type") == ET.MODEL_FALLBACK_NOTICE] == []
  assert [e for e in persisted if e.get("type") == ET.ASSISTANT_ERROR] == []


@pytest.mark.asyncio
@pytest.mark.parametrize("backend_type", ["codex", "opencode"])
async def test_live_non_cc_backend_emits_nothing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, backend_type: str) -> None:
  """Non-cc backends never trigger the detector, whatever their stream served."""
  option = BackendOption(id="other", label="Other", type=backend_type, model="some-model")
  events = [_assistant("glm-5.2", "served by another kind"), _result()]
  persisted = await _run_live_round(tmp_path, monkeypatch, f"fb-notice-live-{backend_type}", option, events)

  assert [e for e in persisted if e.get("type") == ET.MODEL_FALLBACK_NOTICE] == []


# ---------------------------------------------------------------------------
# Wiring — re-attach path (reuses the whole-round projection, zero new I/O)
# ---------------------------------------------------------------------------


def _resume_patch_setup(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr(master_cc_run, "_build_fresh_translate", lambda *a, **k: (lambda event: [event]))
  monkeypatch.setattr(master_cc_queue.streaming_manager, "broadcast", AsyncMock())
  monkeypatch.setattr(
      SESSIONS_SESSION_MANAGER_PATCH_TARGET,
      lambda *a, **k: MagicMock(_has_running_tasks=AsyncMock(return_value=False)))


@pytest.mark.asyncio
async def test_resume_round_emits_identical_notice_from_projection(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The re-attach site detects over the existing whole-round projection (no
  second file read) and emits the identical notice after the round's result."""
  session_id = "fb-notice-resume"
  log_dir = tmp_path / "run"
  log_dir.mkdir(parents=True)
  raw_path = log_dir / runs.RAW_LOG_NAME
  assistant_line = json.dumps(_assistant("claude-opus-4-8", "served reply")) + "\n"
  raw_path.write_text(assistant_line + json.dumps(_result()) + "\n", encoding="utf-8")
  runs.write_raw_cursor(log_dir / runs.CURSOR_NAME, 0)

  record = MasterRunRecord(
      pid=None,
      pid_start=None,
      started_at=datetime.now(UTC) - timedelta(seconds=60),
      raw_log=str(raw_path),
  )
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", backend_options=[FABLE_OPTION])
  meta = SessionMetadata(id=session_id, name="t", backend=FABLE_OPTION.id)
  cb = mock_session_callbacks()

  _resume_patch_setup(monkeypatch)
  async with fresh_master_state(session_id):
    future = await master_cc.enqueue_master_resume(cfg, meta, record, cb, is_alive=lambda: False)
    await asyncio.wait_for(future, timeout=5)
    await drain_session_consumer(session_id, timeout=5)

  persisted = [c.args[1] for c in cb.persist_and_broadcast.await_args_list]
  notices = [e for e in persisted if e.get("type") == ET.MODEL_FALLBACK_NOTICE]
  assert notices == [
      {
          "type": ET.MODEL_FALLBACK_NOTICE,
          "backend": FABLE_OPTION.id,
          "configured_model": CONFIGURED,
          "served_models": ["claude-opus-4-8"],
      }
  ]
  types = [e.get("type") for e in persisted]
  assert types.index(ET.RESULT) < types.index(ET.MODEL_FALLBACK_NOTICE)


@pytest.mark.asyncio
async def test_resume_notice_persists_exactly_once_with_full_fields(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(d) Through the real persistence layer, the round gains exactly one
  model_fallback_notice line carrying every schema field."""
  session_id = "fb-notice-persist"
  log_dir = tmp_path / "run"
  log_dir.mkdir(parents=True)
  raw_path = log_dir / runs.RAW_LOG_NAME
  raw_path.write_text(
      json.dumps(_assistant("claude-opus-4-8", "one")) + "\n" + json.dumps(_assistant("claude-sonnet-5", "two")) +
      "\n" + json.dumps(_result()) + "\n",
      encoding="utf-8")
  runs.write_raw_cursor(log_dir / runs.CURSOR_NAME, 0)

  record = MasterRunRecord(
      pid=None,
      pid_start=None,
      started_at=datetime.now(UTC) - timedelta(seconds=60),
      raw_log=str(raw_path),
  )
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", backend_options=[FABLE_OPTION])
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="fb-persist"))
  meta = await mgr.get_session(session.id)
  assert meta is not None

  monkeypatch.setattr(BROADCAST_PATCH_TARGET, AsyncMock())
  _resume_patch_setup(monkeypatch)
  async with fresh_master_state(session_id):
    future = await master_cc.enqueue_master_resume(cfg, meta, record, mgr.callbacks(), is_alive=lambda: False)
    await asyncio.wait_for(future, timeout=5)
    await drain_session_consumer(session_id, timeout=5)

  events = mgr.load_chat_events_sync(session.id)
  notices = [e for e in events if e.get("type") == ET.MODEL_FALLBACK_NOTICE]
  assert len(notices) == 1
  assert notices[0]["backend"] == FABLE_OPTION.id
  assert notices[0]["configured_model"] == CONFIGURED
  assert notices[0]["served_models"] == ["claude-opus-4-8", "claude-sonnet-5"]
  types = [e.get("type") for e in events]
  assert types.index(ET.RESULT) < types.index(ET.MODEL_FALLBACK_NOTICE)
