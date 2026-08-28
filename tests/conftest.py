import asyncio
import contextlib
import json
import re
import shutil
import subprocess
import sys
from collections.abc import Awaitable, Callable, Coroutine, Iterator
from datetime import UTC, datetime, timedelta
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
import src.core.config as core_config  # noqa: E402,I001
from src.agents import master_cc_queue, master_cc_run, master_cc_state  # noqa: E402
from src.agents.backends import base as backend_base  # noqa: E402
from src.api.cron import router as cron_router  # noqa: E402
from src.api.deps import get_session_manager  # noqa: E402
from src.api.sessions import router as sessions_router  # noqa: E402
from src.core import event_types as ET  # noqa: E402
from src.core import improve_command  # noqa: E402
from src.core import models  # noqa: E402
from src.core import review  # noqa: E402
from src.core.config import CharlieBotConfig, get_config  # noqa: E402
from src.core.git import BaseResolution  # noqa: E402
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
        patch("src.core.sessions.SessionManager", return_value=workers_mock),
    ):
      await asyncio.wait_for(master_cc_queue._session_consumer(session_id), timeout=5)
  finally:
    master_cc_state._session_queues.pop(session_id, None)
    master_cc_state._session_consumers.pop(session_id, None)


def append_events(path: Path, events: list[dict]) -> None:
  """Append seed chat events as JSONL; append (not truncate) is what lets a test stage history first."""
  path.parent.mkdir(parents=True, exist_ok=True)
  with open(path, "a", encoding="utf-8") as f:
    f.writelines(json.dumps(event) + "\n" for event in events)


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


async def make_home_session(
    tmp_path: Path, *, name: str, backend: str | None = None
) -> tuple[CharlieBotConfig, SessionManager, models.SessionMetadata]:
  """(cfg, SessionManager, one created session) over a CharlieBotConfig rooted at tmp_path/"home";
  backend=None takes create_session's default (the first registered backend). A test needing more
  sessions calls mgr.create_session directly; a test needing no session builds the cfg/mgr pair
  inline."""
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(models.CreateSessionRequest(name=name), backend=backend)
  return cfg, mgr, session


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

# Synthetic catalog model id shared by the opencode-backend, autonamer, and session-usage tests;
# the fake /config/providers payloads in test_opencode_backend.py split it into provider id and
# model id on the first "/", so a rename keeps the provider/... shape.
SYNTHETIC_MODEL = "synthetic-provider/nvidia/Synthetic-Model"

# Prompt payload beginning with "--", which a naive argv builder would misread as a CLI flag;
# each backend's build-command test asserts the string reaches the CLI as prompt payload only.
FLAG_LIKE_PROMPT = "--malicious-flag ignore previous"

# Import-path patch target shared by every test that silences or spies on streaming broadcasts.
# Mock resolves the route through the src.core.sessions namespace (src/core/sessions.py:43 imports
# the streaming_manager singleton) and setattr's broadcast on that shared object; a move of the
# sessions-side import updates this one string. src.core.autonamer and src.agents.worker import
# the same singleton, so their routes reach the same attribute.
BROADCAST_PATCH_TARGET = "src.core.sessions.streaming_manager.broadcast"

# Import-path patch target shared by every test that silences or spies on the master wake a
# trigger fires. src/core/triggers.py binds the name with `from src.core.master_trigger import
# trigger_master`, so mock resolves the route to the src.core.triggers namespace and setattr's
# the AsyncMock on that module attribute; _wait_and_fire's own call site then reaches the
# stand-in. Other modules bind the same function in their own namespaces, so their wakes keep
# their own routes; grep `from src.core.master_trigger import trigger_master` for the full set.
TRIGGER_MASTER_PATCH_TARGET = "src.core.triggers.trigger_master"


def plan_page_html(goal_body: str = "Ship the fix.") -> str:
  """Minimal plan page passing the plan assertion set: the shipped template's <style> block
  verbatim (style-verbatim compares after whitespace collapse), six numbered sections, and a footer,
  with *goal_body* as the Problem / Goal section's body."""
  template = (ROOT / "prompts" / "plan_template.html").read_text(encoding="utf-8")
  style = re.search(r"<style>.*?</style>", template, re.DOTALL).group(0)
  titles = [
      "Problem / Goal", "Context", "High Level Solution", "Detailed Design", "Trade-offs", "Other Details"
  ]
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


def cfg_with_repo(repo_root: Path) -> CharlieBotConfig:
  """A cfg-like object whose charlie_bot_repo points at *repo_root* (real CharlieBotConfig's
  charlie_bot_repo is a derived property tied to the installed package location, so a plain
  namespace stand-in is used to redirect it for these isolated fail-loud tests)."""

  class _Cfg:
    charlie_bot_repo = repo_root
    memory_dir = repo_root / "memory"

  return _Cfg()  # type: ignore[return-value]


def build_two_backend_cfg(tmp_path: Path) -> CharlieBotConfig:
  """CharlieBotConfig for cross-backend tests: the .charliebot home lives under tmp_path so each test owns its
  own tree, and the backend list registers the opus-then-codex pair that pin-resolution and fallback-ordering
  cases exercise."""
  return CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          OPUS_BACKEND_OPTION,
          CODEX_BACKEND_OPTION,
      ],
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


def build_codex_worktree_cfg(tmp_path: Path) -> CharlieBotConfig:
  """CharlieBotConfig for spawner worktree-launch tests: the charliebot-home and worktrees dirs live
  under tmp_path so each test owns its own tree, and the backend list registers the codex option the
  launch paths resolve."""
  return CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      worktree_dir=str(tmp_path / "worktrees"),
      backend_options=[
          CODEX_BACKEND_OPTION,
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
    cfg: CharlieBotConfig, session_id: str, name: str = "plan_01.html", content: str | None = None
) -> str:
  """Write one plan artifact under cfg's sessions dir and return its plan-relative path; the default content
  passes the plan assertion set so tests can present/approve directly."""
  if content is None:
    content = plan_page_html()
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
  return


def fake_cli_cfg(monkeypatch: pytest.MonkeyPatch, sessions_dir: Path) -> None:
  """Point the CLI HTTP layer at a fake config so tests never touch a real server."""
  monkeypatch.setattr(
      "src.cli.common.get_config",
      lambda: SimpleNamespace(
          server_base_url="https://server", charliebot_access_key="", sessions_dir=sessions_dir))


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
  wrong one.
  """
  core_config._config = None
  core_config._config_mtime = 0.0
  core_config._cron_snapshot = core_config._CronSnapshot()


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
  patches.append(patch(BROADCAST_PATCH_TARGET, new=AsyncMock()))
  master_patch = patch(TRIGGER_MASTER_PATCH_TARGET, new=AsyncMock())
  with contextlib.ExitStack() as stack:
    for p in patches:
      stack.enter_context(p)
    yield stack.enter_context(master_patch)


async def assert_trigger_fired_completed(
    trigger_mgr: TriggerManager, session_id: str, trigger_id: str, mock_master: AsyncMock
) -> str:
  """Asserts the trigger persisted FIRED with the "completed" reason and the standard fired prefix;
  returns the fired message so the caller can assert its site-specific suffix (pids, slurm states)."""
  stored = await trigger_mgr._load_trigger(session_id, trigger_id)
  assert stored.status == models.TriggerStatus.FIRED
  assert stored.fire_reason == "completed"
  msg = mock_master.await_args.args[1]
  assert "[Scheduled trigger fired | completed]" in msg
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


def make_fake_git_create_worktree(
    *, mkdir: bool = False, captures: dict[str, Any] | None = None
) -> Callable[..., Awaitable[BaseResolution]]:
  """A `git_create_worktree` stand-in returning a `detail="fake"` BaseResolution.

  The signature mirrors src.core.git.git_create_worktree so the monkeypatched attribute
  stays a drop-in replacement. mkdir=True creates the worktree dir for flows that write
  into it after creation; captures records the call under "git_create_worktree" with the
  repo/base_branch/branch_name/wt_path key set the spawn-driving tests assert on.
  """

  async def fake_git_create_worktree(
      repo_path: Path, base_branch: str, branch_name: str, wt_path: Path
  ) -> BaseResolution:
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
  """Install the pass-through git fakes on src.core.improve_command.

  A run_improve_loop test without a real repo relies on the loop running to completion:
  create_worktree reuses make_fake_git_create_worktree(mkdir=True), push_branch succeeds,
  and the finally-cleanup remove removes the empty worktree dir and reports success so
  prune runs. The fakes mirror the src.core.git signatures so the patched attributes stay
  drop-in replacements.
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
  monkeypatch.setattr(improve_command, "git_worktree_remove", fake_git_worktree_remove)
  monkeypatch.setattr(improve_command, "git_worktree_prune", fake_git_worktree_prune)


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


def capturing_worker(captures: dict[str, Any]) -> type:
  """A spawner_launch.Worker stand-in recording its constructor args into ``captures``.

  The signature mirrors the production call in spawner._construct_worker, and the
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
    ) -> None:
      captures["worker_dir"] = working_dir
      captures["worker_backend"] = backend_option
      captures["task_description"] = task_description

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
        t for t in asyncio.all_tasks()
        if t is not current and not t.done() and t.get_name().startswith(prefixes)
    ]
    if not pending:
      return
    await asyncio.gather(*pending)
