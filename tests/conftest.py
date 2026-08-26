import asyncio
import contextlib
import json
import shutil
import subprocess
import sys
from collections.abc import Awaitable, Callable, Iterator
from datetime import datetime, timedelta, timezone
from pathlib import Path
from types import SimpleNamespace
from typing import Any
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
import yaml
from fastapi import FastAPI
from fastapi.testclient import TestClient
from starlette.requests import Request

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
  sys.path.insert(0, str(ROOT))

# Imports must follow the sys.path bootstrap above.
from src.agents import master_cc_run, master_cc_state  # noqa: E402,I001
from src.agents.backends import base as backend_base  # noqa: E402
from src.api.cron import router as cron_router  # noqa: E402
from src.api.deps import get_session_manager  # noqa: E402
from src.api.sessions import router as sessions_router  # noqa: E402
from src.core import event_types as ET  # noqa: E402
from src.core import models  # noqa: E402
from src.core import review  # noqa: E402
from src.core.config import CharlieBotConfig, get_config  # noqa: E402
from src.core.plans import PlanRegistryManager  # noqa: E402
from src.core.sessions import SessionManager  # noqa: E402
from src.core import spawner  # noqa: E402
from src.core.threads import ThreadManager  # noqa: E402
from src.core.triggers import TriggerManager  # noqa: E402


def mock_session_callbacks() -> models.SessionCallbacks:
  """SessionCallbacks with every field mocked; a test needing one real field constructs its own."""
  return models.SessionCallbacks(
      persist_and_broadcast=AsyncMock(),
      update_thinking_state=AsyncMock(),
      mark_unread=AsyncMock(),
      persist_cc_session_id=AsyncMock(side_effect=lambda sid, ccid: ccid),
      has_completed_round=AsyncMock(return_value=False),
      persist_master_run=AsyncMock(),
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


def append_events(path: Path, events: list[dict]) -> None:
  """Append seed chat events as JSONL; append (not truncate) is what lets a test stage history first."""
  path.parent.mkdir(parents=True, exist_ok=True)
  with open(path, "a", encoding="utf-8") as f:
    for event in events:
      f.write(json.dumps(event) + "\n")


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


def archive_cutoff_events() -> tuple[datetime, list[dict]]:
  """(cutoff, events) where five `e{i}` events predate and three `f{i}` events follow the cutoff; the 5/3
  split is what recycle tests assert on (events_archived == 5, archive_offset == 5, live f0..f2)."""
  base = datetime(2026, 5, 10, 0, 0, 0, tzinfo=timezone.utc)
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


async def make_parent(mgr: "SessionManager", *, name: str = "Parent") -> str:
  """A session ready to elone: the two seed events give succession tests a cut point to reference."""
  parent = await mgr.create_session(models.CreateSessionRequest(name=name), backend="claude-opus-4.6")
  append_events(
      mgr.get_chat_events_path(parent.id),
      [
          {"type": "user", "content": "e0"},
          {"type": "assistant", "content": "e1"},
      ],
  )
  return parent.id


def setup_session_cwd(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, sid: str) -> MagicMock:
  """Build a session dir tree at <tmp_path>/sessions/<sid> and chdir into it; the returned mock cfg is
  what the tests patch into src.cli.common.get_config."""
  cfg = MagicMock()
  cfg.server_port = 9443
  cfg.sessions_dir = tmp_path / "sessions"
  session_dir = cfg.sessions_dir / sid
  session_dir.mkdir(parents=True, exist_ok=True)
  monkeypatch.chdir(session_dir)
  return cfg


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


def make_session_mgr(tmp_path: Path) -> SessionManager:
  """SessionManager over a SimpleNamespace cfg whose sessions_dir is tmp_path/"sessions"; a test
  needing a richer cfg builds its own."""
  cfg = SimpleNamespace(sessions_dir=tmp_path / "sessions")
  cfg.sessions_dir.mkdir()
  return SessionManager(cfg)


def make_sessions_client(cfg: CharlieBotConfig, session_mgr: SessionManager) -> TestClient:
  """TestClient mounting the sessions router with cfg/session_mgr as dependency overrides; a test needing
  extra routers or overrides builds its own FastAPI app."""
  app = FastAPI()
  app.include_router(sessions_router, prefix="/api/sessions")
  app.dependency_overrides[get_config] = lambda: cfg
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  return TestClient(app)


def make_cron_client(cfg: CharlieBotConfig, session_mgr: SessionManager) -> TestClient:
  """TestClient mounting the cron router with cfg/session_mgr as dependency overrides; a test needing
  extra routers or overrides builds its own FastAPI app."""
  app = FastAPI()
  app.include_router(cron_router, prefix="/api/cron")
  app.dependency_overrides[get_config] = lambda: cfg
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  return TestClient(app)


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


def session_dir_names(cfg: CharlieBotConfig) -> set[str]:
  """Snapshot the names of session directories on disk (existence, not content)."""
  if not cfg.sessions_dir.exists():
    return set()
  return {d.name for d in cfg.sessions_dir.iterdir() if d.is_dir()}


OPUS_BACKEND_OPTION = models.BackendOption(
    id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"
)
CODEX_BACKEND_OPTION = models.BackendOption(
    id="codex-o3", label="Codex", type="codex", model="o3"
)

THREE_BACKEND_OPTIONS = [
    OPUS_BACKEND_OPTION,
    CODEX_BACKEND_OPTION,
    models.BackendOption(id="kimi-k2.5", label="Kimi", type="cc-kimi", model="kimi-k2.5"),
]

PLAN_TEST_BACKEND_OPTIONS = [OPUS_BACKEND_OPTION]

PLAN_GOAL_OK_HTML = "<html><section><h2>1 Problem / Goal</h2><p>Ship the fix.</p></section></html>"


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
      worktree_dir=str(tmp_path / "worktrees"),
      backend_options=PLAN_TEST_BACKEND_OPTIONS,
      headless_chrome_bin=write_stub_chrome(tmp_path, 800),
  )


def build_scheduler_cfg(tmp_path: Path) -> CharlieBotConfig:
  """CharlieBotConfig for scheduler cron tests: the home and worktrees dirs live under tmp_path so each test
  owns its own tree, and both the opus and codex backends are registered for backend-override cases."""
  return CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      worktree_dir=str(tmp_path / "worktrees"),
      backend_options=[
          OPUS_BACKEND_OPTION,
          CODEX_BACKEND_OPTION,
      ],
  )


def build_sessions_cfg(tmp_path: Path) -> CharlieBotConfig:
  """CharlieBotConfig for sessions tests: the .charliebot home lives under tmp_path so each test owns its own
  tree, and the backend list registers opus only."""
  return CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          OPUS_BACKEND_OPTION,
      ],
  )


def build_slack_cfg(tmp_path: Path) -> CharlieBotConfig:
  """CharlieBotConfig for slack tests: the home dir lives under tmp_path so each test owns its own tree, and the
  test tokens plus the single allowed user id wire the delivery and listener paths under src.core.slack_listener."""
  return CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      slack_bot_token="test-bot-token",
      slack_app_token="test-app-token",
      slack_allowed_user_ids=["U_ALLOWED"],
  )


def build_tui_sessions_cfg(tmp_path: Path) -> CharlieBotConfig:
  """CharlieBotConfig for sessions-API TUI tests: the .charliebot home lives under tmp_path and the backend list
  registers opus plus the claude-tui terminal backend the TUI handlers resolve a session against."""
  return CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          OPUS_BACKEND_OPTION,
          models.BackendOption(id="claude-tui", label="Claude TUI", type="tui-cli"),
      ],
  )


def build_recovery_cfg(home: Path) -> CharlieBotConfig:
  """CharlieBotConfig for restart-recovery tests: the home dir is caller-chosen (the install-invariance test
  runs its two arms under different homes), the worktrees dir lives under it, and the backend list registers
  the cc-claude fake plus the opencode fake-oc whose uncovered transport the recovery legs exercise."""
  return CharlieBotConfig(
      charliebot_home=home,
      worktree_dir=str(home / "worktrees"),
      backend_options=[
          models.BackendOption(id="fake", label="Fake", type="cc-claude", model="fake-model"),
          models.BackendOption(id="fake-oc", label="FakeOC", type="opencode", model="fake-model"),
      ],
  )


def write_plan_artifact(
    cfg: CharlieBotConfig, session_id: str, name: str = "plan_01.html", content: str = PLAN_GOAL_OK_HTML
) -> str:
  """Write one plan artifact under cfg's sessions dir and return its plan-relative path; the default content
  satisfies the page gate's required section so tests can present/approve directly."""
  artifacts_dir = cfg.sessions_dir / session_id / "artifacts"
  artifacts_dir.mkdir(parents=True, exist_ok=True)
  (artifacts_dir / name).write_text(content, encoding="utf-8")
  return f"artifacts/{name}"


async def make_plan_setup(
    tmp_path: Path,
) -> tuple[CharlieBotConfig, SessionManager, ThreadManager, PlanRegistryManager, models.SessionMetadata]:
  """Real manager trio plus one created session under a plan-shaped config, for plan tests that talk to the
  registry or the plan endpoints."""
  cfg = build_plan_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  plan_mgr = PlanRegistryManager(cfg, session_mgr)
  meta = await session_mgr.create_session(models.CreateSessionRequest(name="Test"), backend="claude-opus-4.6")
  return cfg, session_mgr, thread_mgr, plan_mgr, meta


async def make_trigger_setup(tmp_path: Path) -> tuple[CharlieBotConfig, SessionManager, TriggerManager, str]:
  """Real cfg/session_mgr/trigger_mgr trio plus one created session, for the PID/SLURM watch tests."""
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(models.CreateSessionRequest(name="Trigger watch"))
  trigger_mgr = TriggerManager(cfg, session_mgr)
  return cfg, session_mgr, trigger_mgr, session.id


async def no_sleep(_seconds: float) -> None:
  """asyncio.sleep stand-in for watch-loop tests: returns immediately so poll iterations skip wall-clock waits."""
  return None


def dump_yaml(body: Any) -> str:
  """Block-style ``yaml.safe_dump`` with the dict's insertion key order kept; callers write the result
  into cron host files whose key order should read like a hand-written file."""
  return yaml.safe_dump(body, default_flow_style=False, sort_keys=False)


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


class FakeWebSocket:
  """WebSocket double recording every send_json payload in .sent, for tests that pass it to server
  catchup/replay producers duck-typing the FastAPI WebSocket."""

  def __init__(self) -> None:
    self.sent: list[dict] = []

  async def send_json(self, payload: dict) -> None:
    self.sent.append(payload)


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


class FakeBackend:
  """AgentBackend double whose run() yields one canned result event.

  Callers install it through a patched build_backend on the master-cc run path
  and rely on the exit_code/stderr_text attributes that path reads after the
  event stream ends; terminated and terminate() mirror AgentBackend's surface
  so the terminate-on-failure path runs against the double unchanged. The
  cancel let-go path is not covered: detach() and pid_start are absent.
  """

  exit_code = 0
  stderr_text = ""
  terminated = False

  async def terminate(self) -> None:
    self.terminated = True

  async def run(self, prompt: str, cwd: str, env: dict):
    yield backend_base.make_result_event()


def _instructions_content_stub(session_meta: models.SessionMetadata, cfg: CharlieBotConfig,
                               prompt_overlay: str | None) -> str:
  return "instructions"


def patch_instructions_content(monkeypatch: pytest.MonkeyPatch) -> None:
  """Patch the master-cc instructions builder to return the fixed string "instructions"."""
  monkeypatch.setattr(master_cc_run, "_build_instructions_content", _instructions_content_stub)


@contextlib.contextmanager
def patch_trigger_fire(
    subprocess_mock: AsyncMock, sacct_available: bool | None, sleep_mock: Callable[[float], Awaitable[None]] | None
) -> Iterator[AsyncMock]:
  """Patch the externals a trigger-watch fire run reads; yields the trigger_master mock.

  broadcast and trigger_master are always patched. sacct_available=None leaves
  _SACCT_AVAILABLE untouched, for probes that never read it (remote PID);
  sleep_mock=None keeps real sleeps, for runs that assert on elapsed time.
  """
  patches = []
  if sacct_available is not None:
    patches.append(patch("src.core.triggers._SACCT_AVAILABLE", sacct_available))
  patches.append(patch("src.core.triggers.asyncio.create_subprocess_exec", new=subprocess_mock))
  if sleep_mock is not None:
    patches.append(patch("src.core.triggers.asyncio.sleep", new=sleep_mock))
  patches.append(patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()))
  master_patch = patch("src.core.triggers.trigger_master", new=AsyncMock())
  with contextlib.ExitStack() as stack:
    for p in patches:
      stack.enter_context(p)
    yield stack.enter_context(master_patch)


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


def recording_notify_completion(captures: dict[str, Any]) -> Callable[..., Awaitable[None]]:
  """A spawner._notify_completion stand-in recording the finalized outcome and thread.

  The signature mirrors the production call in spawner._run_finalize_effects; each
  monkeypatching test reads back only the captured fields it asserts on.
  """

  async def fake_notify_completion(
      session_id: str,
      description: str,
      thread_meta: models.ThreadMetadata,
      outcome: spawner._WorkerRunOutcome,
      thread_mgr: Any,
      session_mgr: Any,
      _notify_cfg: CharlieBotConfig,
      verify_report: str | None = None,
  ) -> None:
    captures["notified"] = True
    captures["notify_exit_code"] = outcome.exit_code
    captures["notify_thread"] = thread_meta

  return fake_notify_completion


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
    return models.SessionMetadata(id=session_id, name=self._session_name, backend="claude-opus-4.6")


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
  return None


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


def patch_review_spawn_path(monkeypatch: pytest.MonkeyPatch, captured: dict[str, Any]) -> None:
  """Patch all three spawn-path seams; an unpatched one shells out to git or forks a backend."""
  monkeypatch.setattr(review, "git_current_branch", fake_git_current_branch)
  monkeypatch.setattr(spawner, "spawn_worker", fake_spawn_worker)
  monkeypatch.setattr(review, "create_logged_task", capture_create_logged_task(captured))
