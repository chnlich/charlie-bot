"""Queued-input batching on the legacy master consumer.

The consumer takes the dequeued head plus every immediately following queue
item whose run settings equal the head's, and stops at the first item that
differs (that item stays at the front for the next turn). Resume and task_run
items never merge. A batch of one runs byte-identical to the unbatched path;
a batch of N>1 runs one turn whose prompt joins the parts under per-part
headers, and every constituent's future resolves with the turn's result or
receives its exception.
"""

from __future__ import annotations

import asyncio
from datetime import UTC, datetime, timedelta
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from conftest import (
    BROADCAST_PATCH_TARGET,
    SESSIONS_SESSION_MANAGER_PATCH_TARGET,
    drain_session_consumer,
    fresh_master_state,
    make_sound_round,
    mocked_callback_fields,
    run_session_consumer,
)

from src.agents import master_cc_queue, master_cc_run, master_cc_state
from src.core import event_types as ET
from src.core.models import MasterRunRecord, SessionCallbacks, SessionMetadata
from src.core.session_dispatch import INPUT_EVENT_TYPES


def _meta(session_id: str) -> SessionMetadata:
  return SessionMetadata(id=session_id, name="batch", backend="fake", cc_session_id=None)


def _callbacks(**overrides) -> SessionCallbacks:
  fields = {"persist_and_broadcast": AsyncMock(), **mocked_callback_fields(), **overrides}
  return SessionCallbacks(
      **fields,
      persist_master_run=AsyncMock(),
      persist_claude_account=AsyncMock(side_effect=lambda sid, label: label),
      claude_context_state=AsyncMock(return_value=(None, None)),
  )


def _item(
    session_id: str,
    content: str,
    input_event_type: str | None,
    cfg,
    *,
    event_id: str | None = None,
    auto_trigger: bool = False,
    is_voice: bool = False,
    should_check_tex: bool = False,
    uploaded_files: list[dict] | None = None,
    backend_option=None,
    callbacks: SessionCallbacks | None = None,
    **extra,
) -> master_cc_state._WorkItem:
  """A directly-constructed work item the way run_message builds one."""
  return master_cc_state._WorkItem(
      cfg=cfg,
      session_meta=_meta(session_id),
      user_content=content,
      callbacks=callbacks if callbacks is not None else _callbacks(),
      is_voice=is_voice,
      auto_trigger=auto_trigger,
      backend_option=backend_option,
      extra_claude_flags=None,
      should_check_tex=should_check_tex,
      future=asyncio.get_running_loop().create_future(),
      user_event_ids=[event_id] if event_id else [],
      input_event_type=input_event_type,
      uploaded_files=uploaded_files,
      **extra,
  )


def _headers(prompt: str) -> list[str]:
  return [line for line in prompt.splitlines() if line.startswith("[Queued input ")]


# ---------------------------------------------------------------------------
# The 27-input backlog runs as one turn
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_full_backlog_runs_as_one_turn_in_arrival_order() -> None:
  """24 trigger wakes, 2 user messages, and 1 child-report wake queued behind a
  running turn produce exactly one following turn: 27 headers in arrival order,
  the whole id list on the merged item, every future resolved."""
  session_id = "batch-backlog"
  cfg = build_cfg()

  def make(content: str, etype: str, event_id: str, *, auto: bool) -> master_cc_state._WorkItem:
    return _item(session_id, content, etype, cfg, event_id=event_id, auto_trigger=auto)

  parts = [make(f"[Scheduled trigger fired] wake {i}", ET.SCHEDULED_TRIGGER, f"wake-{i}", auto=True) for i in range(24)]
  parts.append(make("first user message", ET.USER, "user-1", auto=False))
  parts.append(make("second user message", ET.USER, "user-2", auto=False))
  parts.append(make('{"iteration": 3}', ET.CHILD_REPORT, "report-1", auto=True))
  assert len(parts) == 27

  captured: list[master_cc_state._WorkItem] = []

  async def fake_run_cc(item: master_cc_state._WorkItem) -> tuple[str | None, int, str | None, dict]:
    captured.append(item)
    return ("cc-batch", 0, None, {})

  await run_session_consumer(session_id, parts, fake_run_cc)

  assert len(captured) == 1, "the whole backlog must run as one turn"
  merged = captured[0]
  headers = _headers(merged.user_content)
  assert len(headers) == 27
  assert headers[0].startswith("[Queued input 1 of 27 · scheduled_trigger · received ")
  assert headers[24].startswith("[Queued input 25 of 27 · user · received ")
  assert headers[26].startswith("[Queued input 27 of 27 · child_report · received ")
  # Arrival order: the parts' bodies appear under their headers in order.
  assert merged.user_content.index("wake 0") < merged.user_content.index("first user message")
  assert merged.user_content.index("first user message") < merged.user_content.index("second user message")
  assert merged.user_content.index("second user message") < merged.user_content.index('{"iteration": 3}')
  # Field merges: ids in order; machine-wake only when every part is one.
  assert merged.user_event_ids == [f"wake-{i}" for i in range(24)] + ["user-1", "user-2", "report-1"]
  assert merged.auto_trigger is False
  assert merged.is_voice is False
  # Every constituent's future resolved with the turn's result.
  for part in parts:
    assert part.future.done() and part.future.result() == "cc-batch"


def build_cfg():
  from conftest import backend_option

  from src.core.config import CharlieBotConfig

  return CharlieBotConfig(
      charliebot_home=Path("/tmp/charliebot-batching"),
      backends={"options": [backend_option(id="fake", label="Fake", type="codex", model="fake-model")]})


@pytest.mark.asyncio
async def test_merged_turn_emits_one_master_done_with_the_whole_id_list() -> None:
  """One MASTER_DONE per batch turn, carrying the parts' ids in order under
  input_event_ids."""
  session_id = "batch-done-ids"
  cfg = build_cfg()
  persisted: list[dict] = []

  async def persist(sid: str, event: dict) -> None:
    persisted.append(event)

  callbacks = _callbacks(persist_and_broadcast=persist)
  parts = [
      _item(session_id, "a", ET.SCHEDULED_TRIGGER, cfg, event_id="evt-a", auto_trigger=True, callbacks=callbacks),
      _item(session_id, "b", ET.USER, cfg, event_id="evt-b", callbacks=callbacks),
  ]
  await run_session_consumer(session_id, parts, make_sound_round("cc-1"))

  dones = [e for e in persisted if e.get("type") == ET.MASTER_DONE]
  assert len(dones) == 1
  assert dones[0][ET.INPUT_EVENT_IDS] == ["evt-a", "evt-b"]


# ---------------------------------------------------------------------------
# Batch boundaries
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_backend_option_change_splits_the_queue_into_two_turns() -> None:
  session_id = "batch-backend-split"
  cfg = build_cfg()
  option_a = cfg.backends.options[0]
  option_b = option_a.model_copy(update={"id": "other"})

  parts = [
      _item(session_id, "a1", ET.USER, cfg, event_id="e-a1", backend_option=option_a),
      _item(session_id, "a2", ET.USER, cfg, event_id="e-a2", backend_option=option_a),
      _item(session_id, "b1", ET.USER, cfg, event_id="e-b1", backend_option=option_b),
      _item(session_id, "b2", ET.USER, cfg, event_id="e-b2", backend_option=option_b),
  ]
  captured: list[master_cc_state._WorkItem] = []

  async def fake_run_cc(item: master_cc_state._WorkItem) -> tuple[str | None, int, str | None, dict]:
    captured.append(item)
    return (f"cc-{len(captured)}", 0, None, {})

  await run_session_consumer(session_id, parts, fake_run_cc)

  assert len(captured) == 2
  assert captured[0].user_event_ids == ["e-a1", "e-a2"]
  assert captured[1].user_event_ids == ["e-b1", "e-b2"]
  assert captured[0].backend_option.id == "fake"
  assert captured[1].backend_option.id == "other"
  for part in parts:
    assert part.future.done()


@pytest.mark.asyncio
async def test_resume_item_and_task_run_item_each_run_alone() -> None:
  session_id = "batch-unmergeable"
  cfg = build_cfg()
  record = MasterRunRecord(started_at=datetime.now(UTC), raw_log="/x/agent.raw.ndjson", user_event_ids=["e-resume"])

  resume_item = _item(
      session_id, "", None, cfg, event_id="e-resume", resume_record=record, resume_is_alive=lambda: False)
  task_item = _item(
      session_id,
      "run",
      None,
      cfg,
      event_id="e-task",
      task_run=master_cc_state.TaskRunBinding(session_id=session_id, run_id="r1", transport_dir="/tmp/r1"))
  plain = _item(session_id, "after", ET.USER, cfg, event_id="e-after")
  parts = [plain, resume_item, task_item]
  captured: list[master_cc_state._WorkItem] = []

  async def fake_round(item: master_cc_state._WorkItem) -> tuple[str | None, int, str | None, dict]:
    captured.append(item)
    return ("cc-x", 0, None, {})

  with patch.object(master_cc_run, "_resume_cc", side_effect=fake_round):
    await run_session_consumer(session_id, parts, fake_round)

  # The plain head cannot pull the resume follower; the resume item and the
  # task_run item each stand alone as their own turn.
  assert len(captured) == 3
  assert captured[0].user_event_ids == ["e-after"]
  assert captured[1] is resume_item
  assert captured[2] is task_item
  for part in parts:
    assert part.future.done() and part.future.exception() is None


@pytest.mark.asyncio
async def test_single_queued_item_runs_byte_identical_to_today() -> None:
  session_id = "batch-single"
  cfg = build_cfg()
  item = _item(session_id, "the only input", ET.USER, cfg, event_id="e-1", is_voice=True)
  captured: list[master_cc_state._WorkItem] = []

  async def fake_run_cc(seen: master_cc_state._WorkItem) -> tuple[str | None, int, str | None, dict]:
    captured.append(seen)
    return ("cc-1", 0, None, {})

  await run_session_consumer(session_id, [item], fake_run_cc)

  # The dequeued head itself runs, untouched: same object, same prompt fields,
  # no header line anywhere.
  assert captured == [item]
  assert captured[0].user_content == "the only input"
  assert captured[0].is_voice is True
  assert _headers(captured[0].user_content) == []


# ---------------------------------------------------------------------------
# Per-part voice and attachments; shared result and failure
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_voice_disclaimer_and_attachments_are_carried_per_part() -> None:
  session_id = "batch-voice-files"
  cfg = build_cfg()
  files_a = [{"filename": "a.png", "path": "/uploads/a.png", "size": 3}]

  voice_part = _item(
      session_id, "dictated words", ET.USER, cfg, event_id="e-voice", is_voice=True, should_check_tex=True)
  file_part = _item(session_id, "see attachment", ET.USER, cfg, event_id="e-file", uploaded_files=files_a)
  wake_part = _item(
      session_id, "[Scheduled trigger fired] ping", ET.SCHEDULED_TRIGGER, cfg, event_id="e-wake", auto_trigger=True)
  parts = [voice_part, file_part, wake_part]
  captured: list[master_cc_state._WorkItem] = []

  async def fake_run_cc(item: master_cc_state._WorkItem) -> tuple[str | None, int, str | None, dict]:
    captured.append(item)
    return ("cc-1", 0, None, {})

  await run_session_consumer(session_id, parts, fake_run_cc)

  merged = captured[0]
  # The disclaimer rides inside the voice part's section only.
  disclaimer = master_cc_run._VOICE_DISCLAIMER
  voice_header_at = merged.user_content.index("[Queued input 1 of 3")
  file_header_at = merged.user_content.index("[Queued input 2 of 3")
  assert disclaimer in merged.user_content[voice_header_at:file_header_at]
  assert disclaimer not in merged.user_content[file_header_at:]
  # The wake's own fired prefix stays as-is under its header.
  assert "[Scheduled trigger fired] ping" in merged.user_content[merged.user_content.index("[Queued input 3 of 3"):]
  # Attachments concatenate in order; tex is "any"; auto_trigger is "all".
  assert merged.uploaded_files == files_a
  assert merged.should_check_tex is True
  assert merged.auto_trigger is False
  assert merged.user_event_ids == ["e-voice", "e-file", "e-wake"]


@pytest.mark.asyncio
async def test_batch_exception_reaches_every_constituents_future() -> None:
  session_id = "batch-exception"
  cfg = build_cfg()
  parts = [
      _item(session_id, "a", ET.USER, cfg, event_id="e-a"),
      _item(session_id, "b", ET.USER, cfg, event_id="e-b"),
      _item(session_id, "c", ET.USER, cfg, event_id="e-c"),
  ]

  async def exploding_round(item: master_cc_state._WorkItem) -> tuple[str | None, int, str | None, dict]:
    raise RuntimeError("backend exploded")

  await run_session_consumer(session_id, parts, exploding_round)

  for part in parts:
    assert part.future.done()
    with pytest.raises(RuntimeError, match="backend exploded"):
      part.future.result()


# ---------------------------------------------------------------------------
# The declared input type: membership asserted at enqueue
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_enqueue_rejects_a_type_outside_input_event_types() -> None:
  session_id = "batch-bad-type"
  cfg = build_cfg()
  bad = _item(session_id, "x", "tool_result", cfg)
  with pytest.raises(ValueError, match="INPUT_EVENT_TYPES"):
    master_cc_queue._enqueue_work_item(session_id, bad)
  # No state was touched: nothing queued, nothing marked busy.
  assert session_id not in master_cc_state._session_queues


def test_input_event_types_members_cover_the_declared_entry_points() -> None:
  assert INPUT_EVENT_TYPES == frozenset({ET.USER, ET.AGENT_MESSAGE, ET.SCHEDULED_TRIGGER, ET.CHILD_REPORT})


@pytest.mark.asyncio
async def test_received_at_stamps_the_enqueue_moment() -> None:
  session_id = "batch-received-at"
  cfg = build_cfg()
  item = _item(session_id, "x", ET.USER, cfg)
  before = datetime.now(UTC)
  workers_mock = MagicMock()
  workers_mock._has_running_tasks = AsyncMock(return_value=False)
  with (
      patch.object(master_cc_run, "_run_cc", new=AsyncMock(return_value=("cc-1", 0, None, {}))),
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()),
      patch(SESSIONS_SESSION_MANAGER_PATCH_TARGET, return_value=workers_mock),
  ):
    master_cc_queue._enqueue_work_item(session_id, item)
    await drain_session_consumer(session_id, 5)
  assert item.received_at.utcoffset() is not None
  assert before - timedelta(seconds=5) <= item.received_at <= datetime.now(UTC) + timedelta(seconds=5)


# ---------------------------------------------------------------------------
# The backlog through the real funnels: separate chat events, one turn
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_backlog_through_the_real_funnels_persists_user_events_separately(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """24 trigger wakes, 2 chat messages, and 1 child-report wake arrive while a
  turn runs: every entry point declares its input type, the chat log keeps the
  two user messages as separate events, and the following turn runs the whole
  backlog once with all 27 futures resolved."""
  from src.core.config import CharlieBotConfig
  from src.core.master_trigger import trigger_master
  from src.core.sessions import SessionManager

  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      backends={"options": [_backend_option(id="fake", label="Fake", type="codex", model="fake-model")]})
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(_create_session_request("backlog"))
  sid = meta.id
  callbacks = session_mgr.callbacks()

  # 24 wake events persisted ahead of time, the way the fire path delivers them.
  wake_ids = []
  for i in range(24):
    await session_mgr.save_chat_event(
        sid, {
            "type": ET.SCHEDULED_TRIGGER,
            "content": f"[Scheduled trigger fired] wake {i}",
            "timestamp": datetime.now(UTC).isoformat(),
        })
    events = session_mgr.load_chat_events_sync(sid)
    wake_ids.append(events[-1]["id"])

  captured: list[master_cc_state._WorkItem] = []
  release = asyncio.Event()

  async def fake_round(item: master_cc_state._WorkItem) -> tuple[str | None, int, str | None, dict]:
    captured.append(item)
    await release.wait()
    return ("cc-batch", 0, None, {})

  enqueued = [0]
  reached_backlog = asyncio.Event()
  real_enqueue = master_cc_queue._enqueue_work_item

  def counting_enqueue(session_id: str, item: master_cc_state._WorkItem):
    result = real_enqueue(session_id, item)
    enqueued[0] += 1
    print(
        "ENQUEUE-COUNT", enqueued[0], item.input_event_type, item.backend_option.id if item.backend_option else None,
        item.session_meta.cc_session_id is not None, item.extra_claude_flags, item.expect_fresh_session)
    if enqueued[0] >= 28:  # the running turn + the 27-item backlog
      reached_backlog.set()
    return result

  monkeypatch.setattr(master_cc_queue, "_enqueue_work_item", counting_enqueue)
  monkeypatch.setattr(master_cc_run, "_run_cc", fake_round)

  async with fresh_master_state(sid):
    with patch(BROADCAST_PATCH_TARGET, new=AsyncMock()):
      # Every entry point resolves the backend option the way chat.py does
      # before calling run_message, so the run settings match across funnels.
      option = cfg.get_backend_option("fake")
      running = asyncio.create_task(
          master_cc_queue.run_message(
              cfg, meta, "the running turn", callbacks, ET.USER, skip_user_event=True, backend_option=option))
      await asyncio.wait_for(_wait_until(lambda: len(captured) == 1, timeout=5), timeout=6)

      # Each caller is serialized through its own enqueue: the test pins the
      # arrival order (24 wakes, then the two users, then the report), which
      # concurrent callers would otherwise order by await-point interleaving.
      # Every caller snapshots the metadata like an HTTP request would; the
      # shared in-memory object would alias the consumer's post-round anchor
      # write into the comparison.
      calls = []

      async def arrive(coro, ordinal: int) -> None:
        task = asyncio.create_task(coro)
        calls.append(task)
        try:
          await asyncio.wait_for(_wait_until(lambda: enqueued[0] >= ordinal, timeout=10), timeout=11)
        except BaseException:
          if task.done() and not task.cancelled():
            print("ARRIVE-CALLER-RAISED", repr(task.exception()))
          raise

      for i, wake_id in enumerate(wake_ids):
        await arrive(
            trigger_master(
                sid,
                f"[Scheduled trigger fired] wake {i}",
                cfg,
                session_mgr,
                ET.SCHEDULED_TRIGGER,
                user_event_id=wake_id,
                pull_back=False), 2 + i)
      await arrive(
          master_cc_queue.run_message(
              cfg, meta.model_copy(deep=True), "first user message", callbacks, ET.USER, backend_option=option), 26)
      await arrive(
          master_cc_queue.run_message(
              cfg, meta.model_copy(deep=True), "second user message", callbacks, ET.USER, backend_option=option), 27)
      await arrive(trigger_master(sid, '{"iteration": 3}', cfg, session_mgr, ET.CHILD_REPORT), 28)
      await asyncio.wait_for(reached_backlog.wait(), timeout=10)
      release.set()
      await asyncio.wait_for(running, timeout=10)
      await asyncio.gather(*calls, return_exceptions=False)
      consumer = master_cc_state._session_consumers.get(sid)
      if consumer is not None:
        await asyncio.wait_for(consumer, timeout=10)

  # captured[0] is the running turn; exactly one following turn holds the backlog.
  assert len(captured) == 2, "the whole backlog must run as one following turn"
  merged = captured[1]
  assert len(_headers(merged.user_content)) == 27
  events = session_mgr.load_chat_events_sync(sid)
  # The two user messages stayed separate chat events.
  user_events = [e for e in events if e["type"] == ET.USER]
  assert [e["content"] for e in user_events] == ["first user message", "second user message"]
  wake_events = [e for e in events if e["type"] == ET.SCHEDULED_TRIGGER]
  assert len(wake_events) == 24
  # 26 ids: the wakes and the users each persist a chat event; the child report
  # rides the wake summary without one, so it contributes no id.
  user_ids = [e["id"] for e in user_events]
  assert merged.user_event_ids == [*wake_ids, *user_ids]
  # One MASTER_DONE per turn; the backlog turn's done answers the whole batch.
  dones = [e for e in events if e["type"] == ET.MASTER_DONE]
  assert len(dones) == 2
  assert dones[1][ET.INPUT_EVENT_IDS] == merged.user_event_ids


def _backend_option(**kwargs):
  from conftest import backend_option

  return backend_option(**kwargs)


def _create_session_request(name: str):
  from src.core.models import CreateSessionRequest

  return CreateSessionRequest(name=name)


async def _wait_until(predicate, timeout: float) -> None:
  deadline = asyncio.get_running_loop().time() + timeout
  while not predicate():
    if asyncio.get_running_loop().time() > deadline:
      raise TimeoutError("condition not reached")
    await asyncio.sleep(0.01)
