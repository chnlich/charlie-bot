"""Tests for the improve loop's durable state and report-reading helpers.

The loop itself is the v2 improve sequence (tests/test_task_sequences.py); what
lives here is the state store and the event-reading judgments its controller
shares (src/features/improve/improve_command.py).
"""

import json
import pathlib
from unittest import mock

import conftest
import pytest

from src.features.improve import improve_command


def _make_cfg(tmp_path: pathlib.Path) -> mock.MagicMock:
  """Create a minimal config-like object with session and worktree directories."""
  cfg = mock.MagicMock()
  cfg.sessions_dir = tmp_path / "sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  cfg.paths.worktree_dir = str(tmp_path / "worktrees")
  return cfg


@pytest.mark.asyncio
async def test_reserve_loop_state_raises_when_session_already_has_running_loop(tmp_path: pathlib.Path) -> None:
  """Concurrent loop starts fail before they can schedule background work."""
  cfg = _make_cfg(tmp_path)
  first = await improve_command.reserve_loop_state("reserved-session", "optimize", "improve/test", "/tmp/repo", cfg)

  with pytest.raises(improve_command.ImproveLoopAlreadyRunningError, match=f"Loop {first.loop_id} is already running"):
    await improve_command.reserve_loop_state("reserved-session", "optimize", "improve/other", "/tmp/repo", cfg)


def test_quota_blocker_reason_ignores_allowed_rate_limit_event() -> None:
  """A fully allowed rate_limit_event yields no blocker reason."""
  events = [{"type": "rate_limit_event", "rate_limit_info": {"status": "allowed", "overageStatus": "allowed"}}]
  assert improve_command._failed_iteration_judgments(iter(reversed(events)), 1, "failed")[0] is None


def test_quota_blocker_reason_scans_newest_first() -> None:
  """The newest event's match names the blocker: the stream is consumed to the
  first match, so an older rejection never renames a newer one."""
  events = [
      {
          "type": "rate_limit_event",
          "rate_limit_info": {
              "status": "rejected",
              "rateLimitType": "five_hour",
              "overageStatus": "allowed",
          },
      },
      {
          "type": "error",
          "content": "Provider rejected the request: insufficient quota."
      },
  ]
  reason, _ = improve_command._failed_iteration_judgments(iter(reversed(events)), 1, "failed")
  assert reason == "provider quota/token/rate-limit rejection (insufficient quota)"


def test_quota_blocker_match_reads_the_launch_error_events_quota_flag() -> None:
  """A refused launch's error event names its own cause: the flag makes the blocker
  though the message holds no pattern word."""
  ev = {"type": "error", "message": "no login can take this run", "quota_exhausted": True}
  assert improve_command._quota_blocker_match(ev) == "launch refused with quota exhausted: no login can take this run"


def test_quota_blocker_match_leaves_a_false_flag_to_the_text_patterns() -> None:
  """quota_exhausted False is no blocker by the flag; the relay-limit refusal matches no pattern."""
  ev = {
      "type": "error",
      "message": "Claude account relay limit reached (3 relays in one run); this run did not complete.",
      "quota_exhausted": False,
  }
  assert improve_command._quota_blocker_match(ev) is None


def test_quota_blocker_match_judges_an_error_event_without_the_flag_by_text() -> None:
  """An error event with no flag (any failure but a launch refusal) matches the text patterns as before."""
  ev = {"type": "error", "message": "Provider rejected the request: insufficient quota."}
  assert improve_command._quota_blocker_match(ev) == "provider quota/token/rate-limit rejection (insufficient quota)"


def test_extract_iteration_summary_prefers_newest_result_or_assistant() -> None:
  """The newest event carrying text wins, whichever kind it is."""
  events = [
      {
          "type": "result",
          "result": "older result text"
      },
      conftest.assistant_text_event("newest words"),
  ]
  assert improve_command._extract_iteration_summary(iter(reversed(events)), 3, "failed") == "newest words"


def test_newest_first_events_prefiltered_parity(tmp_path: pathlib.Path) -> None:
  # The raw-line type prefilter must change the judgments' inputs by nothing:
  # over a log carrying both writer shapes, a multi-window giant tool_result
  # line, the type-less adopt signal, blank and malformed lines, the filtered
  # newest-first stream equals the full parse restricted to the candidate
  # types — so both judgments answer identically.
  from src.infra import ndjson

  target = tmp_path / "events.jsonl"
  giant = {"type": "tool_result", "tool_name": "Bash", "content": "x" * (512 * 1024 + 11)}
  lines = [
      json.dumps({
          "session_id": "abc",
          "timestamp": "t"
      }),  # the old adopt-signal shape: no type
      json.dumps({
          "type": "rate_limit_event",
          "rate_limit_info": {
              "status": "rejected"
          }
      }),
      "",
      "{not json",
      json.dumps(giant),
      json.dumps({
          "type": "tool_result",
          "content": "more noise"
      }),
      json.dumps({
          "type": "assistant",
          "message": {
              "content": [{
                  "type": "text",
                  "text": "iter words"
              }]
          }
      }),
  ]
  target.write_text("\n".join(lines) + "\n", encoding="utf-8")

  unfiltered = list(ndjson.iter_ndjson_events_from_end(target, log_event="test_skip", log_fields={}))
  candidates = {"result", "assistant", "assistant_error", "error", "rate_limit_event"}
  # Events without a type rode an unproven line (no leading "type"), which
  # the filter always keeps.
  expected = [e for e in unfiltered if e.get("type") is None or e.get("type") in candidates]
  assert list(improve_command._newest_first_events(target)) == expected
  blocker, summary = improve_command._failed_iteration_judgments(
      improve_command._newest_first_events(target), 1, "failed")
  assert blocker is not None and summary == "iter words"


# ---------------------------------------------------------------------------
# Live goal file (per-iteration re-read)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_reserve_loop_state_writes_goal_file(tmp_path: pathlib.Path) -> None:
  """The live goal is written to loops/{id}/goal.md exactly once at reservation."""
  cfg = _make_cfg(tmp_path)
  state = await improve_command.reserve_loop_state("goal-session", "make it faster", "improve/test", "/tmp/repo", cfg)

  goal_path = improve_command.loop_goal_path("goal-session", state.loop_id, cfg)
  assert goal_path == cfg.sessions_dir / "goal-session" / "loops" / str(state.loop_id) / "goal.md"
  assert goal_path.read_text() == "make it faster"
  # state.json keeps the startup snapshot.
  loaded = await improve_command.load_loop_state("goal-session", state.loop_id, cfg)
  assert loaded is not None and loaded.goal == "make it faster"


@pytest.mark.asyncio
async def test_reserve_loop_state_writes_optional_plan_file(tmp_path: pathlib.Path) -> None:
  """plan.md is written only when the caller provides plan content."""
  cfg = _make_cfg(tmp_path)
  state = await improve_command.reserve_loop_state(
      "plan-session",
      "make it faster",
      "improve/test",
      "/tmp/repo",
      cfg,
      plan="1. measure bottleneck",
  )

  plan_path = improve_command.loop_plan_path("plan-session", state.loop_id, cfg)
  assert plan_path == cfg.sessions_dir / "plan-session" / "loops" / str(state.loop_id) / "plan.md"
  assert plan_path.read_text() == "1. measure bottleneck"


@pytest.mark.asyncio
async def test_read_loop_goal_raises_when_missing(tmp_path: pathlib.Path) -> None:
  """A missing goal.md is a hard failure — no fallback to a snapshot."""
  loop_dir = tmp_path / "loops" / "1"
  loop_dir.mkdir(parents=True)
  with pytest.raises(RuntimeError, match="goal file missing"):
    await improve_command.read_loop_goal(loop_dir)


@pytest.mark.asyncio
async def test_read_loop_plan_returns_none_when_missing(tmp_path: pathlib.Path) -> None:
  """A missing plan.md means the loop is using thin-goal behavior."""
  loop_dir = tmp_path / "loops" / "1"
  loop_dir.mkdir(parents=True)
  assert await improve_command.read_loop_plan(loop_dir) is None
