"""Focused tests for session usage resolution."""

import json
from pathlib import Path

import pytest

from src.agents.backends.claude_code import (
    CLAUDE_COMPACT_CONTEXT_RESERVE,
    CLAUDE_COMPACT_OUTPUT_RESERVE,
    HEADLESS_CLAUDE_DEFAULT_ENV,
    headless_claude_declared_window,
)
from src.core.config import CharlieBotConfig
from src.core.codex_usage import _extract_codex_rollout_usage_event
from src.core.models import BackendOption, SessionMetadata
from src.core.sessions import SessionManager


def _build_cfg(tmp_path: Path, **codex_kwargs) -> CharlieBotConfig:
  codex_opt = BackendOption(
      id="codex-test", label="Codex", type="codex", model="codex-test-model", **codex_kwargs)
  return CharlieBotConfig(
      charliebot_home=tmp_path,
      backend_options=[
          BackendOption(id="claude-opus-4.6", label="Claude", type="cc-claude", model="claude-opus-4-6"),
          codex_opt,
      ],
  )


def _write_session(session_mgr: SessionManager, meta: SessionMetadata, events: list[dict]) -> None:
  session_dir = session_mgr._chat_events_path(meta.id).parent
  session_dir.mkdir(parents=True, exist_ok=True)
  (session_dir.parent / "threads").mkdir(parents=True, exist_ok=True)
  session_mgr._metadata_path(meta.id).write_text(meta.model_dump_json(indent=2), encoding="utf-8")
  lines = "\n".join(json.dumps(event) for event in events)
  session_mgr._chat_events_path(meta.id).write_text(lines + "\n", encoding="utf-8")


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
  meta = SessionMetadata(id="session-assistant", name="Assistant", backend="claude-opus-4.6")
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
# Acceptance test 2: context_full from assistant model, not dict order
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_claude_tier_resolves_context_full_from_assistant_model_not_dict_order(
    tmp_path: Path,
) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-modelorder", name="Model Order", backend="claude-opus-4.6")
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
  meta = SessionMetadata(id="session-nomodel", name="No Model Usage", backend="claude-opus-4.6")
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
  meta = SessionMetadata(id="session-ignore", name="Ignore", backend="claude-opus-4.6")
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
  meta = SessionMetadata(id="session-nosource", name="No Source", backend="claude-opus-4.6")
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
  meta = SessionMetadata(id="session-zerocost", name="Zero Cost", backend="claude-opus-4.6")
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
  meta = SessionMetadata(id="session-poscost", name="Positive Cost", backend="claude-opus-4.6")
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
  meta = SessionMetadata(id="session-wholelist", name="Whole List", backend="claude-opus-4.6")
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


def test_declared_window_subtracts_reserves_from_declared_window(_clean_ceiling_env, monkeypatch) -> None:
  monkeypatch.setenv("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "500000")
  expected_point = 500_000 - CLAUDE_COMPACT_OUTPUT_RESERVE - CLAUDE_COMPACT_CONTEXT_RESERVE
  assert headless_claude_declared_window() == (500_000, expected_point)


def test_declared_window_follows_host_export_of_different_window(_clean_ceiling_env, monkeypatch) -> None:
  monkeypatch.setenv("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "1000000")
  expected_point = 1_000_000 - CLAUDE_COMPACT_OUTPUT_RESERVE - CLAUDE_COMPACT_CONTEXT_RESERVE
  assert headless_claude_declared_window() == (1_000_000, expected_point)
  assert headless_claude_declared_window() != (500_000, 500_000 - 33_000)


@pytest.mark.parametrize("override_var", [
    "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE",
    "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
])
def test_declared_window_returns_none_compact_point_when_override_present_and_warns(
    _clean_ceiling_env, monkeypatch, capsys, override_var) -> None:
  monkeypatch.setenv("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "500000")
  monkeypatch.setenv(override_var, "1")
  result = headless_claude_declared_window()
  assert result == (500_000, None)  # declared window, no compaction point
  out = capsys.readouterr().out
  assert override_var in out
  assert "declared_window" in out.lower()


def test_declared_window_returns_default_when_window_unparseable_and_warns(
    _clean_ceiling_env, monkeypatch, capsys) -> None:
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
async def test_claude_tier_full_and_point_for_1m_window_model(tmp_path: Path, _clean_ceiling_env) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-1m", name="1M Window", backend="claude-opus-4.6")
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
async def test_claude_tier_full_and_point_for_200k_window_model(tmp_path: Path, _clean_ceiling_env) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-200k", name="200k Window", backend="claude-opus-4.6")
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
async def test_claude_tier_point_none_under_forwarded_unmodelled_override(
    tmp_path: Path, _clean_ceiling_env, monkeypatch) -> None:
  monkeypatch.setenv("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "433000")
  monkeypatch.setenv("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE", "1")
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-override", name="Override", backend="claude-opus-4.6")
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
              "meshy-sglang-glm52/nvidia/GLM-5.2-NVFP4",
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
  assert usage["model"] == "meshy-sglang-glm52/nvidia/GLM-5.2-NVFP4"


@pytest.mark.asyncio
async def test_snapshot_tier_full_for_limit_without_input(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-snap-noinput", name="Snapshot No Input", backend="opencode-glm52")
  _write_session(session_mgr, meta, [
      _result_event(
          0.5,
          context_snapshot=_snapshot(
              "meshy-sglang-glm52/nvidia/GLM-5.2-NVFP4",
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
              "meshy-sglang-glm52/nvidia/GLM-5.2-NVFP4",
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
  assert usage["model"] == "meshy-sglang-glm52/nvidia/GLM-5.2-NVFP4"


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
          BackendOption(id="claude-opus-4.6", label="Claude", type="cc-claude",
                        model="claude-opus-4-6"),
          BackendOption(id="codex-new", label="Codex New", type="codex", model="gpt-5.5",
                        codex_home=str(tmp_path / "codex-tree")),
      ],
  )
  session_mgr = SessionManager(cfg)
  native_thread_id = "019d45a2-836d-7552-a54f-3c6c5511e502"
  meta = SessionMetadata(
      id="session-codex-old",
      name="Codex Old Backend",
      backend="codex-old",  # absent from config, but starts with "codex"
      cc_session_id=native_thread_id,
  )
  _write_session(session_mgr, meta, [
      {"type": "user", "content": "hello", "timestamp": "2026-03-31T20:42:52Z"},
  ])
  _write_codex_rollout(tmp_path / "codex-tree", native_thread_id, [
      _codex_turn_context("gpt-5.5"),
      {
          "timestamp": "2026-03-31T20:43:12.454Z",
          "type": "event_msg",
          "payload": {
              "type": "token_count",
              "info": {
                  "total_token_usage": {
                      "input_tokens": 1431555,
                      "cached_input_tokens": 1126656,
                      "output_tokens": 16521,
                  },
                  "last_token_usage": {
                      "input_tokens": 179319,
                      "cached_input_tokens": 176640,
                      "output_tokens": 1732,
                      "total_tokens": 181051,
                  },
                  "model_context_window": 258400,
              },
          },
      },
  ])

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
  native_thread_id = "019d45a2-836d-7552-a54f-3c6c5511e5ee"
  meta = SessionMetadata(
      id="session-codex-unconfigured",
      name="Codex Unconfigured",
      backend="codex-test",
      cc_session_id=native_thread_id,
  )
  _write_session(session_mgr, meta, [
      {"type": "user", "content": "hello", "timestamp": "2026-03-31T20:42:52Z"},
  ])
  _write_codex_rollout(tmp_path / "codex-tree", native_thread_id, [
      _codex_turn_context("gpt-5.5"),
      {
          "timestamp": "2026-03-31T20:43:12.454Z",
          "type": "event_msg",
          "payload": {
              "type": "token_count",
              "info": {
                  "total_token_usage": {
                      "input_tokens": 1431555,
                      "cached_input_tokens": 1126656,
                      "output_tokens": 16521,
                  },
                  "last_token_usage": {
                      "input_tokens": 179319,
                      "cached_input_tokens": 176640,
                      "output_tokens": 1732,
                      "total_tokens": 181051,
                  },
                  "model_context_window": 258400,
              },
          },
      },
  ])

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
  native_thread_id = "019d26e4-be1c-7171-a3fd-6f1ab10662de"
  meta = SessionMetadata(
      id="session-autocompact",
      name="Auto Compact",
      backend="codex-test",
      cc_session_id=native_thread_id,
  )
  _write_session(session_mgr, meta, [
      {"type": "user", "content": "hello", "timestamp": "2026-03-25T21:26:57Z"},
  ])
  _write_codex_rollout(tmp_path / "codex-tree", native_thread_id, [
      _codex_turn_context("gpt-5.5"),
      {
          "timestamp": "2026-03-25T21:32:09.989Z",
          "type": "event_msg",
          "payload": {
              "type": "token_count",
              "info": {
                  "total_token_usage": {
                      "input_tokens": 1099429,
                      "cached_input_tokens": 1001472,
                      "output_tokens": 15186,
                  },
                  "last_token_usage": {
                      "input_tokens": 176028,
                      "cached_input_tokens": 168832,
                      "output_tokens": 782,
                      "total_tokens": 176810,
                  },
                  "model_context_window": 258400,
              },
          },
      },
  ])

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
      {
          "timestamp": "2026-03-25T17:50:36.349Z",
          "type": "event_msg",
          "payload": {
              "type": "token_count",
              "info": {
                  "total_token_usage": {
                      "input_tokens": 1252236,
                      "cached_input_tokens": 950016,
                      "output_tokens": 14789,
                  },
                  "last_token_usage": {
                      "input_tokens": 176028,
                      "cached_input_tokens": 168832,
                      "output_tokens": 782,
                      "total_tokens": 176810,
                  },
                  "model_context_window": 258400,
              },
          },
      }
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
  native_thread_id = "019d9f9e-5d7a-7f44-81a8-e9cb8261a51d"
  meta = SessionMetadata(
      id="session-codex-cost",
      name="Codex Cost Session",
      backend="codex-test",
      cc_session_id=native_thread_id,
  )
  _write_session(session_mgr, meta, [
      {"type": "user", "content": "hello", "timestamp": "2026-03-31T20:42:52Z"},
  ])
  _write_codex_rollout(tmp_path / "codex-tree", native_thread_id, [
      _codex_turn_context("gpt-5.5"),
      {
          "timestamp": "2026-03-31T20:43:12.454Z",
          "type": "event_msg",
          "payload": {
              "type": "token_count",
              "info": {
                  "total_token_usage": {
                      "input_tokens": 1951892,
                      "cached_input_tokens": 1858304,
                      "output_tokens": 15209,
                  },
                  "last_token_usage": {
                      "input_tokens": 179319,
                      "cached_input_tokens": 176640,
                      "output_tokens": 1732,
                      "total_tokens": 181051,
                  },
                  "model_context_window": 258400,
              },
          },
      },
  ])

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  assert usage["total_cost_usd"] == pytest.approx(1.85, abs=0.01)
  assert usage["model"] == "gpt-5.5"


@pytest.mark.asyncio
async def test_codex_native_cost_is_none_for_unknown_model(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path, codex_home=str(tmp_path / "codex-tree"))
  session_mgr = SessionManager(cfg)
  native_thread_id = "019d9fb0-c3f8-72ec-9d8d-0ed32d404b30"
  meta = SessionMetadata(
      id="session-codex-unknown-cost",
      name="Codex Unknown Cost Session",
      backend="codex-test",
      cc_session_id=native_thread_id,
  )
  _write_session(session_mgr, meta, [
      {"type": "user", "content": "hello", "timestamp": "2026-03-31T20:42:52Z"},
  ])
  _write_codex_rollout(tmp_path / "codex-tree", native_thread_id, [
      _codex_turn_context("gpt-unknown"),
      {
          "timestamp": "2026-03-31T20:43:12.454Z",
          "type": "event_msg",
          "payload": {
              "type": "token_count",
              "info": {
                  "total_token_usage": {
                      "input_tokens": 1951892,
                      "cached_input_tokens": 1858304,
                      "output_tokens": 15209,
                  },
                  "last_token_usage": {
                      "input_tokens": 179319,
                      "cached_input_tokens": 176640,
                      "output_tokens": 1732,
                      "total_tokens": 181051,
                  },
                  "model_context_window": 258400,
              },
          },
      },
  ])

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
  meta = SessionMetadata(id="session-empty", name="Empty", backend="claude-opus-4.6")
  _write_session(session_mgr, meta, [])

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is None
