import asyncio
import json
import shutil
import subprocess
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock

import pytest

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
  sys.path.insert(0, str(ROOT))

# Imports must follow the sys.path bootstrap above.
from src.agents import master_cc_state  # noqa: E402,I001
from src.core import models  # noqa: E402,I001
from src.core.config import CharlieBotConfig  # noqa: E402,I001
from src.core.plans import PlanRegistryManager  # noqa: E402,I001
from src.core.sessions import SessionManager  # noqa: E402,I001
from src.core.threads import ThreadManager  # noqa: E402,I001


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
    cfg: "CharlieBotConfig", session_meta: models.SessionMetadata, backend_option: models.BackendOption | None
) -> master_cc_state._WorkItem:
  """_WorkItem with the field values the run-path tests share; a test needing any other value builds its own."""
  return master_cc_state._WorkItem(
      cfg=cfg,
      session_meta=session_meta,
      user_content="hello",
      callbacks=mock_session_callbacks(),
      is_voice=False,
      auto_trigger=False,
      backend_option=backend_option,
      extra_claude_flags=None,
      should_check_tex=False,
      future=asyncio.get_running_loop().create_future(),
  )


def append_events(path: Path, events: list[dict]) -> None:
  """Append seed chat events as JSONL; append (not truncate) is what lets a test stage history first."""
  path.parent.mkdir(parents=True, exist_ok=True)
  with open(path, "a", encoding="utf-8") as f:
    for event in events:
      f.write(json.dumps(event) + "\n")


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


PLAN_TEST_BACKEND_OPTIONS = [
    models.BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
]

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
