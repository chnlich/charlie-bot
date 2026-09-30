"""Sonnet compaction: the cold-cache trigger rules, the transcript judgment, and the chat events it emits."""

import asyncio
import json
from collections.abc import Callable, Sequence
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import pytest

from src.core import claude_compaction
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import ClaudeCompactionConfig

NOW = datetime(2026, 9, 6, 20, 0, tzinfo=UTC)
FABLE = "claude-fable-5-1"
SONNET = "claude-sonnet-5"
OPUS = "claude-opus-5"
SLUG = "-home-u--charliebot-sessions-s1"


def _cfg(tmp_path: Path, **floors: int) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / "home", accounts={"claude_compaction": ClaudeCompactionConfig(**floors)})


def _usage(*, inp: int, out: int, cache_read: int = 0, cache_creation: int = 0) -> dict:
  """One model's entry in a ``modelUsage`` dict, with the transcript's camelCase counters."""
  return {
      "inputTokens": inp,
      "outputTokens": out,
      "cacheReadInputTokens": cache_read,
      "cacheCreationInputTokens": cache_creation,
  }


def _cost_state_row(model_usage: dict) -> dict:
  """A ``cost-state`` row in the wire shape Claude Code writes: session totals per model."""
  return {"type": "cost-state", "sessionId": "s1", "totalCostUSD": 37.42, "modelUsage": model_usage}


def _write_transcript(
    config_dir: Path, cc_session_id: str, boundaries: int = 0, cost_states: Sequence[dict] = ()) -> Path:
  transcript = config_dir / "projects" / SLUG / f"{cc_session_id}.jsonl"
  transcript.parent.mkdir(parents=True, exist_ok=True)
  rows = ['{"type":"user","message":{"role":"user","content":"hi"}}']
  rows += [json.dumps(_cost_state_row(usage)) for usage in cost_states]
  rows += [
      json.dumps(
          {
              "type": "system",
              "subtype": "compact_boundary",
              "compactMetadata": {
                  "trigger": "manual",
                  "preTokens": 150000 + i
              }
          }) for i in range(boundaries)
  ]
  transcript.write_text("\n".join(rows) + "\n", encoding="utf-8")
  return transcript


# ---------------------------------------------------------------------------
# Trigger rules
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("model", "context_tokens", "minutes", "wanted"),
    [
        pytest.param(FABLE, 120_000, 61, True, id="fable_cold_above_floor"),
        pytest.param(FABLE, 50_000, 61, True, id="fable_cold_at_floor"),
        pytest.param(FABLE, 40_000, 61, False, id="fable_cold_below_floor"),
        pytest.param(FABLE, 350_000, 59, False, id="fable_warm_cache"),
        pytest.param(SONNET, 350_000, 61, False, id="sonnet_never"),
        pytest.param(OPUS, 350_000, 61, False, id="opus_never"),
        pytest.param(FABLE, None, 61, False, id="no_reading"),
    ],
)
def test_expired_cache_compaction_wanted(
    tmp_path: Path, model: str, context_tokens: int | None, minutes: int, wanted: bool) -> None:
  cfg = _cfg(tmp_path)
  last = NOW - timedelta(minutes=minutes)
  assert claude_compaction.expired_cache_compaction_wanted(cfg, model, context_tokens, last, now=NOW) is wanted


# ---------------------------------------------------------------------------
# The run: a fake `claude` process
# ---------------------------------------------------------------------------


class _FakeProc:
  pid = 4242

  def __init__(
      self,
      *,
      returncode: int,
      stdout: bytes,
      on_communicate: Callable[[], None] | None = None,
      delay: float = 0.0) -> None:
    self.returncode = returncode
    self._stdout = stdout
    self._on_communicate = on_communicate
    self._delay = delay
    self.waited = False

  async def communicate(self, payload: bytes | None = None) -> tuple[bytes, bytes]:
    if self._delay:
      await asyncio.sleep(self._delay)
    if self._on_communicate is not None:
      self._on_communicate()
    return self._stdout, b""

  async def wait(self) -> int:
    self.waited = True
    return self.returncode


def _result_json(models: list[str], *, is_error: bool = False, model_usage: dict | None = None) -> bytes:
  """The run's result JSON; ``model_usage`` replaces the default ``inputTokens: 1`` per named model."""
  return json.dumps(
      {
          "type": "result",
          "subtype": "success",
          "is_error": is_error,
          "result": "",
          "modelUsage": model_usage if model_usage is not None else {
              name: {
                  "inputTokens": 1
              } for name in models
          },
      }).encode("utf-8")


def _install_fake_exec(monkeypatch: pytest.MonkeyPatch, proc: _FakeProc) -> dict[str, Any]:
  captured: dict[str, Any] = {}

  async def fake_exec(*args: Any, **kwargs: Any) -> _FakeProc:
    captured["args"] = list(args)
    captured["kwargs"] = kwargs
    return proc

  monkeypatch.setattr(claude_compaction.asyncio, "create_subprocess_exec", fake_exec)
  return captured


def _append_boundary(transcript: Path, *, with_metadata: bool = True) -> None:
  meta = {"trigger": "manual", "preTokens": 123456, "postTokens": 9111} if with_metadata else None
  row: dict[str, Any] = {"type": "system", "subtype": "compact_boundary"}
  if meta is not None:
    row["compactMetadata"] = meta
  with transcript.open("a", encoding="utf-8") as handle:
    handle.write(json.dumps(row) + "\n")


async def _run(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    proc: _FakeProc,
    *,
    pre_tokens: int | None = 120_000,
    timeout: float = 5.0,
    cost_states: Sequence[dict] = ()) -> tuple[bool, list[dict], dict[str, Any]]:
  login = tmp_path / "login"
  _write_transcript(login, "uuid-3", cost_states=cost_states)
  captured = _install_fake_exec(monkeypatch, proc)
  events: list[dict] = []

  async def persist(event: dict) -> None:
    events.append(event)

  ok = await claude_compaction.compact_with_sonnet(
      cc_session_id="uuid-3",
      cwd=str(tmp_path / "session"),
      config_dir=login,
      pre_tokens=pre_tokens,
      persist_and_broadcast=persist,
      log_context={"session": "s1"},
      timeout=timeout,
  )
  return ok, events, captured


@pytest.mark.asyncio
async def test_fable_growth_fails_and_names_fable_alone(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Fable counters that grew over the baseline mean Fable served the run; the
  failure names only the models that grew, not the restored session totals."""
  baseline = {
      FABLE: _usage(inp=2326, out=209_495, cache_read=15_334_182, cache_creation=1_116_711),
      SONNET: _usage(inp=257_373, out=21_397, cache_creation=12_196),
  }
  transcript = tmp_path / "login" / "projects" / SLUG / "uuid-3.jsonl"
  proc = _FakeProc(
      returncode=0,
      stdout=_result_json(
          [FABLE, SONNET],
          model_usage={
              FABLE: _usage(inp=2326 + 12_000, out=209_495, cache_read=15_334_182, cache_creation=1_116_711),
              SONNET: baseline[SONNET],
          }),
      on_communicate=lambda: _append_boundary(transcript))

  ok, events, _captured = await _run(tmp_path, monkeypatch, proc, cost_states=[baseline])

  assert ok is False
  assert len(events) == 1
  assert events[0]["type"] == ET.CONTEXT_COMPACT_FAILED
  assert events[0]["model"] == SONNET
  assert "served by" in events[0]["error"]
  assert FABLE in events[0]["error"]
  assert SONNET not in events[0]["error"]


@pytest.mark.asyncio
async def test_unparseable_stdout_fails_loudly(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  ok, events, _captured = await _run(tmp_path, monkeypatch, _FakeProc(returncode=0, stdout=b"not json"))
  assert ok is False
  assert "no JSON result" in events[0]["error"]


@pytest.mark.asyncio
async def test_timeout_kills_the_process_group_and_fails(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  killed: list[int] = []
  monkeypatch.setattr(claude_compaction, "kill_process_group", lambda pid, *a, **k: killed.append(pid) or True)
  proc = _FakeProc(returncode=0, stdout=_result_json([SONNET]), delay=0.2)

  ok, events, _captured = await _run(tmp_path, monkeypatch, proc, timeout=0.01)

  assert ok is False
  assert killed == [4242]
  assert proc.waited is True
  assert "timed out" in events[0]["error"]


# ---------------------------------------------------------------------------
# Chat rendering
