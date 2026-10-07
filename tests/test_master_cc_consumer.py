"""Tests for master_cc_queue._session_consumer cc_session_id relay and thinking_state ownership."""

from __future__ import annotations

import asyncio
from collections.abc import AsyncIterator
from dataclasses import replace
from datetime import UTC, datetime, timedelta
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from conftest import (
    BUILD_BACKEND_PATCH_TARGET,
    SESSIONS_SESSION_MANAGER_PATCH_TARGET,
    ConsumerRound,
    TerminateFlagBackend,
    _run_seeded_consumer,
    build_master_cc_cfg,
    drain_session_consumer,
    fresh_master_state,
    make_sound_round,
    make_work_item,
    manager_backed_callbacks,
    mock_session_callbacks,
    mocked_callback_fields,
    patch_instructions_content,
    patch_resume_seams,
    run_resume_round,
    run_session_consumer,
)

from src.features.latex import latex
from src.infra import event_types as ET
from src.infra.models import CreateSessionRequest, MasterRunRecord, SessionCallbacks, SessionMetadata
from src.runtime import master_cc_queue, master_cc_run, master_cc_state, streaming, thinking_state
from src.runtime.agent_process.base import make_result_event
from src.runtime.sessions import SessionManager


def _make_meta(session_id: str) -> SessionMetadata:
  return SessionMetadata(id=session_id, name="t", backend="fake", cc_session_id=None)


async def run_consumer_over_real_disk(
    session_id: str,
    work_items: list[master_cc_state._WorkItem],
    fake_run_cc: ConsumerRound,
) -> None:
  """run_session_consumer with the SessionManager class kept real: the dequeue refresh reads disk
  through it, the teardown probe is silenced at the method, and no class-level patch can shadow
  the refresh's own local import."""
  await _run_seeded_consumer(
      session_id,
      work_items,
      fake_run_cc,
      patch.object(SessionManager, "_has_running_tasks", AsyncMock(return_value=False)),
  )


@pytest.mark.asyncio
async def test_consumer_relays_cc_session_id_across_metadata_instances() -> None:
  """Two queued _WorkItems with distinct SessionMetadata objects must share cc_session_id.

  Reproduces the fork_session race where the bootstrap turn sets cc_session_id on
  meta_A but a concurrently-loaded meta_B still has cc_session_id=None.
  """
  session_id = "test-session-relay"

  meta_bootstrap = _make_meta(session_id)
  meta_user_message = _make_meta(session_id)  # distinct instance, freshly loaded from disk
  cb = mock_session_callbacks()
  item_bootstrap = make_work_item(MagicMock(), meta_bootstrap, None, user_content="hi", callbacks=cb)
  # Different extra flags keep the two items in separate turns under the batch
  # contract (equal run settings would merge them into one round); the relay
  # under test is the consumer's cross-turn cc_session_id fill.
  item_user = replace(
      make_work_item(MagicMock(), meta_user_message, None, user_content="hi", callbacks=cb),
      extra_claude_flags=["--relay-probe"],
  )

  observed_cc_session_ids: list = []

  async def fake_run_cc(item: master_cc_state._WorkItem) -> tuple[str | None, int, str | None, dict]:
    observed_cc_session_ids.append(item.session_meta.cc_session_id)
    return ("cc-id-from-bootstrap", 0, None, {})

  await run_session_consumer(session_id, [item_bootstrap, item_user], fake_run_cc)

  assert observed_cc_session_ids == [None, "cc-id-from-bootstrap"
                                    ], ("second _run_cc must observe cc_session_id relayed from bootstrap meta")
  assert meta_user_message.cc_session_id == "cc-id-from-bootstrap"
  assert item_bootstrap.future.done() and item_bootstrap.future.result() == "cc-id-from-bootstrap"
  assert item_user.future.done() and item_user.future.result() == "cc-id-from-bootstrap"


# ---------------------------------------------------------------------------
# thinking_state: single in-memory owner of busy intervals (T1-T5)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
@pytest.mark.parametrize("inject_at", ["run_cc", "master_done_persist", "worker_probe", "idle_broadcast"])
async def test_busy_invariant_holds_under_adversarial_enqueue(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    inject_at: str,
) -> None:
  """T1: an enqueue landing at any await in the consumer's tail keeps the invariant.

  Parametrised over every await in the tail (the MASTER_DONE persist, the
  worker probe, the idle broadcast) and over _run_cc itself. At each injection
  point one extra work item is enqueued through the real run_message; every
  entry into _run_cc must observe busy_since non-None, and after the consumer
  ends busy_since must be None.
  """
  session_id = f"t1-{inject_at}"
  cfg = build_master_cc_cfg(tmp_path)
  monkeypatch.setattr(latex, "get_tex_path", lambda: tmp_path / "missing.tex")

  entries: list[datetime | None] = []
  injected = False
  injected_task: asyncio.Task | None = None
  real_persist = AsyncMock()

  async def _inject_once() -> None:
    nonlocal injected, injected_task
    if injected:
      return
    injected = True
    injected_task = asyncio.create_task(
        master_cc_queue.run_message(
            cfg,
            SessionMetadata(id=session_id, name="t"),
            "extra",
            callbacks,
            ET.USER,
            skip_user_event=True,
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

  async def probe_hook(sid: str) -> bool:
    if inject_at == "worker_probe":
      await _inject_once()
      await asyncio.sleep(0)
    return False

  workers_mock = MagicMock()
  workers_mock._has_running_tasks = probe_hook
  callbacks = SessionCallbacks(
      persist_and_broadcast=persist_hook,
      **mocked_callback_fields(),
      persist_master_run=AsyncMock(),
  )

  monkeypatch.setattr(master_cc_run, "_run_cc", fake_run_cc)
  monkeypatch.setattr(streaming.streaming_manager, "broadcast", broadcast_hook)
  monkeypatch.setattr(SESSIONS_SESSION_MANAGER_PATCH_TARGET, lambda *a, **k: workers_mock)

  async with fresh_master_state(session_id):
    task1 = asyncio.create_task(
        master_cc_queue.run_message(
            cfg, SessionMetadata(id=session_id, name="t"), "first", callbacks, ET.USER, skip_user_event=True))
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
# Resume anchor single-owner persistence + pre-flight (regression tests)
# ---------------------------------------------------------------------------


class _NoopBackend(TerminateFlagBackend):
  """Minimal backend double: yields no events, exits cleanly."""

  exit_code = 0
  stderr_text = ""

  async def run(self,
                prompt: str,
                cwd: str,
                env: dict,
                uploaded_files: list[dict] | None = None) -> AsyncIterator[dict]:
    if False:
      yield {}  # keeps run() an async generator; the consumer's async-for would TypeError on a coroutine


@pytest.mark.asyncio
async def test_consumer_persists_cc_session_id_to_disk(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Anchor lands on disk: after one round through the consumer, a second,
  cold-cache SessionManager reads the cc_session_id the backend returned —
  an assertion an in-memory-object check cannot make.
  """
  cfg = build_master_cc_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="anchor-on-disk"))
  backend_returned_id = "cc-backend-session-42"

  monkeypatch.setattr(master_cc_run, "_run_cc", make_sound_round(backend_returned_id))
  monkeypatch.setattr(latex, "get_tex_path", lambda: tmp_path / "missing.tex")
  monkeypatch.setattr(streaming.streaming_manager, "broadcast", AsyncMock())

  async with fresh_master_state(session.id):
    result = await master_cc_queue.run_message(
        cfg, session, "hi", session_mgr.callbacks(), ET.USER, skip_user_event=True)
    assert result == backend_returned_id
    await drain_session_consumer(session.id, timeout=5)

  # Cold-cache reader: a fresh SessionManager parses metadata.json from disk.
  cold_reader = SessionManager(cfg)
  cold_meta = await cold_reader.get_session(session.id)
  assert cold_meta is not None
  assert cold_meta.cc_session_id == backend_returned_id


@pytest.mark.asyncio
async def test_pre_flight_fires_anchor_missing_when_round_done_and_anchor_empty(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  """Pre-flight: a resume-capable backend with an empty anchor but a completed
  round emits resume_context_dropped with reason='anchor_missing'."""
  cfg = build_master_cc_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="pre-flight"))
  # Seed a completed round so has_completed_round returns True; anchor stays empty.
  await session_mgr.save_chat_event(session.id, {"type": ET.MASTER_DONE, "exit_code": 0})
  session_mgr._chat_events.clear_cache(session.id)

  meta = await session_mgr.get_session(session.id)
  assert meta is not None
  assert meta.cc_session_id is None

  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, lambda *a, **k: _NoopBackend())
  patch_instructions_content(monkeypatch)
  monkeypatch.setattr(streaming.streaming_manager, "broadcast", AsyncMock())

  item = make_work_item(
      cfg, meta, cfg.backends.options[0], user_content="next round", callbacks=session_mgr.callbacks())
  await master_cc_run._run_cc(item)

  events = session_mgr.load_chat_events_sync(session.id)
  dropped = [e for e in events if e.get("type") == ET.RESUME_CONTEXT_DROPPED]
  assert len(dropped) == 1
  assert dropped[0]["reason"] == "anchor_missing"


# ---------------------------------------------------------------------------
# Zero-output guard: a settled run with all-zero usage and no output fails loudly
# ---------------------------------------------------------------------------


class _EventsBackend(TerminateFlagBackend):
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


def _guard_events(cb: SessionCallbacks) -> tuple[list[dict], list[dict]]:
  """Split persisted/broadcast events into zero-output ERRORs and MASTER_DONEs."""
  errors = [c.args[1] for c in cb.persist_and_broadcast.await_args_list if c.args[1].get("type") == ET.ERROR]
  dones = [c.args[1] for c in cb.persist_and_broadcast.await_args_list if c.args[1].get("type") == ET.MASTER_DONE]
  return errors, dones


async def _run_stream_consumer(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    session_id: str,
    events: list[dict],
    *,
    exit_code: int = 0,
    stderr_text: str = "",
) -> SessionCallbacks:
  """Run a simulated event stream through the production consumer path."""
  cfg = build_master_cc_cfg(tmp_path)
  meta = _make_meta(session_id)
  cb = mock_session_callbacks()
  backend = _EventsBackend(events, exit_code=exit_code, stderr_text=stderr_text)

  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, lambda *a, **k: backend)
  patch_instructions_content(monkeypatch)
  monkeypatch.setattr(latex, "get_tex_path", lambda: tmp_path / "missing.tex")
  monkeypatch.setattr(streaming.streaming_manager, "broadcast", AsyncMock())

  async with fresh_master_state(session_id):
    await master_cc_queue.run_message(cfg, meta, "hi", cb, ET.USER, skip_user_event=True)
    await drain_session_consumer(session_id, timeout=5)
  return cb


@pytest.mark.asyncio
async def test_zero_output_guard_fires_on_all_zero_result(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Positive: a result with all-zero usage and nothing else fails loudly."""
  cb = await _run_stream_consumer(tmp_path, monkeypatch, "zero-pos", [make_result_event()])

  errors, dones = _guard_events(cb)
  assert len(errors) == 1
  assert "zero model output" in errors[0].get("message", "")
  assert "fresh session" in errors[0].get("message", "")
  assert "left unread" in errors[0].get("message", "")
  assert "LESSONS.md" in errors[0].get("message", "")
  assert dones and dones[0]["exit_code"] == 1, "MASTER_DONE must exit nonzero"
  cb.mark_unread.assert_awaited()


@pytest.mark.asyncio
async def test_zero_output_guard_covers_resume_path(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The guard fires on the resume (re-attach) outcome, not just fresh runs."""
  session_id = "zero-resume"
  started_at = datetime.now(UTC) - timedelta(seconds=60)
  record = MasterRunRecord(pid=1234, pid_start="100", started_at=started_at, raw_log="<fake>")

  cfg = build_master_cc_cfg(tmp_path)
  meta = _make_meta(session_id)
  cb = mock_session_callbacks()

  async def fake_resume_cc(item: master_cc_state._WorkItem) -> tuple:
    return "cc-resumed-id", 0, None, {"zero_output": True}

  patch_resume_seams(monkeypatch, resume_cc=fake_resume_cc)
  await run_resume_round(cfg, meta, record, cb, is_alive=lambda: True)

  errors, dones = _guard_events(cb)
  assert len(errors) == 1
  assert "cc-resumed" in errors[0].get("message", "")
  assert dones and dones[0]["exit_code"] == 1


# ---------------------------------------------------------------------------
# Dequeue anchor refresh, post-round copy retirement, and the stale-enqueue-snapshot shape
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_consumer_keeps_the_durable_anchor_when_a_turn_returns_no_session_id(tmp_path: Path) -> None:
  """A turn that ends without a backend session id (a refusal or a spawn /
  transport failure) must not wipe the durable resume anchor: only a truthy id
  ever persists. Clearing for a v2 fresh-native launch is the adapter's
  spawn-time write, not this path."""
  from conftest import build_sessions_cfg

  cfg = build_sessions_cfg(tmp_path)
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="anchor-preserved"))
  await mgr.persist_cc_session_id(session.id, "kept-anchor")

  snapshot = SessionMetadata(id=session.id, name="anchor-preserved", backend=cfg.backends.options[0].id)
  snapshot.cc_session_id = "kept-anchor"
  item = make_work_item(cfg, snapshot, cfg.backends.options[0], callbacks=manager_backed_callbacks(mgr))

  async def refused_round(item: master_cc_state._WorkItem) -> tuple[str | None, int, str | None, dict]:
    return (None, 1, "refused", {})

  await run_consumer_over_real_disk(session.id, [item], refused_round)

  cold_reader = SessionManager(cfg)
  cold_meta = await cold_reader.get_session(session.id)
  assert cold_meta is not None
  assert cold_meta.cc_session_id == "kept-anchor"
