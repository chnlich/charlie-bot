"""Focused tests for session usage resolution."""

import json
from pathlib import Path

import pytest

from src.core.config import CharlieBotConfig
from src.core.codex_usage import _extract_codex_rollout_usage_event
from src.core.models import BackendOption, SessionMetadata
from src.core.sessions import SessionManager


def _build_cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path,
      backend_options=[
          BackendOption(id="claude-opus-4.6", label="Claude", type="cc-claude", model="claude-opus-4-6"),
          BackendOption(id="codex-test", label="Codex", type="codex", model="codex-test-model"),
      ],
  )


def _write_session(session_mgr: SessionManager, meta: SessionMetadata, events: list[dict]) -> None:
  session_dir = session_mgr._chat_events_path(meta.id).parent
  session_dir.mkdir(parents=True, exist_ok=True)
  (session_dir.parent / "threads").mkdir(parents=True, exist_ok=True)
  session_mgr._metadata_path(meta.id).write_text(meta.model_dump_json(indent=2), encoding="utf-8")
  lines = "\n".join(json.dumps(event) for event in events)
  session_mgr._chat_events_path(meta.id).write_text(lines + "\n", encoding="utf-8")


def _write_codex_rollout(codex_root: Path, native_thread_id: str, lines: list[dict]) -> None:
  rollout_dir = codex_root / "2026" / "03" / "31"
  rollout_dir.mkdir(parents=True, exist_ok=True)
  rollout_path = rollout_dir / f"rollout-2026-03-31T20-42-51-{native_thread_id}.jsonl"
  rollout_path.write_text("\n".join(json.dumps(line) for line in lines) + "\n", encoding="utf-8")


def _codex_turn_context(model: str) -> dict:
  return {
      "timestamp": "2026-03-31T20:42:51.000Z",
      "type": "turn_context",
      "payload": {"model": model},
  }


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
async def test_resolve_session_usage_reads_live_codex_thread_id_from_chat_events(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  native_thread_id = "019d45a2-836d-7552-a54f-3c6c5511e502"
  meta = SessionMetadata(
      id="session-live",
      name="Live Codex Session",
      backend="codex-test",
      cc_session_id=None,
  )
  _write_session(
      session_mgr,
      meta,
      [
          {"type": "user", "content": "hello", "timestamp": "2026-03-31T20:42:52Z"},
          {"session_id": native_thread_id, "timestamp": "2026-03-31T20:42:53Z"},
          {"type": "assistant", "message": {"content": [{"type": "text", "text": "streaming"}]}},
      ],
  )
  codex_root = tmp_path / ".codex" / "sessions"
  monkeypatch.setattr("src.core.codex_usage._CODEX_SESSIONS_DIR", codex_root)
  _write_codex_rollout(
      codex_root,
      native_thread_id,
      [
          _codex_turn_context("gpt-5.5"),
          {
              "timestamp": "2026-03-31T20:42:52.358Z",
              "type": "event_msg",
              "payload": {"type": "token_count", "info": None},
          },
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
      ],
  )

  usage = await session_mgr.resolve_session_usage(
      meta.id,
      meta,
      events=[{"type": "assistant", "message": {"content": [{"type": "text", "text": "tail only"}]}}],
  )

  assert usage is not None
  assert usage["context_tokens"] == 179319
  assert usage["context_limit"] == 258400
  assert usage["total_cost_usd"] == pytest.approx(2.5835, abs=0.0001)
  assert usage["model"] == "gpt-5.5"


@pytest.mark.asyncio
async def test_resolve_session_usage_overrides_completed_codex_context_window(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  native_thread_id = "019d26e4-be1c-7171-a3fd-6f1ab10662de"
  meta = SessionMetadata(
      id="session-complete",
      name="Completed Codex Session",
      backend="codex-test",
      cc_session_id=native_thread_id,
  )
  _write_session(
      session_mgr,
      meta,
      [
          {"type": "user", "content": "hello", "timestamp": "2026-03-25T21:26:57Z"},
          {
              "type": "result",
              "usage": {
                  "input_tokens": 9000,
                  "cache_read_input_tokens": 1000,
                  "cache_creation_input_tokens": 0,
              },
              "modelUsage": {"codex-test": {"contextWindow": 200000}},
              "total_cost_usd": 1.25,
              "timestamp": "2026-03-25T21:33:03Z",
          },
      ],
  )
  codex_root = tmp_path / ".codex" / "sessions"
  monkeypatch.setattr("src.core.codex_usage._CODEX_SESSIONS_DIR", codex_root)
  _write_codex_rollout(
      codex_root,
      native_thread_id,
      [
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
      ],
  )

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  assert usage["context_tokens"] == 176028
  assert usage["context_limit"] == 258400
  assert usage["total_cost_usd"] == pytest.approx(1.4461, abs=0.0001)
  assert usage["model"] == "gpt-5.5"


@pytest.mark.asyncio
async def test_resolve_session_usage_computes_codex_cumulative_cost(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  native_thread_id = "019d9f9e-5d7a-7f44-81a8-e9cb8261a51d"
  meta = SessionMetadata(
      id="session-codex-cost",
      name="Codex Cost Session",
      backend="codex-test",
      cc_session_id=native_thread_id,
  )
  _write_session(session_mgr, meta, [])
  codex_root = tmp_path / ".codex" / "sessions"
  monkeypatch.setattr("src.core.codex_usage._CODEX_SESSIONS_DIR", codex_root)
  _write_codex_rollout(
      codex_root,
      native_thread_id,
      [
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
      ],
  )

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  assert usage["total_cost_usd"] == pytest.approx(1.85, abs=0.01)
  assert usage["model"] == "gpt-5.5"


@pytest.mark.asyncio
async def test_resolve_session_usage_unknown_codex_model_cost_is_none(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  native_thread_id = "019d9fb0-c3f8-72ec-9d8d-0ed32d404b30"
  meta = SessionMetadata(
      id="session-codex-unknown-cost",
      name="Codex Unknown Cost Session",
      backend="codex-test",
      cc_session_id=native_thread_id,
  )
  _write_session(session_mgr, meta, [])
  codex_root = tmp_path / ".codex" / "sessions"
  monkeypatch.setattr("src.core.codex_usage._CODEX_SESSIONS_DIR", codex_root)
  _write_codex_rollout(
      codex_root,
      native_thread_id,
      [
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
      ],
  )

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage is not None
  assert usage["total_cost_usd"] is None
  assert usage["model"] == "gpt-unknown"


@pytest.mark.asyncio
async def test_resolve_session_usage_keeps_non_codex_result_usage(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(
      id="session-claude",
      name="Claude Session",
      backend="claude-opus-4.6",
  )
  _write_session(
      session_mgr,
      meta,
      [
          {"type": "user", "content": "hello", "timestamp": "2026-03-31T20:42:52Z"},
          {
              "type": "result",
              "usage": {
                  "input_tokens": 1200,
                  "cache_creation_input_tokens": 300,
                  "cache_read_input_tokens": 500,
              },
              "modelUsage": {"claude-opus-4-6": {"contextWindow": 200000}},
              "total_cost_usd": 0.42,
              "timestamp": "2026-03-31T20:43:12Z",
          },
      ],
  )

  usage = await session_mgr.resolve_session_usage(meta.id, meta)

  assert usage == {
      "context_tokens": 2000,
      "context_limit": 200000,
      "total_cost_usd": 0.42,
      "model": "claude-opus-4-6",
  }
