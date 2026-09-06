"""Sonnet compaction: the cold-cache trigger rules, the transcript judgment, and the chat events it emits."""

import asyncio
import json
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import pytest

from src.core import claude_compaction
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.message_aggregator import MessageAggregator
from src.core.models import ClaudeCompactionConfig

NOW = datetime(2026, 9, 6, 20, 0, tzinfo=UTC)
FABLE = "claude-fable-5-1"
SONNET = "claude-sonnet-5"
OPUS = "claude-opus-5"
SLUG = "-home-u--charliebot-sessions-s1"


def _cfg(tmp_path: Path, **floors: int) -> CharlieBotConfig:
  return CharlieBotConfig(charliebot_home=tmp_path / "home", claude_compaction=ClaudeCompactionConfig(**floors))


def _write_transcript(config_dir: Path, cc_session_id: str, boundaries: int = 0) -> Path:
  transcript = config_dir / "projects" / SLUG / f"{cc_session_id}.jsonl"
  transcript.parent.mkdir(parents=True, exist_ok=True)
  rows = ['{"type":"user","message":{"role":"user","content":"hi"}}']
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


@pytest.mark.parametrize(("minutes", "expired"), [(59, False), (60, False), (61, True)])
def test_cache_expired_is_strictly_more_than_one_hour(minutes: int, expired: bool) -> None:
  assert claude_compaction.cache_expired(NOW - timedelta(minutes=minutes), now=NOW) is expired


def test_cache_expired_is_false_without_a_previous_request() -> None:
  assert claude_compaction.cache_expired(None, now=NOW) is False


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


@pytest.mark.parametrize(
    ("model", "context_tokens", "wanted"),
    [
        pytest.param(FABLE, 80_000, False, id="fable_below_floor"),
        pytest.param(FABLE, 100_000, True, id="fable_at_floor"),
        pytest.param(FABLE, 400_000, True, id="fable_above_floor"),
        pytest.param(OPUS, 400_000, False, id="opus_never"),
        pytest.param(FABLE, None, False, id="no_reading"),
    ],
)
def test_relay_compaction_wanted(tmp_path: Path, model: str, context_tokens: int | None, wanted: bool) -> None:
  assert claude_compaction.relay_compaction_wanted(_cfg(tmp_path), model, context_tokens) is wanted


def test_floors_follow_the_config_override(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path, relay_tokens=200_000, expired_cache_tokens=30_000)
  assert claude_compaction.relay_compaction_wanted(cfg, FABLE, 150_000) is False
  assert claude_compaction.relay_compaction_wanted(cfg, FABLE, 200_000) is True
  cold = NOW - timedelta(minutes=61)
  assert claude_compaction.expired_cache_compaction_wanted(cfg, FABLE, 30_000, cold, now=NOW) is True


def test_compaction_command_and_env_pin_sonnet_and_the_login_dir(tmp_path: Path, monkeypatch) -> None:
  monkeypatch.setenv("CLAUDE_CONFIG_DIR", "/elsewhere")
  monkeypatch.setenv("CLAUDECODE", "1")
  cmd = claude_compaction.compaction_command("uuid-1")
  env = claude_compaction.compaction_env(tmp_path / "login")

  assert cmd[:6] == ["claude", "-p", "--resume", "uuid-1", "--model", SONNET]
  assert cmd[cmd.index("--output-format") + 1] == "json"
  assert "Bash" in cmd[cmd.index("--disallowed-tools") + 1].split(",")
  assert env["CLAUDE_CONFIG_DIR"] == str(tmp_path / "login")
  assert "CLAUDECODE" not in env
  assert env["CLAUDE_CODE_DISABLE_AUTO_MEMORY"] == "1"


def test_count_compact_boundaries_reads_on_disk_rows(tmp_path: Path) -> None:
  transcript = _write_transcript(tmp_path / "login", "uuid-2", boundaries=2)
  assert claude_compaction.count_compact_boundaries(transcript) == 2
  assert claude_compaction._newest_boundary_pre_tokens(transcript) == 150001


# ---------------------------------------------------------------------------
# The run: a fake `claude` process
# ---------------------------------------------------------------------------


class _FakeProc:
  pid = 4242

  def __init__(self, *, returncode: int, stdout: bytes, on_communicate=None, delay: float = 0.0):
    self.returncode = returncode
    self._stdout = stdout
    self._on_communicate = on_communicate
    self._delay = delay
    self.stdin_payload: bytes | None = None
    self.waited = False

  async def communicate(self, payload: bytes | None = None) -> tuple[bytes, bytes]:
    self.stdin_payload = payload
    if self._delay:
      await asyncio.sleep(self._delay)
    if self._on_communicate is not None:
      self._on_communicate()
    return self._stdout, b""

  async def wait(self) -> int:
    self.waited = True
    return self.returncode


def _result_json(models: list[str], *, is_error: bool = False) -> bytes:
  return json.dumps(
      {
          "type": "result",
          "subtype": "success",
          "is_error": is_error,
          "result": "",
          "modelUsage": {
              name: {
                  "inputTokens": 1
              } for name in models
          },
      }).encode("utf-8")


def _install_fake_exec(monkeypatch, proc: _FakeProc) -> dict[str, Any]:
  captured: dict[str, Any] = {}

  async def fake_exec(*args: Any, **kwargs: Any) -> _FakeProc:
    captured["args"] = list(args)
    captured["kwargs"] = kwargs
    return proc

  monkeypatch.setattr(claude_compaction.asyncio, "create_subprocess_exec", fake_exec)
  return captured


def _append_boundary(transcript: Path) -> None:
  with transcript.open("a", encoding="utf-8") as handle:
    handle.write(
        json.dumps(
            {
                "type": "system",
                "subtype": "compact_boundary",
                "compactMetadata": {
                    "trigger": "manual",
                    "preTokens": 123456
                }
            }) + "\n")


async def _run(tmp_path: Path,
               monkeypatch,
               proc: _FakeProc,
               *,
               pre_tokens: int | None = 120_000,
               timeout: float = 5.0) -> tuple[bool, list[dict], dict[str, Any]]:
  login = tmp_path / "login"
  _write_transcript(login, "uuid-3")
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
async def test_success_emits_context_compacted_naming_sonnet(tmp_path: Path, monkeypatch) -> None:
  login = tmp_path / "login"
  transcript = login / "projects" / SLUG / "uuid-3.jsonl"
  proc = _FakeProc(returncode=0, stdout=_result_json([SONNET]), on_communicate=lambda: _append_boundary(transcript))

  ok, events, captured = await _run(tmp_path, monkeypatch, proc)

  assert ok is True
  assert events == [{"type": ET.CONTEXT_COMPACTED, "trigger": "manual", "pre_tokens": 120_000, "model": SONNET}]
  assert proc.stdin_payload == b"/compact\n"
  assert captured["args"][:6] == ["claude", "-p", "--resume", "uuid-3", "--model", SONNET]
  assert captured["kwargs"]["cwd"] == str(tmp_path / "session")
  assert captured["kwargs"]["env"]["CLAUDE_CONFIG_DIR"] == str(login)
  assert captured["kwargs"]["start_new_session"] is True


@pytest.mark.asyncio
async def test_success_without_a_caller_reading_uses_the_boundary_row(tmp_path: Path, monkeypatch) -> None:
  transcript = tmp_path / "login" / "projects" / SLUG / "uuid-3.jsonl"
  proc = _FakeProc(returncode=0, stdout=_result_json([SONNET]), on_communicate=lambda: _append_boundary(transcript))

  ok, events, _captured = await _run(tmp_path, monkeypatch, proc, pre_tokens=None)

  assert ok is True
  assert events[0]["pre_tokens"] == 123456


@pytest.mark.parametrize(
    ("returncode", "models", "add_boundary", "is_error", "fragment"),
    [
        pytest.param(1, [SONNET], True, False, "exit 1", id="nonzero_exit"),
        pytest.param(0, [SONNET], True, True, "exit 0", id="result_is_error"),
        pytest.param(0, [SONNET], False, False, "no compact_boundary", id="no_new_boundary"),
        pytest.param(0, [SONNET, FABLE], True, False, "served by", id="fable_took_part"),
        pytest.param(0, [], True, False, "no model", id="no_model_usage"),
    ],
)
@pytest.mark.asyncio
async def test_failed_runs_emit_context_compact_failed_and_return_false(
    tmp_path: Path, monkeypatch, returncode: int, models: list[str], add_boundary: bool, is_error: bool,
    fragment: str) -> None:
  transcript = tmp_path / "login" / "projects" / SLUG / "uuid-3.jsonl"
  proc = _FakeProc(
      returncode=returncode,
      stdout=_result_json(models, is_error=is_error),
      on_communicate=(lambda: _append_boundary(transcript)) if add_boundary else None,
  )

  ok, events, _captured = await _run(tmp_path, monkeypatch, proc)

  assert ok is False
  assert len(events) == 1
  assert events[0]["type"] == ET.CONTEXT_COMPACT_FAILED
  assert events[0]["model"] == SONNET
  assert fragment in events[0]["error"]


@pytest.mark.asyncio
async def test_unparseable_stdout_fails_loudly(tmp_path: Path, monkeypatch) -> None:
  ok, events, _captured = await _run(tmp_path, monkeypatch, _FakeProc(returncode=0, stdout=b"not json"))
  assert ok is False
  assert "no JSON result" in events[0]["error"]


@pytest.mark.asyncio
async def test_timeout_kills_the_process_group_and_fails(tmp_path: Path, monkeypatch) -> None:
  killed: list[int] = []
  monkeypatch.setattr(claude_compaction, "kill_process_group", lambda pid, *a, **k: killed.append(pid) or True)
  proc = _FakeProc(returncode=0, stdout=_result_json([SONNET]), delay=0.2)

  ok, events, _captured = await _run(tmp_path, monkeypatch, proc, timeout=0.01)

  assert ok is False
  assert killed == [4242]
  assert proc.waited is True
  assert "timed out" in events[0]["error"]


@pytest.mark.asyncio
async def test_missing_transcript_fails_without_spawning(tmp_path: Path, monkeypatch) -> None:
  spawned = []

  async def fake_exec(*args: Any, **kwargs: Any):
    spawned.append(args)
    raise AssertionError("must not spawn")

  monkeypatch.setattr(claude_compaction.asyncio, "create_subprocess_exec", fake_exec)
  events: list[dict] = []

  async def persist(event: dict) -> None:
    events.append(event)

  ok = await claude_compaction.compact_with_sonnet(
      cc_session_id="absent",
      cwd=str(tmp_path),
      config_dir=tmp_path / "login",
      pre_tokens=None,
      persist_and_broadcast=persist,
      log_context={},
  )

  assert ok is False and spawned == []
  assert events[0]["type"] == ET.CONTEXT_COMPACT_FAILED and "no transcript" in events[0]["error"]


# ---------------------------------------------------------------------------
# Chat rendering
# ---------------------------------------------------------------------------


def test_context_compacted_projection_names_the_compacting_model() -> None:
  agg = MessageAggregator()
  deltas = list(
      agg.feed(
          {
              "type": ET.CONTEXT_COMPACTED,
              "trigger": "manual",
              "pre_tokens": 120_000,
              "model": SONNET,
              "timestamp": "t",
          }))
  assert deltas[0]["message"]["content"] == "Context compacted (manual, by Sonnet) — was 120k tokens"

  failed = list(agg.feed({"type": ET.CONTEXT_COMPACT_FAILED, "error": "timed out", "model": SONNET, "timestamp": "t"}))
  assert failed[0]["message"]["content"] == "Compaction by Sonnet failed — timed out"


def test_context_compacted_projection_without_model_is_unchanged() -> None:
  agg = MessageAggregator()
  deltas = list(agg.feed({"type": ET.CONTEXT_COMPACTED, "trigger": "auto", "pre_tokens": 400_000, "timestamp": "t"}))
  assert deltas[0]["message"]["content"] == "Context compacted (auto) — was 400k tokens"
