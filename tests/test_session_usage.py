"""Focused tests for session usage resolution."""

import json
import pathlib
from typing import Any

import conftest
import pytest

from src.backends.claude_code import claude_code, claude_launch
from src.backends.codex import codex_usage
from src.infra import config, models
from src.runtime.agent_process import base


def _build_cfg(tmp_path: pathlib.Path, **codex_kwargs: Any) -> config.CharlieBotConfig:
  codex_opt = conftest.backend_option(
      id="codex-test", label="Codex", type="codex", model="codex-test-model", **codex_kwargs)
  return config.CharlieBotConfig(
      charliebot_home=tmp_path,
      backends={
          "options":
              [
                  conftest.backend_option(
                      id=conftest.OPUS_BACKEND_ID, label="Claude", type="cc-claude", model="claude-opus-4-6"),
                  codex_opt,
              ]
      },
  )


def _write_session(session_blocks: conftest.SessionBlocks, meta: models.SessionMetadata, events: list[dict]) -> None:
  session_dir = session_blocks.events.get_chat_events_path(meta.id).parent
  session_dir.mkdir(parents=True, exist_ok=True)
  (session_dir.parent / "threads").mkdir(parents=True, exist_ok=True)
  session_blocks.store.metadata_path(meta.id).write_text(meta.model_dump_json(indent=2), encoding="utf-8")
  lines = "\n".join(json.dumps(event) for event in events)
  session_blocks.events.get_chat_events_path(meta.id).write_text(lines + "\n", encoding="utf-8")


def _session_rig(tmp_path: pathlib.Path, session_id: str, name: str,
                 backend: str) -> tuple[conftest.SessionBlocks, models.SessionMetadata]:
  """Session blocks over a fresh _build_cfg config plus one session's metadata: the pair a resolve test starts from."""
  cfg = _build_cfg(tmp_path)
  session_blocks = conftest.build_session_blocks(cfg)
  meta = models.SessionMetadata(profile="manager", id=session_id, name=name, backend=backend)
  return session_blocks, meta


async def _resolved_usage(session_blocks: conftest.SessionBlocks, meta: models.SessionMetadata) -> dict:
  """The common resolve rig: default-kwargs resolve asserting a mapping came back."""
  usage = await session_blocks.anchors.resolve_session_usage(meta.id, meta)
  assert usage is not None
  return usage


def _assert_no_context_tier(usage: dict | None) -> None:
  """Every tier declined: the resolution returned a mapping with no context reading and no model."""
  assert usage is not None
  assert usage["context_tokens"] is None
  assert usage["context_full"] is None
  assert usage["context_compact_at"] is None
  assert usage["model"] == ""


@pytest.fixture(autouse=True)
def _codex_home_under_tmp(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> pathlib.Path:
  """Pin the codex resolver's default home under tmp_path: codex runs from the default
  home, so the rollout tree the tests seed is ``<tmp>/codex-home/sessions``."""
  home = tmp_path / "codex-home"
  monkeypatch.setattr(codex_usage, "DEFAULT_CODEX_HOME", home)
  return home


def _assistant_event(
    model: str,
    input_tokens: int,
    cache_creation: int = 0,
    cache_read: int = 0,
    parent_tool_use_id: str | None = None) -> dict:
  return {
      "type": "assistant",
      "parent_tool_use_id": parent_tool_use_id,
      "message":
          {
              "model": model,
              "usage":
                  {
                      "input_tokens": input_tokens,
                      "cache_creation_input_tokens": cache_creation,
                      "cache_read_input_tokens": cache_read,
                  },
          },
  }


def _result_event(
    total_cost_usd: float,
    model_usage: dict | None = None,
    input_tokens: int = 0,
    context_snapshot: dict | None = None) -> dict:
  return {
      "type": "result",
      "usage": {
          "input_tokens": input_tokens,
          "cache_creation_input_tokens": 0,
          "cache_read_input_tokens": 0,
      },
      "modelUsage": model_usage or {},
      "total_cost_usd": total_cost_usd,
      **({
          "context_snapshot": context_snapshot
      } if context_snapshot is not None else {}),
  }


def _cumulative_result_with_assistant_reading() -> list[dict]:
  """A result event carrying the turn-cumulative 1.5M sum plus the per-request 150k assistant reading
  the claude tier must prefer."""
  return [
      _result_event(0.5, {"claude-opus-4-6": {
          "contextWindow": 200_000
      }}, input_tokens=1_500_000),
      _assistant_event("claude-opus-4-6", input_tokens=100_000, cache_creation=20_000, cache_read=30_000),
  ]


# ---------------------------------------------------------------------------
# Acceptance test 1: assistant event beats turn-cumulative result usage
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_claude_tier_uses_assistant_event_tokens_not_result_cumulative(tmp_path: pathlib.Path) -> None:
  session_blocks, meta = _session_rig(tmp_path, "session-assistant", "Assistant", conftest.OPUS_BACKEND_ID)
  _write_session(session_blocks, meta, _cumulative_result_with_assistant_reading())

  usage = await _resolved_usage(session_blocks, meta)
  assert usage["context_tokens"] == 150_000  # assistant event sum, not 1.5M
  assert usage["context_full"] == 200_000
  assert usage["model"] == "claude-opus-4-6"
  assert usage["total_cost_usd"] == pytest.approx(0.5)


# ---------------------------------------------------------------------------
# Acceptance test 4: sub-agent and synthetic events are ignored
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_claude_tier_ignores_subagent_and_synthetic_assistant_events(tmp_path: pathlib.Path) -> None:
  session_blocks, meta = _session_rig(tmp_path, "session-ignore", "Ignore", conftest.OPUS_BACKEND_ID)
  _write_session(
      session_blocks,
      meta,
      [
          # sub-agent event with large usage — must be ignored (parent_tool_use_id set)
          _assistant_event("claude-opus-4-6", input_tokens=400_000, parent_tool_use_id="tool_1"),
          # synthetic zero-usage event — must be ignored (prompt-token sum is 0)
          _assistant_event("claude-opus-4-6", input_tokens=0),
          # the real main-chain event — must be picked
          _assistant_event("claude-opus-4-6", input_tokens=90_000, cache_read=10_000),
      ])

  usage = await _resolved_usage(session_blocks, meta)
  assert usage["context_tokens"] == 100_000  # the real event, not 400_000


# ---------------------------------------------------------------------------
# Acceptance test 6: cost 0 -> None; positive cost sums
# ---------------------------------------------------------------------------

# Rows are the two result-cost shapes the resolver must distinguish: every
# result reporting 0.0 costs nothing (None, not 0.0) and positive costs sum
# across the full event list. The columns are the per-result total_cost_usd
# values in order, then the expected resolved cost.
_COST_ROWS = [
    pytest.param((0.0, 0.0), None, id="all-zero-reports-none"),
    pytest.param((0.10, 0.20), pytest.approx(0.30), id="positive-costs-sum"),
]


@pytest.mark.asyncio
@pytest.mark.parametrize("result_costs, expected_cost", _COST_ROWS)
async def test_total_cost_across_results(
    tmp_path: pathlib.Path, result_costs: tuple[float, float], expected_cost: object) -> None:
  session_blocks, meta = _session_rig(tmp_path, "session-cost", "Cost", conftest.OPUS_BACKEND_ID)
  _write_session(
      session_blocks, meta, [
          _result_event(result_costs[0], {"claude-opus-4-6": {
              "contextWindow": 200_000
          }}, input_tokens=1000),
          _result_event(result_costs[1], {"claude-opus-4-6": {
              "contextWindow": 200_000
          }}, input_tokens=2000),
          _assistant_event("claude-opus-4-6", input_tokens=50_000),
      ])

  usage = await _resolved_usage(session_blocks, meta)
  assert usage["total_cost_usd"] == expected_cost


# ---------------------------------------------------------------------------
# Acceptance test 8: headless_claude_declared_window
# ---------------------------------------------------------------------------

_reset_declared_window_warnings = conftest.fresh_state_fixture(claude_code._DECLARED_WINDOW_WARNINGS_SEEN.clear)


@pytest.fixture
def _clean_ceiling_env(monkeypatch: pytest.MonkeyPatch) -> None:
  """Remove env vars that would change the declared window so each test starts clean."""
  # The declared-window logic reads exactly these three names; the module constants are
  # the one-spelling home the default pin, the forward allowlist, and the degradation
  # checks must agree on.
  for name in (claude_launch.AUTO_COMPACT_WINDOW_ENV, claude_launch.AUTOCOMPACT_PCT_OVERRIDE_ENV,
               claude_launch.MAX_CONTEXT_TOKENS_ENV):
    monkeypatch.delenv(name, raising=False)


@pytest.mark.usefixtures("_clean_ceiling_env")
def test_declared_window_subtracts_reserves_from_declared_window(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setenv(claude_launch.AUTO_COMPACT_WINDOW_ENV, "500000")
  expected_point = 500_000 - claude_launch.CLAUDE_COMPACT_OUTPUT_RESERVE - claude_launch.CLAUDE_COMPACT_CONTEXT_RESERVE
  assert claude_code.headless_claude_declared_window() == (500_000, expected_point)


# ---------------------------------------------------------------------------
# Latest-reading slot: the newest reading-bearing event decides the readout
# regardless of which backend produced it (system/context_reading tier).
# ---------------------------------------------------------------------------

_K3_MODEL = "openai/moonshotai/Kimi-K3"


def _k3_reading() -> dict:
  return base.make_context_reading_event(_K3_MODEL, 118_234, 262_144, 170_393)


def _assert_k3_reading(usage: dict) -> None:
  """The resolved slot carries the reading payload unchanged in all four fields."""
  assert usage["context_tokens"] == 118_234
  assert usage["context_full"] == 262_144
  assert usage["context_compact_at"] == 170_393
  assert usage["model"] == _K3_MODEL


@pytest.mark.asyncio
async def test_context_reading_tier_beats_cumulative_result_usage(tmp_path: pathlib.Path) -> None:
  session_blocks, meta = _session_rig(tmp_path, "session-reading", "Reading", conftest.OPUS_BACKEND_ID)
  # Several result events with turn-cumulative usage and no context_snapshot
  # (charlie-code today), plus context_reading events: the newest reading
  # decides all four fields, the cumulative usage never reaches the readout.
  _write_session(
      session_blocks, meta, [
          _result_event(0.10, input_tokens=1_000_000),
          _result_event(0.20, input_tokens=2_000_000),
          base.make_context_reading_event("old/model", 10_000, 100_000, 90_000),
          _k3_reading(),
      ])

  usage = await _resolved_usage(session_blocks, meta)
  _assert_k3_reading(usage)
  # Cost still comes from the shared fold over result events.
  assert usage["total_cost_usd"] == pytest.approx(0.30)


@pytest.mark.asyncio
async def test_empty_slot_keeps_context_unknown(tmp_path: pathlib.Path) -> None:
  session_blocks, meta = _session_rig(tmp_path, "session-emptyslot", "Empty Slot", conftest.OPUS_BACKEND_ID)
  # Only text assistant events (no usage -> no claude slot) and cumulative
  # result events: the slot stays empty and the context fields stay unknown.
  _write_session(
      session_blocks, meta, [
          {
              "type": "assistant",
              "message": {
                  "model": "claude-opus-4-6",
                  "content": [{
                      "type": "text",
                      "text": "hello"
                  }],
              },
          },
          _result_event(0.5, {"claude-opus-4-6": {
              "contextWindow": 200_000
          }}, input_tokens=999_999),
      ])

  usage = await session_blocks.anchors.resolve_session_usage(meta.id, meta)

  _assert_no_context_tier(usage)

  empty_meta = models.SessionMetadata(
      profile="manager", id="session-emptyslot-none", name="Empty Slot None", backend=conftest.OPUS_BACKEND_ID)
  _write_session(session_blocks, empty_meta, [])
  assert await session_blocks.anchors.resolve_session_usage(empty_meta.id, empty_meta) is None
