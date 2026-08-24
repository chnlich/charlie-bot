import asyncio
import json
import shutil
import subprocess
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import TYPE_CHECKING, Any
from unittest.mock import AsyncMock

import pytest

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
  sys.path.insert(0, str(ROOT))

# Imports must follow the sys.path bootstrap above.
from src.agents import master_cc_state  # noqa: E402,I001
from src.core import models  # noqa: E402,I001

if TYPE_CHECKING:
  from src.core.config import CharlieBotConfig
  from src.core.sessions import SessionManager


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
