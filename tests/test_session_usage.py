"""Focused tests for session usage resolution."""

import json
from pathlib import Path

import pytest

from src.agents.backends.claude_code import (
    CLAUDE_COMPACT_CONTEXT_RESERVE,
    CLAUDE_COMPACT_OUTPUT_RESERVE,
    HEADLESS_CLAUDE_DEFAULT_ENV,
    headless_claude_context_ceiling,
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
                  cache_creation: int = 0, cache_read: int = 0) -> dict:
  return {
      "type": "result",
      "usage": {
          "input_tokens": input_tokens,
          "cache_creation_input_tokens": cache_creation,
          "cache_read_input_tokens": cache_read,
      },
      "modelUsage": model_usage or {},
      "total_cost_usd": total_cost_usd,
  }


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
  assert usage["context_limit"] == 200_000
  assert usage["model"] == "claude-opus-4-6"
  assert usage["total_cost_usd"] == pytest.approx(0.5)


# ---------------------------------------------------------------------------
# Acceptance test 2: context_limit from assistant model, not dict order
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_claude_tier_resolves_context_limit_from_assistant_model_not_dict_order(
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
  assert usage["context_limit"] == 200_000  # from the real model, not 50_000


# ---------------------------------------------------------------------------
# Acceptance test 3: modelUsage absent -> context_limit is the ceiling
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_claude_tier_context_limit_is_ceiling_when_model_usage_absent(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(id="session-nomodel", name="No Model Usage", backend="claude-opus-4.6")
  _write_session(session_mgr, meta, [
      _result_event(0.3),  # no modelUsage
      _assistant_event("claude-opus-4-6", input_tokens=70_000),
  ])

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  assert usage["context_limit"] == headless_claude_context_ceiling()


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
  assert usage["context_limit"] is None
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
# Acceptance test 8: headless_claude_context_ceiling
# ---------------------------------------------------------------------------


@pytest.fixture
def _clean_ceiling_env(monkeypatch: pytest.MonkeyPatch) -> None:
  """Remove env vars that would change the ceiling so each test starts clean."""
  for name in ("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE",
               "CLAUDE_CODE_MAX_CONTEXT_TOKENS"):
    monkeypatch.delenv(name, raising=False)


def test_ceiling_subtracts_reserves_from_declared_window(_clean_ceiling_env, monkeypatch) -> None:
  monkeypatch.setenv("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "500000")
  expected = 500_000 - CLAUDE_COMPACT_OUTPUT_RESERVE - CLAUDE_COMPACT_CONTEXT_RESERVE
  assert headless_claude_context_ceiling() == expected


def test_ceiling_follows_host_export_of_different_window(_clean_ceiling_env, monkeypatch) -> None:
  monkeypatch.setenv("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "1000000")
  expected = 1_000_000 - CLAUDE_COMPACT_OUTPUT_RESERVE - CLAUDE_COMPACT_CONTEXT_RESERVE
  assert headless_claude_context_ceiling() == expected
  assert headless_claude_context_ceiling() != 500_000 - 33_000


@pytest.mark.parametrize("override_var", [
    "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE",
    "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
])
def test_ceiling_returns_declared_window_when_override_present_and_warns(
    _clean_ceiling_env, monkeypatch, capsys, override_var) -> None:
  monkeypatch.setenv("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "500000")
  monkeypatch.setenv(override_var, "1")
  result = headless_claude_context_ceiling()
  assert result == 500_000  # declared window, no subtraction
  out = capsys.readouterr().out
  assert override_var in out
  assert "ceiling" in out.lower()


def test_ceiling_returns_default_when_window_unparseable_and_warns(
    _clean_ceiling_env, monkeypatch, capsys) -> None:
  monkeypatch.setenv("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "not-a-number")
  default_window = int(HEADLESS_CLAUDE_DEFAULT_ENV["CLAUDE_CODE_AUTO_COMPACT_WINDOW"])
  result = headless_claude_context_ceiling()
  assert result == default_window
  out = capsys.readouterr().out
  assert "CLAUDE_CODE_AUTO_COMPACT_WINDOW" in out
  assert "ceiling" in out.lower()


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
  assert usage["context_limit"] == 258400  # model_context_window (no auto-compact configured)
  assert usage["model"] == "gpt-5.5"


@pytest.mark.asyncio
async def test_codex_context_limit_uses_auto_compact_limit_when_configured(tmp_path: Path) -> None:
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
  assert usage["context_limit"] == 180_000  # auto-compact limit, not 258400
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
      "context_limit": 258400,
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
