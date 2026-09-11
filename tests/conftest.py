import asyncio
import contextlib
import json
import os
import re
import shutil
import subprocess
import sys
import time
from collections.abc import AsyncIterator, Awaitable, Callable, Coroutine, Iterator
from datetime import UTC, datetime, timedelta
from pathlib import Path
from types import SimpleNamespace
from typing import Any, NamedTuple
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
import yaml
from fastapi import APIRouter, FastAPI
from fastapi.testclient import TestClient
from starlette.requests import Request

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
  sys.path.insert(0, str(ROOT))

# Imports must follow the sys.path bootstrap above.
import src.core.config as core_config  # noqa: E402,I001
from src.agents import master_cc_queue, master_cc_run, master_cc_state  # noqa: E402
from src.agents.backends import base as backend_base  # noqa: E402
from src.api.cron import router as cron_router  # noqa: E402
from src.api.deps import get_session_manager  # noqa: E402
from src.api.internal import router as internal_router  # noqa: E402
from src.api.sessions import router as sessions_router  # noqa: E402
from src.core import claude_accounts  # noqa: E402
from src.core import event_types as ET  # noqa: E402
from src.core import improve_command  # noqa: E402
from src.core import init as init_module  # noqa: E402
from src.core import runs  # noqa: E402
from src.core import thinking_state  # noqa: E402
from src.core import init_worker_recovery as worker_recovery_module  # noqa: E402
from src.core import models  # noqa: E402
from src.core import review  # noqa: E402
from src.api.deps import get_config_on_loop  # noqa: E402
from src.core.config import CharlieBotConfig, get_config  # noqa: E402
from src.core.git import BaseResolution  # noqa: E402
from src.core.plans import PlanRegistryManager  # noqa: E402
from src.core.scheduler import Scheduler  # noqa: E402
from src.core.sessions import SessionManager  # noqa: E402
from src.core import spawner  # noqa: E402
from src.core import spawner_finalize  # noqa: E402
from src.core import spawner_launch  # noqa: E402
from src.core.spawner import resume_worker as _real_resume_worker  # noqa: E402
from src.core.threads import ThreadManager  # noqa: E402
from src.core.triggers import TriggerManager  # noqa: E402

import src.core.headless_render as headless_render  # noqa: E402


def backend_option(**kwargs: Any) -> models.BackendBase:
  """Build a typed backend option from raw kwargs (the config.yaml shape), dispatching on ``type``."""
  return models.BACKEND_OPTION_ADAPTER.validate_python(kwargs)


def fake_backends() -> dict[str, list[models.BackendBase]]:
  """One cc-claude entry so SessionManager.create_session has a default backend."""
  return {"options": [backend_option(id="fake", label="Fake", type="cc-claude", model="fake-model")]}


@pytest.fixture(autouse=True)
def _stub_headless_renderer(monkeypatch: pytest.MonkeyPatch) -> None:
  """The suite's renderer is the write_stub_chrome shell script (answers --dump-dom only); give the drive seam that shape."""

  def dump_dom_drive(chrome_bin: Path, probe_uri: str) -> int:
    proc = subprocess.run(
        [str(chrome_bin), "--headless", "--dump-dom", probe_uri], capture_output=True, check=False, timeout=60)
    if proc.returncode != 0:
      stderr = proc.stderr.decode("utf-8", errors="replace").strip()[-400:] or "no stderr output"
      raise ValueError(f"headless renderer exited {proc.returncode} while measuring the plan page height: {stderr}")
    if not (match := re.search(r'<pre id="page-height">(\d+)</pre>', proc.stdout.decode("utf-8", errors="replace"))):
      raise ValueError("headless renderer output carried no page-height marker; cannot measure the plan page")
    return int(match.group(1))

  monkeypatch.setattr(headless_render, "render_height", dump_dom_drive)


def mock_session_callbacks() -> models.SessionCallbacks:
  """SessionCallbacks with every field mocked; a test needing one real field constructs its own."""
  return models.SessionCallbacks(
      persist_and_broadcast=AsyncMock(),
      update_thinking_state=AsyncMock(),
      mark_unread=AsyncMock(),
      persist_cc_session_id=AsyncMock(side_effect=lambda sid, ccid: ccid),
      has_completed_round=AsyncMock(return_value=False),
      persist_master_run=AsyncMock(),
      persist_claude_account=AsyncMock(side_effect=lambda sid, label: label),
      claude_context_state=AsyncMock(return_value=(None, None)),
  )


def make_work_item(
    cfg: CharlieBotConfig,
    session_meta: models.SessionMetadata,
    backend_option: models.BackendOption | None,
    *,
    user_content: str = "hello",
    callbacks: models.SessionCallbacks | None = None,
    is_voice: bool = False,
    should_check_tex: bool = False,
    user_event_id: str | None = None,
) -> master_cc_state._WorkItem:
  """_WorkItem with the field values the run-path tests share: non-voice round, mocked callbacks,
  no extra flags or tex check, live-loop future. callbacks=None installs mock_session_callbacks();
  the keyword fields carry the values the cancel/voice/consumer sites vary, and a test needing any
  other field (expect_fresh_session, resume_record) builds its own."""
  return master_cc_state._WorkItem(
      cfg=cfg,
      session_meta=session_meta,
      user_content=user_content,
      callbacks=callbacks if callbacks is not None else mock_session_callbacks(),
      is_voice=is_voice,
      auto_trigger=False,
      backend_option=backend_option,
      extra_claude_flags=None,
      should_check_tex=should_check_tex,
      future=asyncio.get_running_loop().create_future(),
      user_event_id=user_event_id,
  )


async def run_session_consumer(
    session_id: str,
    work_items: list[master_cc_state._WorkItem],
    fake_run_cc: Callable[[master_cc_state._WorkItem], Awaitable[tuple[str | None, int, str | None, dict]]],
) -> None:
  """Run _session_consumer over a seeded queue with a fake CC: _run_cc is replaced by fake_run_cc,
  broadcasts are silenced, and the SessionManager double reports no running tasks. The session's
  queue and consumer registry entries are dropped on exit; the consumer run is bounded at 5s."""
  master_cc_state._session_queues.pop(session_id, None)
  master_cc_state._session_queues[session_id] = asyncio.Queue()
  for item in work_items:
    master_cc_state._session_queues[session_id].put_nowait(item)
  workers_mock = MagicMock()
  workers_mock._has_running_tasks = AsyncMock(return_value=False)
  try:
    with (
        patch.object(master_cc_run, "_run_cc", side_effect=fake_run_cc),
        patch.object(master_cc_queue.streaming_manager, "broadcast", new=AsyncMock()),
        patch(SESSIONS_SESSION_MANAGER_PATCH_TARGET, return_value=workers_mock),
    ):
      await asyncio.wait_for(master_cc_queue._session_consumer(session_id), timeout=5)
  finally:
    master_cc_state._session_queues.pop(session_id, None)
    master_cc_state._session_consumers.pop(session_id, None)


def reset_master_state(session_id: str) -> None:
  """Reset a session's master-cc state: drop the queue and consumer registry entries and clear
  the thinking-busy mark. Suites that run _session_consumer by hand call this between runs so
  leftover state from one run cannot leak into the next."""
  master_cc_state._session_queues.pop(session_id, None)
  master_cc_state._session_consumers.pop(session_id, None)
  thinking_state.clear_busy(session_id)


@contextlib.asynccontextmanager
async def fresh_master_state(session_id: str) -> AsyncIterator[None]:
  """Run the wrapped body against a reset master-cc state for one session.

  The entry reset drops leftover queue/consumer/busy state from an earlier run;
  the exit reset runs on every path out of the body, so a failing test cannot
  leak that state into the next one.
  """
  reset_master_state(session_id)
  try:
    yield
  finally:
    reset_master_state(session_id)


async def drain_session_consumer(session_id: str, timeout: float) -> None:
  """Await the session's registered _session_consumer task; no-op when none is registered.

  A round's persist/broadcast work finishes inside the consumer after
  run_message/enqueue_master_resume return, so draining is what makes those side
  effects observable before a test asserts. The consumer deregisters itself on
  every exit path, so the registry only ever holds an in-flight task: one that
  already ended makes this a no-op, and one that misses the timeout raises.
  """
  consumer = master_cc_state._session_consumers.get(session_id)
  if consumer is not None:
    await asyncio.wait_for(consumer, timeout=timeout)


def patch_resume_seams(
    monkeypatch: pytest.MonkeyPatch,
    resume_cc: Callable[[master_cc_state._WorkItem], Awaitable[tuple]] | None = None,
) -> AsyncMock:
  """Install the three seams every re-attach round needs, and return the broadcast stub.

  The re-attach path (enqueue_master_resume) otherwise reads the SessionManager,
  broadcasts deltas, and builds a fresh event translator: the SessionManager
  double answers _has_running_tasks=False, and the broadcast stub silences the
  streaming-manager singleton (the same attribute BROADCAST_PATCH_TARGET names).
  resume_cc=None keeps the real _resume_cc over the test's raw log and installs
  the identity _build_fresh_translate instead; a callable replaces _resume_cc.
  """
  if resume_cc is not None:
    monkeypatch.setattr(master_cc_run, "_resume_cc", resume_cc)
  else:
    monkeypatch.setattr(master_cc_run, "_build_fresh_translate", lambda *a, **k: (lambda event: [event]))
  broadcast = AsyncMock()
  monkeypatch.setattr(master_cc_queue.streaming_manager, "broadcast", broadcast)
  workers_mock = MagicMock()
  workers_mock._has_running_tasks = AsyncMock(return_value=False)
  monkeypatch.setattr(SESSIONS_SESSION_MANAGER_PATCH_TARGET, lambda *a, **k: workers_mock)
  return broadcast


async def run_resume_round(
    cfg: CharlieBotConfig,
    meta: models.SessionMetadata,
    record: models.MasterRunRecord,
    callbacks: models.SessionCallbacks,
    *,
    is_alive: Callable[[], bool],
) -> None:
  """Enqueue one re-attach round and wait until it settles.

  Resolves the round future and drains the session consumer, so a following
  assertion reads the persisted and broadcast state. fresh_master_state
  brackets the round, so a failing test cannot leak queue or consumer state.
  """
  async with fresh_master_state(meta.id):
    future = await master_cc_queue.enqueue_master_resume(cfg, meta, record, callbacks, is_alive=is_alive)
    await asyncio.wait_for(future, timeout=5)
    await drain_session_consumer(meta.id, timeout=5)


def stall_before_call(delay: float, real: Callable[..., Any]) -> Callable[..., Any]:
  """A drop-in stand-in that blocks *delay* seconds, then delegates to *real*.

  Event-loop-responsiveness tests install it for the call whose off-loop execution
  they verify: the stall is long enough that an inline run pins a ticker gap near
  it, and the tests pair a 0.25 s stall with a 0.15 s ceiling.
  """

  def slow(*args: Any, **kwargs: Any) -> Any:
    time.sleep(delay)
    return real(*args, **kwargs)

  return slow


@contextlib.asynccontextmanager
async def loop_stall_gaps() -> AsyncIterator[list[float]]:
  """Record event-loop stall gaps (s) on a 5 ms ticker while the body runs; yields the gap list.

  The ticker is mid-sleep before the body runs, so an inline block cannot starve it
  unrecorded. Callers assert ``max(gaps)`` against a ceiling that separates off-loop
  execution from an inline run of the stalled call (see ``stall_before_call``).
  """
  gaps: list[float] = []
  stop = False

  async def ticker() -> None:
    prev = time.perf_counter()
    while not stop:
      await asyncio.sleep(0.005)
      now = time.perf_counter()
      gaps.append(now - prev)
      prev = now

  task = asyncio.create_task(ticker())
  await asyncio.sleep(0.01)
  try:
    yield gaps
  finally:
    stop = True
    await task


def append_events(path: Path, events: list[dict]) -> None:
  """Append seed chat events as JSONL; append (not truncate) is what lets a test stage history first."""
  path.parent.mkdir(parents=True, exist_ok=True)
  with open(path, "a", encoding="utf-8") as f:
    f.writelines(json.dumps(event) + "\n" for event in events)


def read_chat_events(home: Path, session_id: str) -> list[dict]:
  """Parse a session's chat_events.jsonl under a staged CHARLIEBOT_HOME; [] when absent.

  Shared raw reader for the crash-recovery e2e suites (test_restart_recovery_e2e,
  test_master_restart_recovery_e2e); each event stays an unmodeled dict so tests
  assert the exact persisted shape. A missing file means the run never wrote
  events, which callers assert on directly rather than treat as an error.
  """
  path = home / "sessions" / session_id / "data" / "chat_events.jsonl"
  if not path.exists():
    return []
  return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


def count_path_read_text(monkeypatch: pytest.MonkeyPatch, include: Callable[[Path], bool]) -> list[Path]:
  """Patch ``Path.read_text`` to collect every read path that *include* accepts; returns the live list.

  The patch starts where the helper is called, so stage warmup reads first; the returned list grows
  with each matching read until monkeypatch reverts at teardown. An empty-list assert is the
  "steady state pays no file read" check the per-file memo suites share.
  """
  real_read_text = Path.read_text
  reads: list[Path] = []

  def counting_read_text(path: Path, *args: object, **kwargs: object) -> str:
    if include(path):
      reads.append(path)
    return real_read_text(path, *args, **kwargs)

  monkeypatch.setattr(Path, "read_text", counting_read_text)
  return reads


def fresh_state_fixture(reset: Callable[[], None]) -> Callable[[], Iterator[None]]:
  """Build an autouse fixture that runs *reset* before and after every test of the module
  assigning it, so process-wide memos and warn-once registries cannot leak between tests.

  Pytest registers the returned fixture under the module attribute it is assigned to,
  so the assigning module keeps its historical fixture name.
  """

  @pytest.fixture(autouse=True)
  def _fresh_state() -> Iterator[None]:
    reset()
    yield
    reset()

  return _fresh_state


def user_event(content: str, timestamp: str | None = None) -> dict:
  """A USER chat event; a test needing extra fields builds its own or merges them in."""
  event: dict[str, Any] = {"type": ET.USER, "content": content}
  if timestamp is not None:
    event["timestamp"] = timestamp
  return event


def scheduled_trigger_event(content: str, timestamp: str | None = None) -> dict:
  """A SCHEDULED_TRIGGER chat event; a test needing extra fields builds its own or merges them in."""
  event: dict[str, Any] = {"type": ET.SCHEDULED_TRIGGER, "content": content}
  if timestamp is not None:
    event["timestamp"] = timestamp
  return event


def assistant_event(content: str, event_id: str = "assistant") -> dict:
  """An ASSISTANT event whose message is a single text block; projection and aggregator tests build on this
  shape, and a test needing extra fields (timestamp, token usage) builds its own or merges them in."""
  return {
      "id": event_id,
      "type": ET.ASSISTANT,
      "message": {
          "content": [{
              "type": "text",
              "text": content
          }]
      },
  }


def assistant_text_event(text: str) -> dict:
  """An ASSISTANT event whose message is a single text block and nothing else: the translated event
  a backend translator emits for model text. No id or timestamp; a test needing extra fields merges
  them in."""
  return {
      "type": ET.ASSISTANT,
      "message": {
          "content": [{
              "type": "text",
              "text": text,
          }],
      },
  }


def assistant_text_tool_use_event(text: str, tool_name: str, tool_input: dict, timestamp: str) -> dict:
  """An ASSISTANT event whose message is one text block followed by one tool_use block: the
  draft-with-tools shape the aggregator tool_result tests feed through both entry points."""
  return {
      "type": ET.ASSISTANT,
      "message":
          {
              "content": [
                  {
                      "type": "text",
                      "text": text
                  },
                  {
                      "type": "tool_use",
                      "name": tool_name,
                      "input": tool_input
                  },
              ]
          },
      "timestamp": timestamp,
  }


def rate_limit_event(status: str, utilization: float, resets_in: timedelta = timedelta(hours=3)) -> dict:
  """A Claude RATE_LIMIT_EVENT: five_hour window at *utilization*, seven_day at a low fixed value."""
  return {
      "type": ET.RATE_LIMIT_EVENT,
      "rate_limit_info":
          {
              "status": status,
              "rateLimitType": "five_hour",
              "resetsAt": (datetime.now(UTC) + resets_in).timestamp(),
              "unifiedWindows": {
                  "five_hour": {
                      "utilization": utilization
                  },
                  "seven_day": {
                      "utilization": 0.3
                  },
              },
          },
  }


def codex_token_count_event(timestamp: Any, **payload_inner: Any) -> dict:
  """The codex token_count event envelope: an ``event_msg`` wrapping a ``token_count`` payload.

  The innards (``rate_limits``, ``info``) are the per-fixture part each suite
  passes through; the envelope — timestamp at the event level, the
  ``event_msg``/``token_count`` typing — is the wire shape the codex readers
  parse, defined here once.
  """
  return {"timestamp": timestamp, "type": "event_msg", "payload": {"type": "token_count", **payload_inner}}


def compact_boundary_event(
    trigger: str | None = "manual", pre_tokens: int | None = None, post_tokens: int | None = None) -> dict:
  """A translated compact_boundary system event in the shape the stream carries and
  handle_compaction_events reads; *trigger* = None omits the key (the shape some fixtures carry)."""
  meta: dict[str, Any] = {}
  if trigger is not None:
    meta["trigger"] = trigger
  if pre_tokens is not None:
    meta["pre_tokens"] = pre_tokens
  if post_tokens is not None:
    meta["post_tokens"] = post_tokens
  return {"type": ET.SYSTEM, "subtype": ET.COMPACT_BOUNDARY, ET.COMPACT_METADATA: meta}


def queued_user_reorder_events() -> list[dict]:
  """Two runs on one session: thinking + tool_use + tool_result + assistant + master_done in the first, a queued
  USER event inside the first run's interval, then a repeated session_id marker and a second assistant + master_done.
  The queued user sitting inside the closed run is what stable-history projection moves past that run, so the
  aggregator and projection suites both assert the reordered id sequence off this one list."""
  return [
      {
          "session_id": "opencode-session"
      },
      {
          "id": "thinking-1",
          "type": ET.THINKING,
          "content": "final thought"
      },
      {
          "id": "tool-1",
          "type": ET.TOOL_USE,
          "name": "Read",
          "input": {
              "file_path": "report.txt"
          }
      },
      {
          "id": "queued-user",
          "type": ET.USER,
          "content": "second question"
      },
      {
          "id": "tool-result-1",
          "type": ET.TOOL_RESULT,
          "content": "report contents"
      },
      assistant_event("first conclusion", "assistant-1"),
      {
          "id": "done-1",
          "type": ET.MASTER_DONE,
          "thinking_seconds": 4
      },
      {
          "session_id": "opencode-session"
      },
      assistant_event("second answer", "assistant-2"),
      {
          "id": "done-2",
          "type": ET.MASTER_DONE,
          "thinking_seconds": 2
      },
  ]


def make_json_response(payload: dict[str, Any]) -> MagicMock:
  """A `requests.Response` stand-in for patched CLI `requests.post` calls: `.json()` returns payload,
  `raise_for_status()` is a configured no-op so the CLI's success path runs straight through."""
  resp = MagicMock()
  resp.json.return_value = payload
  resp.raise_for_status.return_value = None
  return resp


def archive_cutoff_events() -> tuple[datetime, list[dict]]:
  """(cutoff, events) where five `e{i}` events predate and three `f{i}` events follow the cutoff; the 5/3
  split is what recycle tests assert on (events_archived == 5, archive_offset == 5, live f0..f2)."""
  base = datetime(2026, 5, 10, 0, 0, 0, tzinfo=UTC)
  cutoff = base + timedelta(days=3)
  events = [
      {
          "type": "user",
          "content": f"e{i}",
          "timestamp": (base + timedelta(hours=i)).isoformat()
      } for i in range(5)
  ]
  events += [
      {
          "type": "user",
          "content": f"f{i}",
          "timestamp": (cutoff + timedelta(hours=i)).isoformat()
      } for i in range(3)
  ]
  return cutoff, events


async def recycle_archive_cutoff_events(mgr: SessionManager, session_id: str) -> tuple[datetime, Path]:
  """Seed archive_cutoff_events()'s corpus on the session's live file and recycle at the cutoff;
  returns (cutoff, live path). The 5 e-events end up in the weekly archive and the 3 f-events stay
  live (archive_offset == 5). A test that asserts on recycle's return value, or that must read
  between seed and recycle, keeps the explicit calls instead."""
  cutoff, events = archive_cutoff_events()
  live_path = mgr.get_chat_events_path(session_id)
  append_events(live_path, events)
  await mgr.recycle_scheduled_session(session_id, cutoff)
  return cutoff, live_path


async def make_parent(mgr: SessionManager, *, name: str = "Parent") -> str:
  """A session ready to elone: the two seed events give succession tests a cut point to reference."""
  parent = await mgr.create_session(models.CreateSessionRequest(name=name), backend=OPUS_BACKEND_ID)
  append_events(
      mgr.get_chat_events_path(parent.id),
      [
          {
              "type": "user",
              "content": "e0"
          },
          {
              "type": "assistant",
              "content": "e1"
          },
      ],
  )
  return parent.id


def make_sessions_dir_config(tmp_path: Path) -> MagicMock:
  """A MagicMock config for patched CLI ``get_config`` calls: ``sessions_dir`` points at a created
  dir under tmp_path. Callers rely on the sessions root existing while the test's cwd (tmp_path
  itself) sits outside it, so session resolution reads the cwd as sessionless. The dir name is
  arbitrary and no test reads it back."""
  cfg = MagicMock()
  cfg.sessions_dir = tmp_path / "fake_sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  return cfg


def setup_session_cwd(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, sid: str) -> MagicMock:
  """Build a session dir tree at <tmp_path>/sessions/<sid> and chdir into it; the returned mock cfg is
  what the tests patch into src.cli.common.get_config."""
  cfg = MagicMock()
  cfg.server.port = 9443
  cfg.sessions_dir = tmp_path / "sessions"
  session_dir = cfg.sessions_dir / sid
  session_dir.mkdir(parents=True, exist_ok=True)
  monkeypatch.chdir(session_dir)
  return cfg


def _assert_stderr_fragments(capsys: pytest.CaptureFixture[str], *fragments: str) -> None:
  err = capsys.readouterr().err
  for fragment in fragments:
    assert fragment in err


def assert_cli_reject(
    exc_info: pytest.ExceptionInfo[SystemExit], capsys: pytest.CaptureFixture[str], *err_fragments: str) -> None:
  """Shared tail of CLI reject tests: main() exited nonzero and every fragment landed on stderr."""
  assert exc_info.value.code != 0
  _assert_stderr_fragments(capsys, *err_fragments)


def assert_cli_reject_exit2(
    exc_info: pytest.ExceptionInfo[SystemExit], capsys: pytest.CaptureFixture[str], *err_fragments: str) -> None:
  """Same as assert_cli_reject with the exit code pinned at 2 (CLI usage error, e.g. bad file input)."""
  assert exc_info.value.code == 2
  _assert_stderr_fragments(capsys, *err_fragments)


def run_node_js_test(node_test: Path, skip_reason: str) -> None:
  """Run one node --test file; hosts without node skip rather than fail, and cwd=ROOT keeps repo-relative asset
  loads working."""
  node = shutil.which('node')
  if node is None:
    pytest.skip(skip_reason)

  result = subprocess.run(
      [node, '--test', str(node_test)],
      cwd=ROOT,
      capture_output=True,
      text=True,
      check=False,
      # The suites finish in ~1s; the bound turns a hung node child into a test failure instead of a CI hang.
      timeout=300,
  )
  if result.returncode != 0:
    pytest.fail(f'Node tests failed.\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}')


def make_home_config(tmp_path: Path) -> CharlieBotConfig:
  """CharlieBotConfig rooted at tmp_path/"charliebot-home". Leaves the home dir un-created:
  one call site (the sherpa streaming test) mkdirs it itself, and most sites never touch disk.
  One Opus backend registered so SessionManager.create_session's default (backends.options[0])
  resolves."""
  return CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home", backends={"options": [OPUS_BACKEND_OPTION]})


def build_master_cc_cfg(tmp_path: Path) -> CharlieBotConfig:
  """CharlieBotConfig rooted at tmp_path/".charliebot" with one fake codex backend registered: the
  shape the master-cc round tests drive run_message, replay_user_message, and _run_cc against."""
  return CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backends={"options": [backend_option(id="fake", label="Fake", type="codex", model="fake-model")]},
  )


def make_session_mgr(tmp_path: Path) -> SessionManager:
  """SessionManager over a SimpleNamespace cfg whose sessions_dir is tmp_path/"sessions"; a test
  needing a richer cfg builds its own."""
  cfg = SimpleNamespace(sessions_dir=tmp_path / "sessions")
  cfg.sessions_dir.mkdir()
  return SessionManager(cfg)


async def make_home_session(
    tmp_path: Path,
    *,
    name: str,
    backend: str | None = None) -> tuple[CharlieBotConfig, SessionManager, models.SessionMetadata]:
  """(cfg, SessionManager, one created session) over a CharlieBotConfig rooted at tmp_path/"home";
  backend=None takes create_session's default (the first registered backend). A test needing more
  sessions calls mgr.create_session directly; a test needing no session builds the cfg/mgr pair
  inline."""
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", backends={"options": [OPUS_BACKEND_OPTION]})
  mgr = SessionManager(cfg)
  session = await mgr.create_session(models.CreateSessionRequest(name=name), backend=backend)
  return cfg, mgr, session


def make_router_client(
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    router: APIRouter,
    prefix: str,
) -> TestClient:
  """TestClient mounting one router with cfg/session_mgr as dependency overrides; a test needing
  extra routers or overrides builds its own FastAPI app."""
  app = FastAPI()
  app.include_router(router, prefix=prefix)
  app.dependency_overrides[get_config] = lambda: cfg
  # The polled routes resolve cfg through the on-loop dependency (same instance
  # the sync key serves), so both keys carry the override.
  app.dependency_overrides[get_config_on_loop] = lambda: cfg
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  return TestClient(app)


def make_sessions_client(cfg: CharlieBotConfig, session_mgr: SessionManager) -> TestClient:
  """make_router_client over the sessions router, mounted at /api/sessions."""
  return make_router_client(cfg, session_mgr, sessions_router, "/api/sessions")


def make_internal_router_client(cfg: Any, session_mgr: Any) -> TestClient:
  """make_router_client over the internal router, mounted at /api/internal; the internal routes
  take cfg through their own get_config import (same function object), so the override key in
  make_router_client covers them. cfg may be a MagicMock when the tested route never reads it."""
  return make_router_client(cfg, session_mgr, internal_router, "/api/internal")


def make_cron_client(cfg: CharlieBotConfig, session_mgr: SessionManager) -> TestClient:
  """make_router_client over the cron router, mounted at /api/cron."""
  return make_router_client(cfg, session_mgr, cron_router, "/api/cron")


def make_page_request(path: str) -> Request:
  """Starlette Request for a GET against path with the full test-server scope (scheme/server/client);
  a test needing headers, cookies, or a non-GET method builds its own scope."""
  scope = {
      "type": "http",
      "method": "GET",
      "path": path,
      "headers": [],
      "query_string": b"",
      "scheme": "http",
      "server": ("testserver", 80),
      "client": ("127.0.0.1", 12345),
  }
  return Request(scope)


def make_transcript(config_dir: Path, cc_session_id: str) -> Path:
  """Write a fake Claude Code session transcript under config_dir and return its path."""
  transcript = config_dir / "projects" / "slug" / f"{cc_session_id}.jsonl"
  transcript.parent.mkdir(parents=True, exist_ok=True)
  transcript.write_text("[]", encoding="utf-8")
  return transcript


def write_pool_credentials(config_dir: Path, access_token: str = "token") -> None:
  """Write Claude OAuth credentials into a pool account's config dir, marking the login present."""
  config_dir.mkdir(parents=True, exist_ok=True)
  (config_dir / claude_accounts.CREDENTIALS_FILE).write_text(
      json.dumps({"claudeAiOauth": {
          "accessToken": access_token,
          "refreshToken": "r"
      }}), encoding="utf-8")


def pool_cfg(
    tmp_path: Path,
    backend_options: list[models.BackendOption],
    *,
    home: Path,
    worktree_dir: Path,
    labels: tuple[str, ...] = ("main", "ext-1", "ext-2"),
) -> CharlieBotConfig:
  """A pooled CharlieBotConfig: one ClaudeAccount per label, pool credentials planted in each config dir."""
  accounts = [models.ClaudeAccount(label=label, config_dir=str(tmp_path / f"claude-{label}")) for label in labels]
  for account in accounts:
    write_pool_credentials(Path(account.config_dir))
  return CharlieBotConfig(
      charliebot_home=home,
      paths={"worktree_dir": str(worktree_dir)},
      accounts={"claude": accounts},
      backends={"options": backend_options},
  )


# Resolved model and option id the account-pool suites pin for the pooled Fable backend.
# One home so a rename stays a one-line change across the master-turn, worker, and ledger
# suites; wire-payload assertions and yaml text keep the raw strings (same rule as
# OPUS_BACKEND_ID above).
FABLE_MODEL = "claude-fable-5-1"
POOLED_FABLE_ID = "claude-fable-5"


def fable_pool_cfg(tmp_path: Path, labels: tuple[str, ...] = ("main", "ext-1", "ext-2")) -> CharlieBotConfig:
  """A pooled config with one pooled Fable option and a second pinned Fable entry."""
  return pool_cfg(
      tmp_path,
      [
          backend_option(id=POOLED_FABLE_ID, label="Fable", type="cc-claude", model=FABLE_MODEL),
          backend_option(id="pinned", label="Pinned", type="cc-claude", model=FABLE_MODEL),
      ],
      home=tmp_path / ".charliebot",
      worktree_dir=tmp_path / "worktrees",
      labels=labels,
  )


def session_dir_names(cfg: CharlieBotConfig) -> set[str]:
  """Snapshot the names of session directories on disk (existence, not content)."""
  if not cfg.sessions_dir.exists():
    return set()
  return {d.name for d in cfg.sessions_dir.iterdir() if d.is_dir()}


# Backend id the conftest configs register for the Opus option; session fixtures across the
# suite must spell it through this constant so a rename stays a one-line change. The option's
# model is spelled through OPUS_BACKEND_OPTION.model wherever a fixture pairs it with the id
# (resolve stubs, resolved-model oracles, thread backend/model fields); Claude transcript
# payload builders keep the raw string because it is wire data there.
OPUS_BACKEND_ID = "claude-opus-4.6"

OPUS_BACKEND_OPTION = backend_option(id=OPUS_BACKEND_ID, label="Opus", type="cc-claude", model="claude-opus-4-6")
CODEX_BACKEND_OPTION = backend_option(id="codex-o3", label="Codex", type="codex", model="o3")

# Antigravity option as the antigravity-routing tests register it: model-less, so the
# model-is-required rejection and the resume-id routing keep their fixture shape.
AGY_BACKEND_OPTION = backend_option(id="agy", label="Antigravity", type="antigravity")

THREE_BACKEND_OPTIONS = [
    OPUS_BACKEND_OPTION,
    CODEX_BACKEND_OPTION,
    backend_option(id="kimi-k2.5", label="Kimi", type="cc-kimi", model="kimi-k2.5", credential="test-kimi"),
]

PLAN_TEST_BACKEND_OPTIONS = [OPUS_BACKEND_OPTION]

# Synthetic catalog model id shared by the opencode-backend, autonamer, and session-usage tests;
# the fake /config/providers payloads in test_opencode_backend.py split it into provider id and
# model id on the first "/", so a rename keeps the provider/... shape.
SYNTHETIC_MODEL = "synthetic-provider/nvidia/Synthetic-Model"

# Prompt payload beginning with "--", which a naive argv builder would misread as a CLI flag;
# each backend's build-command test asserts the string reaches the CLI as prompt payload only.
FLAG_LIKE_PROMPT = "--malicious-flag ignore previous"

# Success-path counterpart of src/core/spawner_finalize.py's _QUOTA_EXHAUSTED_OUTCOME: the
# shared clean-exit worker-run outcome (exit 0, no quota, no setup error) for spawner tests.
CLEAN_EXIT_OUTCOME = spawner._WorkerRunOutcome(exit_code=0, quota_exhausted=False, error="")

# Import-path patch target shared by every test that silences or spies on streaming broadcasts.
# Mock resolves the route through the src.core.sessions namespace (src/core/sessions.py imports
# the streaming_manager singleton) and setattr's broadcast on that shared object; a move of the
# sessions-side import updates this one string. src.core.autonamer and src.agents.worker import
# the same singleton, so their routes reach the same attribute.
BROADCAST_PATCH_TARGET = "src.core.sessions.streaming_manager.broadcast"

# Import-path patch target shared by every test that stubs the workers-running probe the master
# consumer's teardown runs. master_cc_queue binds the class with call-time `from
# src.core.sessions import SessionManager`, so mock and
# monkeypatch.setattr land the stand-in on the src.core.sessions module attribute and the
# teardown's SessionManager(...) construction resolves it at call time.
SESSIONS_SESSION_MANAGER_PATCH_TARGET = "src.core.sessions.SessionManager"

# Import-path patch target shared by every test that silences or spies on the master wake a
# trigger fires. src/core/triggers.py binds the name with `from src.core.master_trigger import
# trigger_master`, so mock resolves the route to the src.core.triggers namespace and setattr's
# the AsyncMock on that module attribute; _wait_and_fire's own call site then reaches the
# stand-in. Other modules bind the same function in their own namespaces, so their wakes keep
# their own routes; grep `from src.core.master_trigger import trigger_master` for the full set.
TRIGGER_MASTER_PATCH_TARGET = "src.core.triggers.trigger_master"

# Import-path patch target for the wake's inner run. Every wake fires through trigger_master,
# whose body reads run_message_with_resume_recovery as a module global of its defining module,
# so mock setattrs the stand-in there and the run is intercepted no matter which outer seam
# fired the wake; the other *TRIGGER_MASTER_PATCH_TARGET constants in this block name the
# outer seam, not this inner one.
MASTER_TRIGGER_RUN_MESSAGE_WITH_RESUME_RECOVERY_PATCH_TARGET = (
    "src.core.master_trigger.run_message_with_resume_recovery")

# Import-path patch target for the config re-read a firing trigger passes to the master wake.
# src/core/triggers.py binds the name at import scope (`from src.core.config import get_config`),
# so mock setattrs the stand-in on the src.core.triggers module attribute and _wait_and_fire's
# wake path reads it at call time. Sibling modules binding get_config in their own namespaces
# keep their own routes.
TRIGGERS_GET_CONFIG_PATCH_TARGET = "src.core.triggers.get_config"

# Import-path patch target for the subprocess spawn the trigger watchers probe through.
# src/core/triggers.py binds the library with module-scope `import asyncio`, and its
# watch paths read asyncio.create_subprocess_exec at call time, so mock lands the
# stand-in on the asyncio module through the src.core.triggers route.
TRIGGERS_ASYNCIO_CREATE_SUBPROCESS_EXEC_PATCH_TARGET = "src.core.triggers.asyncio.create_subprocess_exec"

# Library-root patch target for stubs that intercept the subprocess spawn for any caller:
# the bare spelling sets the stand-in on the asyncio module itself, so every importer's
# asyncio.create_subprocess_exec read resolves to the stand-in during the patch window.
# TRIGGERS_ASYNCIO_CREATE_SUBPROCESS_EXEC_PATCH_TARGET above reaches the same asyncio
# attribute through the src.core.triggers namespace; the two routes are not interchangeable
# spellings — a test names this one when the interception, not the caller, is the point.
ASYNCIO_CREATE_SUBPROCESS_EXEC_PATCH_TARGET = "asyncio.create_subprocess_exec"

# Import-path patch target for the host capability probe the slurm watchers read.
# src/core/triggers.py runs the probe once at import scope (`_SACCT_AVAILABLE =
# shutil.which("sacct") is not None`), and create_trigger's local-slurm guard and
# _wait_sacct_group's no-sacct skip read the module attribute at call time, so mock
# patch setattrs the stand-in on the src.core.triggers module attribute.
TRIGGERS_SACCT_AVAILABLE_PATCH_TARGET = "src.core.triggers._SACCT_AVAILABLE"

# Import-path patch target for the CLI HTTP layer's config read. src/cli/common.py binds the
# name with `from src.core.config import get_config`, so mock setattrs the stand-in on the
# src.cli.common module attribute and every helper defined there reads it at call time.
CLI_COMMON_GET_CONFIG_PATCH_TARGET = "src.cli.common.get_config"

# Import-path patch target for the version-skew hint the CLI error paths append. src/cli/common.py
# defines _maybe_version_skew_hint and _exit_server_rejection reads it as a module global at call
# time, so mock setattrs the stand-in on the src.cli.common module attribute.
CLI_COMMON_MAYBE_VERSION_SKEW_HINT_PATCH_TARGET = "src.cli.common._maybe_version_skew_hint"

# Import-path patch target for the publish command's config read. src/cli/publish.py binds the
# name with `from src.core.config import get_config`, so mock setattrs the stand-in on the
# src.cli.publish module attribute and main's preflight reads it at call time.
CLI_PUBLISH_GET_CONFIG_PATCH_TARGET = "src.cli.publish.get_config"

# Import-path patch target for the Telegram delivery the cron-load alert posts. The alert helper
# in src/core/config.py imports send_telegram at call time (lazy, notifications imports config),
# so that import resolves the stand-in landed on the src.core.notifications module attribute;
# import-scope binders of the same function (spawner_finalize) keep their own bound object and
# are not intercepted through this route.
NOTIFICATIONS_SEND_TELEGRAM_PATCH_TARGET = "src.core.notifications.send_telegram"

# Import-path patch targets for the master wake a Slack message fires. src/core/slack_listener.py
# binds both names at import scope (`from src.core.master_trigger import trigger_master`,
# `from src.core.tasks import create_logged_task`), so mock setattrs the stand-ins on the
# src.core.slack_listener module attributes and the listener's handlers read them at call time;
# sibling modules binding the same functions (e.g. TRIGGER_MASTER_PATCH_TARGET's route) keep
# their own namespaces.
SLACK_LISTENER_TRIGGER_MASTER_PATCH_TARGET = "src.core.slack_listener.trigger_master"
SLACK_LISTENER_CREATE_LOGGED_TASK_PATCH_TARGET = "src.core.slack_listener.create_logged_task"

# Import-path patch target for the Slack client factory every listener outbound path posts
# through. src/core/slack_listener.py defines _bot_client at module scope, and its handlers
# and reply/backfill helpers resolve the name at call time, so mock setattrs the stand-in
# on the src.core.slack_listener module attribute.
SLACK_LISTENER_BOT_CLIENT_PATCH_TARGET = "src.core.slack_listener._bot_client"

# Import-path patch target for the background-task spawner a scheduled task fires through.
# src/core/scheduler.py binds the name at import scope (`from src.core.tasks import
# create_logged_task`), so monkeypatch.setattr lands the stand-in on the src.core.scheduler
# module attribute and _execute_master_task/_spawn_scheduled_worker read it at call time; the
# src.core.slack_listener route above reaches a different namespace.
SCHEDULER_CREATE_LOGGED_TASK_PATCH_TARGET = "src.core.scheduler.create_logged_task"

# Import-path patch targets for the seams a scheduled run fires through. src/core/scheduler.py
# binds each name at import scope (`from src.core.config import get_config`, `from
# src.core.master_trigger import trigger_master`, `from src.core.spawner import
# resolve_requested_subagent_backend_model, spawn_worker`, `from src.core.threads import
# ThreadManager`), so monkeypatch.setattr lands the stand-in on the src.core.scheduler module
# attribute and _reload_config, _execute_master_task, and _spawn_scheduled_worker read it at
# call time; sibling modules binding the same functions keep their own routes.
SCHEDULER_GET_CONFIG_PATCH_TARGET = "src.core.scheduler.get_config"
SCHEDULER_RESOLVE_SUBAGENT_BACKEND_MODEL_PATCH_TARGET = ("src.core.scheduler.resolve_requested_subagent_backend_model")
SCHEDULER_SPAWN_WORKER_PATCH_TARGET = "src.core.scheduler.spawn_worker"
SCHEDULER_THREAD_MANAGER_PATCH_TARGET = "src.core.scheduler.ThreadManager"
SCHEDULER_TRIGGER_MASTER_PATCH_TARGET = "src.core.scheduler.trigger_master"

# Import-path patch target for the master wake a review-chain finalize fires. src/core/review.py
# binds the name at import scope (`from src.core.master_trigger import trigger_master`), so
# monkeypatch.setattr lands the stand-in on the src.core.review module attribute and
# _trigger_master_judged reads it at call time; sibling modules binding the same function
# (TRIGGER_MASTER_PATCH_TARGET, SLACK_LISTENER_TRIGGER_MASTER_PATCH_TARGET,
# SCHEDULER_TRIGGER_MASTER_PATCH_TARGET above) keep their own routes.
REVIEW_TRIGGER_MASTER_PATCH_TARGET = "src.core.review.trigger_master"

# Import-path patch targets for the worker spawn/resume seam a recovery or improve-loop run
# fires through. The spawner facade binds both names at import scope (`from
# src.core.spawner_lifecycle import resume_worker, spawn_worker` in src/core/spawner.py), so
# monkeypatch.setattr lands the stand-in on the src.core.spawner module attribute; the
# attribute-read call sites (init_worker_recovery's `spawner.spawn_worker(...)`/
# `spawner.resume_worker(...)`) and the call-time `from src.core.spawner import spawn_worker`
# inside improve_command/review resolve it. scheduler.py binds spawn_worker at import scope
# and keeps its own route (SCHEDULER_SPAWN_WORKER_PATCH_TARGET above).
SPAWNER_SPAWN_WORKER_PATCH_TARGET = "src.core.spawner.spawn_worker"
SPAWNER_RESUME_WORKER_PATCH_TARGET = "src.core.spawner.resume_worker"

# Import-path patch targets for the chat API's message bootstrap and cancel route.
# src/api/chat.py defines run_and_finalize itself and binds create_logged_task
# (`from src.core.tasks import create_logged_task`) and cancel_master (`from
# src.agents.master_cc import cancel_master`) at import scope, so mock and
# monkeypatch.setattr land the stand-ins on the src.api.chat module attributes and
# send_message's fire-and-forget bootstrap, launch_prompt_dispatch's slash-dispatch
# run, run_and_finalize's auto-name task, and cancel_master_agent read them at call
# time. src/api/slash.py binds launch_prompt_dispatch at import scope and
# src/api/sessions.py re-imports run_and_finalize at call time, so both reach the
# same src.api.chat namespace attributes; src.core.tasks.create_logged_task stays a
# separate route.
CHAT_RUN_AND_FINALIZE_PATCH_TARGET = "src.api.chat.run_and_finalize"
CHAT_CREATE_LOGGED_TASK_PATCH_TARGET = "src.api.chat.create_logged_task"
CHAT_CANCEL_MASTER_PATCH_TARGET = "src.api.chat.cancel_master"

# Import-path patch targets for the CLI HTTP layer's transport. src/cli/common.py binds the
# library with module-scope `import requests`, and its helpers read requests.get at call time
# and pick requests.post inside _request_with_contract's `request_fn = requests.post if ... else
# ...`, so mock and monkeypatch.setattr land the stand-in on the requests module through the
# src.cli.common route and every helper defined there picks it up at call time.
CLI_COMMON_REQUESTS_POST_PATCH_TARGET = "src.cli.common.requests.post"
CLI_COMMON_REQUESTS_GET_PATCH_TARGET = "src.cli.common.requests.get"

# Import-path patch target shared by every test that swaps the backend factory a master session
# runs under. src/agents/master_cc_run.py binds the factory with call-time `from
# src.agents.backends.registry import build_backend` inside its run/resume helpers, so
# monkeypatch.setattr on the registry module attribute lands the stand-in where those imports
# resolve. Module-scope binders of the same function (worker.py, autonamer.py, recap.py, ...)
# keep their own namespaces and are not intercepted through this route.
BUILD_BACKEND_PATCH_TARGET = "src.agents.backends.registry.build_backend"
# The worker reads build_backend off its own module binding (src/agents/worker.py
# imports it at module scope), so a patched registry binding never reaches it.
WORKER_BUILD_BACKEND_PATCH_TARGET = "src.agents.worker.build_backend"

# Import-path patch target for the /proc stat read the backend start contract pins. src/core/runs.py
# defines read_pid_stat; src/agents/backends/base.py binds the module (`from src.core import runs`)
# and reads runs.read_pid_stat at call time, so monkeypatch.setattr lands the stand-in on the
# src.core.runs module attribute where that read resolves.
RUNS_READ_PID_STAT_PATCH_TARGET = "src.core.runs.read_pid_stat"

# Import-path patch target for the subprocess spawn the backend start contract drives through the
# AgentBackend base path. src/agents/backends/base.py binds the library with module-scope
# `import asyncio`, so monkeypatch.setattr lands the stand-in on the shared asyncio module through
# this route and base.run's spawn read resolves it at call time; the library-root spelling
# (ASYNCIO_CREATE_SUBPROCESS_EXEC_PATCH_TARGET above) reaches the same attribute, so a
# caller-qualified constant here records which backend's spawn a test drives.
BASE_ASYNCIO_CREATE_SUBPROCESS_EXEC_PATCH_TARGET = "src.agents.backends.base.asyncio.create_subprocess_exec"

# Import-path patch target for the binary resolution an OpenCodeBackend construction runs.
# src/agents/backends/opencode.py binds the helper at import scope (`from
# src.agents.backends.base import resolve_binary`), so monkeypatch.setattr lands the
# stand-in on the src.agents.backends.opencode module attribute and OpenCodeBackend.__init__
# reads it at call time; sibling backends binding the same helper (codex.py,
# antigravity_cli.py, charlie_code.py) keep their own namespaces.
OPENCODE_RESOLVE_BINARY_PATCH_TARGET = "src.agents.backends.opencode.resolve_binary"

# Import-path patch target for the subprocess spawn the backend start contract drives through the
# OpenCodeBackend path. src/agents/backends/opencode.py binds the library with module-scope
# `import asyncio`, so monkeypatch.setattr lands the stand-in on the shared asyncio module through
# this route and the SSE run loop's spawn read resolves it at call time; the library-root
# spelling (ASYNCIO_CREATE_SUBPROCESS_EXEC_PATCH_TARGET above) reaches the same attribute.
OPENCODE_ASYNCIO_CREATE_SUBPROCESS_EXEC_PATCH_TARGET = "src.agents.backends.opencode.asyncio.create_subprocess_exec"

# Import-path patch target for the binary resolution a CodexBackend construction runs.
# src/agents/backends/codex.py binds the helper at import scope (`from
# src.agents.backends.base import resolve_binary`), so monkeypatch.setattr lands the
# stand-in on the src.agents.backends.codex module attribute and CodexBackend.__init__
# reads it at call time; sibling backends binding the same helper (opencode.py,
# antigravity_cli.py, charlie_code.py, gemini_cli.py) keep their own namespaces.
CODEX_RESOLVE_BINARY_PATCH_TARGET = "src.agents.backends.codex.resolve_binary"

# Import-path patch target for the binary resolution an AntigravityCliBackend construction
# runs. src/agents/backends/antigravity_cli.py binds the helper at import scope (`from
# src.agents.backends.base import resolve_binary`), so monkeypatch.setattr lands the
# stand-in on the src.agents.backends.antigravity_cli module attribute and
# AntigravityCliBackend.__init__ reads it at call time; sibling backends binding the same
# helper (charlie_code.py, codex.py, gemini_cli.py, opencode.py) keep their own namespaces.
ANTIGRAVITY_RESOLVE_BINARY_PATCH_TARGET = "src.agents.backends.antigravity_cli.resolve_binary"

# Import-path patch target for the binary resolution a CharlieCodeBackend construction
# runs. src/agents/backends/charlie_code.py binds the helper at import scope (`from
# src.agents.backends.base import resolve_binary`), so monkeypatch.setattr lands the
# stand-in on the src.agents.backends.charlie_code module attribute and
# CharlieCodeBackend.__init__ reads it at call time; sibling backends binding the same
# helper (antigravity_cli.py, codex.py, gemini_cli.py, opencode.py) keep their own namespaces.
CHARLIE_CODE_RESOLVE_BINARY_PATCH_TARGET = "src.agents.backends.charlie_code.resolve_binary"

# Import-path patch target for the binary resolution a GeminiCliBackend construction runs.
# src/agents/backends/gemini_cli.py binds the helper at import scope (`from
# src.agents.backends.base import resolve_binary`), so monkeypatch.setattr lands the
# stand-in on the src.agents.backends.gemini_cli module attribute and GeminiCliBackend.__init__
# reads it at call time; sibling backends binding the same helper (antigravity_cli.py,
# charlie_code.py, codex.py, opencode.py) keep their own namespaces.
GEMINI_RESOLVE_BINARY_PATCH_TARGET = "src.agents.backends.gemini_cli.resolve_binary"

# Patch target for the atomic-write swap hook. src/core/json_utils.py publishes each staged
# payload with an ``os.replace`` attribute lookup on its module-scope ``import os`` binding, and
# atomic_write_stream's docstring pins that lookup as the test hook, so mock setattrs the
# stand-in on the shared os module through this route for the patch window and the write side's
# call-time read picks it up.
JSON_UTILS_OS_REPLACE_PATCH_TARGET = "src.core.json_utils.os.replace"

# Import-path patch targets for the tmux seams the TUI session handlers read. The handlers
# bind the names with call-time `from src.agents.backends.tui import ...`
# (src/api/sessions.py's tui status/stop handlers, src/core/sessions.py's delete path). The
# import resolves the src.agents.backends.tui module attribute at call time, so
# monkeypatch.setattr lands the stand-in exactly there. kill_tmux_session and
# tmux_session_exists are pty_common re-exports on that namespace; _claude_jsonl_busy is
# defined in tui.py itself. src/api/threads.py binds tmux_session_exists at import scope and
# keeps its own route.
TUI_KILL_TMUX_SESSION_PATCH_TARGET = "src.agents.backends.tui.kill_tmux_session"
TUI_TMUX_SESSION_EXISTS_PATCH_TARGET = "src.agents.backends.tui.tmux_session_exists"
TUI_CLAUDE_JSONL_BUSY_PATCH_TARGET = "src.agents.backends.tui._claude_jsonl_busy"

# Import-path patch targets for the server's terminal websocket. server.py defines _check_ws_auth
# and its websocket handlers read it as a module global at call time, and the terminal handler
# imports run_terminal_attachment at call time (`from src.agents.backends.terminal import
# run_terminal_attachment` inside terminal_websocket), so monkeypatch.setattr lands both stand-ins
# on their defining module attributes and the handler's reads resolve them.
SERVER_CHECK_WS_AUTH_PATCH_TARGET = "server._check_ws_auth"
TERMINAL_RUN_TERMINAL_ATTACHMENT_PATCH_TARGET = "src.agents.backends.terminal.run_terminal_attachment"


def build_cli_backend(
    monkeypatch: pytest.MonkeyPatch,
    backend_cls: type[backend_base.AgentBackend],
    resolve_patch_target: str,
    fake_binary: str,
    defaults: dict[str, Any] | None = None,
    **kwargs: Any,
) -> backend_base.AgentBackend:
  """Construct a CLI backend with its resolve_binary pinned to *fake_binary*.

  The one home of the backend-test construction contract: the patch lands the stand-in
  on the module named by *resolve_patch_target* (each ``*_RESOLVE_BINARY_PATCH_TARGET``
  constant above states that module's binding scope), so ``__init__`` never probes PATH,
  and *defaults* fill kwargs the caller left out.
  """
  monkeypatch.setattr(resolve_patch_target, lambda name, fallback: fake_binary)
  for key, value in (defaults or {}).items():
    kwargs.setdefault(key, value)
  return backend_cls(**kwargs)


def stub_subprocess_spawn(monkeypatch: pytest.MonkeyPatch, patch_target: str, pid: int) -> MagicMock:
  """Install a MagicMock asyncio subprocess on a spawn patch target and return it.

  The one home of the backend-test spawn stub: stdin's drain and wait_closed are
  AsyncMocks because ``AgentBackend._write_stdin_prompt`` awaits them when a
  backend feeds a prompt over stdin, and a bare MagicMock attribute would fail
  that await. A test asserting the spawn call's own kwargs builds its AsyncMock
  instead, to hold the reference ``await_args`` reads.
  """
  process = MagicMock()
  process.pid = pid
  process.stdin = MagicMock()
  process.stdin.drain = AsyncMock()
  process.stdin.wait_closed = AsyncMock()
  monkeypatch.setattr(patch_target, AsyncMock(return_value=process))
  return process


def plan_page_html(goal_body: str = "Ship the fix.") -> str:
  """Minimal plan page passing the plan assertion set: the shipped template's <style> block
  verbatim (style-verbatim compares after whitespace collapse), six numbered sections, and a footer,
  with *goal_body* as the Problem / Goal section's body."""
  template = (ROOT / "prompts" / "plan_template.html").read_text(encoding="utf-8")
  style = re.search(r"<style>.*?</style>", template, re.DOTALL).group(0)
  titles = ["Problem / Goal", "Context", "High Level Solution", "Detailed Design", "Trade-offs", "Other Details"]
  sections = "".join(
      f'<section class="plan-section"><h2><span class="n">{i}</span> {title}</h2>'
      f"<p>{goal_body if i == 1 else title}</p></section>" for i, title in enumerate(titles, 1))
  return (f"<html><head>{style}</head><body>{sections}"
          '<div class="foot"><p>How to respond.</p></div></body></html>')


def open_fork_html() -> str:
  """Minimal open fork passing the fork-open-shape assertion: numbered question,
  recommendation line, trade-off line."""
  return (
      '<div class="fork"><p class="q"><span class="fn">1</span>Scope?</p>'
      '<p class="rec"><b>Recommendation:</b> R</p><p class="trade">Tradeoff: T</p></div>')


def write_stub_chrome(tmp_path: Path, height: int) -> str:
  """Write a fake headless-chrome binary printing a wrapper-shaped DOM with the chosen measured height."""
  stub = tmp_path / f"stub-chrome-{height}.sh"
  stub.write_text(f"#!/bin/sh\necho 'probe output <pre id=\"page-height\">{height}</pre>'\n", encoding="utf-8")
  stub.chmod(0o755)
  return str(stub)


def build_plan_cfg(tmp_path: Path) -> CharlieBotConfig:
  """CharlieBotConfig for plan tests: the 800px stub chrome sits under the 1-page height limit, and the
  sessions/worktrees dirs live under tmp_path so each test owns its own tree."""
  return CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      paths={"worktree_dir": str(tmp_path / "worktrees")},
      backends={"options": PLAN_TEST_BACKEND_OPTIONS},
      headless_chrome_bin=write_stub_chrome(tmp_path, 800),
  )


def build_scheduler_cfg(tmp_path: Path) -> CharlieBotConfig:
  """CharlieBotConfig for scheduler cron tests: the home and worktrees dirs live under tmp_path so each test
  owns its own tree, and both the opus and codex backends are registered for backend-override cases."""
  return CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      paths={"worktree_dir": str(tmp_path / "worktrees")},
      backends={"options": [OPUS_BACKEND_OPTION, CODEX_BACKEND_OPTION]},
  )


def build_sessions_cfg(tmp_path: Path) -> CharlieBotConfig:
  """CharlieBotConfig for sessions tests: the .charliebot home lives under tmp_path so each test owns its own
  tree, and the backend list registers opus only."""
  return CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backends={"options": [OPUS_BACKEND_OPTION]},
  )


def build_slack_cfg(tmp_path: Path) -> CharlieBotConfig:
  """CharlieBotConfig for slack tests: the home dir lives under tmp_path so each test owns its own tree, and the
  stubbed test tokens plus the single allowed user id wire the delivery and listener paths under
  src.core.slack_listener."""
  stub_credentials({"slack": {"bot_token": "test-bot-token", "app_token": "test-app-token"}})
  return CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      slack={"allowed_user_ids": ["U_ALLOWED"]},
      backends=fake_backends(),
  )


def make_instruction_cfg(tmp_path: Path, *, manager_contract: str | None) -> SimpleNamespace:
  """Fake instruction inputs for the master-instruction builder: a repo whose prompts/master.md
  reads "BASE PROMPT", plus prompts/project_manager.md carrying the manager_contract text when
  given. claude_md_file and memory_dir name paths that do not exist, so the built instructions
  carry neither host override nor memory block."""
  home = tmp_path / "home"
  repo = tmp_path / "repo"
  (repo / "prompts").mkdir(parents=True)
  (repo / "prompts" / "master.md").write_text("BASE PROMPT", encoding="utf-8")
  if manager_contract is not None:
    (repo / "prompts" / "project_manager.md").write_text(manager_contract, encoding="utf-8")
  return SimpleNamespace(
      charlie_bot_repo=repo,
      claude_md_file=home / "MASTER_AGENT_PROMPT.md",
      memory_dir=home / "memory",
      charliebot_home=home,
  )


def cfg_with_repo(repo_root: Path) -> CharlieBotConfig:
  """A cfg-like object whose charlie_bot_repo points at *repo_root* (real CharlieBotConfig's
  charlie_bot_repo is a derived property tied to the installed package location, so a plain
  namespace stand-in is used to redirect it for these isolated fail-loud tests)."""

  class _Cfg:
    charlie_bot_repo = repo_root
    memory_dir = repo_root / "memory"

  return _Cfg()  # type: ignore[return-value]


def build_antigravity_cfg(tmp_path: Path) -> CharlieBotConfig:
  """CharlieBotConfig for antigravity-routing tests: the .charliebot home lives under tmp_path so each
  test owns its own tree, and the backend list registers the model-less antigravity option the
  resume-id routing resolves against."""
  return CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backends={"options": [AGY_BACKEND_OPTION]},
  )


def build_two_backend_cfg(tmp_path: Path) -> CharlieBotConfig:
  """CharlieBotConfig for cross-backend tests: the .charliebot home lives under tmp_path so each test owns its
  own tree, and the backend list registers the opus-then-codex pair that pin-resolution and fallback-ordering
  cases exercise."""
  return CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backends={"options": [OPUS_BACKEND_OPTION, CODEX_BACKEND_OPTION]},
  )


def build_tui_sessions_cfg(tmp_path: Path) -> CharlieBotConfig:
  """CharlieBotConfig for sessions-API TUI tests: the .charliebot home lives under tmp_path and the backend list
  registers opus plus the claude-tui terminal backend the TUI handlers resolve a session against."""
  return CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backends={"options": [
          OPUS_BACKEND_OPTION,
          backend_option(id="claude-tui", label="Claude TUI", type="tui-cli"),
      ]},
  )


def build_codex_worktree_cfg(tmp_path: Path) -> CharlieBotConfig:
  """CharlieBotConfig for spawner worktree-launch tests: the charliebot-home and worktrees dirs live
  under tmp_path so each test owns its own tree, and the backend list registers the codex option the
  launch paths resolve."""
  return CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      paths={"worktree_dir": str(tmp_path / "worktrees")},
      backends={"options": [CODEX_BACKEND_OPTION]},
  )


def build_worktree_cfg(tmp_path: Path) -> CharlieBotConfig:
  """CharlieBotConfig for tests that create and remove worktree dirs: both the charliebot-home and the
  worktrees dirs live under tmp_path so each test owns its own tree — the default (~/worktrees) would
  touch real host worktrees. One cc-claude backend registered so SessionManager.create_session
  (which reads cfg.backends.options[0]) resolves its default."""
  return CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      paths={"worktree_dir": str(tmp_path / "worktrees")},
      backends=fake_backends(),
  )


def build_recovery_cfg(home: Path) -> CharlieBotConfig:
  """CharlieBotConfig for restart-recovery tests: the home dir is caller-chosen (the install-invariance test
  runs its two arms under different homes), the worktrees dir lives under it, and the backend list registers
  the cc-claude fake plus the opencode fake-oc whose uncovered transport the recovery legs exercise."""
  return CharlieBotConfig(
      charliebot_home=home,
      paths={"worktree_dir": str(home / "worktrees")},
      backends={
          "options":
              [
                  backend_option(id="fake", label="Fake", type="cc-claude", model="fake-model"),
                  backend_option(id="fake-oc", label="FakeOC", type="opencode", model="fake-model"),
              ]
      },
  )


def build_light_cc_cfg() -> CharlieBotConfig:
  """CharlieBotConfig whose only backend is a light cc-claude option, also the sole model preference."""
  return CharlieBotConfig(
      backends={
          "options": [backend_option(id="light-cc", label="Light CC", type="cc-claude", model="haiku")],
          "preference": ["light-cc"],
      })


def build_chain_cfg(*options: models.BackendOption) -> CharlieBotConfig:
  """CharlieBotConfig whose backends.preference chains the given options in the order given.

  One-shot fallback tests read the chain off backends.options and backends.preference
  together, so the pair must not drift; deriving the preference list here is what
  keeps the order stated once per test.
  """
  return CharlieBotConfig(backends={"options": list(options), "preference": [option.id for option in options]})


PUBLISH_BASE_URL = "https://pub.example.test/charliebot_pub"


def build_publish_cfg(
    tmp_path: Path, *, publish_dir: Path | None = None, public_base_url: str | None = None) -> CharlieBotConfig:
  """CharlieBotConfig with the publish lane deployed under tmp_path: publish_dir (default
  ``tmp_path / "publish"``) created the way the host's deployment step leaves it, public_base_url
  (default ``PUBLISH_BASE_URL``) set; each argument overridable.
  """
  resolved_dir = publish_dir if publish_dir is not None else tmp_path / "publish"
  resolved_dir.mkdir(parents=True, exist_ok=True)
  resolved_url = public_base_url if public_base_url is not None else PUBLISH_BASE_URL
  return CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      publish={
          "dir": resolved_dir,
          "public_base_url": resolved_url
      },
  )


def write_artifact(tmp_path: Path, name: str = "page.html", body: str = "<p>hello</p>") -> Path:
  """Write one fake artifact source file under tmp_path/artifacts and return its path; publish and
  slack publish-lane tests stage here the file a published URL points at."""
  path = tmp_path / "artifacts" / name
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(body, encoding="utf-8")
  return path


def write_plan_artifact(
    cfg: CharlieBotConfig, session_id: str, name: str = "plan_01.html", content: str | None = None) -> str:
  """Write one plan artifact under cfg's sessions dir and return its plan-relative path; the default content
  passes the plan assertion set so tests can present/approve directly."""
  if content is None:
    content = plan_page_html()
  artifacts_dir = cfg.sessions_dir / session_id / "artifacts"
  artifacts_dir.mkdir(parents=True, exist_ok=True)
  (artifacts_dir / name).write_text(content, encoding="utf-8")
  return f"artifacts/{name}"


def write_thread_meta(cfg: CharlieBotConfig, session_id: str, meta: dict) -> Path:
  """Write meta as the session's threads/<meta["id"]>/metadata.json and return the file path."""
  thread_dir = cfg.sessions_dir / session_id / "threads" / meta["id"]
  thread_dir.mkdir(parents=True, exist_ok=True)
  path = thread_dir / "metadata.json"
  path.write_text(json.dumps(meta), encoding="utf-8")
  return path


def write_plans(cfg: CharlieBotConfig, session_id: str, data: dict) -> Path:
  """Write data as the session's plans.json and return the file path."""
  plans_path = cfg.sessions_dir / session_id / "plans.json"
  plans_path.parent.mkdir(parents=True, exist_ok=True)
  plans_path.write_text(json.dumps(data, indent=2), encoding="utf-8")
  return plans_path


def plan_version_v1(file: str = "artifacts/plan_01.html") -> dict:
  """One plan-registry version in the current schema: v1, initial trigger, no base, no note
  (present has no predecessor), fixed timestamp."""
  return {
      "v": 1,
      "file": file,
      "created_at": "2026-07-20T00:00:00+00:00",
      "trigger": "initial",
      "base": None,
      "note": None,
  }


def plan_doc(
    plan_id: int = 1,
    versions: list[dict] | None = None,
    *,
    title: str = "Plan",
    takeoff: dict | None = None,
    closed: dict | None = None,
) -> dict:
  """One plan-registry document wrapping versions (default: one plan_version_v1) in the registry schema."""
  return {
      "id": plan_id,
      "title": title,
      "versions": versions if versions is not None else [plan_version_v1()],
      "takeoff": takeoff,
      "closed": closed,
  }


def write_trigger(path: Path, trigger: models.PendingTrigger) -> None:
  """Write trigger as a pending-trigger JSON file at path, creating parent dirs."""
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(trigger.model_dump_json(indent=2), encoding="utf-8")


def publish_via_tmp_rename(path: Path, text: str, tmp_name: str) -> None:
  """Publish *text* at *path* through a tmp sibling and os.replace — the rename move every
  metadata/trigger writer performs, so the memo scans see mtime_ns move. *tmp_name* is a fixed
  sibling name (not atomic_write_text's uuid shape) so failure output stays greppable."""
  tmp = path.with_name(tmp_name)
  tmp.write_text(text, encoding="utf-8")
  os.replace(tmp, path)


def publish_same_size_rewrite(path: Path, text: str, tmp_name: str) -> None:
  """Publish a rewrite that keeps the byte size fixed, then move mtime_ns, so a memo keyed on
  (mtime_ns, size) must answer on the mtime half alone. A suite whose rewrites all change size
  could pass on the size half while the mtime half was broken; this move is the one that would
  catch it. The utime bump keeps the mtime move deterministic.
  """
  assert len(text.encode("utf-8")) == path.stat().st_size, (
      f"{path}: rewrite must keep the byte size fixed; a size change moves the memo's size half "
      "and the mtime half goes untested")
  publish_via_tmp_rename(path, text, tmp_name)
  st = path.stat()
  os.utime(path, ns=(st.st_atime_ns, st.st_mtime_ns + 1_000_000))


def make_scheduler_setup(tmp_path: Path) -> tuple[CharlieBotConfig, SessionManager, Scheduler]:
  """Real cfg/session_mgr/scheduler trio for scheduler and cron-task tests; the scheduler holds the
  process-wide SessionManager because a private instance keeps its own chat-event cache and its
  rounds would never reach the HTTP/WS read paths."""
  cfg = build_scheduler_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  return cfg, session_mgr, Scheduler(cfg, session_mgr)


async def make_plan_setup(
    tmp_path: Path,
) -> tuple[CharlieBotConfig, SessionManager, ThreadManager, PlanRegistryManager, models.SessionMetadata]:
  """Real manager trio plus one created session under a plan-shaped config, for plan tests that talk to the
  registry or the plan endpoints."""
  cfg = build_plan_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  plan_mgr = PlanRegistryManager(cfg, session_mgr)
  meta = await session_mgr.create_session(models.CreateSessionRequest(name="Test"), backend=OPUS_BACKEND_ID)
  return cfg, session_mgr, thread_mgr, plan_mgr, meta


async def make_trigger_setup(tmp_path: Path) -> tuple[CharlieBotConfig, SessionManager, TriggerManager, str]:
  """Real cfg/session_mgr/trigger_mgr trio plus one created session, for the PID/SLURM watch tests."""
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(models.CreateSessionRequest(name="Trigger watch"))
  trigger_mgr = TriggerManager(cfg, session_mgr)
  return cfg, session_mgr, trigger_mgr, session.id


async def no_sleep(_seconds: float) -> None:
  """asyncio.sleep stand-in for watch-loop tests: returns immediately so poll iterations skip wall-clock waits."""
  return


@pytest.fixture
def pidfd_open_available() -> None:
  """Skip when the production pidfd helpers are not supported on this host."""
  from src.core.triggers import _PIDFD_SUPPORTED
  if not _PIDFD_SUPPORTED:
    pytest.skip("pidfd not supported on this host (not even via syscall)")


def fake_cli_cfg(monkeypatch: pytest.MonkeyPatch, sessions_dir: Path) -> None:
  """Point the CLI HTTP layer at a fake config so tests never touch a real server."""
  stub_credentials({"charliebot": {"access_key": ""}})
  monkeypatch.setattr(
      CLI_COMMON_GET_CONFIG_PATCH_TARGET,
      lambda: SimpleNamespace(server_base_url="https://server", sessions_dir=sessions_dir))


def _patched_cli_transport(transport_target: str, cfg: object, argv: list[str],
                           **transport_kw: object) -> Iterator[MagicMock]:
  """The externals a CLI main() call touches: sys.argv becomes argv, get_config returns cfg, and the
  transport verb at transport_target is a MagicMock built from transport_kw (bare when empty, so a
  test can set the response after entering). The mock is yielded for that and for call assertions."""
  with patch("sys.argv", argv), \
       patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg), \
       patch(transport_target, **transport_kw) as transport_mock:
    yield transport_mock


@contextlib.contextmanager
def patched_cli_post(cfg: object, argv: list[str], **post_kw: object) -> Iterator[MagicMock]:
  """_patched_cli_transport with requests.post as the patched verb (the POST path every CLI command
  shares)."""
  yield from _patched_cli_transport(CLI_COMMON_REQUESTS_POST_PATCH_TARGET, cfg, argv, **post_kw)


@contextlib.contextmanager
def patched_cli_get(cfg: object, argv: list[str], **get_kw: object) -> Iterator[MagicMock]:
  """_patched_cli_transport with requests.get as the patched verb (the GET-only commands, e.g.
  plan list/diff)."""
  yield from _patched_cli_transport(CLI_COMMON_REQUESTS_GET_PATCH_TARGET, cfg, argv, **get_kw)


def schedule_trigger_argv(message: str, *extra: str) -> list[str]:
  """The schedule_trigger CLI argv the CLI tests share: session s1, --max-wait 60, --message."""
  return ["schedule_trigger", "--session", "s1", "--max-wait", "60", "--message", message, *extra]


def make_task_spawner(tasks: list[asyncio.Task]) -> Callable[..., asyncio.Task]:
  """A create_logged_task substitute that spawns eagerly and captures every task."""

  def _spawn(coro: Coroutine, *, name: str | None = None) -> asyncio.Task:
    task = asyncio.get_running_loop().create_task(coro, name=name)
    tasks.append(task)
    return task

  return _spawn


def dump_yaml(body: Any) -> str:
  """Block-style ``yaml.safe_dump`` with the dict's insertion key order kept; callers write the result
  into cron host files whose key order should read like a hand-written file."""
  return yaml.safe_dump(body, default_flow_style=False, sort_keys=False)


def reset_config_caches() -> None:
  """Clear src.core.config's module-level caches so the next read reloads from disk.

  The config cache and the cron snapshot both key freshness on a fingerprint,
  so an instance cached under an earlier test's profile would answer for the
  wrong one. The credentials cache joins the reset: a stub planted by an
  earlier test (or a value loaded from the host's real credentials.yaml) must
  not answer for this one.
  """
  core_config._config_cache.reset()
  core_config._credentials_cache.reset()
  core_config._home_cache.clear()
  core_config._cron_snapshot = core_config._CronSnapshot()


def stub_credentials(sections: dict[str, dict[str, str | int]]) -> None:
  """Plant in-memory credentials for get_credentials(): the given sections become the cached
  Credentials, stamped with the current credentials.yaml fingerprint, so the answer comes from
  memory and no file is read."""
  core_config._credentials_cache.seed(core_config.Credentials(path=Path("credentials.yaml"), sections=sections))


@pytest.fixture(autouse=True)
def _isolate_profile(tmp_path_factory: pytest.TempPathFactory, monkeypatch: pytest.MonkeyPatch) -> Iterator[None]:
  """Every test runs under its own empty profile home, so a code path that calls the real
  get_config() loads defaults instead of the host's ~/.charliebot; a test that asserts
  default-home behavior deletes the variable itself, as temp_home does. Autouse fixtures run
  before requested ones, so temp_home (deletes the variable, points HOME at a tmp dir) and
  profile_home (sets it) keep working unchanged. The directory lives outside the test's own
  tmp_path, so a test that creates its own profile directory under tmp_path does not collide
  with it."""
  profile = tmp_path_factory.mktemp("profile")
  monkeypatch.setenv(core_config.CHARLIEBOT_HOME_ENV, str(profile))
  reset_config_caches()
  yield
  reset_config_caches()


@pytest.fixture
def temp_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
  """Point HOME at a temp dir and reset the config/cron module-level caches.

  ``CHARLIEBOT_HOME`` wins over ``HOME`` in ``src.core.config.charliebot_home_dir`` when both
  are set, so the fixture deletes it: a shell exported with a real profile must not leak that
  profile's config.d into a test run.
  """
  monkeypatch.delenv("CHARLIEBOT_HOME", raising=False)
  monkeypatch.setenv("HOME", str(tmp_path))
  reset_config_caches()
  return tmp_path


@pytest.fixture
def profile_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[Path]:
  """Point ``CHARLIEBOT_HOME`` at a fresh tmp dir and clear the config module caches around it.

  ``get_config()`` caches process-wide on a fingerprint, so an instance cached under an
  earlier test's profile would answer with the wrong profile.
  """
  monkeypatch.setenv(core_config.CHARLIEBOT_HOME_ENV, str(tmp_path))
  reset_config_caches()
  yield tmp_path
  reset_config_caches()


def cron_d_dir(home: Path) -> Path:
  """The per-job cron dir under a HOME-rooted test dir; once the temp_home fixture points HOME at
  ``home``, this is the dir ``get_scheduled_tasks`` scans for per-job host files."""
  return Path(home) / ".charliebot" / "config.d" / "cron.d"


def write_cron_task(home: Path, name: str, text: str) -> Path:
  """Write one per-job cron host file ``<name>.yaml`` under ``cron_d_dir(home)`` verbatim (dump_yaml
  output, or raw text for a loader-rejection case); the returned path is what assertions read back."""
  p = cron_d_dir(home) / f"{name}.yaml"
  p.parent.mkdir(parents=True, exist_ok=True)
  p.write_text(text, encoding="utf-8")
  return p


MEMORY_DEFAULT_TOPICS = [
    "profile resident",
    "communication resident",
    "workflow resident",
    "rulings resident",
    "host resident",
    "charliebot",
]


def memory_entry_text(
    topic: str,
    slug: str,
    *,
    scope: str = "user",
    audience: str = "master, worker",
    title: str | None = None,
    revises: str | None = None,
    body: str | None = None,
) -> str:
  """One format-v2 memory entry file's text: title in frontmatter, comma-list audience,
  pure-markdown body."""
  if title is None:
    title = slug.replace("-", " ").title()
  header = ["---", f"scope: {scope}", f"topic: {topic}", f"audience: {audience}", f"title: {title}"]
  if revises is not None:
    header.append(f"revises: {revises}")
  header.append("---")
  if body is None:
    body = f"body for {slug}\n"
  return "\n".join(header) + "\n" + body


def legacy_memory_entry_text(
    topic: str,
    slug: str,
    *,
    scope: str = "user",
    audience: str = "both",
    created: str = "2026-07-28",
    source: str = "test",
    revises: str | None = None,
    body: str | None = None,
) -> str:
  """One format-v1 (legacy) memory entry file's text: created/source header, three-value audience,
  ``# <title>`` body opener."""
  header = [
      "---", f"scope: {scope}", f"topic: {topic}", f"audience: {audience}", f"created: {created}", f"source: {source}"
  ]
  if revises is not None:
    header.append(f"revises: {revises}")
  header.append("---")
  if body is None:
    body = f"# {slug.replace('-', ' ').title()}\n\nbody for {slug}\n"
  return "\n".join(header) + "\n" + body


def write_memory_topics(memory_dir: Path, lines: list[str] | None = None) -> None:
  """Write a memory store's ``topics`` file (one topic per line, default ``MEMORY_DEFAULT_TOPICS``)
  and create the ``entries/`` dir the loader scans."""
  memory_dir.mkdir(parents=True, exist_ok=True)
  (memory_dir / "entries").mkdir(exist_ok=True)
  (memory_dir / "topics").write_text(
      "".join(line + "\n" for line in (lines or MEMORY_DEFAULT_TOPICS)), encoding="utf-8")


def write_memory_entry(memory_dir: Path, topic: str, slug: str, legacy: bool = False, **kw: Any) -> Path:
  """Write one entry file ``entries/<topic>/<slug>.md`` via memory_entry_text (or
  legacy_memory_entry_text with ``legacy=True``); the returned path is what assertions read back."""
  d = memory_dir / "entries" / topic
  d.mkdir(parents=True, exist_ok=True)
  p = d / f"{slug}.md"
  text = legacy_memory_entry_text(topic, slug, **kw) if legacy else memory_entry_text(topic, slug, **kw)
  p.write_text(text, encoding="utf-8")
  return p


def write_memory_staging(memory_dir: Path, name: str, topic: str, slug: str, legacy: bool = False, **kw: Any) -> Path:
  """Write one staging candidate ``staging/<name>.md``; same text rules as write_memory_entry."""
  memory_dir.joinpath("staging").mkdir(parents=True, exist_ok=True)
  p = memory_dir / "staging" / f"{name}.md"
  text = legacy_memory_entry_text(topic, slug, **kw) if legacy else memory_entry_text(topic, slug, **kw)
  p.write_text(text, encoding="utf-8")
  return p


class FakeWebSocket:
  """WebSocket double recording every sent frame in .sent, for tests that pass it to server
  catchup/replay producers duck-typing the FastAPI WebSocket.

  Frames arrive as pre-rendered text (the replay's send path) or dicts
  (send_json callers); both land parsed in .sent, and send_text keeps the raw
  strings in .sent_text for wire-byte assertions.
  """

  def __init__(self) -> None:
    self.sent: list[dict] = []
    self.sent_text: list[str] = []

  async def send_json(self, payload: dict) -> None:
    self.sent.append(payload)

  async def send_text(self, text: str) -> None:
    self.sent_text.append(text)
    self.sent.append(json.loads(text))


class FakeSessionManager:
  """SessionManager double replaying a canned chat-event list.

  Callers rely on load_chat_events_sync returning the constructor's events
  (takeoff-gate probes) and on persist_and_broadcast being an AsyncMock
  (delegate/agent-message route tests); get_session answers a bare
  SessionMetadata for any id.
  """

  def __init__(self, events: list[dict[str, Any]]) -> None:
    self.events = events
    self.persist_and_broadcast = AsyncMock()

  async def get_session(self, session_id: str) -> models.SessionMetadata:
    return models.SessionMetadata(id=session_id, name="Test")

  def load_chat_events_sync(self, session_id: str) -> list[dict[str, Any]]:
    return self.events


class FakeThreadManager:
  """ThreadManager double for scheduler tests: hands back one fixed thread.

  Callers patch src.core.scheduler.ThreadManager to return an instance; they
  rely on create_thread overwriting that thread's session_id, description,
  and require_review from the call arguments.
  """

  def __init__(self) -> None:
    self.thread = models.ThreadMetadata(id="thread-1", session_id="session-1", description="nightly prompt")

  async def create_thread(
      self,
      session: models.SessionMetadata,
      description: str,
      require_review: bool = True,
  ) -> models.ThreadMetadata:
    self.thread.session_id = session.id
    self.thread.description = description
    self.thread.require_review = require_review
    return self.thread


class FakeAsyncProcess:
  """asyncio.subprocess.Process double replaying canned stdout/stderr.

  Callers rely on communicate answering the constructor's streams, on kill
  being a no-op, and on wait answering returncode (trigger watch-loop
  subprocess factories).
  """

  def __init__(self, stdout: bytes, stderr: bytes = b"", returncode: int = 0) -> None:
    self._stdout = stdout
    self._stderr = stderr
    self.returncode = returncode

  async def communicate(self) -> tuple[bytes, bytes]:
    return self._stdout, self._stderr

  def kill(self) -> None:
    pass

  async def wait(self) -> int:
    return self.returncode


class FakeStdout:
  """proc.stdout double: async iterator over canned NDJSON byte lines.

  Callers assign an instance to a mocked process's stdout and rely on
  __anext__ replaying the constructor's lines in order before raising
  StopAsyncIteration (backend one-shot subprocess tests).
  """

  def __init__(self, lines: list[bytes]) -> None:
    self._lines = list(lines)

  def __aiter__(self) -> "FakeStdout":
    return self

  async def __anext__(self) -> bytes:
    if not self._lines:
      raise StopAsyncIteration
    return self._lines.pop(0)


def fake_one_shot_proc(lines: list[bytes], *, stderr: bytes = b"", returncode: int = 0, pid: int = 9000) -> MagicMock:
  """Process double for one-shot backend subprocess tests.

  stdout replays *lines* then ends (a FakeStdout). stderr.read() yields *stderr*
  once and b"" after; an empty *stderr* (the default) reads b"" on every call,
  matching the read-to-empty loop the one-shot paths drive. wait() resolves
  *returncode*, which returncode also holds. Callers assert on proc.pid where
  the production kill path uses it.
  """
  proc = MagicMock()
  proc.stdout = FakeStdout(lines)
  proc.stderr = MagicMock()
  if stderr:
    proc.stderr.read = AsyncMock(side_effect=[stderr, b""])
  else:
    proc.stderr.read = AsyncMock(return_value=b"")
  proc.wait = AsyncMock(return_value=returncode)
  proc.returncode = returncode
  proc.pid = pid
  return proc


class FakeChunkedResponse:
  """httpx streaming-response double replaying the constructor's byte chunks from aiter_bytes().

  Callers pass it where production code holds an httpx streaming response
  (the SSE consumers) and rely on their pre-cut chunks reaching that consumer
  as-is; raise_for_status() and aclose() no-op to mirror httpx's response
  surface.
  """

  def __init__(self, chunks: list[bytes]) -> None:
    self._chunks = chunks

  def raise_for_status(self) -> None:
    pass

  async def aclose(self) -> None:
    pass

  async def aiter_bytes(self) -> AsyncIterator[bytes]:
    for chunk in self._chunks:
      yield chunk


class ScriptedRelayBackend:
  """Account-relay backend double whose run() yields a script of events.

  terminate() ends the stream and records the kill (exit_code -15), mirroring
  the relay paths' safe-point stop. Carries the union of the surface the
  master-cc and worker relay paths read: exit_code/stderr_text/terminated
  after the stream ends, the prompt/env/cwd the run launched with, and the
  cancel let-go trio (detach, pid_start, hang_diagnostics) the worker cancel
  path touches.
  """

  def __init__(self, events: list[dict], exit_code: int, stderr_text: str = "") -> None:
    self._events = events
    self.exit_code = exit_code
    self.stderr_text = stderr_text
    self.terminated = False
    self.hang_diagnostics = None
    self.pid_start = "1-1"
    self.prompt: str | None = None
    self.env: dict | None = None
    self.cwd: str | None = None

  async def terminate(self) -> None:
    self.terminated = True
    self.exit_code = -15

  def detach(self) -> None:
    pass

  async def run(self, prompt: str, cwd: str, env: dict, uploaded_files: list[dict] | None = None):
    self.prompt = prompt
    self.cwd = cwd
    self.env = env
    for event in self._events:
      if self.terminated:
        return
      yield event


def install_scripted_backends(
    monkeypatch: pytest.MonkeyPatch,
    backends: list[ScriptedRelayBackend],
    patch_target: str,
) -> list[dict]:
  """Serve *backends* one build at a time from a patched build_backend.

  *patch_target* is the dotted import path of the build_backend binding the
  tested path reads: the master-cc run path re-imports through
  src.agents.backends.registry on every call (BUILD_BACKEND_PATCH_TARGET),
  while the worker path reads src.agents.worker's module binding
  (WORKER_BUILD_BACKEND_PATCH_TARGET). Returns the build records (option,
  kwargs, backend) in build order.
  """
  builds: list[dict] = []
  queue = list(backends)

  def fake_build_backend(option: models.BackendOption, cfg: CharlieBotConfig, **kwargs: Any) -> ScriptedRelayBackend:
    backend = queue.pop(0)
    builds.append({"option": option, "kwargs": kwargs, "backend": backend})
    return backend

  monkeypatch.setattr(patch_target, fake_build_backend)
  return builds


class TerminateFlagBackend:
  """terminate() surface for plain backend doubles: record the signal, touch no process.

  The master-cc cancel and terminate-on-failure paths call terminate() and read
  terminated; a double with no child process keeps that surface with no other
  effect. Real signalling lives on AgentBackend.terminate.
  """

  terminated = False

  async def terminate(self) -> None:
    self.terminated = True


class FakeBackend(TerminateFlagBackend):
  """AgentBackend double whose run() yields one canned result event.

  Callers install it through a patched build_backend on the master-cc run path
  and rely on the exit_code/stderr_text attributes that path reads after the
  event stream ends. The cancel let-go path is not covered: detach() and
  pid_start are absent.
  """

  exit_code = 0
  stderr_text = ""

  async def run(self, prompt: str, cwd: str, env: dict, uploaded_files: list[dict] | None = None):
    yield backend_base.make_result_event()


class CapturingBackend(TerminateFlagBackend):
  """AgentBackend double that records each run() call instead of yielding events.

  Callers install it through a patched build_backend on the master-cc round path and assert on
  `calls`: one dict per run() carrying the prompt and the uploaded_files the round passed.
  """

  exit_code = 0
  stderr_text = ""

  def __init__(self) -> None:
    self.calls: list[dict] = []

  async def run(self, prompt: str, cwd: str, env: dict, uploaded_files: list[dict] | None = None):
    self.calls.append({"prompt": prompt, "uploaded_files": uploaded_files})
    if False:
      yield {}  # keeps run() an async generator; the consumer's async-for would TypeError on a coroutine


def _instructions_content_stub(
    session_meta: models.SessionMetadata, cfg: CharlieBotConfig, prompt_overlay: str | None) -> str:
  return "instructions"


def patch_instructions_content(monkeypatch: pytest.MonkeyPatch) -> None:
  """Patch the master-cc instructions builder to return the fixed string "instructions"."""
  monkeypatch.setattr(master_cc_run, "_build_instructions_content", _instructions_content_stub)


async def run_captured_round(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    *,
    session_id: str,
    name: str,
    backend: CapturingBackend,
    drive: Callable[[CharlieBotConfig, models.SessionMetadata, models.SessionCallbacks], Awaitable[None]],
) -> models.SessionCallbacks:
  """Drive one master-cc round through `drive` with the backend captured: build_backend returns
  `backend`, the instructions builder is stubbed, broadcasts are silenced, and the SessionManager
  double reports no running tasks. Drains the consumer before returning, so the round's
  persist/broadcast work is observable on the returned callbacks; master-cc state resets on
  entry and exit.
  """
  cfg = build_master_cc_cfg(tmp_path)
  meta = models.SessionMetadata(id=session_id, name=name, backend="fake")
  callbacks = mock_session_callbacks()
  async with fresh_master_state(meta.id):
    monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, lambda *a, **kw: backend)
    patch_instructions_content(monkeypatch)
    workers_mock = MagicMock()
    workers_mock._has_running_tasks = AsyncMock(return_value=False)
    with (
        patch.object(master_cc_queue.streaming_manager, "broadcast", new=AsyncMock()),
        patch(SESSIONS_SESSION_MANAGER_PATCH_TARGET, return_value=workers_mock),
    ):
      await drive(cfg, meta, callbacks)
      await drain_session_consumer(meta.id, timeout=5)
  return callbacks


@contextlib.contextmanager
def patch_trigger_fire(
    subprocess_mock: AsyncMock, sacct_available: bool | None,
    sleep_mock: Callable[[float], Awaitable[None]] | None) -> Iterator[AsyncMock]:
  """Patch the externals a trigger-watch fire run reads; yields the trigger_master mock.

  broadcast and trigger_master are always patched. sacct_available=None leaves
  _SACCT_AVAILABLE untouched, for probes that never read it (remote PID);
  sleep_mock=None keeps real sleeps, for runs that assert on elapsed time.
  """
  patches = []
  if sacct_available is not None:
    patches.append(patch(TRIGGERS_SACCT_AVAILABLE_PATCH_TARGET, sacct_available))
  patches.append(patch(TRIGGERS_ASYNCIO_CREATE_SUBPROCESS_EXEC_PATCH_TARGET, new=subprocess_mock))
  if sleep_mock is not None:
    patches.append(patch("src.core.triggers.asyncio.sleep", new=sleep_mock))
  patches.append(patch(BROADCAST_PATCH_TARGET, new=AsyncMock()))
  master_patch = patch(TRIGGER_MASTER_PATCH_TARGET, new=AsyncMock())
  with contextlib.ExitStack() as stack:
    for p in patches:
      stack.enter_context(p)
    yield stack.enter_context(master_patch)


@contextlib.contextmanager
def patch_trigger_mocks() -> Iterator[AsyncMock]:
  """Patch the two module seams a firing trigger reads: streaming broadcast and trigger_master.

  Yields the trigger_master mock. Rigs that must run the real watch internals (real pids,
  real sleeps) use this instead of patch_trigger_fire, whose subprocess stub would hide them.
  """
  with (
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()),
      patch(TRIGGER_MASTER_PATCH_TARGET, new=AsyncMock()) as mock_master,
  ):
    yield mock_master


async def assert_trigger_fired(
    trigger_mgr: TriggerManager, session_id: str, trigger_id: str, mock_master: AsyncMock, *, reason: str) -> str:
  """Asserts the trigger persisted FIRED with the given reason and the standard fired prefix;
  returns the fired message so the caller can assert its site-specific suffix (pids, slurm states).

  Only watch-target triggers take the prefixed message form, so the bare-form pure-delay path
  asserts its whole message at the test site instead of calling this helper."""
  stored = await trigger_mgr._load_trigger(session_id, trigger_id)
  assert stored.status == models.TriggerStatus.FIRED
  assert stored.fire_reason == reason
  msg = mock_master.await_args.args[1]
  assert f"[Scheduled trigger fired | {reason}]" in msg
  return msg


def make_fake_run_tmux(calls: list[tuple[str, ...]]) -> Callable[..., Awaitable[tuple[int, str]]]:
  """A `_run_tmux` stand-in that answers "has-session" as missing and records every call.

  "has-session" exits rc 1 so the patched session-setup path takes its create branch;
  every other tmux invocation exits rc 0 with empty stderr. The signature mirrors
  src.agents.backends.pty_common._run_tmux so the monkeypatched attribute stays a
  drop-in replacement.
  """

  async def fake_run_tmux(*args: str, check: bool = False) -> tuple[int, str]:
    calls.append(args)
    if args[0] == "has-session":
      return 1, ""
    return 0, ""

  return fake_run_tmux


# Captured before any patch window opens: the os.replace spies below delegate through it, so a
# spy running inside its own patch window still performs the real swap.
REAL_OS_REPLACE = os.replace


def make_os_replace_spy(captured_targets: list[str]) -> Callable[[str, str], None]:
  """os.replace side-effect recording each destination path, then performing the real swap.

  The delegation back to the real os.replace is the contract: a stand-in that skips the swap
  lets a write side pass while never publishing the staged payload, which is the defect the
  atomic-write tests discriminate against.
  """

  def capture_replace(src: str, dst: str) -> None:
    captured_targets.append(str(dst))
    return REAL_OS_REPLACE(src, dst)

  return capture_replace


def make_read_at_os_replace(read_at_swap: list[str], target: Path) -> Callable[[str, str], None]:
  """os.replace side-effect reading ``target`` at the swap instant, then performing the real swap.

  The read must precede the real swap: it observes the previous document at the instant a
  concurrent reader could, and a read after the swap would see the new one instead. Delegation
  back to the real os.replace carries the same contract as make_os_replace_spy's.
  """

  def read_then_replace(src: str, dst: str) -> None:
    read_at_swap.append(target.read_text(encoding="utf-8"))
    return REAL_OS_REPLACE(src, dst)

  return read_then_replace


def make_fake_git_create_worktree(*,
                                  mkdir: bool = False,
                                  captures: dict[str, Any] | None = None) -> Callable[..., Awaitable[BaseResolution]]:
  """A `git_create_worktree` stand-in returning a `detail="fake"` BaseResolution.

  The signature mirrors src.core.git.git_create_worktree so the monkeypatched attribute
  stays a drop-in replacement. mkdir=True creates the worktree dir for flows that write
  into it after creation; captures records the call under "git_create_worktree" with the
  repo/base_branch/branch_name/wt_path key set the spawn-driving tests assert on.
  """

  async def fake_git_create_worktree(
      repo_path: Path,
      base_branch: str,
      branch_name: str,
      wt_path: Path,
      *,
      remote_tip: str | None = None) -> BaseResolution:
    del remote_tip
    if captures is not None:
      captures["git_create_worktree"] = {
          "repo": repo_path,
          "base_branch": base_branch,
          "branch_name": branch_name,
          "wt_path": wt_path,
      }
    if mkdir:
      wt_path.mkdir(parents=True, exist_ok=True)
    return BaseResolution(canonical=base_branch, start_point=base_branch, detail="fake")

  return fake_git_create_worktree


def patch_improve_git_ops(monkeypatch: pytest.MonkeyPatch) -> None:
  """Install the pass-through git fakes a run_improve_loop test needs without a real repo.

  create_worktree and push_branch are patched on src.core.improve_command (create_worktree
  reuses make_fake_git_create_worktree(mkdir=True), push_branch succeeds). remove and prune
  are patched on src.core.git: the finally-cleanup resolves them there through
  git_worktree_remove_reporting, so the faked remove still clears the worktree dir and
  prune runs. The fakes mirror the real signatures so each patch stays a drop-in
  replacement.
  """

  async def fake_git_push_branch(repo_path: Path, branch_name: str) -> tuple[bool, str]:
    del repo_path, branch_name
    return True, ""

  async def fake_git_worktree_remove(
      repo_path: str,
      wt_path: Path,
      session: str,
      *,
      allowed_parent: Path,
      expected_residue_name: str,
  ) -> bool:
    del repo_path, session, allowed_parent, expected_residue_name
    if wt_path.exists():
      wt_path.rmdir()
    return True

  async def fake_git_worktree_prune(repo_path: str, session: str) -> None:
    del repo_path, session

  monkeypatch.setattr(improve_command, "git_create_worktree", make_fake_git_create_worktree(mkdir=True))
  monkeypatch.setattr(improve_command, "git_push_branch", fake_git_push_branch)
  monkeypatch.setattr("src.core.git.git_worktree_remove", fake_git_worktree_remove)
  monkeypatch.setattr("src.core.git.git_worktree_prune", fake_git_worktree_prune)


def build_worker_prompt(
    description: str,
    cfg: CharlieBotConfig,
    *,
    task_type: models.TaskType = models.TaskType.IMPLEMENT,
    loop_dir: str | None = None,
    iteration_number: int | None = None,
    is_continuation: bool = False,
    keep_worktree: bool = False,
) -> str:
  """spawner._build_worker_prompt with the arguments the prompt-content tests share: /tmp/repo
  as the repo, charliebot/task-xyz branched off main, a one-field SessionMetadata, no
  start_point. The keyword fields are the knobs the prompt tests vary; a test needing any
  other field (repo_path, branch_name, wt_path, session_meta, start_point) builds its own."""
  return spawner._build_worker_prompt(
      description=description,
      repo_path=Path("/tmp/repo"),
      base_branch="main",
      branch_name="charliebot/task-xyz",
      wt_path="/tmp/worktrees/charliebot-task-xyz",
      session_meta=models.SessionMetadata(id="session-id", name="test"),
      cfg=cfg,
      task_type=task_type,
      loop_dir=loop_dir,
      iteration_number=iteration_number,
      is_continuation=is_continuation,
      keep_worktree=keep_worktree,
      start_point=None,
  )


def recording_notify_completion(captures: dict[str, Any]) -> Callable[..., Awaitable[None]]:
  """A spawner_finalize._notify_completion stand-in recording the finalized outcome and thread.

  The signature mirrors the production call in spawner_finalize._run_finalize_effects
  (one _FinalizeCtx plus keyword-only verify_report); each monkeypatching test reads back
  only the captured fields it asserts on.
  """

  async def fake_notify_completion(
      ctx: spawner_finalize._FinalizeCtx,
      verify_report: str | None = None,
  ) -> None:
    del verify_report
    captures["notified"] = True
    captures["notify_exit_code"] = ctx.outcome.exit_code
    captures["notify_thread"] = ctx.thread

  return fake_notify_completion


def build_finalize_ctx(
    thread: models.ThreadMetadata,
    outcome: spawner._WorkerRunOutcome,
    thread_mgr: Any,
    session_mgr: Any,
    cfg: CharlieBotConfig,
) -> spawner_finalize._FinalizeCtx:
  """The _FinalizeCtx the finalize and broadcast-completion tests share.

  Callers rely on session_id and description being the thread's own; the
  thread manager, session manager, outcome, and cfg stay per-site arguments.
  """
  return spawner_finalize._FinalizeCtx(
      session_id=thread.session_id,
      description=thread.description,
      thread=thread,
      outcome=outcome,
      thread_mgr=thread_mgr,
      session_mgr=session_mgr,
      cfg=cfg,
  )


def capturing_worker(captures: dict[str, Any]) -> type:
  """A spawner_launch.Worker stand-in recording its constructor args into ``captures``.

  The signature mirrors the production call in spawner_launch._construct_worker, and the
  fixed worker_dir/worker_backend/task_description key set is the shared contract
  the spawn-driving tests assert on; thread_metadata, events_log_path, worker_cfg,
  and on_spawned go unrecorded because no test reads them back off the worker.
  """

  class CapturingWorker:

    def __init__(
        self,
        thread_metadata: models.ThreadMetadata,
        working_dir: Path,
        events_log_path: Path,
        task_description: str,
        worker_cfg: CharlieBotConfig,
        backend_option: models.BackendOption | None = None,
        on_spawned: Callable | None = None,
        claude_account: models.ClaudeAccount | None = None,
    ) -> None:
      captures["worker_dir"] = working_dir
      captures["worker_backend"] = backend_option
      captures["task_description"] = task_description
      captures["worker_account"] = claude_account

    async def run(self) -> int:
      return 0

    async def terminate(self) -> None:
      return None

  return CapturingWorker


def make_one_shot_backend(one_shot: AsyncMock) -> MagicMock:
  """A stand-in backend whose one_shot_text is the given AsyncMock."""
  backend = MagicMock()
  backend.one_shot_text = one_shot
  return backend


def make_one_shot_chain(*one_shots: AsyncMock) -> list[MagicMock]:
  """One-shot backends for a full preference-chain walk: one per candidate, in preference order.

  Chain tests hand this list to patch(build_backend, side_effect=...) so each
  build_backend call serves the next candidate, and a chain that stops early
  leaves the surplus backends unbuilt.
  """
  return [make_one_shot_backend(one_shot) for one_shot in one_shots]


class JudgmentShim:
  """Default finalize-judgment reads for test fakes: no prior effects recorded.

  The finalize chain gates its side effects on judgment reads
  (src/core/finalize_effects): chat events for the summary/master-wake checks,
  thread lists for the reviewer-exists check, the thread dir for the raw-log
  completion-time read. Fakes that predate those gates inherit "nothing
  recorded yet" from here so their tests keep exercising the always-persist /
  always-trigger / always-spawn behavior.
  """

  def load_chat_events_sync(self, session_id: str) -> list[dict[str, Any]]:
    return []

  async def finalize_summary_present(self, session_id: str, thread_id: str) -> bool:
    return False

  async def finalize_master_woke(self, session_id: str, thread_id: str) -> bool:
    return False

  async def deliver_to_successor(self, session_id: str, event: dict[str, Any]) -> str:
    """Default succession-aware delivery for test fakes: no successor, write into itself.

    The stage-C migrated producers call ``deliver_to_successor`` instead of
    ``persist_and_broadcast``. Fakes that never elone their sessions inherit this
    no-successor behavior: the event is persisted into the owning session and the
    id is returned, so those tests keep exercising the unchanged no-redirect path.
    """
    await self.persist_and_broadcast(session_id, event)
    return session_id

  async def list_threads(self, session_id: str) -> list[Any]:
    return []

  def thread_dir(self, session_id: str, thread_id: str) -> Path:
    return Path("/nonexistent-thread-dir") / session_id / thread_id


class CapturingThreadManager(JudgmentShim):
  """ThreadManager double recording spawn/finalize-path calls into a captures dict.

  Callers pass the test's fixed thread and captures dict and rely on get_thread
  answering that thread, on update_status recording the ``status``/``exit_code``
  keys, on save_metadata recording ``saved_thread``, and on get_events_log_path
  answering the constructor's events_log (spawner._finalize_worker and the
  spawner_launch process builders). Sites whose production path reads the
  thread dir off the manager pass thread_root to override the JudgmentShim
  default.
  """

  def __init__(
      self,
      thread: models.ThreadMetadata | None,
      captures: dict[str, Any],
      events_log: Path | None = None,
      thread_root: Path | None = None,
  ) -> None:
    self._thread = thread
    self._captures = captures
    self._events_log = events_log
    self._thread_root = thread_root

  def thread_dir(self, session_id: str, thread_id: str) -> Path:
    if self._thread_root is None:
      return super().thread_dir(session_id, thread_id)
    return self._thread_root / session_id / "threads" / thread_id

  async def get_thread(self, session_id: str, thread_id: str) -> models.ThreadMetadata | None:
    return self._thread

  async def save_metadata(self, meta: models.ThreadMetadata) -> None:
    self._captures["saved_thread"] = meta

  async def get_events_log_path(self, session_id: str, thread_id: str) -> Path:
    assert self._events_log is not None, f"CapturingThreadManager built without events_log ({session_id}/{thread_id})"
    return self._events_log

  async def update_status(
      self,
      session_id: str,
      thread_id: str,
      status: Any,
      pid: int | None = None,
      exit_code: int | None = None,
      completed_at: Any = None,
  ) -> None:
    self._captures["status"] = status
    self._captures["exit_code"] = exit_code


class ReviewSpawnSessionManager(JudgmentShim):
  """SessionManager double for spawn_review_worker tests: one fixed session for any id.

  Callers rely on get_session answering a claude-backed SessionMetadata carrying the
  constructor's session name; spawn_review_worker only None-checks the result and
  hands it to the thread manager.
  """

  def __init__(self, session_name: str) -> None:
    self._session_name = session_name

  async def get_session(self, session_id: str) -> models.SessionMetadata:
    return models.SessionMetadata(id=session_id, name=self._session_name, backend=OPUS_BACKEND_ID)


class SpawnFlowSessionManager(JudgmentShim):
  """SessionManager double for spawner spawn/finalize flow tests.

  Callers rely on get_session answering a bare SessionMetadata for any id and on
  persist_and_broadcast absorbing broadcast events: JudgmentShim's
  deliver_to_successor default, which the finalize chain's delivery paths call,
  routes into it.
  """

  async def get_session(self, session_id: str) -> models.SessionMetadata:
    return models.SessionMetadata(id=session_id, name="Test Session")

  async def persist_and_broadcast(self, session_id: str, event: dict[str, Any]) -> None:
    pass


class WorktreeSpawnRig(NamedTuple):
  """The staged inputs the worktree spawn_worker e2e tests share.

  stage_worktree_spawn installs the three spawn-flow fakes (git_create_worktree, Worker,
  _notify_completion) and builds the thread/cfg/repo/event-log stage; run_worktree_spawn
  drives the shared spawn_worker invocation and the test asserts on the rig's fields.
  """

  cfg: CharlieBotConfig
  repo_path: Path
  thread: models.ThreadMetadata
  captures: dict[str, Any]
  thread_mgr: CapturingThreadManager
  description: str


def stage_worktree_spawn(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    *,
    description: str = "Do work",
    git_fake_mkdir: bool = False,
) -> WorktreeSpawnRig:
  """Stage the worktree spawn_worker e2e rig on a fresh codex-backend cfg.

  The git fake records its call into the shared captures dict (key "git_create_worktree")
  alongside the Worker and _notify_completion fakes; git_fake_mkdir additionally creates
  the worktree dir for flows that write into it after creation.
  """
  cfg = build_codex_worktree_cfg(tmp_path)
  repo_path = (tmp_path / "repo").resolve()
  repo_path.mkdir(parents=True, exist_ok=True)
  thread = models.ThreadMetadata(id="thread-1", session_id="session-id", description=description)
  captures: dict[str, Any] = {}
  monkeypatch.setattr(
      spawner_launch, "git_create_worktree", make_fake_git_create_worktree(mkdir=git_fake_mkdir, captures=captures))
  monkeypatch.setattr(spawner_launch, "Worker", capturing_worker(captures))
  monkeypatch.setattr(spawner_finalize, "_notify_completion", recording_notify_completion(captures))
  events_log = tmp_path / "events.jsonl"
  thread_mgr = CapturingThreadManager(thread, captures, events_log)
  return WorktreeSpawnRig(
      cfg=cfg, repo_path=repo_path, thread=thread, captures=captures, thread_mgr=thread_mgr, description=description)


async def run_worktree_spawn(rig: WorktreeSpawnRig, *, resolved_model: str, keep_worktree: bool) -> None:
  """The one spawn_worker invocation both worktree-spawn e2e tests drive; the rig stages its fakes."""
  await spawner.spawn_worker(
      session_id="session-id",
      description=rig.description,
      thread_id="thread-1",
      cfg=rig.cfg,
      session_mgr=SpawnFlowSessionManager(),
      thread_mgr=rig.thread_mgr,
      request=models.SpawnRequest(
          repo_path=str(rig.repo_path),
          base_branch="main",
          resolved_backend="codex-o3",
          resolved_model=resolved_model,
          keep_worktree=keep_worktree,
      ),
  )


class ReviewSpawnThreadManager(JudgmentShim):
  """ThreadManager double for spawn_review_worker tests: builds and records the review thread.

  Callers rely on create_thread answering a thread with id review-thread-id and on
  save_metadata appending to .saved; the no-reviewer-exists list_threads default comes
  from JudgmentShim.
  """

  def __init__(self) -> None:
    self.saved: list[models.ThreadMetadata] = []

  async def create_thread(
      self,
      session_meta: models.SessionMetadata,
      description: str,
      branch_name: str | None = None,
      review_of: str | None = None,
      require_review: bool = True,
  ) -> models.ThreadMetadata:
    return models.ThreadMetadata(
        id="review-thread-id",
        session_id=session_meta.id,
        description=description,
        branch_name=branch_name,
        review_of=review_of,
    )

  async def save_metadata(self, meta: models.ThreadMetadata) -> None:
    self.saved.append(meta)


async def fake_git_current_branch(repo_path: Path) -> str:
  """git_current_branch stand-in answering main; the signature mirrors src.core.git."""
  return "main"


async def fake_spawn_worker(
    session_id: str,
    description: str,
    thread_id: str,
    cfg: CharlieBotConfig,
    session_mgr: Any,
    thread_mgr: Any,
    request: models.SpawnRequest | None = None,
) -> None:
  """spawn_worker stand-in that never forks a backend; the signature mirrors the review.py call."""
  return


async def _noop() -> None:
  """Awaitable stand-in returned by fakes patched over coroutine-returning helpers."""
  return None


async def _ok_asgi_downstream(scope: Any, receive: Any, send: Any) -> None:
  """Downstream ASGI app the auth-middleware tests wrap; records that it ran."""
  _ok_asgi_downstream.called = True
  await send({"type": "http.response.start", "status": 200, "headers": [(b"content-type", b"text/plain")]})
  await send({"type": "http.response.body", "body": b"OK"})


_ok_asgi_downstream.called = False


async def run_through_asgi_middleware(middleware: Any, scope: dict) -> list[dict]:
  """Drive one ASGI middleware over *scope* with the shared OK downstream; return the sent messages."""
  _ok_asgi_downstream.called = False
  sent: list[dict] = []

  async def receive() -> dict:
    return {"type": "http.request", "body": b"", "more_body": False}

  async def send(message: dict) -> None:
    sent.append(message)

  await middleware(scope, receive, send)
  return sent


def asgi_downstream_called() -> bool:
  """Whether the shared downstream ran during the last run_through_asgi_middleware call."""
  return _ok_asgi_downstream.called


def close_create_logged_task(coro: Any, *, name: str | None = None) -> None:
  """create_logged_task stand-in: closes the coroutine instead of scheduling it as a task."""
  coro.close()


def capture_create_logged_task(captured: dict[str, Any]) -> Callable[..., Any]:
  """Return a create_logged_task stand-in that captures the spawn coroutine's locals."""

  def fake_create_logged_task(coro: Any, *, name: str | None = None) -> Any:
    if coro.cr_frame is not None:
      captured.update(coro.cr_frame.f_locals)
    coro.close()

    class DummyTask:

      def add_done_callback(self, cb: Any) -> None:
        pass

    return DummyTask()

  return fake_create_logged_task


def record_create_logged_task(names: list[str]) -> Callable[..., Any]:
  """Return a create_logged_task stand-in that records each spawn's task name."""

  def fake_create_logged_task(coro: Any, *, name: str | None = None) -> None:
    names.append(name or "")
    coro.close()

  return fake_create_logged_task


def patch_review_spawn_path(monkeypatch: pytest.MonkeyPatch, captured: dict[str, Any]) -> None:
  """Patch all three spawn-path seams; an unpatched one shells out to git or forks a backend."""
  monkeypatch.setattr(review, "git_current_branch", fake_git_current_branch)
  monkeypatch.setattr(spawner, "spawn_worker", fake_spawn_worker)
  monkeypatch.setattr(review, "create_logged_task", capture_create_logged_task(captured))


# Crash-recovery follow-ups are dispatched through create_logged_task under fixed name
# prefixes: resume-drain/resume-follow/respawn-worker/recomplete-finalize in
# src/core/init_worker_recovery.py, master-resume/master-replay in
# src/core/init_master_recovery.py, master-consumer in src/agents/master_cc_queue.py.
# A recovery test must drain those tasks before asserting on rewritten metadata.
RECOVERY_TASK_PREFIXES = ("resume-", "respawn-", "recomplete-")
MASTER_RECOVERY_TASK_PREFIXES = (*RECOVERY_TASK_PREFIXES, "master-resume-", "master-replay-", "master-consumer-")


async def await_recovery_tasks(prefixes: tuple[str, ...]) -> None:
  """Gather every unfinished named recovery task, repeating until none is left.

  A drained task may itself dispatch another named recovery task, so one gather
  pass can still leave work pending.
  """
  current = asyncio.current_task()
  while True:
    pending = [
        t for t in asyncio.all_tasks() if t is not current and not t.done() and t.get_name().startswith(prefixes)
    ]
    if not pending:
      return
    await asyncio.gather(*pending)


# Restart-recovery e2e helpers, single-homed here for test_restart_recovery_e2e.py
# and its sibling files. The A/B protocol's driver side (fake `claude` shim,
# driver template, launcher) stays in test_restart_recovery_e2e.py; these are
# the waits, readers, and the startup-crash-recovery entry the sibling files
# share.
def _cfg(home: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=home,
      paths={"worktree_dir": str(home / "worktrees")},
      backends={"options": [backend_option(id="fake", label="Fake", type="cc-claude", model="fake-model")]},
  )


def _wait_for(predicate, timeout: float, what: str) -> None:
  deadline = time.monotonic() + timeout
  while time.monotonic() < deadline:
    if predicate():
      return
    time.sleep(0.05)
  raise TimeoutError(what)


def _read_meta(home: Path, session_id: str, thread_id: str) -> dict:
  meta_path = home / "sessions" / session_id / "threads" / thread_id / "metadata.json"
  return json.loads(meta_path.read_text(encoding="utf-8"))


async def _await_recovery_tasks() -> None:
  await await_recovery_tasks(RECOVERY_TASK_PREFIXES)


def _kill_driver_mid_run(proc: subprocess.Popen, home: Path, ids: dict) -> None:
  """SIGKILL the driver once the run's identity is persisted and output is flowing."""
  thread_dir = home / "sessions" / ids["session"] / "threads" / ids["thread"]
  raw = thread_dir / "data" / runs.RAW_LOG_NAME

  def run_started() -> bool:
    if not raw.exists() or "E2E-ASSISTANT-MARKER" not in raw.read_text(encoding="utf-8", errors="replace"):
      return False
    try:
      meta = _read_meta(home, ids["session"], ids["thread"])
    except json.JSONDecodeError:
      # metadata.json is a plain (non-atomic) "w"-mode write; the driver may be
      # mid-write when this polls, which is exactly "not ready yet".
      return False
    return meta.get("pid") is not None and meta.get("pid_start") is not None and meta.get("status") == "running"

  _wait_for(run_started, timeout=20.0, what="worker run did not start/persist identity")
  proc.kill()
  proc.wait(timeout=10)


async def _recover(monkeypatch: pytest.MonkeyPatch,
                   home: Path,
                   cfg: CharlieBotConfig | None = None) -> tuple[int, list[bool], list[str], list[runs.RunOutcome]]:
  """Run startup crash recovery as process B; record reattach mode, master
  wakes, and the resolve outcome each interrupted run received."""
  alive_at_reattach: list[bool] = []
  master_wakes: list[str] = []
  outcomes: list[runs.RunOutcome] = []

  async def spy_resume(*args, **kwargs) -> None:
    alive_at_reattach.append(bool(kwargs["is_alive"]()))
    await _real_resume_worker(*args, **kwargs)

  async def fake_trigger_master(session_id: str, summary: str, cfg, session_mgr) -> None:
    master_wakes.append(summary)

  real_resolve = runs.resolve_run

  def spy_resolve(**kwargs):
    resolution = real_resolve(**kwargs)
    outcomes.append(resolution.outcome)
    return resolution

  monkeypatch.setattr(SPAWNER_RESUME_WORKER_PATCH_TARGET, spy_resume)
  monkeypatch.setattr(REVIEW_TRIGGER_MASTER_PATCH_TARGET, fake_trigger_master)
  monkeypatch.setattr("src.core.runs.resolve_run", spy_resolve)

  cfg = cfg or _cfg(home)
  recovered = await init_module.run_crash_recovery(cfg, datetime.now(UTC))
  await _await_recovery_tasks()
  return recovered, alive_at_reattach, master_wakes, outcomes


def _recovery_reports(home: Path, session_id: str) -> list[dict]:
  return [e for e in read_chat_events(home, session_id) if e.get("source") == "crash_recovery"]


def _terminal_summaries(home: Path, ids: dict) -> list[dict]:
  return [
      e for e in read_chat_events(home, ids["session"])
      if e.get("type") == "worker_summary" and e.get("thread_id") == ids["thread"] and e.get("status") != "running"
  ]


def _assert_failed_with_transport_reason(home: Path, ids: dict) -> None:
  """Shared tail of the uncovered-backend recovery tests: the thread finalizes failed with exit
  code -1 and resolve_run's transport reason lands in exactly one terminal worker_summary."""
  meta = _read_meta(home, ids["session"], ids["thread"])
  assert meta["status"] == "failed"
  assert meta["exit_code"] == -1
  summaries = _terminal_summaries(home, ids)
  assert len(summaries) == 1
  assert runs.TRANSPORT_NOT_COVERED_REASON in summaries[0]["full_content"]


def spy_on_load_json_meta(monkeypatch: pytest.MonkeyPatch) -> list[Path]:
  """Record every path init.iter_recent_thread_metas actually reads+parses."""
  read_paths: list[Path] = []
  real_load = worker_recovery_module.load_json_meta

  def spy(path: Path, log_event: str, **kwargs: Any) -> Any:
    read_paths.append(Path(path))
    return real_load(path, log_event, **kwargs)

  monkeypatch.setattr(worker_recovery_module, "load_json_meta", spy)
  return read_paths
