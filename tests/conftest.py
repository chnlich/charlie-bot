import asyncio
import atexit
import contextlib
import importlib
import io
import itertools
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import uuid
from collections.abc import AsyncIterator, Awaitable, Callable, Coroutine, Iterator
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from pathlib import Path
from types import SimpleNamespace
from typing import TYPE_CHECKING, Any, Self
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
import yaml
from _pytest.runner import runtestprotocol
from fastapi import APIRouter, FastAPI
from fastapi.testclient import TestClient
from starlette.middleware.gzip import GZipMiddleware
from starlette.requests import Request

_REAL_ASYNCIO_SLEEP = asyncio.sleep

if TYPE_CHECKING:
  # Annotation-only: the fake-VAD seam's feed sizes; the runtimes import numpy locally.
  pass

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
  sys.path.insert(0, str(ROOT))

# ---------------------------------------------------------------------------
# Hermetic test environment. Conftest import time (not a fixture): it must run
# before collection (some modules parametrize from the environment at import)
# and before the src imports below. Subprocesses the tests spawn inherit it.
# ---------------------------------------------------------------------------
# HOME points at a fresh throwaway home, so the suite can never read or write
# the live ~/.charliebot (real sessions, credentials, cron config) and the
# collection count is the same on the host and in CI. The process deletes it on
# exit: every run creates one, and /tmp otherwise keeps them until reboot.
_TEST_HOME = tempfile.mkdtemp(prefix="charliebot-test-home-")
atexit.register(shutil.rmtree, _TEST_HOME, ignore_errors=True)
os.environ["HOME"] = _TEST_HOME
# A CharlieBot session's own identity leaks into CLI/API tests and flips them to
# 'ambiguous session' refusals; the suite always starts unauthenticated.
for _leaked in ("CHARLIEBOT_RUN_TOKEN", "CHARLIEBOT_HOME", "CHARLIEBOT_SESSION_ID"):
  os.environ.pop(_leaked, None)
# Subprocess probes (`python -c` import checks, the restart-recovery drivers)
# must import the tree UNDER TEST: the venv's editable install is a meta-path
# finder pinning `src` to the main checkout for any process started outside this
# tree, and it sits BEHIND the path finder, so a PYTHONPATH prepend wins.
if os.environ.get("PYTHONPATH", "").split(os.pathsep)[0] != str(ROOT):
  os.environ["PYTHONPATH"] = os.pathsep.join([str(ROOT), os.environ.get("PYTHONPATH", "")]).rstrip(os.pathsep)

# ---------------------------------------------------------------------------
# Per-test wall-time budget and the integration cap. A unit test stays under 2s
# and a test marked `integration` (real processes / real time) under 10s; at
# most 50 collected tests may carry the marker. The budgets are ini options so
# the mechanism's own test can shrink them. Enforcement lives here so the limit
# is mechanical, not a convention. A test whose only failure is its wall time
# reruns once and the rerun's timing decides: CI hosts jitter past the budget
# on runs that pass unchanged seconds later.
# ---------------------------------------------------------------------------

_UNIT_BUDGET_INI = "unit_test_budget_seconds"
_INTEGRATION_BUDGET_INI = "integration_test_budget_seconds"
_MAX_INTEGRATION_INI = "max_integration_tests"
_ELAPSED_ATTR = "_charliebot_wall_seconds"
_BUDGET_REPORTED_ATTR = "_charliebot_budget_reported"
# Rerun-tier state. Attempt 1 runs unlogged (pytest_runtest_protocol below);
# its budget-trip stamp is that hook's rerun evidence, and the rerun's reports
# are the only ones any consumer sees. A rerun fires only when every
# non-tripped report passed, so it can never mask an assertion failure, an
# error, or a skip.
_ATTEMPT_ATTR = "_charliebot_attempt"
_FIRST_ELAPSED_ATTR = "_charliebot_first_wall_seconds"
_TRIP_ELAPSED_ATTR = "_charliebot_budget_trip_seconds"
_RERUN_EVENTS = pytest.StashKey[list]()


def pytest_addoption(parser: pytest.Parser) -> None:
  parser.addini(
      _UNIT_BUDGET_INI, "Per-test wall-time budget in seconds (setup + call + teardown) for tests that carry no "
      "integration or local_only marker; enforced by this conftest.",
      type="float",
      default=2.0)
  parser.addini(
      _INTEGRATION_BUDGET_INI,
      "Per-test wall-time budget in seconds for tests marked @pytest.mark.integration.",
      type="float",
      default=10.0)
  parser.addini(
      _MAX_INTEGRATION_INI,
      "Maximum number of collected tests that may carry the integration marker.",
      type="int",
      default=50)


def _budget_seconds(item: pytest.Item) -> float | None:
  """This item's wall-time budget, or None when the budget does not apply."""
  if item.get_closest_marker("local_only"):
    # Runs only by hand against host-local resources (GPU, tailnet); it is
    # outside the CI surface the budgets keep small and fast.
    return None
  ini = _INTEGRATION_BUDGET_INI if item.get_closest_marker("integration") else _UNIT_BUDGET_INI
  return float(item.config.getini(ini))


def _accrue(item: pytest.Item, seconds: float) -> None:
  setattr(item, _ELAPSED_ATTR, getattr(item, _ELAPSED_ATTR, 0.0) + seconds)


@pytest.hookimpl(wrapper=True)
def _accrue_stage_time(item: pytest.Item) -> Any:
  start = time.perf_counter()
  result = yield
  _accrue(item, time.perf_counter() - start)
  return result


# pluggy names each hook after the module attribute it found the wrapper under
# (pytest requires that name to start with "pytest_"), so the one shared
# wrapper must be bound under all three stage names.
pytest_runtest_setup = pytest_runtest_call = pytest_runtest_teardown = _accrue_stage_time


def _budget_longrepr(item: pytest.Item, budget: float, elapsed: float, kind: str) -> str:
  """The budget-failure message: the timing, the budget, and the escape hatch."""
  return (
      f"test wall time {elapsed:.2f}s exceeds the {budget:g}s {kind} budget "
      f"(setup + call + teardown, enforced by tests/conftest.py). Make the test faster, or mark "
      f"it @pytest.mark.integration if it truly needs real processes or real time "
      f"(integration budget {float(item.config.getini(_INTEGRATION_BUDGET_INI)):g}s).")


@pytest.hookimpl(wrapper=True)
def pytest_runtest_makereport(item: pytest.Item, call: pytest.CallInfo[None]) -> Any:
  report: pytest.TestReport = yield
  if report.outcome != "passed":
    return report  # a real failure or a skip already owns this report
  budget = _budget_seconds(item)
  if budget is None:
    return report
  elapsed = getattr(item, _ELAPSED_ATTR, 0.0)
  if elapsed <= budget or getattr(item, _BUDGET_REPORTED_ATTR, False):
    return report
  setattr(item, _BUDGET_REPORTED_ATTR, True)
  kind = "integration" if item.get_closest_marker("integration") else "unit"
  report.outcome = "failed"
  report.longrepr = _budget_longrepr(item, budget, elapsed, kind)
  if getattr(item, _ATTEMPT_ATTR, 1) == 1:
    # Attempt 1 is unlogged, so this report reaches no consumer: it is the
    # protocol hook's evidence for a rerun, and the rerun's own trip carries
    # the verdict with both timings.
    setattr(report, _TRIP_ELAPSED_ATTR, elapsed)
  else:
    first = getattr(item, _FIRST_ELAPSED_ATTR, 0.0)
    report.longrepr += (
        f" Budget rerun: attempt 1 {first:.2f}s, rerun {elapsed:.2f}s"
        f" - both exceed the {budget:g}s {kind} budget.")
  return report


@pytest.hookimpl(tryfirst=True)
def pytest_runtest_protocol(item: pytest.Item, nextitem: pytest.Item | None) -> bool | None:
  """Run the item once, then once more when only its wall time tripped.

  Attempt 1 runs with logging off, so its reports reach no consumer until this
  hook decides: logged untouched, or discarded for a rerun whose reports carry
  the outcome. Returning True halts the firstresult hook chain, keeping the
  default protocol out; returning None would defer to it.
  """
  item.ihook.pytest_runtest_logstart(nodeid=item.nodeid, location=item.location)
  reports = runtestprotocol(item, log=False, nextitem=nextitem)
  trips = [rep for rep in reports if getattr(rep, _TRIP_ELAPSED_ATTR, None) is not None]
  if len(trips) != 1 or not all(rep.passed or rep is trips[0] for rep in reports):
    # Nothing tripped, or the attempt owns a failure the budget cannot explain:
    # attempt 1 is the outcome.
    for rep in reports:
      item.ihook.pytest_runtest_logreport(report=rep)
    item.ihook.pytest_runtest_logfinish(nodeid=item.nodeid, location=item.location)
    return True
  setattr(item, _ATTEMPT_ATTR, 2)
  setattr(item, _FIRST_ELAPSED_ATTR, getattr(trips[0], _TRIP_ELAPSED_ATTR))
  setattr(item, _ELAPSED_ATTR, 0.0)
  setattr(item, _BUDGET_REPORTED_ATTR, False)
  item.ihook.pytest_runtest_logstart(nodeid=item.nodeid, location=item.location)
  runtestprotocol(item, log=True, nextitem=nextitem)
  _record_rerun_event(item)
  item.ihook.pytest_runtest_logfinish(nodeid=item.nodeid, location=item.location)
  return True


def _record_rerun_event(item: pytest.Item) -> None:
  """Both timings of a finished rerun; a rerun pass has no failure block to carry them."""
  first = getattr(item, _FIRST_ELAPSED_ATTR, 0.0)
  second = getattr(item, _ELAPSED_ATTR, 0.0)
  budget = _budget_seconds(item)
  kind = "integration" if item.get_closest_marker("integration") else "unit"
  verdict = "within budget" if second <= budget else "also over budget"
  item.config.stash.setdefault(_RERUN_EVENTS, []).append((item.nodeid, first, second, budget, kind, verdict))


def pytest_terminal_summary(terminalreporter: pytest.TerminalReporter, exitstatus: int, config: pytest.Config) -> None:
  for nodeid, first, second, budget, kind, verdict in config.stash.get(_RERUN_EVENTS, ()):
    terminalreporter.write_line(
        f"BUDGET RERUN {nodeid}: attempt 1 {first:.2f}s over the {budget:g}s {kind} budget; rerun {second:.2f}s "
        f"{verdict}")


def pytest_collection_finish(session: pytest.Session) -> None:
  marked = sum(1 for item in session.items if item.get_closest_marker("integration"))
  cap = int(session.config.getini(_MAX_INTEGRATION_INI))
  if marked > cap:
    raise pytest.UsageError(
        f"{marked} collected tests carry @pytest.mark.integration; the cap is {cap} "
        f"(max_integration_tests, enforced by tests/conftest.py). The marker is for tests that "
        f"need real processes or real time - prune tests, or de-mark the ones that no longer "
        f"need it.")


# Imports must follow the sys.path bootstrap above.
from src.app import registrations  # noqa: E402

# Every process registers the backend packages before its first config parse; the suite registers
# at import, ahead of every test module's own src imports.
registrations.register_all()

import src.infra.config as core_config  # noqa: E402,I001
from src.runtime import (  # noqa: E402
    master_cc_queue,
    master_cc_run,
    master_cc_state,
    session_anchors,
    session_events,
    session_fork,
    session_lifecycle,
    session_listing,
    session_search,
    session_sidebar,
    session_store,
    session_successor,
    task_execution,
)
from src.runtime import worker as worker_module  # noqa: E402
from src.runtime.agent_process import base as backend_base  # noqa: E402
from src.backends.antigravity.antigravity_cli import AntigravityCliBackend  # noqa: E402
from src.backends.charlie_code.charlie_code import CharlieCodeBackend  # noqa: E402
from src.backends.codex.codex import CodexBackend  # noqa: E402
from src.backends.gemini.gemini_cli import GeminiCliBackend  # noqa: E402
from src.backends.opencode.opencode import OpenCodeBackend  # noqa: E402
from src.runtime.worker import Worker  # noqa: E402
from src.features.cron import loader as cron_loader  # noqa: E402
from src.features.cron.api import router as cron_router  # noqa: E402
from src.runtime.api.deps import get_session_store, get_task_manager  # noqa: E402
from src.runtime.api.internal import router as internal_router  # noqa: E402
from src.app.pages import router as pages_router  # noqa: E402
from src.runtime.api.sessions import router as sessions_router  # noqa: E402
from src.infra import event_types as ET  # noqa: E402
from src.runtime import runs  # noqa: E402
from src.runtime import thinking_state  # noqa: E402
from src.infra import backend_models, models  # noqa: E402
from src.backends.claude_code.claude_config import ClaudeAccount  # noqa: E402
from src.runtime import streaming  # noqa: E402
from src.features.memory.memory import DEFAULT_MEMORY_TOPICS  # noqa: E402
from src.runtime.api.deps import (  # noqa: E402
    get_config_on_loop,
    get_session_anchors,
    get_session_events,
    get_session_fork,
    get_session_lifecycle,
    get_session_listing,
    get_session_search,
    get_session_sidebar,
    get_session_successor,
)
from src.infra.config import CharlieBotConfig, get_config  # noqa: E402
from src.infra.constants import RUN_TOKEN_ENV, SESSION_ID_ENV_VAR  # noqa: E402
from src.backends.claude_code.login_dirs import CREDENTIALS_FILE  # noqa: E402
from src.features.artifacts.plans import PlanRegistryManager  # noqa: E402
from src.features.cron.scheduler import Scheduler  # noqa: E402
from src.runtime.hooks import scheduled_handlers, wiring  # noqa: E402
from src.runtime.session_anchors import SessionAnchors  # noqa: E402
from src.runtime.session_events import SessionEvents  # noqa: E402
from src.runtime.session_fork import SessionFork  # noqa: E402
from src.runtime.session_lifecycle import SessionLifecycle  # noqa: E402
from src.runtime.session_listing import SessionListing  # noqa: E402
from src.runtime.session_search import SessionSearch  # noqa: E402
from src.runtime.session_sidebar import SessionSidebar  # noqa: E402
from src.runtime.session_store import SessionStore  # noqa: E402
from src.runtime.session_successor import SessionSuccessor  # noqa: E402
from src.runtime.run_token import CallerIdentity, RunTokenClaims, sign_run_token  # noqa: E402
from src.runtime.task_execution import TaskExecutionAdapter  # noqa: E402
from src.runtime.task_sessions import TaskTreeManager  # noqa: E402
from src.runtime.triggers import TriggerManager  # noqa: E402

from src.features.artifacts import headless_render  # noqa: E402
from src.app import registrations  # noqa: E402

# Tests that build an app or run the CLI see the registered routers, commands and services
# the way the server does; the registry fills before collection.
registrations.register_all()

# Each session block module with the name of its process singleton.
_BLOCK_SINGLETONS = (
    (session_store, "_store"),
    (session_events, "_events"),
    (session_sidebar, "_sidebar"),
    (session_listing, "_listing"),
    (session_search, "_search"),
    (session_lifecycle, "_lifecycle"),
    (session_fork, "_fork"),
    (session_anchors, "_anchors"),
    (session_successor, "_successor"),
)

# The pytester fixture: the budget mechanism's own test drives inner pytest
# sessions (tests/test_pytest_budget.py).
pytest_plugins = ["pytester"]


def backend_option(**kwargs: Any) -> models.BackendOption:
  """Build a typed backend option from raw kwargs (the config.yaml shape), dispatching on ``type``."""
  return backend_models.parse_option(kwargs)


def fake_backends() -> dict[str, list[models.BackendOption]]:
  """One cc-claude entry so a root created without a backend has a default backend."""
  return {"options": [backend_option(id="fake", label="Fake", type="cc-claude", model="fake-model")]}


@pytest.fixture(autouse=True)
def _stub_headless_renderer(monkeypatch: pytest.MonkeyPatch) -> None:
  """The suite's renderer is the write_stub_chrome shell script (answers --dump-dom only);
  give the drive seam that shape."""

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


def mocked_callback_fields(**overrides: Any) -> dict[str, Any]:
  """The SessionCallbacks fields shared by test bundles; overrides replace a default.

  ``persist_cc_session_id`` resolves to the id it was handed, the read-back-after-persist shape
  the consumer relies on; the consumer's producing-backend keyword rides through and is ignored.
  """
  fields: dict[str, Any] = {
      "mark_unread": AsyncMock(),
      "persist_cc_session_id": AsyncMock(side_effect=lambda sid, ccid, native_backend=None: ccid),
  }
  fields.update(overrides)
  return fields


def mock_session_callbacks() -> models.SessionCallbacks:
  """SessionCallbacks with every field mocked; a test needing one real field constructs its own."""
  return models.SessionCallbacks(
      persist_and_broadcast=AsyncMock(),
      **mocked_callback_fields(),
      persist_account_label=AsyncMock(side_effect=lambda sid, label: label),
      context_state=AsyncMock(return_value=(None, None)),
  )


def manager_backed_callbacks(mgr: SessionBlocks) -> models.SessionCallbacks:
  """SessionCallbacks whose anchor funnels are the real blocks', so anchor persistence is
  observable on disk rather than on a mock's call list; broadcast stays mocked."""
  return models.SessionCallbacks(
      persist_and_broadcast=AsyncMock(),
      **mocked_callback_fields(
          persist_cc_session_id=mgr.anchors.persist_cc_session_id,
          task_tree_activity=mgr.sidebar.task_tree_activity,
      ),
      persist_account_label=mgr.anchors.persist_account_label,
      context_state=AsyncMock(return_value=(None, None)),
  )


def make_work_item(
    cfg: CharlieBotConfig,
    session_meta: models.SessionMetadata,
    backend_option: models.BackendOption | None,
    *,
    user_content: str = "hello",
    callbacks: models.SessionCallbacks | None = None,
    user_event_id: str | None = None,
) -> master_cc_state._WorkItem:
  """Task manager Run item with the field values shared by backend tests."""
  run_id = str(uuid.uuid4())
  transport_dir = _test_transport_dir(cfg, session_meta.id, run_id)
  transport_dir.mkdir(parents=True, exist_ok=True)
  return master_cc_state._WorkItem(
      cfg=cfg,
      session_meta=session_meta,
      user_content=user_content,
      callbacks=callbacks if callbacks is not None else mock_session_callbacks(),
      auto_trigger=False,
      backend_option=backend_option,
      extra_claude_flags=None,
      future=asyncio.get_running_loop().create_future(),
      task_run=master_cc_state.TaskRunBinding(
          session_id=session_meta.id, run_id=run_id, transport_dir=str(transport_dir)),
      user_event_ids=[user_event_id] if user_event_id else [],
      task_instructions="instructions",
      on_task_spawn=_noop_task_spawn,
      on_task_finish=_noop_task_finish,
  )


def _test_transport_dir(cfg: CharlieBotConfig, session_id: str, run_id: str) -> Path:
  sessions_dir = getattr(cfg, "sessions_dir", None)
  root = sessions_dir if isinstance(sessions_dir, Path) else Path(tempfile.gettempdir()) / "charliebot-test-runs"
  return root / session_id / "data" / "runs" / run_id


async def _noop_task_spawn(_pid: int, _pid_start: str | None) -> None:
  return


async def _noop_task_finish(_cc_session_id: str | None, _exit_code: int, _finish_extras: dict) -> None:
  return


async def run_task_manager_message(
    cfg: CharlieBotConfig,
    session_meta: models.SessionMetadata,
    user_content: str,
    callbacks: models.SessionCallbacks,
    *,
    user_event_ids: list[str] | None = None,
    auto_trigger: bool = False,
    backend_option: models.BackendOption | None = None,
    uploaded_files: list[dict] | None = None,
) -> str | None:
  """Queue one task manager Run for consumer tests without launching its TaskTree owner."""
  run_id = str(uuid.uuid4())
  transport_dir = _test_transport_dir(cfg, session_meta.id, run_id)
  transport_dir.mkdir(parents=True, exist_ok=True)
  return await master_cc_queue.run_message(
      cfg,
      session_meta,
      user_content,
      callbacks,
      user_event_ids=list(user_event_ids or []),
      task_instructions="instructions",
      task_run=master_cc_state.TaskRunBinding(
          session_id=session_meta.id, run_id=run_id, transport_dir=str(transport_dir)),
      on_task_spawn=_noop_task_spawn,
      on_task_finish=_noop_task_finish,
      auto_trigger=auto_trigger,
      backend_option=backend_option,
      uploaded_files=uploaded_files,
  )


# One consumer round: the master-cc queue replaces _run_cc with this callable and
# awaits its (cc_session_id, exit_code, error_msg, finish_extras) verdict.
ConsumerRound = Callable[[master_cc_state._WorkItem], Awaitable[tuple[str | None, int, str | None, dict]]]


def make_sound_round(cc_session_id: str) -> ConsumerRound:
  """One consumer round whose CC answers with *cc_session_id* and a clean exit."""

  async def fake_run_cc(item: master_cc_state._WorkItem) -> tuple[str | None, int, str | None, dict]:
    return (cc_session_id, 0, None, {})

  return fake_run_cc


async def _run_seeded_consumer(
    session_id: str,
    work_items: list[master_cc_state._WorkItem],
    fake_run_cc: ConsumerRound,
    manager_patch: Any,
) -> None:
  """Run _session_consumer with fake run execution, a supplied metadata read, and silent broadcasts."""
  master_cc_state._session_queues.pop(session_id, None)
  master_cc_state._session_queues[session_id] = asyncio.Queue()
  for item in work_items:
    master_cc_state._session_queues[session_id].put_nowait(item)
  try:
    with (
        patch.object(master_cc_run, "_run_cc", side_effect=fake_run_cc),
        patch.object(streaming.streaming_manager, "broadcast", new=AsyncMock()),
        manager_patch,
    ):
      await asyncio.wait_for(master_cc_queue._session_consumer(session_id), timeout=5)
  finally:
    master_cc_state._session_queues.pop(session_id, None)
    master_cc_state._session_consumers.pop(session_id, None)


async def run_session_consumer(
    session_id: str,
    work_items: list[master_cc_state._WorkItem],
    fake_run_cc: ConsumerRound,
) -> None:
  """Run _session_consumer with fake run execution, a missing metadata read, and silent broadcasts."""
  workers_mock = MagicMock()
  workers_mock.read_metadata_fresh = AsyncMock(return_value=None)
  await _run_seeded_consumer(
      session_id,
      work_items,
      fake_run_cc,
      patch(SESSION_STORE_ACCESSOR_PATCH_TARGET, return_value=workers_mock),
  )


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

  The re-attach path (enqueue_master_resume) reads fresh metadata, broadcasts
  deltas, and builds a fresh event translator. The session store double
  returns no metadata; the broadcast stub silences the streaming manager.
  resume_cc=None keeps the real _resume_cc over the test's raw log and installs
  the identity _build_fresh_translate instead; a callable replaces _resume_cc.
  """
  if resume_cc is not None:
    monkeypatch.setattr(master_cc_run, "_resume_cc", resume_cc)
  else:
    monkeypatch.setattr(master_cc_run, "_build_fresh_translate", lambda *a, **k: (lambda event: [event]))
  broadcast = AsyncMock()
  monkeypatch.setattr(streaming.streaming_manager, "broadcast", broadcast)
  workers_mock = MagicMock()
  workers_mock.read_metadata_fresh = AsyncMock(return_value=None)
  monkeypatch.setattr(SESSION_STORE_ACCESSOR_PATCH_TARGET, lambda *a, **k: workers_mock)
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
    run_id = str(uuid.uuid4())
    transport_dir = _test_transport_dir(cfg, meta.id, run_id)
    transport_dir.mkdir(parents=True, exist_ok=True)
    future = await master_cc_queue.enqueue_master_resume(
        cfg,
        meta,
        record,
        callbacks,
        is_alive=is_alive,
        task_run=master_cc_state.TaskRunBinding(session_id=meta.id, run_id=run_id, transport_dir=str(transport_dir)),
        on_task_spawn=_noop_task_spawn,
        on_task_finish=_noop_task_finish,
    )
    await asyncio.wait_for(future, timeout=5)
    await drain_session_consumer(meta.id, timeout=5)


def crashed_run_record(raw_log: Path) -> models.MasterRunRecord:
  """MasterRunRecord for the re-attach tests' crashed run: raw log on disk, no process left behind.

  pid=None leaves the record with no liveness probe and no kill path, so a test pairs it with
  ``run_resume_round(..., is_alive=lambda: False)`` and the follower drains the raw file and
  stops instead of waiting out the post-result timeout.
  """
  return models.MasterRunRecord(
      pid=None,
      pid_start=None,
      started_at=datetime.now(UTC) - timedelta(seconds=60),
      raw_log=str(raw_log),
  )


def append_events(path: Path, events: list[dict]) -> None:
  """Append seed chat events as JSONL; append (not truncate) is what lets a test stage history first."""
  path.parent.mkdir(parents=True, exist_ok=True)
  with open(path, "a", encoding="utf-8") as f:
    f.writelines(json.dumps(event) + "\n" for event in events)


def read_chat_events(home: Path, session_id: str) -> list[dict]:
  """Parse a session's chat_events.jsonl under a staged CHARLIEBOT_HOME; [] when absent.

  Each event stays an unmodeled dict so tests assert the exact persisted shape.
  A missing file means the run never wrote events, which callers assert on
  directly rather than treat as an error.
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


def user_tool_result_event() -> dict:
  """The wrapped-format user event carrying one tool_result: the safe point the relay paths fire at."""
  return {"type": ET.USER, "message": {"content": [{"type": ET.TOOL_RESULT, "tool_use_id": "t1", "content": "ok"}]}}


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


def delegate_invocation(**overrides: Any) -> dict:
  """The canonical ``delegate_invocation`` payload: the delegation metadata a TASK_DELEGATED chat
  event carries and the delegate CLI posts in its request body. A test needing different values
  passes the replacement keys as keyword overrides; the gate tests that feed minimal metadata
  build their partial dict by hand."""
  invocation: dict[str, Any] = {
      "task_type": "implement",
      "repo_path": "/tmp/repo",
      "base_branch": "main",
      "task_spec_file": None,
      "reviewer_context_file": None,
      "keep_worktree": False,
      "backend": "codex-o3",
  }
  invocation.update(overrides)
  return invocation


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


def make_json_response(payload: dict[str, Any], status_code: int = 200) -> MagicMock:
  """A transport-response stand-in for patched CLI transport calls: `.status_code` is 200 (or the
  given code) and `.json()` returns payload, so the CLI's success path runs straight through."""
  resp = MagicMock()
  resp.status_code = status_code
  resp.json.return_value = payload
  return resp


def archive_cutoff_events() -> tuple[datetime, list[dict]]:
  """(cutoff, events) where five `e{i}` events predate and three `f{i}` events follow the cutoff."""
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


def backdate_task_created_event(mgr: SessionBlocks, session_id: str, timestamp: datetime) -> None:
  """Place a task fixture's creation fact before the archived event corpus."""
  from src.infra import event_types as ET

  path = mgr.events.get_chat_events_path(session_id)
  events = [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]
  created = next(event for event in events if event.get("type") == ET.TASK_CREATED)
  created["timestamp"] = timestamp.isoformat()
  path.write_text("".join(json.dumps(event) + "\n" for event in events), encoding="utf-8")
  mgr.events.chat_events.clear_cache(session_id)


async def recycle_archive_cutoff_events(mgr: SessionBlocks, session_id: str) -> tuple[datetime, Path]:
  """Seed the task-created fixture and archive_cutoff_events()'s corpus in timestamp order.

  Returns (cutoff, live path). The task creation fact and five e-events end up
  in the weekly archive; the three f-events stay live.
  """
  cutoff, events = archive_cutoff_events()
  backdate_task_created_event(mgr, session_id, cutoff - timedelta(days=1))
  live_path = mgr.events.get_chat_events_path(session_id)
  append_events(live_path, events)
  await mgr.lifecycle.recycle_history_before(session_id, cutoff)
  return cutoff, live_path


async def make_parent(mgr: SessionBlocks, *, name: str = "Parent") -> str:
  """A session ready to elone: the two seed events give succession tests a cut point to reference."""
  parent = await create_root_session(mgr, models.CreateSessionRequest(name=name), backend=OPUS_BACKEND_ID)
  append_events(
      mgr.events.get_chat_events_path(parent.id),
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
  what the tests patch into src.runtime.cli.common.get_config (the readback/diff readers). The sessions
  root itself rides the light seam resolve_session_id reads, so the cwd derivation answers from
  the built tree."""
  cfg = MagicMock()
  cfg.server.port = 9443
  cfg.sessions_dir = tmp_path / "sessions"
  session_dir = cfg.sessions_dir / sid
  session_dir.mkdir(parents=True, exist_ok=True)
  monkeypatch.chdir(session_dir)
  monkeypatch.setattr(CLI_COMMON_SESSIONS_DIR_PATCH_TARGET, lambda: cfg.sessions_dir)
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


def make_home_config(tmp_path: Path) -> CharlieBotConfig:
  """CharlieBotConfig rooted at tmp_path/"charliebot-home". Leaves the home dir un-created:
  most sites never touch disk, and a site that does mkdirs it itself. One Opus backend
  registered so a root created without a backend resolves its default (backends.options[0])."""
  return CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home", backends={"options": [OPUS_BACKEND_OPTION]})


@dataclass
class SessionBlocks:
  """The nine session blocks of one test, all built on one cfg, and the task tree last built over them.

  A test helper: src holds no such bundle, and each holder there takes the blocks it uses.
  ``tree`` is None until ``build_task_tree`` builds one; ``create_root_session`` builds one on demand.
  """
  cfg: Any
  store: SessionStore
  events: SessionEvents
  sidebar: SessionSidebar
  listing: SessionListing
  search: SessionSearch
  lifecycle: SessionLifecycle
  fork: SessionFork
  anchors: SessionAnchors
  successor: SessionSuccessor
  tree: TaskTreeManager | None = None

  def callbacks(self) -> models.SessionCallbacks:
    """The run callbacks over these blocks, as ``master_cc_queue.session_callbacks`` builds them."""
    return master_cc_queue.session_callbacks(self.events, self.lifecycle, self.anchors, self.sidebar)


def build_session_blocks(cfg: Any) -> SessionBlocks:
  """The store, events, sidebar, listing, search, lifecycle, fork, anchors and successor blocks, all built on *cfg*."""
  store = SessionStore(cfg)
  sidebar = SessionSidebar(cfg, store)
  events = SessionEvents(cfg, store)
  return SessionBlocks(
      cfg, store, events, sidebar, SessionListing(cfg, store, sidebar), SessionSearch(cfg, store, events, sidebar),
      SessionLifecycle(cfg, store, events), SessionFork(cfg, store, events), SessionAnchors(cfg, store, events),
      SessionSuccessor(cfg, store, events))


def build_task_tree(cfg: Any, blocks: SessionBlocks) -> TaskTreeManager:
  """A task tree over *blocks*; the blocks remember it as their tree."""
  blocks.tree = TaskTreeManager(
      cfg, blocks.store, blocks.events, blocks.sidebar, blocks.listing, blocks.lifecycle, blocks.fork, blocks.anchors,
      blocks.successor)
  return blocks.tree


def build_execution_adapter(cfg: Any, blocks: SessionBlocks, tree: TaskTreeManager) -> TaskExecutionAdapter:
  """A task execution adapter over *blocks* and *tree*."""
  return TaskExecutionAdapter(
      cfg, blocks.successor, blocks.events, blocks.lifecycle, blocks.anchors, blocks.sidebar, tree)


def thread_blocks(blocks: Any) -> tuple[SessionStore, SessionLifecycle, SessionEvents, SessionSuccessor]:
  """The blocks the chat-thread entry points take, in their argument order: store, lifecycle, events, successor."""
  return blocks.store, blocks.lifecycle, blocks.events, blocks.successor


def build_env(tmp_path: Path) -> tuple[object, SessionBlocks, TaskTreeManager]:
  """(cfg, SessionBlocks, TaskTreeManager) over make_home_config(tmp_path); the tree shares the
  blocks' cfg, so tree-created sessions land in the same home."""
  cfg = make_home_config(tmp_path)
  blocks = build_session_blocks(cfg)
  return cfg, blocks, build_task_tree(cfg, blocks)


def bind_session_blocks(monkeypatch: pytest.MonkeyPatch, blocks: SessionBlocks) -> None:
  """Install *blocks* as the process singletons the block accessors answer with.

  The group rides one patch: a test binding some of the blocks leaves each remaining accessor
  free to build a second block over the same home, whose private chat-event cache never sees
  the first block's rounds.
  """
  monkeypatch.setattr(session_store, "_store", blocks.store)
  monkeypatch.setattr(session_events, "_events", blocks.events)
  monkeypatch.setattr(session_sidebar, "_sidebar", blocks.sidebar)
  monkeypatch.setattr(session_listing, "_listing", blocks.listing)
  monkeypatch.setattr(session_search, "_search", blocks.search)
  monkeypatch.setattr(session_lifecycle, "_lifecycle", blocks.lifecycle)
  monkeypatch.setattr(session_fork, "_fork", blocks.fork)
  monkeypatch.setattr(session_anchors, "_anchors", blocks.anchors)
  monkeypatch.setattr(session_successor, "_successor", blocks.successor)


def bind_deps_blocks(monkeypatch: pytest.MonkeyPatch, tree: TaskTreeManager, blocks: SessionBlocks) -> None:
  """Install *tree* and *blocks* as the process singletons.

  A task tree bound without its blocks leaves each block accessor free to build a second block
  over the same home, so the tree and the blocks go in together.
  """
  monkeypatch.setattr(task_execution, "_task_manager", tree)
  bind_session_blocks(monkeypatch, blocks)


def identity_of(pid: int) -> tuple[int, str]:
  """(pid, start_time) for a live pid; asserts the /proc stat read succeeded, so callers can pin
  a RunRecord to the pair without a None check."""
  pair = runs.read_pid_stat(pid)
  assert pair is not None
  return pid, pair[0]


OPERATOR = CallerIdentity(kind="operator")


async def create_task(
    tree: TaskTreeManager,
    *,
    parent: str | None,
    request_id: str,
    profile: str = "manager",
    task: models.TaskSpec | None = None,
    name: str | None = None):
  """One operator-created task node; the default shape task-tree tests build their trees with.

  The caller is the verified operator CallerIdentity: create_task skips agent
  authorization for it, and the task_created fact records the user actor
  (_create_actor_for) — the label the production operator path records too.
  """
  return await tree.create_task(
      request_id=request_id,
      task_parent_id=parent,
      profile=profile,
      task=task,
      name=name,
      backend=None,
      caller=OPERATOR)


async def create_root_session(
    mgr: SessionBlocks, req: models.CreateSessionRequest, backend: str | None = None) -> models.SessionMetadata:
  """One operator-created manager root, created through the task tree wired over *mgr*.

  The tree is the one last built over *mgr*, or a new one when none has been.
  """
  tree = mgr.tree or build_task_tree(mgr.cfg, mgr)
  return await tree.create_task(
      request_id=f"session-create:{req.session_id or uuid.uuid4()}",
      task_parent_id=None,
      profile="manager",
      task=None,
      name=req.name,
      backend=backend,
      group=req.group,
      session_id=req.session_id,
      slot_values=dict(req.model_extra or {}),
      caller=OPERATOR)


async def create_scheduled_node(tree: TaskTreeManager, *, name: str, backend: str | None):
  """The manager node one ScheduledTaskConfig binds to, created the auto-bind way.

  request_id rides the scheduler's auto-bind prefix over the task name alone
  (``_AUTO_BIND_REQUEST_PREFIX`` in src/features/cron/scheduler.py: a re-created task
  replays into the same node), and the caller is the scheduler's ``"system"``
  sentinel — actor ``system`` on the task_created fact, no agent authorization.
  """
  return await tree.create_task(
      request_id=f"scheduled-node:{name}",
      task_parent_id=None,
      profile="manager",
      task=None,
      name=name,
      backend=backend,
      caller="system")


def live_subprocess() -> subprocess.Popen:
  """An owned, isolated sleeper: the only process identity any test here signals."""
  return subprocess.Popen(["/bin/sleep", "30"])


def run_git(cwd: Path | str, *args: str, check: bool = True, env: dict[str, str] | None = None) -> str:
  """Run one git command in *cwd* and return its stripped stdout.

  check=True fails the test with the exact command and git's stderr;
  check=False returns an expected failure's output for the caller to judge.
  """
  result = subprocess.run(["git", *args], cwd=str(cwd), env=env, capture_output=True, text=True, check=False)
  if check and result.returncode != 0:
    raise AssertionError(f"git {' '.join(args)} failed in {cwd}: {result.stderr.strip()}")
  return result.stdout.strip()


def init_repo_with_origin(tmp_path: Path) -> tuple[Path, Path]:
  """A synthetic repo with a bare origin carrying main (the landing target)."""
  origin = tmp_path / "origin.git"
  subprocess.run(["git", "init", "-q", "--bare", "-b", "main", str(origin)], check=True)
  repo = tmp_path / "repo"
  subprocess.run(["git", "clone", "-q", str(origin), str(repo)], check=True)
  run_git(repo, "config", "user.email", "t@example.com")
  run_git(repo, "config", "user.name", "t")
  (repo / "seed.txt").write_text("seed\n")
  run_git(repo, "add", ".")
  run_git(repo, "commit", "-q", "-m", "seed")
  run_git(repo, "push", "-q", "origin", "main")
  return repo, origin


@pytest.fixture()
def repo(tmp_path: Path) -> Path:
  """One synthetic git repo for a delegate or landing target test."""
  r, _origin = init_repo_with_origin(tmp_path / "authz-repo")
  return r


def delegate_payload(session_id: str, repo: Path, *, task_type: str = "quick-edit") -> dict:
  """The /api/internal/delegate request body one test delegation sends."""
  return {
      "session_id": session_id,
      "description": "## Goal\n\nfix the thing\n",
      "task_type": task_type,
      "keep_worktree": False,
      "repo_path": None if task_type == "verify" else str(repo),
      "base_branch": None if task_type == "verify" else "main",
  }


def build_master_cc_cfg(tmp_path: Path) -> CharlieBotConfig:
  """CharlieBotConfig rooted at tmp_path/".charliebot" with one fake codex backend registered: the
  shape task-tree manager-turn tests drive through the queue and backend runner."""
  return CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backends={"options": [backend_option(id="fake", label="Fake", type="codex", model="fake-model")]},
  )


def make_session_blocks(tmp_path: Path) -> SessionBlocks:
  """Session blocks over a SimpleNamespace cfg whose sessions_dir is tmp_path/"sessions"; a test
  needing a richer cfg builds its own."""
  cfg = SimpleNamespace(sessions_dir=tmp_path / "sessions")
  cfg.sessions_dir.mkdir()
  return build_session_blocks(cfg)


async def make_home_session(
    tmp_path: Path,
    *,
    name: str,
    backend: str | None = None) -> tuple[CharlieBotConfig, SessionBlocks, models.SessionMetadata]:
  """(cfg, SessionBlocks, one created session) over a CharlieBotConfig rooted at tmp_path/"home";
  backend=None takes the default (the first registered backend). A test needing more
  sessions calls create_root_session directly; a test needing no session builds the cfg/mgr pair
  inline."""
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", backends={"options": [OPUS_BACKEND_OPTION]})
  mgr = build_session_blocks(cfg)
  session = await create_root_session(mgr, models.CreateSessionRequest(name=name), backend=backend)
  return cfg, mgr, session


def apply_config_overrides(app: FastAPI, cfg: CharlieBotConfig) -> None:
  """Bind both config dependency keys to the one instance on *app*.

  A mounted route resolves cfg through either dependency (get_config_on_loop
  on the polled routes, get_config on the sync ones), so a rig must carry the
  override on both keys — one key alone leaves routes on the other resolving
  the real config.
  """
  app.dependency_overrides[get_config] = lambda: cfg
  app.dependency_overrides[get_config_on_loop] = lambda: cfg


def override_session_blocks(app: FastAPI, session_blocks: Any) -> None:
  """Bind the session dependency keys on *app* to the blocks of *session_blocks*."""
  app.dependency_overrides[get_session_store] = lambda: session_blocks.store
  app.dependency_overrides[get_session_events] = lambda: session_blocks.events
  app.dependency_overrides[get_session_sidebar] = lambda: session_blocks.sidebar
  app.dependency_overrides[get_session_listing] = lambda: session_blocks.listing
  app.dependency_overrides[get_session_search] = lambda: session_blocks.search
  app.dependency_overrides[get_session_lifecycle] = lambda: session_blocks.lifecycle
  app.dependency_overrides[get_session_fork] = lambda: session_blocks.fork
  app.dependency_overrides[get_session_anchors] = lambda: session_blocks.anchors
  app.dependency_overrides[get_session_successor] = lambda: session_blocks.successor


def make_router_client(
    cfg: CharlieBotConfig,
    session_blocks: SessionBlocks,
    router: APIRouter,
    prefix: str,
) -> TestClient:
  """TestClient mounting one router with cfg/session_blocks as dependency overrides; a test needing
  extra routers or overrides builds its own FastAPI app."""
  app = FastAPI()
  app.include_router(router, prefix=prefix)
  apply_config_overrides(app, cfg)
  override_session_blocks(app, session_blocks)
  return TestClient(app)


def include_registered_routers(app: FastAPI, prefix: str, *, before_runtime: bool = False) -> None:
  """Include the routers that packages registered under *prefix*, the way server.py includes them.

  *before_runtime* picks the routers registered to answer ahead of the runtime's own: a rig that
  mounts a runtime router includes those first.
  """
  for module, router_prefix, tags, attr in wiring.routers(before_runtime=before_runtime):
    if router_prefix == prefix:
      app.include_router(getattr(importlib.import_module(module), attr), prefix=router_prefix, tags=list(tags))


def make_sessions_client(cfg: CharlieBotConfig, session_blocks: SessionBlocks) -> TestClient:
  """make_router_client over the sessions router, mounted at /api/sessions."""
  return make_router_client(cfg, session_blocks, sessions_router, "/api/sessions")


def make_internal_router_client(cfg: Any, session_blocks: Any, task_mgr: Any | None = None) -> TestClient:
  """make_router_client over the internal router, mounted at /api/internal, plus the feature routers
  registered under that prefix; the internal routes take cfg through the on-loop dependency
  (same instance the sync key serves), so the override keys in make_router_client cover them.
  cfg may be a MagicMock when the tested route never reads it."""
  client = make_router_client(cfg, session_blocks, internal_router, "/api/internal")
  if task_mgr is not None:
    client.app.dependency_overrides[get_task_manager] = lambda: task_mgr
  include_registered_routers(client.app, "/api/internal")
  return client


def make_cron_client(cfg: CharlieBotConfig, session_blocks: SessionBlocks) -> TestClient:
  """make_router_client over the cron router, mounted at /api/cron."""
  return make_router_client(cfg, session_blocks, cron_router, "/api/cron")


def make_cron_sessions_client(
    cfg: CharlieBotConfig, session_blocks: SessionBlocks, tree: TaskTreeManager) -> TestClient:
  """TestClient mounting the cron router plus the sessions router (the scheduled listing and
  unarchive endpoints) with cfg/session_blocks/tree as dependency overrides."""
  app = FastAPI()
  app.include_router(cron_router, prefix="/api/cron")
  app.include_router(sessions_router, prefix="/api/sessions")
  apply_config_overrides(app, cfg)
  override_session_blocks(app, session_blocks)
  app.dependency_overrides[get_task_manager] = lambda: tree
  return TestClient(app)


def make_sessions_listing_client(
    cfg: CharlieBotConfig, session_blocks: SessionBlocks, tree: TaskTreeManager) -> TestClient:
  """TestClient mounting the sessions router with its config and manager overrides."""
  app = FastAPI()
  include_registered_routers(app, "/api/sessions", before_runtime=True)
  app.include_router(sessions_router, prefix="/api/sessions")
  apply_config_overrides(app, cfg)
  override_session_blocks(app, session_blocks)
  app.dependency_overrides[get_task_manager] = lambda: tree
  return TestClient(app)


def make_sessions_listing_page_client(
    cfg: CharlieBotConfig, session_blocks: SessionBlocks, tree: TaskTreeManager) -> TestClient:
  """TestClient mounting the pages router (the homepage's server-rendered sidebar) with the
  same overrides make_sessions_listing_client carries."""
  app = FastAPI()
  app.include_router(pages_router)
  apply_config_overrides(app, cfg)
  override_session_blocks(app, session_blocks)
  app.dependency_overrides[get_task_manager] = lambda: tree
  return TestClient(app)


def page_initial_sessions(page_client: TestClient, session_id: str) -> list[dict]:
  """The homepage render's INITIAL_SESSIONS row list for *session_id*, parsed.

  The sidebar-list tests read the server-rendered page, not a JSON endpoint:
  the rows ride the page as ``const INITIAL_SESSIONS = [...]``.
  """
  resp = page_client.get("/", params={"session": session_id})
  assert resp.status_code == 200
  match = re.search(r"const INITIAL_SESSIONS = (\[.*?\]);", resp.text)
  assert match is not None
  return json.loads(match.group(1))


def walk_archived_pages(client: TestClient, limit: int = 2) -> list[dict]:
  """GET /api/sessions/archived page by page, threading the keyset cursor.

  Returns every page's JSON body in walk order. The limit stays pinned, and
  each page's ``next_before``/``next_before_id`` ride the next request only
  while that page says ``has_more`` — the walk stops at the first exhausted
  page. The 10-page bound keeps a runaway cursor a bounded failure: the last
  returned page then still says ``has_more``, and the callers' exactly-once
  and membership asserts catch the short walk.
  """
  pages: list[dict] = []
  before = None
  before_id = None
  for _ in range(10):
    params: dict = {"limit": limit}
    if before is not None:
      params.update({"before": before, "before_id": before_id})
    resp = client.get("/api/sessions/archived", params=params)
    assert resp.status_code == 200
    page = resp.json()
    pages.append(page)
    if not page["has_more"]:
      break
    before, before_id = page["next_before"], page["next_before_id"]
  return pages


def make_http_scope(url: str, *, headers: list[tuple[bytes, bytes]]) -> dict[str, Any]:
  """HTTP ASGI scope for the tests that drive an app directly, no server boot.

  *url* splits into ``path``, ``raw_path`` (the full URL, uvicorn's wire shape),
  and ``query_string``; *headers* rides verbatim as the scope's header list.
  """
  path, _, qs = url.partition("?")
  return {
      "type": "http",
      "asgi": {
          "version": "3.0",
          "spec_version": "2.3"
      },
      "http_version": "1.1",
      "method": "GET",
      "scheme": "http",
      "path": path,
      "raw_path": url.encode(),
      "query_string": qs.encode(),
      "root_path": "",
      "headers": headers,
      "client": ("t", 1),
      "server": ("t", 80),
  }


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


def mount_production_gzip(app: FastAPI) -> None:
  """Mount the stock gzip middleware with the server's production numbers.

  The precompressed-response tests' served-as-is and zero-deflate assertions pin production
  behavior only while this pair matches the server's gzip mount — minimum_size=1000,
  compresslevel=1 on ``_CharlieBotGZipMiddleware`` (server.py). The stock middleware is
  deliberate: the subclass changes which responder deflates, not whether an already-compressed
  body deflates, so the skip contract under test is the same.
  """
  app.add_middleware(GZipMiddleware, minimum_size=1000, compresslevel=1)


def assert_gzip_served(resp: Any) -> None:
  """Assert the response served a route's pre-compressed gzip form.

  The route set the encoding upstream — that header is what makes the
  middleware skip its own deflate — and carries the negotiation vary.
  """
  assert resp.headers["content-encoding"] == "gzip"
  assert resp.headers["vary"] == "Accept-Encoding"


def make_transcript(config_dir: Path, cc_session_id: str) -> Path:
  """Write a fake Claude Code session transcript under config_dir and return its path."""
  transcript = config_dir / "projects" / "slug" / f"{cc_session_id}.jsonl"
  transcript.parent.mkdir(parents=True, exist_ok=True)
  transcript.write_text("[]", encoding="utf-8")
  return transcript


def seed_transcript_copy(config_dir: Path, cc_session_id: str, body: str, *, mtime_ns: int) -> Path:
  """One transcript copy with an explicit mtime, so newness never rides on timing."""
  path = make_transcript(config_dir, cc_session_id)
  path.write_text(body, encoding="utf-8")
  os.utime(path, ns=(mtime_ns, mtime_ns))
  return path


def write_pool_credentials(config_dir: Path, access_token: str = "token") -> None:
  """Write Claude OAuth credentials into a pool account's config dir, marking the login present."""
  config_dir.mkdir(parents=True, exist_ok=True)
  (config_dir / CREDENTIALS_FILE).write_text(
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
    labels: tuple[str, ...],
    claude_pools: dict[str, list[str]] | None = None,
) -> CharlieBotConfig:
  """A pooled CharlieBotConfig: one ClaudeAccount per label, pool credentials planted in each config dir.

  ``claude_pools`` splits the labels into named pools; the caller's cc-claude options then name
  their pool in ``account_pool`` (the config validator refuses an unpaired combination).
  """
  accounts = [ClaudeAccount(label=label, config_dir=str(tmp_path / f"claude-{label}")) for label in labels]
  for account in accounts:
    write_pool_credentials(Path(account.config_dir))
  return CharlieBotConfig(
      charliebot_home=home,
      paths={"worktree_dir": str(worktree_dir)},
      accounts={
          "claude": accounts,
          "claude_pools": claude_pools or {}
      },
      backends={"options": backend_options},
  )


# Resolved model and option id the account-pool suites pin for the pooled Fable backend.
# One home so a rename stays a one-line change across the master-turn, worker, and ledger
# suites; wire-payload assertions and yaml text keep the raw strings (same rule as
# OPUS_BACKEND_ID above).
FABLE_MODEL = "claude-fable-5-1"
POOLED_FABLE_ID = "claude-fable-5"


def fable_pool_cfg(
    tmp_path: Path,
    labels: tuple[str, ...] = ("main", "ext-1", "ext-2"),
    claude_pools: dict[str, list[str]] | None = None,
) -> CharlieBotConfig:
  """A pooled config with one pooled Fable option and a second pinned Fable entry.

  ``claude_pools`` splits *labels* into named pools; both Fable options then draw from the
  first pool (their ``account_pool``), as the config validator requires.
  """
  extra = {} if not claude_pools else {"account_pool": next(iter(claude_pools))}
  return pool_cfg(
      tmp_path,
      [
          backend_option(id=POOLED_FABLE_ID, label="Fable", type="cc-claude", model=FABLE_MODEL, **extra),
          backend_option(id="pinned", label="Pinned", type="cc-claude", model=FABLE_MODEL, **extra),
      ],
      home=tmp_path / ".charliebot",
      worktree_dir=tmp_path / "worktrees",
      labels=labels,
      claude_pools=claude_pools,
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

# Prompt payload beginning with "--", which a naive argv builder would misread as a CLI flag;
# each backend's build-command test asserts the string reaches the CLI as prompt payload only.
FLAG_LIKE_PROMPT = "--malicious-flag ignore previous"

# Import-path patch target shared by every test that silences or spies on streaming broadcasts.
# Mock resolves the route through the src.runtime.session_events namespace (src/runtime/session_events.py
# imports the streaming_manager singleton) and setattr's broadcast on that shared object; a move of the
# events-side import updates this one string. src.runtime.autonamer and src.runtime.worker import
# the same singleton, so their routes reach the same attribute.
BROADCAST_PATCH_TARGET = "src.runtime.session_events.streaming_manager.broadcast"

# Import-path patch target for consumer tests that stub fresh metadata reads.
# master_cc_queue reads the process store through session_store.store() at call time.
SESSION_STORE_ACCESSOR_PATCH_TARGET = "src.runtime.session_store.store"

# The trigger watcher tests stop at the task-tree admission seam. The tree
# delivery itself is covered by tests/test_trigger_succession.py.
TRIGGER_TASK_DELIVERY_PATCH_TARGET = "src.runtime.triggers.TriggerManager._fire_task_tree"

# Import-path patch target for the subprocess spawn the trigger watchers probe through.
# src/runtime/triggers.py binds the library with module-scope `import asyncio`, and its
# watch paths read asyncio.create_subprocess_exec at call time, so mock lands the
# stand-in on the asyncio module through the src.runtime.triggers route.
TRIGGERS_ASYNCIO_CREATE_SUBPROCESS_EXEC_PATCH_TARGET = "src.runtime.triggers.asyncio.create_subprocess_exec"

# Import-path patch target for the host capability probe the slurm watchers read.
# src/runtime/triggers.py runs the probe once at import scope (`_SACCT_AVAILABLE =
# shutil.which("sacct") is not None`), and create_trigger's local-slurm guard and
# _wait_sacct_group's no-sacct skip read the module attribute at call time, so mock
# patch setattrs the stand-in on the src.runtime.triggers module attribute.
TRIGGERS_SACCT_AVAILABLE_PATCH_TARGET = "src.runtime.triggers._SACCT_AVAILABLE"

# Import-path patch target for the CLI HTTP layer's config read. src/runtime/cli/common.py defines a
# get_config forwarder (config's module imports lazily on first call, the M92 floor rule), so
# mock setattrs the stand-in on the src.runtime.cli.common module attribute and every helper defined
# there reads it as a module global at call time. The request path itself reads only the
# server port through the fingerprint-keyed document (_internal_base_url), so the transport
# harness patches the base-url seam below and this target covers the remaining direct readers
# (the sent-but-lost readback, plan diff's version files).
CLI_COMMON_GET_CONFIG_PATCH_TARGET = "src.runtime.cli.common.get_config"

# Import-path patch target for the config read itself. Verbs that defer the import into the
# call (`from src.infra.config import get_config` at call scope) read the src.infra.config
# module attribute at call time, so the stand-in lands there. src.runtime.cli.common's forwarder
# (CLI_COMMON_GET_CONFIG_PATCH_TARGET above) defer-imports the same attribute per call, so a
# stand-in set here flows through its callers too; the reverse does not hold — a stand-in on
# common's module global leaves this attribute real for every deferred import.
CONFIG_GET_CONFIG_PATCH_TARGET = "src.infra.config.get_config"

# Import-path patch target for the CLI request path's server base URL. src/runtime/cli/common.py
# resolves it through the fingerprint-keyed port document (_internal_base_url; a hit keeps
# config's model stack out of the verb process), so mock setattrs the stand-in on the
# src.runtime.cli.common module attribute and _request_with_contract reads it as a module global.
CLI_COMMON_BASE_URL_PATCH_TARGET = "src.runtime.cli.common._internal_base_url"

# Import-path patch target for the CLI's sessions root. src/runtime/cli/common.py derives it from the
# env-resolved home (_sessions_dir, the M102 wrap-verb light path), so mock setattrs the
# stand-in on the src.runtime.cli.common module attribute and resolve_session_id reads it as a module
# global at call time.
CLI_COMMON_SESSIONS_DIR_PATCH_TARGET = "src.runtime.cli.common._sessions_dir"

# Import-path patch target for the version-skew hint the CLI error paths append. src/runtime/cli/common.py
# defines _maybe_version_skew_hint and _exit_server_rejection reads it as a module global at call
# time, so mock setattrs the stand-in on the src.runtime.cli.common module attribute.
CLI_COMMON_MAYBE_VERSION_SKEW_HINT_PATCH_TARGET = "src.runtime.cli.common._maybe_version_skew_hint"

# Import-path patch target for the CLI connect-retry budget. src/runtime/cli/common.py binds the name
# at import scope (`from src.infra.timeouts import CLI_CONNECT_TOTAL_TIMEOUT`), so mock setattrs
# the test budget on the src.runtime.cli.common module attribute and post_internal_api's retry loop
# reads it as a module global at call time; the source value stays the timeouts module's own.
CLI_COMMON_CONNECT_TOTAL_TIMEOUT_PATCH_TARGET = "src.runtime.cli.common.CLI_CONNECT_TOTAL_TIMEOUT"

# Import-path patch target for the logged-task spawner the shared thread core
# (src/features/chat_threads/thread_entry.py) fires: the round side's ack-clear and nudge tasks
# are created there once the round side moves into the core, so mock setattrs
# the stand-in on the src.features.chat_threads.thread_entry module attribute.
THREAD_ENTRY_CREATE_LOGGED_TASK_PATCH_TARGET = "src.features.chat_threads.thread_entry.create_logged_task"

# Import-path patch target for the master wake the shared thread core's round-end
# audit fires (the nudge): once the audit moves into src/features/chat_threads/thread_entry.py, mock
# setattrs the stand-in on the src.features.chat_threads.thread_entry module attribute.
THREAD_ENTRY_TRIGGER_MASTER_PATCH_TARGET = "src.features.chat_threads.thread_entry.trigger_master"

# Import-path patch targets for the platform client factories every listener outbound path
# posts through. src/features/slack/slack_listener.py and src/features/discord/discord_listener.py each define
# _bot_client at module scope, and their handlers and reply/backfill helpers resolve the
# name at call time, so mock setattrs each stand-in on that listener module's own attribute.
SLACK_LISTENER_BOT_CLIENT_PATCH_TARGET = "src.features.slack.slack_listener._bot_client"
DISCORD_LISTENER_BOT_CLIENT_PATCH_TARGET = "src.features.discord.discord_listener._bot_client"

# Import-path patch targets for the scheduler's config reads. src/features/cron/scheduler.py binds
# both names at import scope (`from src.infra.config import load_config`,
# `from src.features.cron.loader import get_scheduled_tasks`), so monkeypatch.setattr lands the
# stand-in on the src.features.cron.scheduler module attribute and
# _maybe_run/_reload_config resolve it at call time.
SCHEDULER_LOAD_CONFIG_PATCH_TARGET = "src.features.cron.scheduler.load_config"
SCHEDULER_GET_SCHEDULED_TASKS_PATCH_TARGET = "src.features.cron.scheduler.get_scheduled_tasks"


@contextlib.contextmanager
def registered_cron_handler(name: str, handler: Callable[[], Awaitable[str]]) -> Iterator[None]:
  """Register ``handler`` as cron handler ``name`` through the scheduled-handler hook for the body.

  The hook resolves a module path and an attribute name, so the handler rides an attribute of this
  module. The registry and the attribute return to their prior state on exit.
  """
  attr = f"_cron_handler_{name}"
  with patch.dict(scheduled_handlers._HANDLERS), patch.object(sys.modules[__name__], attr, handler, create=True):
    scheduled_handlers.register_handler(name, __name__, attr=attr)
    yield


# Import-path patch targets for the chat API's message bootstrap and cancel route.
# src/runtime/api/chat.py defines run_and_finalize itself and binds create_logged_task
# (`from src.infra.tasks import create_logged_task`) at import scope; cancel_master
# binds lazily (PEP 562 __getattr__ + _load_cancel_master's globals-first loader,
# the M99 server import floor), and the module attribute stays the seam either way —
# mock and monkeypatch.setattr land the stand-ins on the src.runtime.api.chat module
# attributes and send_message's fire-and-forget bootstrap, run_and_finalize's
# auto-name task, and cancel_master_agent read them at call time.
# src/runtime/api/sessions.py re-imports run_and_finalize at call time, so both reach the
# same src.runtime.api.chat namespace attributes; src.infra.tasks.create_logged_task stays
# a separate route.
CHAT_RUN_AND_FINALIZE_PATCH_TARGET = "src.runtime.api.chat.run_and_finalize"
CHAT_CREATE_LOGGED_TASK_PATCH_TARGET = "src.runtime.api.chat.create_logged_task"
CHAT_CANCEL_MASTER_PATCH_TARGET = "src.runtime.api.chat.cancel_master"

# Import-path patch targets for the CLI HTTP layer's transport. src/runtime/cli/common.py exposes one
# adapter per verb (`_request_post`/`_request_get`, both over the phase-separated client
# `_send_request`), and `_request_with_contract` reads the adapter as a module global at
# call time, so mock and monkeypatch.setattr land the stand-in on the src.runtime.cli.common module
# attribute and every helper defined there picks it up at call time.
CLI_COMMON_TRANSPORT_POST_PATCH_TARGET = "src.runtime.cli.common._request_post"
CLI_COMMON_TRANSPORT_GET_PATCH_TARGET = "src.runtime.cli.common._request_get"

# Import-path patch target for the memory CLI's home read. src/features/memory/cli.py binds the
# name at import scope (`from src.infra.home import charliebot_home_dir`), so mock and
# monkeypatch.setattr land the stand-in on the src.features.memory.cli module attribute and the
# CLI's entry points read it at call time.
CLI_MEMORY_HOME_PATCH_TARGET = "src.features.memory.cli.charliebot_home_dir"

# Import-path patch target shared by every test that swaps the backend factory a master session
# runs under. src/runtime/master_cc_run.py reads `backend_types.build_backend` as a module
# attribute at every build, so monkeypatch.setattr on the backend type table's module attribute
# lands the stand-in where that read resolves. The lazy carriers (worker.py, autonamer.py —
# each deferring through the shared load_build_backend in
# src/runtime/agent_process/deferred_build.py, which returns an existing module binding
# untouched) resolve the same function at first build, so a patch applied before that first
# build reaches them too; a patch applied after binds their module attribute directly.
BUILD_BACKEND_PATCH_TARGET = "src.runtime.hooks.backend_types.build_backend"
# The worker builds through the shared lazy loader it binds at import scope
# (src/runtime/agent_process/deferred_build.py load_build_backend),
# so the worker path's stand-in binds here — an existing binding is returned untouched,
# exactly the semantics the master-cc type-table route relies on.
WORKER_BUILD_BACKEND_PATCH_TARGET = "src.runtime.worker.build_backend"

# Import-path patch target for the worker's binary-free translate fallback. src/runtime/worker.py
# calls `backend_types.build_translate_fallback` from _build_backend's fallback return — reached
# when no backend_option is set or a translate-only build fails — as a module attribute at call
# time, so tests that drive that fallback set the stand-in on the type table's module attribute.
WORKER_TRANSLATE_FALLBACK_PATCH_TARGET = "src.runtime.hooks.backend_types.build_translate_fallback"

# Import-path patch target for the /proc stat read the backend start contract pins. src/runtime/runs.py
# defines read_pid_stat; src/runtime/agent_process/base.py binds the module (`from src.runtime import runs`)
# and reads runs.read_pid_stat at call time, so monkeypatch.setattr lands the stand-in on the
# src.runtime.runs module attribute where that read resolves.
RUNS_READ_PID_STAT_PATCH_TARGET = "src.runtime.runs.read_pid_stat"

# Patch targets for request_stop's exit-wait loop. runs.py defines both timings as module
# globals and the stop path reads them at call time, so monkeypatch.setattr lands the
# shortened waits on the src.runtime.runs module attribute; the pair travels together because
# the loop's deadline and its poll step share one mechanism.
RUNS_STOP_EXIT_WAIT_SECONDS_PATCH_TARGET = "src.runtime.runs.STOP_EXIT_WAIT_SECONDS"

# The backend-construction seams, stated once for every constant below: the CLI
# backends all read resolve_binary through the base module at call time
# (`base.resolve_binary`, module-style import), so every
# ``*_RESOLVE_BINARY_PATCH_TARGET`` is the shared base attribute, where
# monkeypatch.setattr lands the stand-in and the __init__ reads the helper at
# call time and never probes PATH. The backend start contract spawns through the
# off-loop spawn seam (src/runtime/agent_process/spawn.py), which base.py imports and reads
# as a module global at call time, so both ``*_SPAWN_SUBPROCESS_PATCH_TARGET`` spellings
# land the stand-in on that one shared attribute; the caller-qualified form records
# which backend's spawn a test drives.
BASE_SPAWN_SUBPROCESS_PATCH_TARGET = "src.runtime.agent_process.base.spawn_subprocess"
OPENCODE_RESOLVE_BINARY_PATCH_TARGET = "src.runtime.agent_process.base.resolve_binary"
OPENCODE_SPAWN_SUBPROCESS_PATCH_TARGET = "src.runtime.agent_process.base.spawn_subprocess"
CODEX_RESOLVE_BINARY_PATCH_TARGET = "src.runtime.agent_process.base.resolve_binary"
ANTIGRAVITY_RESOLVE_BINARY_PATCH_TARGET = "src.runtime.agent_process.base.resolve_binary"
CHARLIE_CODE_RESOLVE_BINARY_PATCH_TARGET = "src.runtime.agent_process.base.resolve_binary"
GEMINI_RESOLVE_BINARY_PATCH_TARGET = "src.runtime.agent_process.base.resolve_binary"

# Import-path patch target for the improve loop's commit step. src/features/backlog/backlog_loop.py binds
# the git module at import scope (`from src.infra import git`) and its stale-item handler reads
# the commit function off that binding at call time, so mock setattrs the stand-in on the
# src.features.backlog.backlog_loop.git module attribute -- the shared src.infra.git module object, restored
# by monkeypatch after the test.
BACKLOG_LOOP_GIT_ADD_COMMIT_PUSH_PATCH_TARGET = "src.features.backlog.backlog_loop.git.git_add_commit_push"

# Patch target for the atomic-write swap hook. src/infra/json_utils.py publishes each staged
# payload with an ``os.replace`` attribute lookup on its module-scope ``import os`` binding, and
# atomic_write_stream's docstring pins that lookup as the test hook, so mock setattrs the
# stand-in on the shared os module through this route for the patch window and the write side's
# call-time read picks it up.
JSON_UTILS_OS_REPLACE_PATCH_TARGET = "src.infra.json_utils.os.replace"

# Patch target for the ssh control-master directory. src/infra/ssh.py derives it from the
# operator's home at import scope and the argv builder reads the module global at call time,
# so monkeypatch.setattr redirects the dir onto the src.infra.ssh module attribute and keeps
# the suite off the real ~/.ssh/controlmasters.
SSH_CONTROL_DIR_PATCH_TARGET = "src.infra.ssh._CONTROL_DIR"


def build_cli_backend(
    monkeypatch: pytest.MonkeyPatch,
    backend_cls: type[backend_base.AgentBackend],
    resolve_patch_target: str,
    fake_binary: str,
    defaults: dict[str, Any],
    **kwargs: Any,
) -> backend_base.AgentBackend:
  """Construct a CLI backend with its resolve_binary pinned to *fake_binary*.

  The one home of the backend-test construction contract: the patch lands the stand-in
  on the module named by *resolve_patch_target* (the shared note above the
  ``*_RESOLVE_BINARY_PATCH_TARGET`` constants states that binding scope), so ``__init__``
  never probes PATH, and *defaults* fill kwargs the caller left out.
  """
  monkeypatch.setattr(resolve_patch_target, lambda name, fallback: fake_binary)
  for key, value in defaults.items():
    kwargs.setdefault(key, value)
  return backend_cls(**kwargs)


# One row per CLI backend: the resolve_binary patch target, the fake binary
# build_cli_backend pins on it, and the constructor defaults a plain test build
# relies on. The per-backend test modules build through build_cli_backend_rig and
# the cross-backend contract tables read these rows, so the (target, binary,
# defaults) triple is spelled exactly once per backend.
CLI_BACKEND_RIGS: dict[type[backend_base.AgentBackend], tuple[str, str, dict[str, Any]]] = {
    AntigravityCliBackend: (ANTIGRAVITY_RESOLVE_BINARY_PATCH_TARGET, "/usr/bin/agy", {}),
    CharlieCodeBackend:
        (
            CHARLIE_CODE_RESOLVE_BINARY_PATCH_TARGET,
            "/usr/bin/charlie-code",
            {
                "model": "charlie-code-test-model",
                "api_base": "http://test.invalid/v1"
            },
        ),
    CodexBackend: (CODEX_RESOLVE_BINARY_PATCH_TARGET, "/usr/bin/codex", {
        "model": "codex-test-model"
    }),
    GeminiCliBackend: (GEMINI_RESOLVE_BINARY_PATCH_TARGET, "/usr/bin/gemini", {
        "model": "gemini-test-model"
    }),
    OpenCodeBackend: (OPENCODE_RESOLVE_BINARY_PATCH_TARGET, "/usr/bin/opencode", {}),
}


def build_cli_backend_rig(
    monkeypatch: pytest.MonkeyPatch,
    backend_cls: type[backend_base.AgentBackend],
    **kwargs: Any,
) -> backend_base.AgentBackend:
  """Construct *backend_cls* through its CLI_BACKEND_RIGS row.

  A caller relies on the row's fake binary being pinned and the row's defaults
  filling kwargs it leaves out; *kwargs* override the defaults per test.
  """
  patch_target, fake_binary, defaults = CLI_BACKEND_RIGS[backend_cls]
  return build_cli_backend(monkeypatch, backend_cls, patch_target, fake_binary, defaults=defaults, **kwargs)


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


def write_stub_chrome(tmp_path: Path, height: int) -> str:
  """Write a fake headless-chrome binary printing a wrapper-shaped DOM with the chosen measured height."""
  stub = tmp_path / f"stub-chrome-{height}.sh"
  stub.write_text(f"#!/bin/sh\necho 'probe output <pre id=\"page-height\">{height}</pre>'\n", encoding="utf-8")
  stub.chmod(0o755)
  return str(stub)


def build_plan_cfg(tmp_path: Path) -> CharlieBotConfig:
  """CharlieBotConfig for plan tests: the 800px stub chrome answers the page-height measurement, and the
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
  src.features.slack.slack_listener."""
  stub_credentials({"slack": {"bot_token": "test-bot-token", "app_token": "test-app-token"}})
  return CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      slack={"allowed_user_ids": ["U_ALLOWED"]},
      backends=fake_backends(),
  )


class WsServerNeverAnswersClose:
  """A raw-TCP WebSocket endpoint that behaves like Slack at close time.

  It completes the upgrade, sends the given envelopes once per connection, then
  reads and discards everything - close frames included - never answering a
  close frame. Raw TCP on purpose: every WebSocket server library answers the
  close handshake automatically, and the behavior under test is the client
  waiting for an answer that never comes.
  """

  _GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

  def __init__(self, envelopes_per_connection: list[dict]) -> None:
    self._envelopes = envelopes_per_connection
    self.upgrade_times: list[float] = []
    self.received_chunks = 0
    self._server: asyncio.AbstractServer | None = None
    self._writers: list[asyncio.StreamWriter] = []

  async def start(self) -> str:
    """Bind an ephemeral loopback port; return the ws: URL."""
    self._server = await asyncio.start_server(self._handle, "127.0.0.1", 0)
    port = self._server.sockets[0].getsockname()[1]
    return f"ws://127.0.0.1:{port}/"

  async def stop(self) -> None:
    assert self._server is not None
    self._server.close()
    for writer in self._writers:
      # End the handler's read loop even when the client side leaked its
      # transport (a cancel landing inside the close handshake skips the
      # transport abort, so no FIN reaches the handler on its own).
      writer.close()
    await self._server.wait_closed()

  async def _handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
    import base64
    import hashlib

    request = (await reader.readuntil(b"\r\n\r\n")).decode()
    key = next(
        line.split(":", 1)[1].strip()
        for line in request.split("\r\n")
        if line.lower().startswith("sec-websocket-key:"))
    accept = base64.b64encode(hashlib.sha1((key + self._GUID).encode()).digest()).decode()
    writer.write(
        (
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Accept: {accept}\r\n\r\n").encode())
    for envelope in self._envelopes:
      writer.write(_ws_text_frame(json.dumps(envelope)))
    await writer.drain()
    self.upgrade_times.append(time.monotonic())
    self._writers.append(writer)
    while await reader.read(4096):
      self.received_chunks += 1  # read and discard everything


def _ws_text_frame(payload: str) -> bytes:
  """One unmasked server-to-client text frame carrying a small JSON envelope."""
  data = payload.encode()
  assert len(data) < 126
  return bytes([0x81, len(data)]) + data


class _PendingAudio:
  """An audio stream that never delivers a chunk: the transcription stays open."""

  def __aiter__(self) -> Self:
    return self

  async def __anext__(self) -> bytes:
    await asyncio.Event().wait()
    raise AssertionError("unreachable: the pending audio never yields")


async def assert_transcribe_cancel_honors_close_timeout(
    build_backend: Callable[[str], Any], handshake_reply: list[dict]) -> None:
  """Cancel a transcription driven into its open state against a stand-in that
  never answers the close frame, and assert the cancel waited only the pinned
  `timeouts.WS_CLIENT_CLOSE_TIMEOUT`, not websockets' 10 s default.

  build_backend receives the stand-in's ws: URL and returns a backend whose
  transcribe() consumes `_PendingAudio`; the caller stubs the backend's
  credentials seam and pins WS_CLIENT_CLOSE_TIMEOUT before calling.
  """
  from src.infra import timeouts

  stand_in = WsServerNeverAnswersClose(handshake_reply)
  url = await stand_in.start()
  backend = build_backend(url)

  async def consume() -> None:
    async for _event in backend.transcribe(_PendingAudio(), vocabulary=[], languages=["zh"]):
      pass

  task = asyncio.create_task(consume(), name="transcribe-under-test")
  try:
    async with asyncio.timeout(5):
      while stand_in.received_chunks < 1:
        await asyncio.sleep(0.02)
    # The handshake reply was already on the wire when the stand-in saw the
    # client's first frame, so by now the handshake is consumed and the
    # transcription is in progress (the client's own frames coalesce into one
    # TCP read, so the read count alone cannot say which ones arrived).
    await asyncio.sleep(0.1)
    started = time.perf_counter()
    task.cancel()
    with contextlib.suppress(asyncio.CancelledError):
      await task
    elapsed = time.perf_counter() - started
    assert elapsed < timeouts.WS_CLIENT_CLOSE_TIMEOUT + 0.5, (
        f"transcription cancel took {elapsed:.3f}s; the close wait did not honor "
        f"WS_CLIENT_CLOSE_TIMEOUT={timeouts.WS_CLIENT_CLOSE_TIMEOUT}")
  finally:
    task.cancel()
    with contextlib.suppress(asyncio.CancelledError):
      await task
    await stand_in.stop()


class FakeSlackClient:
  """Recording Slack Web API double for slack_listener tests; never touches the network.

  Every call lands in ``calls`` as ``(method, kwargs)`` — the completeness
  assertions built on it fail on any call a path was not expected to make.
  ``reactions`` models the live per-message reaction set the ack-clear path
  reads back, ``thread`` is what the conversations.replies seam returns, and
  permalinks and channel names are fabricated. Implements only what the
  listener paths may call: a regression to reading anything else fails here
  with an AttributeError by construction.
  """

  def __init__(self, *, fail_posts: bool = False, fail_remove: bool = False) -> None:
    self.calls: list[tuple[str, dict]] = []
    self.posts: list[dict] = []
    self.remove_calls: list[dict] = []
    self.reactions: dict[str, set[str]] = {}
    self.thread: list[dict] = []
    self.fail_posts = fail_posts
    self._fail_remove = fail_remove

  async def open_connection(self) -> str:
    self.calls.append(("open_connection", {"channel": None}))
    return "wss://fake.example/socket"

  async def get_thread_replies(self, channel: str, thread_ts: str) -> list[dict]:
    """The thread-read seam the reply gate consumes; the seeded ``thread`` as a copy."""
    self.calls.append(("get_thread_replies", {"channel": channel, "thread_ts": thread_ts}))
    return list(self.thread)

  async def post_message(self, channel: str, text: str, thread_ts: str) -> dict:
    if self.fail_posts:
      raise RuntimeError("chat.postMessage failed")
    self.calls.append(("post_message", {"channel": channel, "text": text, "thread_ts": thread_ts}))
    self.posts.append({"channel": channel, "text": text, "thread_ts": thread_ts})
    return {"ok": True}

  async def add_reaction(self, channel: str, name: str, ts: str) -> dict:
    self.calls.append(("add_reaction", {"channel": channel, "name": name, "ts": ts}))
    self.reactions.setdefault(ts, set()).add(name)
    return {"ok": True}

  async def remove_reaction(self, channel: str, name: str, ts: str) -> dict:
    """Mirror SlackClient's contract: no_reaction is a payload, other failures raise."""
    self.calls.append(("remove_reaction", {"channel": channel, "name": name, "ts": ts}))
    self.remove_calls.append({"channel": channel, "name": name, "ts": ts})
    if self._fail_remove:
      raise RuntimeError("reactions.remove failed: missing_scope")
    names = self.reactions.setdefault(ts, set())
    if name not in names:
      return {"ok": False, "error": "no_reaction"}
    names.discard(name)
    return {"ok": True}

  async def get_permalink(self, channel: str, ts: str) -> str:
    self.calls.append(("get_permalink", {"channel": channel, "ts": ts}))
    return f"https://fake.slack.test/archives/{channel}/p{ts}"

  async def get_channel_name(self, channel_id: str) -> str | None:
    self.calls.append(("get_channel_name", {"channel": channel_id}))
    return f"name-of-{channel_id}"


def cfg_with_repo(repo_root: Path) -> CharlieBotConfig:
  """A cfg-like object whose charlie_bot_repo points at *repo_root* (real CharlieBotConfig's
  charlie_bot_repo is a derived property tied to the installed package location, so a plain
  namespace stand-in is used to redirect it for these isolated fail-loud tests)."""

  class _Cfg:
    charlie_bot_repo = repo_root
    charliebot_home = repo_root

  return _Cfg()  # type: ignore[return-value]


def build_two_backend_cfg(tmp_path: Path) -> CharlieBotConfig:
  """CharlieBotConfig for cross-backend tests: the .charliebot home lives under tmp_path so each test owns its
  own tree, and the backend list registers the opus-then-codex pair that pin-resolution and fallback-ordering
  cases exercise."""
  return CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backends={"options": [OPUS_BACKEND_OPTION, CODEX_BACKEND_OPTION]},
  )


def build_worktree_cfg(tmp_path: Path) -> CharlieBotConfig:
  """CharlieBotConfig for tests that create and remove worktree dirs: both the charliebot-home and the
  worktrees dirs live under tmp_path so each test owns its own tree — the default (~/worktrees) would
  touch real host worktrees. One cc-claude backend registered so a root created without a backend
  resolves its default (cfg.backends.options[0])."""
  return CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      paths={"worktree_dir": str(tmp_path / "worktrees")},
      backends=fake_backends(),
  )


PUBLISH_BASE_URL = "https://pub.example.test/charliebot_pub"


def deploy_publish_lane(tmp_path: Path) -> Path:
  """Create ``tmp_path / "publish"`` the way the host's deployment step leaves it (the directory
  plus its blank index.html) and return it."""
  publish_dir = tmp_path / "publish"
  publish_dir.mkdir(parents=True, exist_ok=True)
  (publish_dir / "index.html").write_text("", encoding="utf-8")
  return publish_dir


def build_publish_cfg(tmp_path: Path) -> CharlieBotConfig:
  """CharlieBotConfig with the publish lane deployed under tmp_path (``deploy_publish_lane``) and
  ``PUBLISH_BASE_URL`` set."""
  return CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      publish={
          "dir": deploy_publish_lane(tmp_path),
          "public_base_url": PUBLISH_BASE_URL
      },
  )


def write_artifact(tmp_path: Path, name: str = "page.html", body: str = "<p>hello</p>") -> Path:
  """Write one fake artifact source file under tmp_path/artifacts and return its path; publish and
  slack publish-lane tests stage here the file a published URL points at."""
  path = tmp_path / "artifacts" / name
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(body, encoding="utf-8")
  return path


def write_plan_artifact(cfg: CharlieBotConfig, session_id: str, name: str, content: str | None = None) -> str:
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
    plan_id: int,
    versions: list[dict],
    *,
    title: str = "Plan",
    takeoff: dict | None = None,
    closed: dict | None = None,
) -> dict:
  """One plan-registry document wrapping *versions* in the registry schema."""
  return {
      "id": plan_id,
      "title": title,
      "versions": versions,
      "takeoff": takeoff,
      "closed": closed,
  }


def build_scheduler(cfg: Any, blocks: SessionBlocks) -> Scheduler:
  """A scheduler over the store, events, listing and lifecycle blocks of *blocks*."""
  return Scheduler(cfg, blocks.store, blocks.events, blocks.listing, blocks.lifecycle)


def make_scheduler_setup(tmp_path: Path) -> tuple[CharlieBotConfig, SessionBlocks, Scheduler]:
  """Real cfg/blocks/scheduler trio for scheduler and cron-task tests; the scheduler holds the
  process-wide blocks because a private events block keeps its own chat-event cache and its
  rounds would never reach the HTTP/WS read paths."""
  cfg = build_scheduler_cfg(tmp_path)
  session_blocks = build_session_blocks(cfg)
  return cfg, session_blocks, build_scheduler(cfg, session_blocks)


async def make_plan_setup(
    tmp_path: Path,) -> tuple[CharlieBotConfig, SessionBlocks, PlanRegistryManager, models.SessionMetadata]:
  """Session blocks and the plan manager plus one created task for plan endpoint tests."""
  cfg = build_plan_cfg(tmp_path)
  session_blocks = build_session_blocks(cfg)
  plan_mgr = PlanRegistryManager(cfg, session_blocks.events)
  meta = await create_root_session(session_blocks, models.CreateSessionRequest(name="Test"), backend=OPUS_BACKEND_ID)
  return cfg, session_blocks, plan_mgr, meta


async def make_trigger_setup(tmp_path: Path) -> tuple[CharlieBotConfig, SessionBlocks, TriggerManager, str]:
  """Real cfg/blocks/trigger_mgr trio plus one created session, for the PID/SLURM watch tests."""
  cfg = make_home_config(tmp_path)
  session_blocks = build_session_blocks(cfg)
  session = await create_root_session(session_blocks, models.CreateSessionRequest(name="Trigger watch"))
  trigger_mgr = TriggerManager(cfg, session_blocks.tree)
  return cfg, session_blocks, trigger_mgr, session.id


async def no_sleep(_seconds: float) -> None:
  """Watch-loop sleep stand-in that skips time while yielding to other tasks."""
  await _REAL_ASYNCIO_SLEEP(0)


@pytest.fixture
def pidfd_open_available() -> None:
  """Skip when the production pidfd helpers are not supported on this host."""
  from src.runtime.triggers import _PIDFD_SUPPORTED
  if not _PIDFD_SUPPORTED:
    pytest.skip("pidfd not supported on this host (not even via syscall)")


def fake_cli_cfg(monkeypatch: pytest.MonkeyPatch, sessions_dir: Path) -> None:
  """Point the CLI HTTP layer at a fake config so tests never touch a real server."""
  stub_credentials({"charliebot": {"access_key": ""}})
  monkeypatch.setattr(
      CLI_COMMON_GET_CONFIG_PATCH_TARGET,
      lambda: SimpleNamespace(server_base_url="https://server", sessions_dir=sessions_dir))
  monkeypatch.setattr(CLI_COMMON_BASE_URL_PATCH_TARGET, lambda: "https://server")


def _patched_cli_transport(transport_target: str, cfg: object, argv: list[str],
                           **transport_kw: object) -> Iterator[MagicMock]:
  """The externals a CLI main() call touches: sys.argv becomes argv, get_config returns cfg (the
  readback/diff paths still read it), the request path's base URL derives from cfg's
  server_base_url when it is a plain attribute (MagicMock auto-attrs stringify harmlessly — the
  transport verb is patched, nothing dials), and the transport verb at transport_target is a
  MagicMock built from transport_kw (a default 200/{} success response when no return_value is
  given, so the CLI's success path runs; a test can set the response after entering). The mock is
  yielded for that and for call assertions."""
  base_url = str(getattr(cfg, "server_base_url", "http://localhost:18498"))
  with patch("sys.argv", argv), \
       patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg), \
       patch(CLI_COMMON_BASE_URL_PATCH_TARGET, return_value=base_url), \
       patch(transport_target, **transport_kw) as transport_mock:
    if "return_value" not in transport_kw:
      transport_mock.return_value = make_json_response({})
    yield transport_mock


@contextlib.contextmanager
def patched_cli_post(cfg: object, argv: list[str], **post_kw: object) -> Iterator[MagicMock]:
  """_patched_cli_transport with _request_post as the patched verb (the POST path every CLI command
  shares)."""
  yield from _patched_cli_transport(CLI_COMMON_TRANSPORT_POST_PATCH_TARGET, cfg, argv, **post_kw)


def schedule_trigger_argv(message: str, *extra: str) -> list[str]:
  """The schedule_trigger CLI argv the CLI tests share: session s1, --max-wait 60, --message."""
  return ["schedule_trigger", "--session", "s1", "--max-wait", "60", "--message", message, *extra]


def run_reply_stdin_case(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, capsys: pytest.CaptureFixture[str], verb: str,
    readback: dict[str, Any]) -> None:
  """Drive one platform reply CLI's ``--file -`` arm with the reply piped on stdin.

  The reply mains share their transport (src.runtime.cli.common's read_reply_text and
  post_internal_api, the one-JSON-line readback print), so the stdin contract
  is asserted once here per verb: the piped text is the body's ``text``, the
  readback prints unmodified, and the post names the platform's endpoint.
  """
  cfg = setup_session_cwd(tmp_path, monkeypatch, "abc")
  monkeypatch.setattr("sys.stdin", io.StringIO("piped reply\n"))
  with patched_cli_post(cfg, [verb, "reply", "--file", "-"], return_value=make_json_response(readback)) as post_mock:
    importlib.import_module(f"src.features.{verb}.cli").main()
  assert post_mock.call_args.args[0].endswith(f"/api/internal/{verb}/reply")
  assert post_mock.call_args.kwargs["json"]["text"] == "piped reply\n"
  assert json.loads(capsys.readouterr().out) == readback


def run_reply_file_case(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, capsys: pytest.CaptureFixture[str], verb: str,
    readback: dict[str, Any]) -> None:
  """Drive one platform reply CLI's ``--file <path>`` arm with the reply text on disk.

  The reply mains share their transport (src.runtime.cli.common's read_reply_text and
  post_internal_api, the one-JSON-line readback print), so the file contract
  is asserted once here per verb: the file's text rides the body's ``text``
  beside the resolved session id, the readback prints as one JSON line, and
  the post names the platform's endpoint.
  """
  cfg = setup_session_cwd(tmp_path, monkeypatch, "abc")
  reply_file = tmp_path / "reply.md"
  reply_file.write_text("the answer", encoding="utf-8")
  with patched_cli_post(cfg, [verb, "reply", "--file", str(reply_file)],
                        return_value=make_json_response(readback)) as post_mock:
    importlib.import_module(f"src.features.{verb}.cli").main()
  assert post_mock.call_args.args[0].endswith(f"/api/internal/{verb}/reply")
  assert post_mock.call_args.kwargs["json"] == {"session_id": "abc", "text": "the answer"}
  out = capsys.readouterr().out
  assert out.count("\n") == 1
  assert json.loads(out) == readback


def make_task_spawner(tasks: list[asyncio.Task]) -> Callable[..., asyncio.Task]:
  """A create_logged_task substitute that spawns eagerly and captures every task."""

  def _spawn(coro: Coroutine, *, name: str | None = None) -> asyncio.Task:
    task = asyncio.get_running_loop().create_task(coro, name=name)
    tasks.append(task)
    return task

  return _spawn


@contextlib.contextmanager
def mention_seam(tasks: list[asyncio.Task] | None = None) -> Iterator[AsyncMock]:
  """Patch the seams an accepted mention fires through; yields the trigger mock.

  The one home both listener suites (Slack and Discord) drive an accepted
  mention through: the yielded mock replaces ``trigger_master`` (an accepted
  mention wakes the master exactly once), and *tasks*, when given, collects
  the tasks the mention spawns through ``create_logged_task`` for the test to
  drain. Any further patch a test needs stays visible at the call site as a
  sibling context.
  """
  with contextlib.ExitStack() as stack:
    trigger = stack.enter_context(patch(THREAD_ENTRY_TRIGGER_MASTER_PATCH_TARGET, new=AsyncMock()))
    if tasks is not None:
      stack.enter_context(patch(THREAD_ENTRY_CREATE_LOGGED_TASK_PATCH_TARGET, side_effect=make_task_spawner(tasks)))
    yield trigger


def dump_yaml(body: Any) -> str:
  """Block-style ``yaml.safe_dump`` with the dict's insertion key order kept; callers write the result
  into cron host files whose key order should read like a hand-written file."""
  return yaml.safe_dump(body, default_flow_style=False, sort_keys=False)


def reset_config_caches() -> None:
  """Clear the config and cron-loader module-level caches so the next read reloads from disk.

  The config cache and the cron snapshot both key freshness on a fingerprint,
  so an instance cached under an earlier test's profile would answer for the
  wrong one. The credentials cache joins the reset: a stub planted by an
  earlier test (or a value loaded from the host's real credentials.yaml) must
  not answer for this one.
  """
  core_config._config_cache.reset()
  core_config._credentials_cache.reset()
  from src.infra import home as core_home
  core_home._home_cache.clear()
  cron_loader._cron_snapshot = cron_loader._CronSnapshot()


def stub_credentials(sections: dict[str, dict[str, str | int]]) -> None:
  """Plant in-memory credentials for get_credentials(): the given sections become the cached
  Credentials, stamped with the current credentials.yaml fingerprint, so the answer comes from
  memory and no file is read."""
  core_config._credentials_cache.seed(core_config.Credentials(path=Path("credentials.yaml"), sections=sections))


def agent_headers(session_id: str, run_id: str) -> dict[str, str]:
  """The run-token credential of one node's own active Run (agent="manager-agent").

  The signing key must equal the access key planted via stub_credentials, or the API
  rejects the token as a bad credential. This is the credential the delegating CLI
  really carries — never the operator access key the operator-header tests use — and
  the only credential that exercises the agent-creation check on the task tree.
  """
  token = sign_run_token(RunTokenClaims(session_id=session_id, run_id=run_id, agent="manager-agent"), "op-secret")
  return {"Authorization": f"Bearer {token}"}


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
  # The CLI session resolution reads the server's own identity variable first;
  # a shell that runs the suite from inside a live master process carries it
  # and would override every test's cwd-derived session id. The run token rides
  # the same leak shape: internal_api_auth_headers sends it instead of the
  # credential each test planted, so the suite fences both.
  monkeypatch.delenv(SESSION_ID_ENV_VAR, raising=False)
  monkeypatch.delenv(RUN_TOKEN_ENV, raising=False)
  # The worker Run busy map is process-memory state keyed by task-node id,
  # and task-node ids derive deterministically from (parent, request_id) —
  # two tests that create the "w" worker under a "root" manager address the
  # same node. Without a per-test reset, one test's launch mark leaks into the
  # next (a mark_busy setdefault keeps the earlier start; a finish closes only
  # the interval its own Run opened) and a recovered node can read as still
  # running.
  from src.runtime import thinking_state as _thinking_state
  _thinking_state.reset_run_state_for_tests()
  reset_config_caches()
  yield
  _thinking_state.reset_run_state_for_tests()
  reset_config_caches()


def _guarded_get_config(real: Callable[[], CharlieBotConfig], accessor: str, tmp_path: Path,
                        violations: list[str]) -> Callable[[], CharlieBotConfig]:
  """*real* wrapped to record and raise when the config it returns has its home outside *tmp_path*."""

  def guarded() -> CharlieBotConfig:
    cfg = real()
    if not cfg.charliebot_home.resolve().is_relative_to(tmp_path.resolve()):
      message = (
          f"{accessor} built its block from the home {cfg.charliebot_home}, outside this test's tmp dir {tmp_path}; "
          "build the blocks on the test's own cfg and bind them with bind_session_blocks or bind_deps_blocks")
      violations.append(message)
      raise AssertionError(message)
    return cfg

  return guarded


@pytest.fixture(autouse=True)
def block_home_violations(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[list[str]]:
  """A session block accessor builds its block from get_config(); its home must sit under this test's tmp_path.

  Each test starts with no block singleton, so every accessor call builds one and passes the check;
  the singleton the build leaves behind goes away with the test, and it never answers a later test
  whose home is another directory. A build outside tmp_path raises in the caller and is recorded
  here, so the test fails at teardown even when the caller swallowed the error. The list is the
  test's to inspect: a test that provokes the guard clears it.
  """
  violations: list[str] = []
  for module, singleton in _BLOCK_SINGLETONS:
    monkeypatch.setattr(module, singleton, None)
    monkeypatch.setattr(
        module, "get_config", _guarded_get_config(module.get_config, module.__name__, tmp_path, violations))
  yield violations
  if violations:
    pytest.fail("\n".join(violations), pytrace=False)


@pytest.fixture
def temp_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
  """Point HOME at a temp dir and reset the config/cron module-level caches.

  ``CHARLIEBOT_HOME`` wins over ``HOME`` in ``src.infra.config.charliebot_home_dir`` when both
  are set, so the fixture deletes it: a shell exported with a real profile must not leak that
  profile's config.d into a test run.
  """
  monkeypatch.delenv("CHARLIEBOT_HOME", raising=False)
  monkeypatch.setenv("HOME", str(tmp_path))
  reset_config_caches()
  return tmp_path


@pytest.fixture
def path_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
  """Point ``Path.home()`` at a created ``tmp_path / "home"`` and return it.

  ``Path.home()`` honors a redirected ``HOME`` env (the ``temp_home`` route), so this
  fixture exists for its layout: the home sits in its own ``tmp_path / "home"``
  subdirectory, distinct from the sibling trees (config dirs, spec files) a test puts
  directly under ``tmp_path``.
  """
  home = tmp_path / "home"
  home.mkdir()
  monkeypatch.setattr(Path, "home", staticmethod(lambda: home))
  return home


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


async def make_cron_session(
    session_blocks: SessionBlocks,
    task_name: str,
    backend: str = OPUS_BACKEND_ID,
) -> models.SessionMetadata:
  """Create a scheduled manager task with a cron-owned metadata stamp."""
  return await create_root_session(
      session_blocks,
      models.CreateSessionRequest(name=f"Scheduled: {task_name}", scheduled_task=task_name),
      backend=backend)


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


def write_nightly_prompt(home: Path, body: str) -> Path:
  """Write the nightly job's prompt source ``<home>/prompts/nightly.md`` and return its path; a
  pointer-backed host file's ``prompt_file`` names this path and the pointed file owns the body."""
  p = Path(home) / "prompts" / "nightly.md"
  p.parent.mkdir(parents=True, exist_ok=True)
  p.write_text(body, encoding="utf-8")
  return p


def write_nightly_task(
    home: Path, *, project: str | None = None, backend: str | None = None, repo: str | None = None) -> Path:
  """Seed one healthy 'nightly' cron job (pointer-backed host file, as production files look)
  and return its yaml path; the optional keys are the task fields the scheduler tests vary."""
  prompt_path = write_nightly_prompt(home, "run nightly\n")
  body: dict[str, Any] = {
      "cron": "0 3 * * *",
      "prompt_file": str(prompt_path),
      "timezone": "America/Los_Angeles",
      "enabled": True,
  }
  if project is not None:
    body["project"] = project
  if backend is not None:
    body["backend"] = backend
  if repo is not None:
    body["repo"] = repo
  return write_cron_task(home, "nightly", dump_yaml(body))


def memory_entry_text(
    topic: str,
    slug: str,
    *,
    scope: str = "user",
    audience: str = "master, worker",
    title: str | None = None,
    body: str | None = None,
) -> str:
  """One memory entry file's text: title in frontmatter, comma-list audience,
  pure-markdown body."""
  if title is None:
    title = slug.replace("-", " ").title()
  header = ["---", f"scope: {scope}", f"topic: {topic}", f"audience: {audience}", f"title: {title}"]
  header.append("---")
  if body is None:
    body = f"body for {slug}\n"
  return "\n".join(header) + "\n" + body


def write_memory_topics(memory_dir: Path, lines: list[str] | None = None) -> None:
  """Write a memory store's ``topics`` file (one topic per line; the default is the
  seeded production vocabulary DEFAULT_MEMORY_TOPICS) and create the ``entries/``
  dir the loader scans."""
  memory_dir.mkdir(parents=True, exist_ok=True)
  (memory_dir / "entries").mkdir(exist_ok=True)
  (memory_dir / "topics").write_text(
      "".join(line + "\n" for line in (lines or DEFAULT_MEMORY_TOPICS.splitlines())), encoding="utf-8")


def write_memory_entry(memory_dir: Path, topic: str, slug: str, **kw: Any) -> Path:
  """Write one entry file ``entries/<topic>/<slug>.md`` via memory_entry_text; the returned path is
  what assertions read back."""
  d = memory_dir / "entries" / topic
  d.mkdir(parents=True, exist_ok=True)
  p = d / f"{slug}.md"
  text = memory_entry_text(topic, slug, **kw)
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

  Each instance is one process: it owns a pid (drawn one per instance from the
  same unheld 424xxx range SpawningScriptedBackend uses) and its own pid_start,
  and the launch callback that install_scripted_backends wires fires at run()
  start with that pid — a relay's second process registers through the same
  record_launch path the first one did.
  """

  _pids = itertools.count(424100)

  def __init__(self, events: list[dict], exit_code: int, stderr_text: str = "") -> None:
    self._events = events
    self.exit_code = exit_code
    self.stderr_text = stderr_text
    self.terminated = False
    self.hang_diagnostics = None
    self.pid = next(self._pids)
    self.pid_start = "1-1"
    self.prompt: str | None = None
    self.env: dict | None = None
    self.cwd: str | None = None
    self._on_spawn: Callable[[int], Awaitable[None]] | None = None

  def set_on_spawn(self, on_spawn: Callable[[int], Awaitable[None]]) -> None:
    self._on_spawn = on_spawn

  async def terminate(self) -> None:
    self.terminated = True
    self.exit_code = -15

  def detach(self) -> None:
    pass

  def cgroup_exit_report(self) -> str | None:
    """Session memory-cap attribution read: doubles never run inside a cgroup, so None."""
    return None

  async def run(self,
                prompt: str,
                cwd: str,
                env: dict,
                uploaded_files: list[dict] | None = None) -> AsyncIterator[dict]:
    self.prompt = prompt
    self.cwd = cwd
    self.env = env
    self.pid_start = f"1-{self.pid}"
    if self._on_spawn is not None:
      await self._on_spawn(self.pid)
    for event in self._events:
      if self.terminated:
        return
      yield event


def install_scripted_backends(
    monkeypatch: pytest.MonkeyPatch,
    backends: list[ScriptedRelayBackend],
    patch_target: str,
) -> list[dict]:
  """Serve *backends* one build at a time from a patched build_backend, wiring
  each build's on_spawn into the double.

  *patch_target* is the dotted import path of the build_backend binding the
  tested path reads: the master-cc run path reads the type table's attribute
  on every call (BUILD_BACKEND_PATCH_TARGET),
  while the worker path reads src.runtime.worker's module binding
  (WORKER_BUILD_BACKEND_PATCH_TARGET). Returns the build records (option,
  kwargs, backend) in build order.
  """
  builds: list[dict] = []
  queue = list(backends)

  def fake_build_backend(option: models.BackendOption, cfg: CharlieBotConfig, **kwargs: Any) -> ScriptedRelayBackend:
    backend = queue.pop(0)
    on_spawn = kwargs.get("on_spawn")
    if on_spawn is not None:
      backend.set_on_spawn(on_spawn)
    builds.append({"option": option, "kwargs": kwargs, "backend": backend})
    return backend

  monkeypatch.setattr(patch_target, fake_build_backend)
  return builds


def make_worker(tmp_path: Path, thread_id: str) -> Worker:
  """A Worker whose events log and config home both live under tmp_path."""
  return Worker(
      models.ThreadMetadata.model_construct(id=thread_id),
      tmp_path,
      tmp_path / "events.jsonl",
      "",
      CharlieBotConfig(charliebot_home=tmp_path / "home"),
  )


async def process_worker_event(worker: Worker, tmp_path: Path, event: dict, monkeypatch: pytest.MonkeyPatch) -> str:
  """Append one event through Worker._process_event with the broadcast seam stubbed.

  The events log is driven through a real O_APPEND fd and closed even when the
  event raises (the quota scan path); returns the log text so callers assert on
  the persisted lines without re-reading the file.
  """
  monkeypatch.setattr(worker_module.streaming.streaming_manager, "broadcast", AsyncMock())
  fd = os.open(tmp_path / "events.jsonl", os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o666)
  try:
    await worker._process_event(event, fd)
  finally:
    os.close(fd)
  return (tmp_path / "events.jsonl").read_text(encoding="utf-8")


class TerminateFlagBackend:
  """terminate() surface for plain backend doubles: record the signal, touch no process.

  The master-cc cancel and terminate-on-failure paths call terminate() and read
  terminated; a double with no child process keeps that surface with no other
  effect. Real signalling lives on AgentBackend.terminate.
  """

  terminated = False

  async def terminate(self) -> None:
    self.terminated = True

  def cgroup_exit_report(self) -> str | None:
    """Session memory-cap attribution read: doubles never run inside a cgroup, so None."""
    return None


class FakeBackend(TerminateFlagBackend):
  """AgentBackend double whose run() yields one canned result event.

  Callers install it through a patched build_backend on the master-cc run path
  and rely on the exit_code/stderr_text attributes that path reads after the
  event stream ends. The cancel let-go path is not covered: detach() and
  pid_start are absent.
  """

  exit_code = 0
  stderr_text = ""

  async def run(self,
                prompt: str,
                cwd: str,
                env: dict,
                uploaded_files: list[dict] | None = None) -> AsyncIterator[dict]:
    yield backend_base.make_result_event()


def make_sacct_mock(scripted: dict[tuple[str | None, int], list[str]]) -> AsyncMock:
  """Mock ``asyncio.create_subprocess_exec`` answering each sacct probe's scripted stdout.

  *scripted* maps ``(host, job_id)`` to that probe's sacct stdout payloads; each call pops the
  next entry, and the last entry repeats indefinitely. The factory identifies the probe from the
  argv the production caller builds (``src/runtime/triggers.py``): a remote ssh call keeps the host
  in its second-to-last word and the quoted ``sacct -j ID ...`` command last; a local call is
  ``sacct -j ID ...`` with the id third.
  """
  queues: dict[tuple[str | None, int], list[str]] = {k: list(v) for k, v in scripted.items()}

  async def _factory(*args: Any, **kwargs: Any) -> FakeAsyncProcess:
    if args[0] == "ssh":
      host = args[-2]
      job_id = int(args[-1].split()[2])
    else:
      host = None
      job_id = int(args[2])
    queue = queues[(host, job_id)]
    out = queue[0] if len(queue) == 1 else queue.pop(0)
    return FakeAsyncProcess(stdout=out.encode())

  return AsyncMock(side_effect=_factory)


@contextlib.contextmanager
def patch_trigger_fire(
    subprocess_mock: AsyncMock, sacct_available: bool | None,
    sleep_mock: Callable[[float], Awaitable[None]] | None) -> Iterator[AsyncMock]:
  """Patch the watcher externals and task-tree delivery seam.

  Broadcast and delivery are always patched. sacct_available=None leaves
  _SACCT_AVAILABLE untouched, for probes that never read it (remote PID);
  sleep_mock=None keeps real sleeps, for runs that assert on elapsed time.
  """
  patches = []
  if sacct_available is not None:
    patches.append(patch(TRIGGERS_SACCT_AVAILABLE_PATCH_TARGET, sacct_available))
  patches.append(patch(TRIGGERS_ASYNCIO_CREATE_SUBPROCESS_EXEC_PATCH_TARGET, new=subprocess_mock))
  if sleep_mock is not None:
    patches.append(patch("src.runtime.triggers.asyncio.sleep", new=sleep_mock))
  patches.append(patch(BROADCAST_PATCH_TARGET, new=AsyncMock()))
  delivery_patch = patch(TRIGGER_TASK_DELIVERY_PATCH_TARGET, new=AsyncMock())
  with contextlib.ExitStack() as stack:
    for p in patches:
      stack.enter_context(p)
    yield stack.enter_context(delivery_patch)


@contextlib.contextmanager
def patch_trigger_mocks() -> Iterator[AsyncMock]:
  """Patch the watcher seams: streaming broadcast and task-tree delivery.

  Yields the delivery mock. Rigs that must run the real watch internals (real pids,
  real sleeps) use this instead of patch_trigger_fire, whose subprocess stub would hide them.
  """
  with (
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()),
      patch(TRIGGER_TASK_DELIVERY_PATCH_TARGET, new=AsyncMock()) as mock_delivery,
  ):
    yield mock_delivery


async def assert_trigger_fired(
    trigger_mgr: TriggerManager, session_id: str, trigger_id: str, mock_delivery: AsyncMock, *, reason: str) -> str:
  """Asserts the trigger persisted FIRED with the given reason and the standard fired prefix;
  returns the fired message so the caller can assert its site-specific suffix (pids, slurm states).

  Only watch-target triggers take the prefixed message form, so the bare-form pure-delay path
  asserts its whole message at the test site instead of calling this helper."""
  stored = await trigger_mgr._load_trigger(session_id, trigger_id)
  assert stored.status == models.TriggerStatus.FIRED
  assert stored.fire_reason == reason
  msg = mock_delivery.await_args.args[1]
  assert f"[Scheduled trigger fired | {reason}]" in msg
  return msg


def shut_down_trigger_tasks(trigger_mgr: TriggerManager) -> None:
  """Cancel every sleeping trigger task; persisted records are untouched.

  Reads ``TriggerManager._tasks`` because cancel-all has no public route: the
  manager cancels a task only through the per-trigger paths a test is not
  driving. Tests also call it mid-test to stand in for a process death, where
  the in-memory tasks vanish and the records stay PENDING.
  """
  for task in list(trigger_mgr._tasks.values()):
    task.cancel()


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


async def _noop() -> None:
  """Awaitable stand-in returned by fakes patched over coroutine-returning helpers."""
  return


async def _ok_asgi_downstream(scope: Any, receive: Any, send: Any) -> None:
  """Downstream ASGI app the auth-middleware tests wrap; records that it ran."""
  _ok_asgi_downstream.called = True
  await send({"type": "http.response.start", "status": 200, "headers": [(b"content-type", b"text/plain")]})
  await send({"type": "http.response.body", "body": b"OK"})


_ok_asgi_downstream.called = False


def asgi_downstream_called() -> bool:
  """Whether the shared downstream ran during the last run_through_asgi_middleware call."""
  return _ok_asgi_downstream.called


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


def asgi_response(sent: list[dict]) -> tuple[int, dict[bytes, bytes], bytes]:
  """Flatten the messages run_through_asgi_middleware collected into (status, headers, body)."""
  start = next(m for m in sent if m["type"] == "http.response.start")
  headers = dict(start["headers"])
  body = b"".join(m.get("body", b"") for m in sent if m["type"] == "http.response.body")
  return start["status"], headers, body


async def cancel_and_drain(task: asyncio.Task) -> None:
  """Cancel *task*, then await it under a suppressed CancelledError so the task's
  own finally block finishes before the caller continues.

  Deliberately no None-guard, unlike src.infra.tasks.cancel_and_wait: shutdown's
  optional task is legitimately quiet, while a test teardown holding None where
  a task was expected is a bug to fail loudly on. An already-finished task's
  pending exception still surfaces here — the suppressed await re-raises it.
  """
  task.cancel()
  with contextlib.suppress(asyncio.CancelledError):
    await task


# Shared test helpers, single-homed here: the waits and the settle/reader
# spies, imported across the suite's test files.
def _cfg(home: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=home,
      paths={"worktree_dir": str(home / "worktrees")},
      backends={"options": [backend_option(id="fake", label="Fake", type="cc-claude", model="fake-model")]},
  )


def _wait_for(predicate: Callable[[], bool], timeout: float, what: str) -> None:
  deadline = time.monotonic() + timeout
  while time.monotonic() < deadline:
    if predicate():
      return
    time.sleep(0.05)
  raise TimeoutError(what)


async def _async_wait_for(predicate: Callable[[], bool], timeout: float, what: str, *, poll: float = 0.05) -> None:
  # Async sibling of _wait_for: the tasks an async test waits on advance only while
  # the test yields to the event loop, so the poll must asyncio.sleep, not block.
  # The 0.05 s default fits the wait-once callers; a caller that stacks many short
  # waits inside one test's budget passes a tighter poll.
  deadline = time.monotonic() + timeout
  while time.monotonic() < deadline:
    if predicate():
      return
    await asyncio.sleep(poll)
  raise TimeoutError(what)


async def _settle_parent(
    tree: TaskTreeManager, manager: models.SessionMetadata, *, timeout: float, poll: float) -> None:
  # Shared by the task-tree execution and recovery files, whose report-delivery
  # assertions must wait out the parent's report turn without pinning a sleep.
  # Settled: the dispatch queue is drained and no run on the parent lacks a
  # terminal outcome. On timeout the test fails with the pending inputs and the
  # run table, the state a hung parent is debugged from.
  deadline = time.monotonic() + timeout
  while time.monotonic() < deadline:
    pm_events = tree.runs.load_events_sync(manager.id)
    active = [
        r for r in tree.runs.list_run_records_sync(manager.id) if tree.runs.terminal_outcome(pm_events, r.id) is None
    ]
    if not tree.dispatch.pending_inputs(manager.id) and not active:
      return
    await asyncio.sleep(poll)
  # The dump re-reads the state so a late-settling parent is not reported from
  # the last poll's stale snapshot.
  pending = [str(e.get("id")) for e in tree.dispatch.pending_inputs(manager.id)]
  pm_events = tree.runs.load_events_sync(manager.id)
  runs_dbg = [
      (r.id, tree.runs.terminal_outcome(pm_events, r.id), r.pid) for r in tree.runs.list_run_records_sync(manager.id)
  ]
  pytest.fail(f"the parent's report turns never settled: pending={pending} runs={runs_dbg}")


class NotificationSpy:
  """Records the tree notifications the sink emits, capturing what an observer
  can read from the tree projection at signal time: the notified row and its
  parent row, each ``None`` when the projection does not hold it yet."""

  def __init__(self, tree: TaskTreeManager) -> None:
    self.calls: list[tuple[str, str | None]] = []
    self.rows_at_signal: list[dict] = []
    self._tree = tree
    self._orig = tree.events.notify_tree_changed

  async def _spy(self, session_id: str, event_type: str | None) -> None:
    self.calls.append((session_id, event_type))
    meta = await self._tree.load_meta(session_id)
    index = await self._tree._get_index()
    row = self._tree.session_row(index, session_id) if meta is not None else None
    parent_row = (self._tree.session_row(index, row.task_parent_id) if row is not None and row.task_parent_id else None)
    self.rows_at_signal.append(
        {
            "node": row.model_dump() if row else None,
            "parent": parent_row.model_dump() if parent_row else None,
        })
    await self._orig(session_id, event_type)

  def install(self) -> None:
    self._tree.events.notify_tree_changed = self._spy  # type: ignore[method-assign]
