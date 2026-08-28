"""The on-screen invariant: what the browser shows equals what the log holds.

A chat view is assembled from three surfaces that must partition the message
list, never overlap it:

  * the bubble list      -- first paint's ``messages`` (projection.tail)
  * the streaming preview -- first paint's ``pending_draft``
  * the replay           -- catchup frames for events at or after the cursor

These tests sweep EVERY cutoff of an event list, model the client exactly as
``web/static/js/websocket.js`` drives the two surfaces, and assert that the
resulting screen contents equal ``events_to_messages`` of the whole log.
A missing entry means a lost message; an extra one means a duplicate.
"""

from __future__ import annotations

from collections import Counter

import pytest
from conftest import FakeWebSocket

from server import _replay_aggregated_catchup
from src.api.message_utils import events_to_messages
from src.core import event_types as ET
from src.core.message_projection import MessageProjection

_UNLIMITED = 10**9


def _assistant(text: str, event_id: str, extra_blocks: tuple = ()) -> dict:
  blocks: list[dict] = [{"type": "text", "text": text}] if text else []
  blocks.extend(extra_blocks)
  return {"id": event_id, "type": ET.ASSISTANT, "message": {"content": blocks}}


def _plain_turn() -> list[dict]:
  return [
      {"id": "u1", "type": ET.USER, "content": "q1"},
      _assistant("reply1", "a1"),
      {"id": "d1", "type": ET.MASTER_DONE},
  ]


def _second_turn_in_flight() -> list[dict]:
  return [
      *_plain_turn(),
      {"id": "u2", "type": ET.USER, "content": "q2"},
      _assistant("IN PROGRESS", "a2"),
  ]


def _multi_block_turn() -> list[dict]:
  return [
      {"id": "u1", "type": ET.USER, "content": "q1"},
      _assistant("part one", "a1"),
      _assistant("part two", "a2"),
      _assistant("part three", "a3"),
      {"id": "d1", "type": ET.MASTER_DONE},
  ]


def _queued_user_inside_completed_run() -> list[dict]:
  return [
      {"session_id": "oc-1"},
      {"id": "u1", "type": ET.USER, "content": "q1"},
      _assistant("working", "a1"),
      {"id": "u2", "type": ET.USER, "content": "q2 queued"},
      _assistant("more", "a2"),
      {"id": "d1", "type": ET.MASTER_DONE},
  ]


def _delegation_and_worker_summary() -> list[dict]:
  return [
      {"id": "u1", "type": ET.USER, "content": "q1"},
      _assistant("analysis", "a1"),
      {"id": "t1", "type": ET.TASK_DELEGATED, "thread_id": "th1", "description": "d"},
      _assistant("more analysis", "a2"),
      {"id": "w1", "type": ET.WORKER_SUMMARY, "content": "merged"},
      {"id": "d1", "type": ET.MASTER_DONE, "still_thinking": True},
      _assistant("second run", "a3"),
      {"id": "d2", "type": ET.MASTER_DONE},
  ]


def _exit_plan_mode() -> list[dict]:
  return [
      {"id": "u1", "type": ET.USER, "content": "q1"},
      _assistant("preamble", "a1"),
      _assistant("", "a2", ({"type": "tool_use", "name": "ExitPlanMode", "input": {"plan": "the plan"}},)),
      {"id": "d1", "type": ET.MASTER_DONE},
  ]


def _tool_result_only_user_event() -> list[dict]:
  return [
      {"id": "u1", "type": ET.USER, "content": "q1"},
      _assistant("", "a1", ({"type": "tool_use", "name": "Read", "input": {}},)),
      {"id": "x1", "type": ET.USER, "message": {"content": [{"type": "tool_result", "content": "out"}]}},
      _assistant("done", "a2"),
      {"id": "d1", "type": ET.MASTER_DONE},
  ]


def _many_turns(turns: int = 25) -> list[dict]:
  events: list[dict] = []
  for i in range(turns):
    events.append({"id": f"u{i}", "type": ET.USER, "content": f"q{i}"})
    events.append(_assistant(f"reply {i} first", f"a{i}x"))
    events.append(_assistant(f"reply {i} second", f"a{i}y"))
    events.append({"id": f"d{i}", "type": ET.MASTER_DONE, "thinking_seconds": i})
  return events


SHAPES: list[tuple[str, list[dict]]] = [
    ("empty", []),
    ("plain_turn", _plain_turn()),
    ("second_turn_in_flight", _second_turn_in_flight()),
    ("multi_block_turn", _multi_block_turn()),
    ("queued_user_inside_completed_run", _queued_user_inside_completed_run()),
    ("delegation_and_worker_summary", _delegation_and_worker_summary()),
    ("exit_plan_mode", _exit_plan_mode()),
    ("tool_result_only_user_event", _tool_result_only_user_event()),
    ("many_turns", _many_turns()),
]


def _bubble_identity(role: str | None, content: str | None) -> tuple[str, str]:
  """Identity is what the user sees.

  Deliberately not the event id or event_index: the aggregator stamps a flushed
  assistant message with the index of the event that flushed it rather than the
  event that buffered it, so the same text carries different indices in a
  truncated view and in a full one. Comparing on those would call correct
  rendering a failure.
  """
  return (role or "", content or "")


async def _screen_contents(events: list[dict], cutoff: int) -> Counter:
  """Model the browser after first paint at *cutoff* plus catchup."""
  projection = MessageProjection(events[:cutoff])
  bubbles, _oldest, _has_more = projection.tail(_UNLIMITED)
  bubbles = list(bubbles)
  preview = (projection.pending_draft or {}).get("content") or None

  websocket = FakeWebSocket()
  await _replay_aggregated_catchup(websocket, events, projection.event_count, "s")
  for frame in websocket.sent:
    if frame.get("type") == "message":
      # A committed bubble supersedes the preview (websocket.js _commitMessage).
      preview = None
      bubbles.append(frame["message"])
    elif frame.get("type") == "stream":
      preview = (frame.get("message") or {}).get("content") or None

  seen = Counter(_bubble_identity(m.get("role"), m.get("content")) for m in bubbles)
  if preview:
    seen[_bubble_identity("assistant", preview)] += 1
  return seen


@pytest.mark.parametrize("name,events", SHAPES)
@pytest.mark.asyncio
async def test_on_screen_contents_equal_the_log_at_every_cutoff(name: str, events: list[dict]) -> None:
  """No cutoff loses a message and no cutoff shows one twice."""
  expected = Counter(_bubble_identity(m.get("role"), m.get("content")) for m in events_to_messages(events))
  for cutoff in range(len(events) + 1):
    seen = await _screen_contents(events, cutoff)
    assert seen - expected == Counter(), f"{name}: duplicated at cutoff {cutoff}: {seen - expected}"
    assert expected - seen == Counter(), f"{name}: lost at cutoff {cutoff}: {expected - seen}"


@pytest.mark.parametrize("name,events", SHAPES)
@pytest.mark.asyncio
async def test_bubble_list_and_preview_never_overlap(name: str, events: list[dict]) -> None:
  """The draft is preview surface only -- it is never also a bubble."""
  for cutoff in range(len(events) + 1):
    projection = MessageProjection(events[:cutoff])
    if projection.pending_draft is None:
      continue
    bubbles, _oldest, _has_more = projection.tail(_UNLIMITED)
    assert projection.pending_draft not in bubbles, f"{name}: draft is also a bubble at cutoff {cutoff}"


@pytest.mark.parametrize("name,events", SHAPES)
def test_event_count_is_the_consumed_prefix(name: str, events: list[dict]) -> None:
  """The first-paint cursor is the boundary of the snapshot the bubbles came from."""
  for cutoff in range(len(events) + 1):
    assert MessageProjection(events[:cutoff]).event_count == cutoff, name
