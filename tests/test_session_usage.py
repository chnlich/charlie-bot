"""Focused tests for session usage resolution."""

import json
from pathlib import Path

import pytest
from conftest import OPUS_BACKEND_ID, SYNTHETIC_MODEL

from src.agents.backends.claude_code import (
  CLAUDE_COMPACT_CONTEXT_RESERVE,
  CLAUDE_COMPACT_OUTPUT_RESERVE,
  HEADLESS_CLAUDE_DEFAULT_ENV,
  headless_claude_declared_window,
)
from src.core.codex_usage import _extract_codex_rollout_usage_event
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, SessionMetadata
from src.core.sessions import SessionManager


def _build_cfg(tmp_path: Path, **codex_kwargs) -> CharlieBotConfig:
  codex_opt = BackendOption(
      id="codex-test", label="Codex", type="codex", model="codex-test-model", **codex_kwargs)
  return CharlieBotConfig(
      charliebot_home=tmp_path,
      backend_options=[
          BackendOption(id=OPUS_BACKEND_ID, label="Claude", type="cc-claude", model="claude-opus-4-6"),
          codex_opt,
      ],
  )


def _write_session(session_mgr: SessionManager, meta: SessionMetadata, events: list[dict]) -> None:
  session_dir = session_mgr.get_chat_events_path(meta.id).parent
  session_dir.mkdir(parents=True, exist_ok=True)
  (session_dir.parent / "threads").mkdir(parents=True, exist_ok=True)
  session_mgr._metadata_path(meta.id).write_text(meta.model_dump_json(indent=2), encoding="utf-8")
  lines = "\n".join(json.dumps(event) for event in events)
  session_mgr.get_chat_events_path(meta.id).write_text(lines + "\n", encoding="utf-8")


def _write_codex_rollout(codex_home: Path, native_thread_id: str, lines: list[dict]) -> None:
  rollout_dir = codex_home / "sessions" / "2026" / "03" / "31"
  rollout_dir.mkdir(parents=True, exist_ok=True)
  rollout_path = rollout_dir / f"rollout-2026-03-31T20-42-51-{native_thread_id}.jsonl"
  rollout_path.write_text("\n".join(json.dumps(line) for line in lines) + "\n", encoding="utf-8")


def _codex_turn_context(model: str) -> dict:
  return {
      "timestamp": "2026-03-31T20:42:51.000Z",
      "type": "turn_context",
      "payload": {"model": model},
  }


def _codex_token_count_event(
    *, timestamp: str, total_input: int, total_cached: int, total_output: int,
    last_input: int, last_cached: int, last_output: int, last_total: int) -> dict:
  """Build one codex token_count event_msg; every fixture sets window 258400."""
  return {
      "timestamp": timestamp,
      "type": "event_msg",
      "payload": {
          "type": "token_count",
          "info": {
              "total_token_usage": {
                  "input_tokens": total_input,
                  "cached_input_tokens": total_cached,
                  "output_tokens": total_output,
              },
              "last_token_usage": {
                  "input_tokens": last_input,
                  "cached_input_tokens": last_cached,
                  "output_tokens": last_output,
                  "total_tokens": last_total,
              },
              "model_context_window": 258400,
          },
      },
  }


def _seed_codex_session(
    session_mgr: SessionManager, *, session_id: str, name: str, backend: str,
    native_thread_id: str, codex_home: Path, turn_model: str, token_event: dict) -> SessionMetadata:
  """Write the session metadata, a filler user event, and a one-turn codex rollout."""
  meta = SessionMetadata(id=session_id, name=name, backend=backend, cc_session_id=native_thread_id)
  _write_session(session_mgr, meta, [
      {"type": "user", "content": "hello", "timestamp": "2026-03-31T20:42:52Z"},
  ])
  _write_codex_rollout(codex_home, native_thread_id, [
      _codex_turn_context(turn_model),
      token_event,
  ])
  return meta


def _assistant_event(model: str, input_tokens: int, cache_creation: int = 0, cache_read: int = 0,
                     parent_tool_use_id: str | None = None) -> dict:
  return {
      "type": "assistant",
      "parent_tool_use_id": parent_tool_use_id,
      "message": {
          "model": model,
          "usage": {
              "input_tokens": input_tokens,
              "cache_creation_input_tokens": cache_creation,
              "cache_read_input_tokens": cache_read,
          },
      },
  }


def _result_event(total_cost_usd, model_usage: dict | None = None, input_tokens: int = 0,
                   cache_creation: int = 0, cache_read: int = 0,
                   context_snapshot: dict | None = None) -> dict:
  return {
      "type": "result",
      "usage": {
          "input_tokens": input_tokens,
          "cache_creation_input_tokens": cache_creation,
          "cache_read_input_tokens": cache_read,
      },
      "modelUsage": model_usage or {},
      "total_cost_usd": total_cost_usd,
      **({"context_snapshot": context_snapshot} if context_snapshot is not None else {}),
  }


def _snapshot(model: str, tokens: dict, limit: dict | None) -> dict:
  """Build a context_snapshot block for a result event."""
  return {"model": model, "tokens": tokens, "limit": limit}


# ---------------------------------------------------------------------------
# Acceptance test 1: assistant event beats turn-cumulative result usage
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_claude_tier_uses_assistant_event_tokens_not_result_cumulative(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-assistant", name="Assistant", backend=OPUS_BACKEND_ID)
  # Result carries a turn-cumulative 1.5M sum; a later main-chain assistant event
  # reports a realistic per-request size.
  _write_session(session_mgr, meta, [
      _result_event(0.5, {"claude-opus-4-6": {"contextWindow": 200_000}},
                   input_tokens=1_500_000),
      _assistant_event("claude-opus-4-6", input_tokens=100_000,
                       cache_creation=20_000, cache_read=30_000),
  ])

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  assert usage["context_tokens"] == 150_000  # assistant event sum, not 1.5M
  assert usage["context_full"] == 200_000
  assert usage["model"] == "claude-opus-4-6"
  assert usage["total_cost_usd"] == pytest.approx(0.5)


# ---------------------------------------------------------------------------
# Acceptance test 1b: context_tokens reads a post-boundary compact_boundary
# ---------------------------------------------------------------------------


def _compact_boundary_event(trigger: str = "manual", pre_tokens=None, post_tokens=None) -> dict:
  meta: dict = {"trigger": trigger}
  if pre_tokens is not None:
    meta["pre_tokens"] = pre_tokens
  if post_tokens is not None:
    meta["post_tokens"] = post_tokens
  return {"type": "system", "subtype": "compact_boundary", "compact_metadata": meta}


@pytest.mark.asyncio
async def test_claude_tier_context_tokens_reads_post_tokens_after_selected_assistant(
    tmp_path: Path,
) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-postboundary", name="Post Boundary", backend=OPUS_BACKEND_ID)
  _write_session(session_mgr, meta, [
      _result_event(0.5, {"claude-opus-4-6": {"contextWindow": 200_000}}),
      _assistant_event("claude-opus-4-6", input_tokens=239_708),
      _compact_boundary_event(pre_tokens=239_708, post_tokens=4_670),
  ])

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  assert usage["context_tokens"] == 4_670
  assert usage["context_full"] == 200_000
  assert usage["model"] == "claude-opus-4-6"
  assert usage["total_cost_usd"] == pytest.approx(0.5)


@pytest.mark.asyncio
async def test_claude_tier_ignores_compact_boundary_before_selected_assistant(
    tmp_path: Path,
) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-preboundary", name="Pre Boundary", backend=OPUS_BACKEND_ID)
  _write_session(session_mgr, meta, [
      _result_event(0.5, {"claude-opus-4-6": {"contextWindow": 200_000}}),
      _compact_boundary_event(pre_tokens=239_708, post_tokens=4_670),
      _assistant_event("claude-opus-4-6", input_tokens=100_000),
  ])

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  # Boundary precedes the chosen assistant event -- reading stays the assistant sum.
  assert usage["context_tokens"] == 100_000
  assert usage["context_full"] == 200_000
  assert usage["model"] == "claude-opus-4-6"
  assert usage["total_cost_usd"] == pytest.approx(0.5)


@pytest.mark.asyncio
async def test_claude_tier_boundary_without_post_tokens_leaves_reading_alone(
    tmp_path: Path,
) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-noboundarytokens", name="No Post Tokens", backend=OPUS_BACKEND_ID)
  _write_session(session_mgr, meta, [
      _result_event(0.5, {"claude-opus-4-6": {"contextWindow": 200_000}}),
      _assistant_event("claude-opus-4-6", input_tokens=100_000),
      # opencode's synthesized shape: trigger + pre_tokens only, no post_tokens.
      _compact_boundary_event(trigger="auto", pre_tokens=50_000),
  ])

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  assert usage["context_tokens"] == 100_000
  assert usage["context_full"] == 200_000
  assert usage["model"] == "claude-opus-4-6"
  assert usage["total_cost_usd"] == pytest.approx(0.5)


@pytest.mark.asyncio
async def test_claude_tier_admission_unaffected_by_boundary_only_events(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-boundaryonly", name="Boundary Only", backend=OPUS_BACKEND_ID)
  _write_session(session_mgr, meta, [
      _compact_boundary_event(pre_tokens=239_708, post_tokens=4_670),
      {"type": "user", "content": "hello", "timestamp": "2026-03-31T20:42:52Z"},
  ])

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  # No qualifying assistant event exists, so the claude tier does not admit --
  # falls through to the no-source tier (context fields None).
  assert usage is not None
  assert usage["context_tokens"] is None
  assert usage["context_full"] is None
  assert usage["context_compact_at"] is None
  assert usage["model"] == ""


# ---------------------------------------------------------------------------
# Acceptance test 2: context_full from assistant model, not dict order
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_claude_tier_resolves_context_full_from_assistant_model_not_dict_order(
    tmp_path: Path,
) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-modelorder", name="Model Order", backend=OPUS_BACKEND_ID)
  # modelUsage lists a small-window sub-model FIRST; the assistant event's model
  # is the real (second) model.
  _write_session(session_mgr, meta, [
      _result_event(0.4, {
          "small-sub-model": {"contextWindow": 50_000},
          "claude-opus-4-6": {"contextWindow": 200_000},
      }),
      _assistant_event("claude-opus-4-6", input_tokens=80_000),
  ])

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  assert usage["context_full"] == 200_000  # from the real model, not 50_000


# ---------------------------------------------------------------------------
# Acceptance test 3: modelUsage absent -> context_full is the declared window
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_claude_tier_context_full_is_declared_window_when_model_usage_absent(
    tmp_path: Path,
) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-nomodel", name="No Model Usage", backend=OPUS_BACKEND_ID)
  _write_session(session_mgr, meta, [
      _result_event(0.3),  # no modelUsage
      _assistant_event("claude-opus-4-6", input_tokens=70_000),
  ])

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  declared_window, compact_point = headless_claude_declared_window()
  assert usage["context_full"] == declared_window
  assert usage["context_compact_at"] == compact_point


# ---------------------------------------------------------------------------
# Acceptance test 4: sub-agent and synthetic events are ignored
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_claude_tier_ignores_subagent_and_synthetic_assistant_events(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-ignore", name="Ignore", backend=OPUS_BACKEND_ID)
  _write_session(session_mgr, meta, [
      # sub-agent event with large usage — must be ignored (parent_tool_use_id set)
      _assistant_event("claude-opus-4-6", input_tokens=400_000, parent_tool_use_id="tool_1"),
      # synthetic zero-usage event — must be ignored (prompt-token sum is 0)
      _assistant_event("claude-opus-4-6", input_tokens=0),
      # the real main-chain event — must be picked
      _assistant_event("claude-opus-4-6", input_tokens=90_000, cache_read=10_000),
  ])

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  assert usage["context_tokens"] == 100_000  # the real event, not 400_000


# ---------------------------------------------------------------------------
# Acceptance test 5: result events but no usable assistant usage -> None fields
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_no_source_tier_when_results_but_no_assistant_usage(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-nosource", name="No Source", backend=OPUS_BACKEND_ID)
  _write_session(session_mgr, meta, [
      _result_event(0.5, {"claude-opus-4-6": {"contextWindow": 200_000}}, input_tokens=1000),
      # only sub-agent / zero-usage assistant events — not usable
      _assistant_event("claude-opus-4-6", input_tokens=0),
      _assistant_event("claude-opus-4-6", input_tokens=5000, parent_tool_use_id="tool_1"),
  ])

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  assert usage["context_tokens"] is None
  assert usage["context_full"] is None
  assert usage["context_compact_at"] is None
  assert usage["model"] == ""
  assert usage["total_cost_usd"] == pytest.approx(0.5)


# ---------------------------------------------------------------------------
# Acceptance test 6: cost 0 -> None; positive cost sums
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_cost_is_none_when_all_results_report_zero(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-zerocost", name="Zero Cost", backend=OPUS_BACKEND_ID)
  _write_session(session_mgr, meta, [
      _result_event(0.0, {"claude-opus-4-6": {"contextWindow": 200_000}}, input_tokens=1000),
      _result_event(0.0, {"claude-opus-4-6": {"contextWindow": 200_000}}, input_tokens=2000),
      _assistant_event("claude-opus-4-6", input_tokens=50_000),
  ])

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  assert usage["total_cost_usd"] is None


@pytest.mark.asyncio
async def test_cost_sums_positive_results_across_full_list(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-poscost", name="Positive Cost", backend=OPUS_BACKEND_ID)
  _write_session(session_mgr, meta, [
      _result_event(0.10, {"claude-opus-4-6": {"contextWindow": 200_000}}, input_tokens=1000),
      _result_event(0.20, {"claude-opus-4-6": {"contextWindow": 200_000}}, input_tokens=2000),
      _assistant_event("claude-opus-4-6", input_tokens=50_000),
  ])

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  assert usage["total_cost_usd"] == pytest.approx(0.30)


# ---------------------------------------------------------------------------
# Acceptance test 7: cost computed over the whole list; events param is gone
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_public_entry_point_has_no_events_parameter(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-wholelist", name="Whole List", backend=OPUS_BACKEND_ID)
  # More than 40 result events with cost, so a tail would under-count.
  events = [_result_event(0.01, {"claude-opus-4-6": {"contextWindow": 200_000}}, input_tokens=i)
            for i in range(50)]
  events.append(_assistant_event("claude-opus-4-6", input_tokens=10_000))
  _write_session(session_mgr, meta, events)

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  # Full list has 50 result events at 0.01 each = 0.50.
  assert usage["total_cost_usd"] == pytest.approx(0.50)
  # The events parameter is gone — passing a tail must be rejected.
  with pytest.raises(TypeError):
    await session_mgr.resolve_session_usage(meta.id, meta, events=events[:5])


# ---------------------------------------------------------------------------
# Acceptance test 8: headless_claude_declared_window
# ---------------------------------------------------------------------------


@pytest.fixture
def _clean_ceiling_env(monkeypatch: pytest.MonkeyPatch) -> None:
  """Remove env vars that would change the declared window so each test starts clean."""
  for name in ("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE",
               "CLAUDE_CODE_MAX_CONTEXT_TOKENS"):
    monkeypatch.delenv(name, raising=False)


@pytest.mark.usefixtures("_clean_ceiling_env")
def test_declared_window_subtracts_reserves_from_declared_window(monkeypatch) -> None:
  monkeypatch.setenv("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "500000")
  expected_point = 500_000 - CLAUDE_COMPACT_OUTPUT_RESERVE - CLAUDE_COMPACT_CONTEXT_RESERVE
  assert headless_claude_declared_window() == (500_000, expected_point)


@pytest.mark.usefixtures("_clean_ceiling_env")
def test_declared_window_follows_host_export_of_different_window(monkeypatch) -> None:
  monkeypatch.setenv("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "1000000")
  expected_point = 1_000_000 - CLAUDE_COMPACT_OUTPUT_RESERVE - CLAUDE_COMPACT_CONTEXT_RESERVE
  assert headless_claude_declared_window() == (1_000_000, expected_point)
  assert headless_claude_declared_window() != (500_000, 500_000 - 33_000)


@pytest.mark.parametrize("override_var", [
    "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE",
    "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
])
@pytest.mark.usefixtures("_clean_ceiling_env")
def test_declared_window_returns_none_compact_point_when_override_present_and_warns(
    monkeypatch, capsys, override_var) -> None:
  monkeypatch.setenv("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "500000")
  monkeypatch.setenv(override_var, "1")
  result = headless_claude_declared_window()
  assert result == (500_000, None)  # declared window, no compaction point
  out = capsys.readouterr().out
  assert override_var in out
  assert "declared_window" in out.lower()


@pytest.mark.usefixtures("_clean_ceiling_env")
def test_declared_window_returns_default_when_window_unparseable_and_warns(
    monkeypatch, capsys) -> None:
  monkeypatch.setenv("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "not-a-number")
  default_window = int(HEADLESS_CLAUDE_DEFAULT_ENV["CLAUDE_CODE_AUTO_COMPACT_WINDOW"])
  expected_point = default_window - CLAUDE_COMPACT_OUTPUT_RESERVE - CLAUDE_COMPACT_CONTEXT_RESERVE
  result = headless_claude_declared_window()
  assert result == (default_window, expected_point)
  out = capsys.readouterr().out
  assert "CLAUDE_CODE_AUTO_COMPACT_WINDOW" in out
  assert "declared_window" in out.lower()


# ---------------------------------------------------------------------------
# Acceptance test 8b: claude tier full / compact point per effective window
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
@pytest.mark.usefixtures("_clean_ceiling_env")
async def test_claude_tier_full_and_point_for_1m_window_model(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-1m", name="1M Window", backend=OPUS_BACKEND_ID)
  _write_session(session_mgr, meta, [
      _result_event(0.5, {"claude-opus-4-6": {"contextWindow": 1_000_000}}),
      _assistant_event("claude-opus-4-6", input_tokens=72_900),
  ])

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  # min(1M model window, 433000 declared) = 433000; point = 433000 - 20000 - 13000.
  assert usage["context_full"] == 433_000
  assert usage["context_compact_at"] == 400_000


@pytest.mark.asyncio
@pytest.mark.usefixtures("_clean_ceiling_env")
async def test_claude_tier_full_and_point_for_200k_window_model(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-200k", name="200k Window", backend=OPUS_BACKEND_ID)
  _write_session(session_mgr, meta, [
      _result_event(0.5, {"claude-opus-4-6": {"contextWindow": 200_000}}),
      _assistant_event("claude-opus-4-6", input_tokens=72_900),
  ])

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  # min(200000 model window, 433000 declared) = 200000; point = 200000 - 20000 - 13000.
  assert usage["context_full"] == 200_000
  assert usage["context_compact_at"] == 167_000


@pytest.mark.asyncio
@pytest.mark.usefixtures("_clean_ceiling_env")
async def test_claude_tier_point_none_under_forwarded_unmodelled_override(
    tmp_path: Path, monkeypatch) -> None:
  monkeypatch.setenv("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "433000")
  monkeypatch.setenv("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE", "1")
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-override", name="Override", backend=OPUS_BACKEND_ID)
  _write_session(session_mgr, meta, [
      _result_event(0.5, {"claude-opus-4-6": {"contextWindow": 1_000_000}}),
      _assistant_event("claude-opus-4-6", input_tokens=72_900),
  ])

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  assert usage["context_full"] == 433_000
  assert usage["context_compact_at"] is None  # degraded override -> no compaction line


# ---------------------------------------------------------------------------
# Acceptance test 10: snapshot tier (opencode context_snapshot)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_snapshot_tier_full_and_point_for_limit_with_input(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-snap-input", name="Snapshot Input", backend="opencode-glm52")
  _write_session(session_mgr, meta, [
      _result_event(
          0.5,
          context_snapshot=_snapshot(
              SYNTHETIC_MODEL,
              {"input": 100_000, "output": 5_000, "reasoning": 2_000,
               "cache_read": 30_000, "cache_write": 10_000},
              {"context": 409_600, "input": 270_000, "output": 131_072})),
  ])

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  # full = limit.input = 270000; point = limit.input - min(20000, limit.output) = 270000 - 20000.
  assert usage["context_full"] == 270_000
  assert usage["context_compact_at"] == 250_000
  assert usage["context_tokens"] == 100_000 + 5_000 + 2_000 + 30_000 + 10_000
  assert usage["model"] == SYNTHETIC_MODEL


@pytest.mark.asyncio
async def test_snapshot_tier_full_for_limit_without_input(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-snap-noinput", name="Snapshot No Input", backend="opencode-glm52")
  _write_session(session_mgr, meta, [
      _result_event(
          0.5,
          context_snapshot=_snapshot(
              SYNTHETIC_MODEL,
              {"input": 100_000, "output": 5_000, "reasoning": 2_000,
               "cache_read": 30_000, "cache_write": 10_000},
              {"context": 409_600, "input": None, "output": 131_072})),
  ])

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  # full = limit.context - limit.output = 409600 - 131072; point = None (no input).
  assert usage["context_full"] == 409_600 - 131_072
  assert usage["context_compact_at"] is None
  assert usage["context_tokens"] == 100_000 + 5_000 + 2_000 + 30_000 + 10_000


@pytest.mark.asyncio
async def test_snapshot_tier_with_none_limit_yields_all_none_context(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-snap-nolimit", name="Snapshot No Limit", backend="opencode-glm52")
  _write_session(session_mgr, meta, [
      _result_event(
          0.5,
          context_snapshot=_snapshot(
              SYNTHETIC_MODEL,
              {"input": 100_000, "output": 5_000, "reasoning": 2_000,
               "cache_read": 30_000, "cache_write": 10_000},
              None)),
  ])

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  assert usage["context_full"] is None
  assert usage["context_compact_at"] is None
  # context_tokens still resolved from the snapshot's token sum.
  assert usage["context_tokens"] == 100_000 + 5_000 + 2_000 + 30_000 + 10_000
  assert usage["model"] == SYNTHETIC_MODEL


@pytest.mark.asyncio
async def test_snapshot_tier_uses_newest_result_event_carrying_snapshot(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-snap-newest", name="Snapshot Newest", backend="opencode-glm52")
  _write_session(session_mgr, meta, [
      _result_event(
          0.1,
          context_snapshot=_snapshot(
              "old/model",
              {"input": 10_000, "output": 1_000, "reasoning": 0,
               "cache_read": 0, "cache_write": 0},
              {"context": 200_000, "input": 150_000, "output": 40_000})),
      _result_event(
          0.2,
          context_snapshot=_snapshot(
              "new/model",
              {"input": 20_000, "output": 2_000, "reasoning": 3_000,
               "cache_read": 4_000, "cache_write": 5_000},
              {"context": 409_600, "input": 270_000, "output": 131_072})),
  ])

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  assert usage["model"] == "new/model"
  assert usage["context_full"] == 270_000
  assert usage["context_tokens"] == 20_000 + 2_000 + 3_000 + 4_000 + 5_000


# ---------------------------------------------------------------------------
# Acceptance test 10a: snapshot tier is decoupled from Claude Code's reserves
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
@pytest.mark.usefixtures("_clean_ceiling_env")
async def test_snapshot_tier_compact_at_ignores_claude_constants_but_claude_tier_follows_them(
    tmp_path: Path, monkeypatch) -> None:
  # Move Claude Code's reserves to a value clearly different from their defaults
  # (20000 / 13000); the snapshot tier must not move, the claude tier must.
  monkeypatch.setattr("src.core.session_usage.CLAUDE_COMPACT_OUTPUT_RESERVE", 50_000)
  monkeypatch.setattr("src.core.session_usage.CLAUDE_COMPACT_CONTEXT_RESERVE", 50_000)
  monkeypatch.setenv("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "600000")

  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)

  # Snapshot (opencode) tier — same limit as the existing 270000 / 250000 case.
  snap_meta = SessionMetadata(id="session-decouple-snap", name="Snap Decouple", backend="opencode-glm52")
  _write_session(session_mgr, snap_meta, [
      _result_event(
          0.5,
          context_snapshot=_snapshot(
              SYNTHETIC_MODEL,
              {"input": 100_000, "output": 5_000, "reasoning": 2_000,
               "cache_read": 30_000, "cache_write": 10_000},
              {"context": 409_600, "input": 270_000, "output": 131_072})),
  ])
  snap_usage = await session_mgr.resolve_session_usage(snap_meta.id, snap_meta)

  assert snap_usage is not None
  # 270000 - min(20000, 131072) = 250000 — opencode's own reserve, unmoved by the
  # Claude-constant monkeypatch above.
  assert snap_usage["context_full"] == 270_000
  assert snap_usage["context_compact_at"] == 250_000

  # Claude tier — its point follows the monkeypatched Claude constants.
  claude_meta = SessionMetadata(id="session-decouple-claude", name="Claude Decouple", backend=OPUS_BACKEND_ID)
  _write_session(session_mgr, claude_meta, [
      _result_event(0.5, {"claude-opus-4-6": {"contextWindow": 600_000}}),
      _assistant_event("claude-opus-4-6", input_tokens=72_900),
  ])
  claude_usage = await session_mgr.resolve_session_usage(claude_meta.id, claude_meta)

  assert claude_usage is not None
  # context_full = min(600000 model window, 600000 declared) = 600000; with the real
  # defaults (20000 / 13000) compact_at would be 567000, but the patched 50000 / 50000
  # moves it to 500000 — the claude tier follows Claude Code's constants.
  assert claude_usage["context_full"] == 600_000
  assert claude_usage["context_compact_at"] == 500_000


# ---------------------------------------------------------------------------
# Acceptance test 10b: non-int output degrades instead of raising
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_snapshot_tier_with_non_int_output_degrades_to_none(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-snap-null-output", name="Snapshot Null Output", backend="opencode-glm52")
  _write_session(session_mgr, meta, [
      _result_event(
          0.5,
          context_snapshot=_snapshot(
              SYNTHETIC_MODEL,
              {"input": 100_000, "output": 5_000, "reasoning": 2_000,
               "cache_read": 30_000, "cache_write": 10_000},
              {"context": 409_600, "input": 270_000, "output": None})),
  ])

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  # A non-int output degrades both limit-derived context fields to None instead of
  # raising TypeError from min(); the token sum is still resolved.
  assert usage["context_full"] is None
  assert usage["context_compact_at"] is None
  assert usage["context_tokens"] == 100_000 + 5_000 + 2_000 + 30_000 + 10_000
  assert usage["model"] == SYNTHETIC_MODEL


# ---------------------------------------------------------------------------
# Acceptance test 9: codex candidate directories + model_auto_compact_token_limit
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_codex_rollout_resolves_via_other_backend_when_session_backend_absent(
    tmp_path: Path,
) -> None:
  # The session's own backend ("codex-old") is absent from config; another
  # configured codex backend points at the tree holding the rollout.
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path,
      backend_options=[
          BackendOption(id=OPUS_BACKEND_ID, label="Claude", type="cc-claude",
                        model="claude-opus-4-6"),
          BackendOption(id="codex-new", label="Codex New", type="codex", model="gpt-5.5",
                        codex_home=str(tmp_path / "codex-tree")),
      ],
  )
  session_mgr = SessionManager(cfg)
  # "codex-old" is absent from config but starts with "codex", so is_codex_backend
  # admits it via the prefix fallback while rollout lookup walks the other backend's tree.
  meta = _seed_codex_session(
      session_mgr,
      session_id="session-codex-old",
      name="Codex Old Backend",
      backend="codex-old",
      native_thread_id="019d45a2-836d-7552-a54f-3c6c5511e502",
      codex_home=tmp_path / "codex-tree",
      turn_model="gpt-5.5",
      token_event=_codex_token_count_event(
          timestamp="2026-03-31T20:43:12.454Z",
          total_input=1_431_555, total_cached=1_126_656, total_output=16_521,
          last_input=179_319, last_cached=176_640, last_output=1_732, last_total=181_051),
  )

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  assert usage["context_tokens"] == 179319
  assert usage["context_full"] == 258400  # model_context_window (no auto-compact configured)
  assert usage["context_compact_at"] is None  # unconfigured — no compaction line
  assert usage["model"] == "gpt-5.5"


# ---------------------------------------------------------------------------
# Acceptance test 9b: codex unconfigured compaction logs no warning
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_codex_unconfigured_compaction_logs_no_warning(tmp_path: Path, capsys) -> None:
  cfg = _build_cfg(tmp_path, codex_home=str(tmp_path / "codex-tree"))
  # _build_cfg creates the codex backend WITHOUT model_auto_compact_token_limit.
  session_mgr = SessionManager(cfg)
  meta = _seed_codex_session(
      session_mgr,
      session_id="session-codex-unconfigured",
      name="Codex Unconfigured",
      backend="codex-test",
      native_thread_id="019d45a2-836d-7552-a54f-3c6c5511e5ee",
      codex_home=tmp_path / "codex-tree",
      turn_model="gpt-5.5",
      token_event=_codex_token_count_event(
          timestamp="2026-03-31T20:43:12.454Z",
          total_input=1_431_555, total_cached=1_126_656, total_output=16_521,
          last_input=179_319, last_cached=176_640, last_output=1_732, last_total=181_051),
  )

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  assert usage["context_full"] == 258400
  assert usage["context_compact_at"] is None  # unconfigured — no compaction line
  captured = capsys.readouterr().out
  assert "auto_compact" not in captured
  assert "degrad" not in captured.lower()


# ---------------------------------------------------------------------------
# Acceptance test 9c: codex full = model_context_window, point = configured limit
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_codex_context_compact_at_uses_auto_compact_limit_when_configured(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path, codex_home=str(tmp_path / "codex-tree"),
                  model_auto_compact_token_limit=180_000)
  session_mgr = SessionManager(cfg)
  meta = _seed_codex_session(
      session_mgr,
      session_id="session-autocompact",
      name="Auto Compact",
      backend="codex-test",
      native_thread_id="019d26e4-be1c-7171-a3fd-6f1ab10662de",
      codex_home=tmp_path / "codex-tree",
      turn_model="gpt-5.5",
      token_event=_codex_token_count_event(
          timestamp="2026-03-25T21:32:09.989Z",
          total_input=1_099_429, total_cached=1_001_472, total_output=15_186,
          last_input=176_028, last_cached=168_832, last_output=782, last_total=176_810),
  )

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  assert usage["context_full"] == 258400  # model_context_window
  assert usage["context_compact_at"] == 180_000  # auto-compact limit
  assert usage["context_tokens"] == 176028
  assert usage["model"] == "gpt-5.5"


# ---------------------------------------------------------------------------
# Codex native cost computation (unchanged behavior)
# ---------------------------------------------------------------------------


def test_extract_codex_rollout_usage_event_uses_last_input_tokens() -> None:
  usage = _extract_codex_rollout_usage_event(
      _codex_token_count_event(
          timestamp="2026-03-25T17:50:36.349Z",
          total_input=1_252_236, total_cached=950_016, total_output=14_789,
          last_input=176_028, last_cached=168_832, last_output=782, last_total=176_810)
  )

  assert usage == {
      "context_tokens": 176028,
      "context_full": 258400,
      "total_token_usage": {
          "input_tokens": 1252236,
          "cached_input_tokens": 950016,
          "output_tokens": 14789,
      },
  }


@pytest.mark.asyncio
async def test_codex_native_cost_sums_cumulative_tokens(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path, codex_home=str(tmp_path / "codex-tree"))
  session_mgr = SessionManager(cfg)
  meta = _seed_codex_session(
      session_mgr,
      session_id="session-codex-cost",
      name="Codex Cost Session",
      backend="codex-test",
      native_thread_id="019d9f9e-5d7a-7f44-81a8-e9cb8261a51d",
      codex_home=tmp_path / "codex-tree",
      turn_model="gpt-5.5",
      token_event=_codex_token_count_event(
          timestamp="2026-03-31T20:43:12.454Z",
          total_input=1_951_892, total_cached=1_858_304, total_output=15_209,
          last_input=179_319, last_cached=176_640, last_output=1_732, last_total=181_051),
  )

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  assert usage["total_cost_usd"] == pytest.approx(1.85, abs=0.01)
  assert usage["model"] == "gpt-5.5"


@pytest.mark.asyncio
async def test_codex_native_cost_is_none_for_unknown_model(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path, codex_home=str(tmp_path / "codex-tree"))
  session_mgr = SessionManager(cfg)
  meta = _seed_codex_session(
      session_mgr,
      session_id="session-codex-unknown-cost",
      name="Codex Unknown Cost Session",
      backend="codex-test",
      native_thread_id="019d9fb0-c3f8-72ec-9d8d-0ed32d404b30",
      codex_home=tmp_path / "codex-tree",
      turn_model="gpt-unknown",
      token_event=_codex_token_count_event(
          timestamp="2026-03-31T20:43:12.454Z",
          total_input=1_951_892, total_cached=1_858_304, total_output=15_209,
          last_input=179_319, last_cached=176_640, last_output=1_732, last_total=181_051),
  )

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  assert usage["total_cost_usd"] is None
  assert usage["model"] == "gpt-unknown"


# ---------------------------------------------------------------------------
# Empty event list -> None
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_empty_event_list_returns_none(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-empty", name="Empty", backend=OPUS_BACKEND_ID)
  _write_session(session_mgr, meta, [])

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is None
