"""Run-json (`opencode run --format json`) backend: probe pins, translator, argv contract.

Plan 02 v3 (opencode transport switch serve+SSE -> run --format json):

- A2: `_build_command` emits `opencode run --format json` — first turn bare, turn
  N+1 with `--session <turn N sessionID>`; a synthesized session_id event from the
  first raw event's top-level sessionID reaches the event stream (master_cc's
  anchor capture at master_cc.py:402-405, zero-diff).
- A3: fixture-driven per-event-type translator assertions; error/terminal and
  denial mappings assert the probe-selected branch (per-event-type mapping),
  with the refuted defaults asserted too; every probe-leg fixture carries a
  "leg observable non-empty" assertion so an empty capture turns RED.
- A4: opencode run-json turns resolve to session_usage.py:280's no-source tier
  (accepted fork-2 regression, pinned).

Probe report (2026-08-15, opencode 1.18.18, meshy-sglang-dsv4-flash, <=3 real
prompt invocations):

- Leg a (permission denial): the headless question-deny can never fire — the
  question tool is not model-exposed in run mode (first invocation evidence;
  the session's enforcement list showed it). A bash-deny injected through the
  same headless config mechanism shows the denial surface: the denied tool is
  UNREGISTERED and the model's call lands as a completed ``tool="invalid"``
  tool_use part whose output explains "Model tried to call unavailable tool".
  No deny/error event type, no state.error, exit code 0. Branch: per-event-type
  mapping; a denial is non-terminal.
- Leg b (error terminal): an invalid model yields exactly one top-level
  ``{"type": "error", "error": {"name": ..., "data": {"message": ...}}}``
  event, NO step_finish of any reason, exit code 1, and the real reason lands
  on stderr (ProviderModelNotFoundError). Branch: per-event-type error mapping;
  exit code + stderr surface through the shared base run machinery.
- Leg c (subagent): a task-tool subagent never surfaces child-session events on
  the parent's run-json stream — every streamed event carries the parent
  sessionID; the child id exists only in the task part's state.metadata
  (``sessionId`` / ``parentSessionId``). Branch: no sessionID filtering —
  nothing interleaves.
"""

from __future__ import annotations

import json
import re
from pathlib import Path

import pytest

from src.agents.backends.opencode import OpenCodeBackend
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import SessionMetadata
from src.core.session_usage import SessionUsageResolver

FIXTURES = Path(__file__).resolve().parent / "fixtures"

TEXT_TURN = "opencode_runjson_text_turn.ndjson"
TOOL_TURN = "opencode_runjson_tool_turn.ndjson"
LEG_A = "opencode_runjson_leg_a_permission.ndjson"
LEG_B = "opencode_runjson_leg_b_error_terminal.ndjson"
LEG_B_STDERR = "opencode_runjson_leg_b_error_terminal.stderr.txt"
LEG_B_META = "opencode_runjson_leg_b_error_terminal.meta.json"
LEG_C = "opencode_runjson_leg_c_subagent.ndjson"
ALL_NDJSON_FIXTURES = (TEXT_TURN, TOOL_TURN, LEG_A, LEG_B, LEG_C)

_RAW_SESSION_ID = re.compile(r"ses_[A-Za-z0-9]{10,}")
_PLACEHOLDER_SESSION_ID = re.compile(r"ses_SESSION_[A-Z]")


def _load_events(name: str) -> list[dict]:
  return [
      json.loads(line)
      for line in (FIXTURES / name).read_text(encoding="utf-8").splitlines()
      if line.strip()
  ]


def _build_backend(monkeypatch: pytest.MonkeyPatch, **kwargs) -> OpenCodeBackend:
  monkeypatch.setattr(
      "src.agents.backends.opencode.resolve_binary",
      lambda name, fallback: "/usr/bin/opencode",
  )
  return OpenCodeBackend(**kwargs)


def _translate_stream(backend: OpenCodeBackend, events: list[dict]) -> list[dict]:
  out: list[dict] = []
  for ev in events:
    out.extend(backend.translate_event(ev))
  return out


# ---------------------------------------------------------------------------
# Fixture hygiene: scrubbed captures, or the pins below are untrustworthy
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("fixture", (*ALL_NDJSON_FIXTURES, LEG_B_STDERR))
def test_fixture_is_scrubbed(fixture: str) -> None:
  text = (FIXTURES / fixture).read_text(encoding="utf-8")
  assert "/home/" not in text
  assert "chaoli" not in text
  assert not _RAW_SESSION_ID.search(text)


@pytest.mark.parametrize("fixture", ALL_NDJSON_FIXTURES)
def test_fixture_session_ids_are_placeholders(fixture: str) -> None:
  for ev in _load_events(fixture):
    sid = ev.get("sessionID")
    if sid is not None:
      assert _PLACEHOLDER_SESSION_ID.fullmatch(sid)
    part = ev.get("part")
    if isinstance(part, dict) and part.get("sessionID") is not None:
      assert _PLACEHOLDER_SESSION_ID.fullmatch(part["sessionID"])


# ---------------------------------------------------------------------------
# Probe-leg observables: each leg must have captured something — an empty leg
# turns these RED (no silent default branch)
# ---------------------------------------------------------------------------


def test_leg_a_denial_observable_is_a_completed_invalid_tool_part() -> None:
  """Denial under a headless-injected deny: tool unregistered; the model's call
  arrives as a completed tool="invalid" part explaining itself. Exit was 0."""
  events = _load_events(LEG_A)
  invalid_parts = [
      e["part"] for e in events
      if isinstance(e.get("part"), dict) and e["part"].get("tool") == "invalid"
  ]
  assert invalid_parts, "leg a captured no denial observable"
  state = invalid_parts[0]["state"]
  assert state["status"] == "completed"
  assert state["input"]["tool"] == "bash"
  assert "unavailable tool" in state["output"]
  # Refuted defaults: no dedicated deny/error event type and no state.error part.
  assert not any(e.get("type") == "error" for e in events)
  assert not any(e.get("type") in ("permission.asked", "permission.replied") for e in events)
  assert not any(
      "error" in e["part"]["state"]
      for e in events
      if isinstance(e.get("part"), dict) and isinstance(e["part"].get("state"), dict))


def test_leg_b_error_terminal_observables() -> None:
  """Error terminal: exactly one top-level error event, no step_finish, exit 1,
  real reason on stderr."""
  events = _load_events(LEG_B)
  assert len(events) == 1, "leg b captured nothing"
  error = events[0]
  assert error["type"] == "error"
  assert error["error"]["data"]["message"]
  # Refuted default: a step_finish reason set does not cover the error terminal.
  assert not any(e.get("type") == "step_finish" for e in events)
  stderr_text = (FIXTURES / LEG_B_STDERR).read_text(encoding="utf-8")
  assert "ProviderModelNotFoundError" in stderr_text
  assert "totally-bogus-provider-zz/no-such-model-qq" in stderr_text
  meta = json.loads((FIXTURES / LEG_B_META).read_text(encoding="utf-8"))
  assert meta["exit_code"] == 1


def test_leg_c_subagent_events_never_leave_the_parent_session_stream() -> None:
  """Subagent: child session exists only in task metadata; no child event ever
  surfaces with a foreign sessionID on the run-json stream."""
  events = _load_events(LEG_C)
  task_parts = [
      e["part"] for e in events
      if isinstance(e.get("part"), dict) and e["part"].get("tool") == "task"
  ]
  assert task_parts, "leg c captured no subagent observable"
  metadata = task_parts[0]["state"]["metadata"]
  assert metadata["sessionId"] == "ses_SESSION_B"  # the child session
  assert metadata["parentSessionId"] == "ses_SESSION_A"
  assert {e["sessionID"] for e in events} == {"ses_SESSION_A"}


# ---------------------------------------------------------------------------
# A2: argv continuity contract
# ---------------------------------------------------------------------------


def test_build_command_first_turn_has_no_session_flag(monkeypatch: pytest.MonkeyPatch) -> None:
  backend = _build_backend(monkeypatch, model="provider/model")
  cmd = backend._build_command("the prompt")
  assert cmd[:6] == ["/usr/bin/opencode", "run", "--format", "json", "-m", "provider/model"]
  assert "--session" not in cmd
  assert cmd[-2:] == ["--", "the prompt"]


def test_build_command_resume_turn_carries_captured_session_id(monkeypatch: pytest.MonkeyPatch) -> None:
  """Turn N+1 argv contains --session <turn N sessionID> (codex resume precedent)."""
  backend = _build_backend(monkeypatch, model="provider/model", resume_session_id="ses_TURN_N")
  cmd = backend._build_command("the prompt")
  idx = cmd.index("--session")
  assert cmd[idx + 1] == "ses_TURN_N"


def test_build_command_extra_flags_before_prompt_and_effective_prompt(monkeypatch: pytest.MonkeyPatch) -> None:
  backend = _build_backend(
      monkeypatch,
      model="provider/model",
      instructions_content="# Instructions",
      extra_flags=["--print-logs"],
  )
  cmd = backend._build_command("the prompt")
  assert cmd[-2] == "--print-logs"
  assert cmd[-1] == "<system-instructions>\n# Instructions\n</system-instructions>\n\nthe prompt"


def test_build_command_requires_a_model(monkeypatch: pytest.MonkeyPatch) -> None:
  backend = _build_backend(monkeypatch)
  with pytest.raises(ValueError, match="requires a model"):
    backend._build_command("the prompt")


# ---------------------------------------------------------------------------
# A3: per-event-type translator mapping (fixture-driven)
# ---------------------------------------------------------------------------


def test_synthesized_session_id_event_is_first_and_exactly_once(monkeypatch: pytest.MonkeyPatch) -> None:
  """The first raw event's top-level sessionID becomes the one session_id event
  that master_cc.py:402-405 anchors on."""
  for fixture in ALL_NDJSON_FIXTURES:
    backend = _build_backend(monkeypatch)
    out = _translate_stream(backend, _load_events(fixture))
    session_indices = [i for i, e in enumerate(out) if "session_id" in e]
    assert session_indices == [0], fixture
    assert out[0] == {"session_id": "ses_SESSION_A"}


def test_translate_text_turn(monkeypatch: pytest.MonkeyPatch) -> None:
  backend = _build_backend(monkeypatch, model="provider/model")
  out = _translate_stream(backend, _load_events(TEXT_TURN))
  assistant = [e for e in out if e.get("type") == ET.ASSISTANT]
  assert assistant == [{"type": ET.ASSISTANT, "message": {"content": [{"type": "text", "text": "OK"}]}}]
  results = [e for e in out if e.get("type") == ET.RESULT]
  assert len(results) == 1
  assert results[0]["usage"] == {
      "input_tokens": 18,
      "output_tokens": 42,
      "cache_read_input_tokens": 9344,
      "cache_creation_input_tokens": 0,
  }
  assert results[0]["total_cost_usd"] == 0
  # Fork-3/fork-2: no context_snapshot is ever synthesized from run-json data.
  assert "context_snapshot" not in results[0]


def test_translate_tool_turn_emits_tool_pair_and_turn_summed_result(monkeypatch: pytest.MonkeyPatch) -> None:
  backend = _build_backend(monkeypatch, model="provider/model")
  out = _translate_stream(backend, _load_events(TOOL_TURN))
  tool_uses = [
      item
      for e in out
      if e.get("type") == ET.ASSISTANT
      for item in e["message"]["content"]
      if item.get("type") == ET.TOOL_USE
  ]
  assert tool_uses == [{
      "type": ET.TOOL_USE,
      "name": "bash",
      "id": "bash:0",
      "input": {"command": "echo probe-123"},
  }]
  tool_results = [e for e in out if e.get("type") == ET.TOOL_RESULT]
  assert tool_results == [{"type": ET.TOOL_RESULT, "tool_use_id": "bash:0", "content": "probe-123\n"}]
  results = [e for e in out if e.get("type") == ET.RESULT]
  assert len(results) == 1
  # Usage is the accumulated turn sum across both step-finish parts.
  assert results[0]["usage"] == {
      "input_tokens": 217,
      "output_tokens": 154,
      "cache_read_input_tokens": 18688,
      "cache_creation_input_tokens": 0,
  }


def test_mid_turn_step_finish_emits_no_result(monkeypatch: pytest.MonkeyPatch) -> None:
  """A step-finish that is not the successful-turn terminal (reason tool-calls)
  accumulates usage but produces no result event."""
  backend = _build_backend(monkeypatch, model="provider/model")
  events = _load_events(TOOL_TURN)
  mid = _translate_stream(backend, events[:4])  # through the first step_finish
  assert not any(e.get("type") == ET.RESULT for e in mid)


def test_translate_error_event_per_event_type_mapping(monkeypatch: pytest.MonkeyPatch) -> None:
  """Leg-b branch: the run-json error event maps to an error event directly;
  no result is synthesized on an error terminal."""
  backend = _build_backend(monkeypatch, model="provider/model")
  out = _translate_stream(backend, _load_events(LEG_B))
  assert out == [
      {"session_id": "ses_SESSION_A"},
      {
          "type": ET.ERROR,
          "message": "Unexpected server error. Check server logs for details.",
          "content": "Unexpected server error. Check server logs for details.",
      },
  ]


def test_translate_leg_a_denial_is_a_tool_level_result_and_the_turn_continues(
    monkeypatch: pytest.MonkeyPatch) -> None:
  """Leg-a branch: a permission denial surfaces as the invalid tool's assistant
  tool_use plus an explanatory tool_result — never a turn-level error event —
  and the turn still completes with its accumulated result."""
  backend = _build_backend(monkeypatch, model="provider/model")
  out = _translate_stream(backend, _load_events(LEG_A))
  assert not any(e.get("type") == ET.ERROR for e in out)
  invalid_uses = [
      item
      for e in out
      if e.get("type") == ET.ASSISTANT
      for item in e["message"]["content"]
      if item.get("type") == ET.TOOL_USE and item.get("name") == "invalid"
  ]
  assert len(invalid_uses) == 1
  explaining_results = [
      e for e in out if e.get("type") == ET.TOOL_RESULT and "unavailable tool" in str(e.get("content"))]
  assert len(explaining_results) == 1
  assert any(e.get("type") == ET.RESULT for e in out)


def test_translate_text_part_keeps_cumulative_delta_semantics(monkeypatch: pytest.MonkeyPatch) -> None:
  """Reuse of _translate_part: repeated deliveries of the same text part id
  emit only the delta (SSE-era contract, unchanged for run-json)."""
  backend = _build_backend(monkeypatch)
  first = backend.translate_event({
      "type": "text",
      "sessionID": "ses_SESSION_A",
      "part": {"id": "p1", "type": "text", "text": "Hello"},
  })
  second = backend.translate_event({
      "type": "text",
      "sessionID": "ses_SESSION_A",
      "part": {"id": "p1", "type": "text", "text": "Hello world"},
  })
  texts = [
      item["text"]
      for e in first + second
      if e.get("type") == ET.ASSISTANT
      for item in e["message"]["content"]
      if item.get("type") == "text"
  ]
  assert texts == ["Hello", " world"]


# ---------------------------------------------------------------------------
# A4: usage no-source tier pin (accepted fork-2 regression)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_runjson_opencode_turn_resolves_to_session_usage_no_source_tier(
    tmp_path: Path) -> None:
  """Run-json result events carry no context_snapshot, so an opencode turn's
  session usage hits session_usage.py:280's no-source tier: context unknown,
  cost still summed. This pins the accepted fork-2 regression so a future fix
  cannot silently re-tier."""
  runjson_events = [
      {"session_id": "ses_SESSION_A"},
      {"type": ET.ASSISTANT, "message": {"content": [{"type": "text", "text": "hi"}]}},
      {
          "type": ET.RESULT,
          "result": "",
          "usage": {
              "input_tokens": 10,
              "output_tokens": 5,
              "cache_read_input_tokens": 0,
              "cache_creation_input_tokens": 0,
          },
          "total_cost_usd": 1.25,
      },
  ]
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  resolver = SessionUsageResolver(cfg, {}, lambda session_id: None, lambda session_id: runjson_events)
  session_meta = SessionMetadata(id="s", name="s", backend="fake-oc")

  usage = await resolver.resolve_session_usage("s", session_meta)

  assert usage == {
      "context_tokens": None,
      "context_full": None,
      "context_compact_at": None,
      "total_cost_usd": 1.25,
      "model": "",
  }


@pytest.mark.asyncio
async def test_snapshot_tier_still_selected_by_data_not_backend_name(
    tmp_path: Path) -> None:
  """Contrast pin for the no-source regression: the tier boundary is the
  context_snapshot key itself — a historical opencode turn whose result event
  carries one still resolves to the snapshot tier."""
  snapshot_events = [
      {
          "type": ET.RESULT,
          "result": "",
          "usage": {
              "input_tokens": 10,
              "output_tokens": 5,
              "cache_read_input_tokens": 0,
              "cache_creation_input_tokens": 0,
          },
          "total_cost_usd": 1.25,
          "context_snapshot": {
              "model": "provider/model",
              "tokens": {"input": 100, "output": 10, "reasoning": 0, "cache_read": 0, "cache_write": 0},
              "limit": {"context": 1000, "input": 800, "output": 200},
          },
      },
  ]
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  resolver = SessionUsageResolver(cfg, {}, lambda session_id: None, lambda session_id: snapshot_events)
  session_meta = SessionMetadata(id="s", name="s", backend="fake-oc")

  usage = await resolver.resolve_session_usage("s", session_meta)

  assert usage is not None
  assert usage["context_tokens"] == 110
  assert usage["context_full"] == 800
  assert usage["model"] == "provider/model"
