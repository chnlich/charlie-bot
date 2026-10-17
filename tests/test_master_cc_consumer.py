"""Tests for master_cc_queue._session_consumer cc_session_id relay and thinking_state ownership."""

from __future__ import annotations

import asyncio
import contextlib
import dataclasses
import datetime
import json
import pathlib
from collections.abc import AsyncIterator
from typing import Any
from unittest import mock

import conftest
import pytest

from src.features.slack import metadata as slack_metadata
from src.infra import config, models
from src.infra import event_types as ET
from src.infra import tasks as infra_tasks
from src.runtime import autonamer, master_cc_queue, master_cc_run, master_cc_state, streaming, thinking_state
from src.runtime.agent_process import base


def _make_meta(session_id: str) -> models.SessionMetadata:
  return models.SessionMetadata(profile="manager", id=session_id, name="t", backend="fake", cc_session_id=None)


async def run_consumer_over_real_disk(
    session_id: str,
    work_items: list[master_cc_state._WorkItem],
    fake_run_cc: conftest.ConsumerRound,
) -> None:
  """conftest.run_session_consumer with the session blocks kept real: the dequeue refresh reads disk
  through it, the teardown probe is silenced at the method, and no class-level patch can shadow
  the refresh's own local import."""
  await conftest._run_seeded_consumer(
      session_id,
      work_items,
      fake_run_cc,
      contextlib.nullcontext(),
  )


@pytest.mark.asyncio
async def test_consumer_relays_cc_session_id_across_metadata_instances() -> None:
  """Two queued _WorkItems with distinct models.SessionMetadata objects must share cc_session_id.

  Reproduces the fork_session race where the bootstrap turn sets cc_session_id on
  meta_A but a concurrently-loaded meta_B still has cc_session_id=None.
  """
  session_id = "test-session-relay"

  meta_bootstrap = _make_meta(session_id)
  meta_user_message = _make_meta(session_id)  # distinct instance, freshly loaded from disk
  cb = conftest.mock_session_callbacks()
  item_bootstrap = conftest.make_work_item(mock.MagicMock(), meta_bootstrap, None, user_content="hi", callbacks=cb)
  # Different extra flags keep the two items in separate turns under the batch
  # contract (equal run settings would merge them into one round); the relay
  # under test is the consumer's cross-turn cc_session_id fill.
  item_user = dataclasses.replace(
      conftest.make_work_item(mock.MagicMock(), meta_user_message, None, user_content="hi", callbacks=cb),
      extra_claude_flags=["--relay-probe"],
  )

  observed_cc_session_ids: list = []

  async def fake_run_cc(item: master_cc_state._WorkItem) -> tuple[str | None, int, str | None, dict]:
    observed_cc_session_ids.append(item.session_meta.cc_session_id)
    return ("cc-id-from-bootstrap", 0, None, {})

  await conftest.run_session_consumer(session_id, [item_bootstrap, item_user], fake_run_cc)

  assert observed_cc_session_ids == [None, "cc-id-from-bootstrap"
                                    ], ("second _run_cc must observe cc_session_id relayed from bootstrap meta")
  assert meta_user_message.cc_session_id == "cc-id-from-bootstrap"
  assert item_bootstrap.future.done() and item_bootstrap.future.result() == "cc-id-from-bootstrap"
  assert item_user.future.done() and item_user.future.result() == "cc-id-from-bootstrap"


# ---------------------------------------------------------------------------
# thinking_state: single in-memory owner of busy intervals (T1-T5)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
@pytest.mark.parametrize("inject_at", ["run_cc", "master_done_persist", "idle_broadcast"])
async def test_busy_invariant_holds_under_adversarial_enqueue(
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
    inject_at: str,
) -> None:
  """T1: an enqueue landing at any await in the consumer's tail keeps the invariant.

  Parametrised over every await in the tail (the MASTER_DONE persist and the
  idle broadcast) and over _run_cc itself. At each injection
  point one extra work item is enqueued through the real run_message; every
  entry into _run_cc must observe busy_since non-None, and after the consumer
  ends busy_since must be None.
  """
  session_id = f"t1-{inject_at}"
  cfg = conftest.build_master_cc_cfg(tmp_path)
  conftest.bind_session_blocks(monkeypatch, conftest.build_session_blocks(cfg))

  entries: list[datetime.datetime | None] = []
  injected = False
  injected_task: asyncio.Task | None = None
  real_persist = mock.AsyncMock()

  async def _inject_once() -> None:
    nonlocal injected, injected_task
    if injected:
      return
    injected = True
    injected_task = asyncio.create_task(
        conftest.run_task_manager_message(
            cfg,
            models.SessionMetadata(id=session_id, name="t", profile="manager"),
            "extra",
            callbacks,
        ))

  async def fake_run_cc(item: master_cc_state._WorkItem) -> tuple:
    entries.append(thinking_state.busy_since(session_id))
    if inject_at == "run_cc":
      await _inject_once()
      await asyncio.sleep(0)
    return ("cc-1", 0, None, {})

  async def persist_hook(sid: str, event: dict) -> None:
    if inject_at == "master_done_persist" and event.get("type") == ET.MASTER_DONE:
      await _inject_once()
      await asyncio.sleep(0)
    await real_persist(sid, event)

  async def broadcast_hook(channel: str, event: dict) -> None:
    if (inject_at == "idle_broadcast" and event.get("type") == ET.RUNNING_CHANGED and
        event.get("thinking_since") is None):
      await _inject_once()
      await asyncio.sleep(0)

  callbacks = models.SessionCallbacks(
      persist_and_broadcast=persist_hook,
      **conftest.mocked_callback_fields(),
  )

  monkeypatch.setattr(master_cc_run, "_run_cc", fake_run_cc)
  monkeypatch.setattr(streaming.streaming_manager, "broadcast", broadcast_hook)

  async with conftest.fresh_master_state(session_id):
    task1 = asyncio.create_task(
        conftest.run_task_manager_message(
            cfg, models.SessionMetadata(id=session_id, name="t", profile="manager"), "first", callbacks))
    assert await asyncio.wait_for(task1, timeout=5) == "cc-1"

    # For injections fired during the consumer's teardown awaits, the work item
    # only appears once that await runs — let the consumer reach it.
    for _ in range(1000):
      if injected_task is not None:
        break
      await asyncio.sleep(0)
    assert injected_task is not None, f"injection did not fire at {inject_at}"
    assert await asyncio.wait_for(injected_task, timeout=5) == "cc-1"

    # Let the (possibly second) consumer task finish its teardown.
    remaining = master_cc_state._session_consumers.get(session_id)
    if remaining is not None:
      await asyncio.wait_for(remaining, timeout=5)

    assert len(entries) == 2
    assert all(start is not None for start in entries), "every _run_cc entry must observe busy_since set"
    assert thinking_state.busy_since(session_id) is None


# ---------------------------------------------------------------------------
# Resume anchor single-owner persistence
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_consumer_persists_cc_session_id_to_disk(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Anchor lands on disk: after one round through the consumer, a second,
  cold-cache session blocks read the cc_session_id the backend returned —
  an assertion an in-memory-object check cannot make.
  """
  cfg = conftest.build_master_cc_cfg(tmp_path)
  session_blocks = conftest.build_session_blocks(cfg)
  session = await conftest.create_root_session(session_blocks, models.CreateSessionRequest(name="anchor-on-disk"))
  conftest.bind_session_blocks(monkeypatch, session_blocks)
  backend_returned_id = "cc-backend-session-42"

  monkeypatch.setattr(master_cc_run, "_run_cc", conftest.make_sound_round(backend_returned_id))
  monkeypatch.setattr(streaming.streaming_manager, "broadcast", mock.AsyncMock())

  async with conftest.fresh_master_state(session.id):
    result = await conftest.run_task_manager_message(cfg, session, "hi", session_blocks.callbacks())
    assert result == backend_returned_id
    await conftest.drain_session_consumer(session.id, timeout=5)

  # Cold-cache reader: fresh session blocks parse metadata.json from disk.
  cold_reader = conftest.build_session_blocks(cfg)
  cold_meta = await cold_reader.store.get_session(session.id)
  assert cold_meta is not None
  assert cold_meta.cc_session_id == backend_returned_id


@pytest.mark.asyncio
# ---------------------------------------------------------------------------
# Zero-output guard: a settled run with all-zero usage and no output fails loudly
# ---------------------------------------------------------------------------
class _EventsBackend(conftest.TerminateFlagBackend):
  """Backend double that yields a fixed event stream, then exits with the given code."""

  def __init__(self, events: list[dict], *, exit_code: int = 0, stderr_text: str = "") -> None:
    self.events = events
    self.exit_code = exit_code
    self.stderr_text = stderr_text

  async def run(self,
                prompt: str,
                cwd: str,
                env: dict,
                uploaded_files: list[dict] | None = None) -> AsyncIterator[dict]:
    for event in self.events:
      yield event


def _guard_events(cb: models.SessionCallbacks) -> tuple[list[dict], list[dict]]:
  """Split persisted/broadcast events into zero-output ERRORs and MASTER_DONEs."""
  errors = [c.args[1] for c in cb.persist_and_broadcast.await_args_list if c.args[1].get("type") == ET.ERROR]
  dones = [c.args[1] for c in cb.persist_and_broadcast.await_args_list if c.args[1].get("type") == ET.MASTER_DONE]
  return errors, dones


async def _run_stream_consumer(
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
    session_id: str,
    events: list[dict],
    *,
    exit_code: int = 0,
    stderr_text: str = "",
    meta: models.SessionMetadata | None = None,
) -> models.SessionCallbacks:
  """Run a simulated event stream through the production consumer path; *meta* defaults to a plain manager session."""
  cfg = conftest.build_master_cc_cfg(tmp_path)
  conftest.bind_session_blocks(monkeypatch, conftest.build_session_blocks(cfg))
  meta = meta if meta is not None else _make_meta(session_id)
  cb = conftest.mock_session_callbacks()
  backend = _EventsBackend(events, exit_code=exit_code, stderr_text=stderr_text)

  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, lambda *a, **k: backend)
  monkeypatch.setattr(streaming.streaming_manager, "broadcast", mock.AsyncMock())

  async with conftest.fresh_master_state(session_id):
    await conftest.run_task_manager_message(cfg, meta, "hi", cb)
    await conftest.drain_session_consumer(session_id, timeout=5)
  return cb


@pytest.mark.asyncio
async def test_zero_output_guard_fires_on_all_zero_result(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Positive: a result with all-zero usage and nothing else fails loudly."""
  cb = await _run_stream_consumer(tmp_path, monkeypatch, "zero-pos", [base.make_result_event()])

  errors, dones = _guard_events(cb)
  assert len(errors) == 1
  assert "zero model output" in errors[0].get("message", "")
  assert "fresh session" in errors[0].get("message", "")
  assert "left unread" in errors[0].get("message", "")
  assert "LESSONS.md" in errors[0].get("message", "")
  assert dones and dones[0]["exit_code"] == 1, "MASTER_DONE must exit nonzero"
  cb.mark_unread.assert_awaited()


@pytest.mark.asyncio
async def test_zero_output_guard_covers_resume_path(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The guard fires on the resume (re-attach) outcome, not just fresh runs."""
  session_id = "zero-resume"
  started_at = datetime.datetime.now(datetime.UTC) - datetime.timedelta(seconds=60)
  record = models.MasterRunRecord(pid=1234, pid_start="100", started_at=started_at, raw_log="<fake>")

  cfg = conftest.build_master_cc_cfg(tmp_path)
  meta = _make_meta(session_id)
  cb = conftest.mock_session_callbacks()

  async def fake_resume_cc(item: master_cc_state._WorkItem) -> tuple:
    return "cc-resumed-id", 0, None, {"zero_output": True}

  conftest.patch_resume_seams(monkeypatch, resume_cc=fake_resume_cc)
  await conftest.run_resume_round(cfg, meta, record, cb, is_alive=lambda: True)

  errors, dones = _guard_events(cb)
  assert len(errors) == 1
  assert "cc-resumed" in errors[0].get("message", "")
  assert dones and dones[0]["exit_code"] == 1


# ---------------------------------------------------------------------------
# Dequeue anchor refresh, post-round copy retirement, and the stale-enqueue-snapshot shape
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_consumer_keeps_the_durable_anchor_when_a_turn_returns_no_session_id(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A turn that ends without a backend session id (a refusal or a spawn /
  transport failure) must not wipe the durable resume anchor: only a truthy id
  ever persists. Clearing for a v2 fresh-native launch is the adapter's
  spawn-time write, not this path."""
  cfg = conftest.build_sessions_cfg(tmp_path)
  mgr = conftest.build_session_blocks(cfg)
  session = await conftest.create_root_session(mgr, models.CreateSessionRequest(name="anchor-preserved"))
  conftest.bind_session_blocks(monkeypatch, mgr)
  await mgr.anchors.persist_cc_session_id(session.id, "kept-anchor")

  snapshot = models.SessionMetadata(
      profile="manager", id=session.id, name="anchor-preserved", backend=cfg.backends.options[0].id)
  snapshot.cc_session_id = "kept-anchor"
  item = conftest.make_work_item(
      cfg, snapshot, cfg.backends.options[0], callbacks=conftest.manager_backed_callbacks(mgr))

  async def refused_round(item: master_cc_state._WorkItem) -> tuple[str | None, int, str | None, dict]:
    return (None, 1, "refused", {})

  await run_consumer_over_real_disk(session.id, [item], refused_round)

  cold_reader = conftest.build_session_blocks(cfg)
  cold_meta = await cold_reader.store.get_session(session.id)
  assert cold_meta is not None
  assert cold_meta.cc_session_id == "kept-anchor"


# ---------------------------------------------------------------------------
# After-round session naming: the consumer starts the autonamer after MASTER_DONE
# ---------------------------------------------------------------------------


class _StubLightBackend:
  """Light-backend double whose one_shot_text answers with the fixed naming verdict."""

  async def one_shot_text(self, prompt: str, system_prompt: str, *, timeout: float) -> str:
    return json.dumps({"name": "Stub Title", "group": "Stub"})


@pytest.mark.asyncio
async def test_consumer_names_a_default_session_after_master_done(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """One round through the real consumer names "Session 7" via the autonamer.

  The consumer resolves the round future and then starts name_after_round
  fire-and-forget; the test awaits that background task (no fixed sleep) before
  reading the renamed metadata. Guards the call itself: with the after-round
  lines removed, no naming task starts and the session keeps its default name.
  """
  cfg = config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backends={
          "options": [conftest.backend_option(id="fake", label="Fake", type="codex", model="fake-model")],
          "pool": ["fake"],
      })
  blocks = conftest.build_session_blocks(cfg)
  session = await conftest.create_root_session(blocks, models.CreateSessionRequest(name="Session 7"))
  conftest.bind_session_blocks(monkeypatch, blocks)
  conftest.append_events(
      blocks.events.get_chat_events_path(session.id),
      [conftest.user_event("Name this conversation."),
       conftest.assistant_event("We planned the session naming fix.")],
  )
  monkeypatch.setattr(autonamer, "build_backend", lambda option, cfg, **_: _StubLightBackend())
  monkeypatch.setattr(streaming.streaming_manager, "broadcast", mock.AsyncMock())

  naming_tasks: list[asyncio.Task] = []
  real_create_logged_task = infra_tasks.create_logged_task

  def spy_create_logged_task(coro: Any, *, name: str | None = None) -> asyncio.Task:
    task = real_create_logged_task(coro, name=name)
    if name == f"after-round-{session.id}":
      naming_tasks.append(task)
    return task

  monkeypatch.setattr(master_cc_queue.tasks, "create_logged_task", spy_create_logged_task)

  snapshot = models.SessionMetadata(
      profile="manager", id=session.id, name="Session 7", backend=cfg.backends.options[0].id)
  item = conftest.make_work_item(cfg, snapshot, cfg.backends.options[0], callbacks=blocks.callbacks())

  async def sound_round(item: master_cc_state._WorkItem) -> tuple[str | None, int, str | None, dict]:
    return ("cc-named-round", 0, None, {})

  await run_consumer_over_real_disk(session.id, [item], sound_round)

  assert naming_tasks, "the consumer never started the after-round naming task"
  await asyncio.wait_for(asyncio.gather(*naming_tasks), timeout=10)

  named = await blocks.store.get_session(session.id)
  assert named is not None
  assert named.name == "7: Stub Title"
  assert named.group == "Stub"


# ---------------------------------------------------------------------------
# The turn-end unread mark: a clean turn that a turn contribution claims leaves the mark to it
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_a_clean_web_turn_leaves_the_unread_mark_to_the_web_reply_contribution(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cb = await _run_stream_consumer(
      tmp_path, monkeypatch, "web-clean",
      [base.make_text_event("answer"), base.make_result_event()])

  cb.mark_unread.assert_not_awaited()


@pytest.mark.asyncio
async def test_an_errored_web_turn_still_marks_unread(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cb = await _run_stream_consumer(
      tmp_path, monkeypatch, "web-error", [base.make_text_event("partial")], exit_code=1, stderr_text="backend failed")

  cb.mark_unread.assert_awaited()


@pytest.mark.asyncio
async def test_a_nonzero_exit_without_an_error_message_still_marks_unread_for_a_web_turn(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A silent failure has no error message to show, so only a zero exit counts as a clean turn."""
  cb = await _run_stream_consumer(tmp_path, monkeypatch, "web-silent", [base.make_text_event("partial")], exit_code=1)

  cb.mark_unread.assert_awaited()


@pytest.mark.asyncio
async def test_a_clean_thread_session_turn_still_marks_unread(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  meta = models.SessionMetadata(
      profile="manager",
      id="thread-clean",
      name="thread",
      backend="fake",
      cc_session_id=None,
      slack_origin=slack_metadata.SlackOrigin(team_id="T1", channel_id="C1", thread_ts="1700000000.000100"))
  cb = await _run_stream_consumer(
      tmp_path, monkeypatch, "thread-clean",
      [base.make_text_event("answer"), base.make_result_event()], meta=meta)

  cb.mark_unread.assert_awaited()


@pytest.mark.asyncio
async def test_a_refused_web_turn_still_marks_unread(tmp_path: pathlib.Path) -> None:
  """A turn with no backend to run on is refused before any spawn, and that refusal marks unread."""
  cfg = conftest.build_master_cc_cfg(tmp_path)
  cb = conftest.mock_session_callbacks()
  item = conftest.make_work_item(
      cfg, models.SessionMetadata(profile="manager", id="refused", name="r"), None, callbacks=cb)

  await master_cc_run._run_cc(item)

  cb.mark_unread.assert_awaited()
