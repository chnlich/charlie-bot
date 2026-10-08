"""In-process state for task manager Runs using the master CC backend harness."""

import asyncio
import dataclasses
from collections.abc import Awaitable, Callable

from src.infra import config, models
from src.runtime.agent_process import base

# Per-session FIFO queue for serializing task manager Runs.
_session_queues: dict[str, asyncio.Queue] = {}

# One consumer task per session — drains the queue sequentially.
_session_consumers: dict[str, asyncio.Task] = {}

# Per-session running backend reference for external cancellation.
_active_procs: dict[str, base.AgentBackend] = {}


@dataclasses.dataclass
class TaskRunBinding:
  """The task-tree Run one work item executes (data only, no behavior)."""

  session_id: str
  run_id: str
  transport_dir: str
  # True when the effective instruction hash or backend identity changed.
  fresh_native_context: bool = False


@dataclasses.dataclass
class _WorkItem:
  """Arguments needed to execute or follow one task manager Run."""

  cfg: config.CharlieBotConfig
  session_meta: models.SessionMetadata
  user_content: str
  callbacks: models.SessionCallbacks
  is_voice: bool
  auto_trigger: bool
  backend_option: models.BackendOption | None
  extra_claude_flags: list[str] | None
  future: asyncio.Future
  task_run: TaskRunBinding
  user_event_ids: list[str] = dataclasses.field(default_factory=list)
  uploaded_files: list[dict] | None = None
  resume_record: models.MasterRunRecord | None = None
  resume_is_alive: Callable[[], bool] | None = None
  task_instructions: str | None = None
  on_task_spawn: Callable[[int, str | None], Awaitable[None]] | None = None
  on_task_finish: Callable[[str | None, int, dict], Awaitable[None]] | None = None
  extra_env: dict[str, str] | None = None


# Current items support Slack reply binding to the active Run's input ids.
_current_items: dict[str, _WorkItem] = {}


def running_user_event_ids(session_id: str) -> list[str]:
  """Input event ids answered by the session's currently running Run."""
  item = _current_items.get(session_id)
  return list(item.user_event_ids) if item is not None else []
