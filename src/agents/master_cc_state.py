"""Per-session master-run state — run queues, consumer tasks, live backends, and work items."""

import asyncio
import dataclasses
from collections.abc import Callable

from src.agents.backends.base import AgentBackend
from src.core.config import CharlieBotConfig
from src.core.models import (
  BackendOption,
  MasterRunRecord,
  SessionCallbacks,
  SessionMetadata,
)

# Per-session FIFO queue for serializing run_message calls.
_session_queues: dict[str, asyncio.Queue] = {}

# One consumer task per session — drains the queue sequentially.
_session_consumers: dict[str, asyncio.Task] = {}

# Per-session running backend reference for external cancellation.
_active_procs: dict[str, AgentBackend] = {}


@dataclasses.dataclass
class _WorkItem:
  """All arguments needed to execute a single CC run, plus a future for the result."""
  cfg: CharlieBotConfig
  session_meta: SessionMetadata
  user_content: str
  callbacks: SessionCallbacks
  is_voice: bool
  auto_trigger: bool
  backend_option: BackendOption | None
  extra_claude_flags: list[str] | None
  should_check_tex: bool
  future: asyncio.Future
  # True only on the scheduled-session weekly-recycle path that deliberately
  # clears the anchor; suppresses the resume-anchor-missing pre-flight alarm.
  expect_fresh_session: bool = False
  # Chat event id of the user message this turn answers; persisted into
  # master_run so restart reconcile can exclude exactly one event from replay.
  user_event_id: str | None = None
  # Set for re-attach items enqueued by startup reconcile: follow a recorded
  # live turn's raw log instead of spawning a new process.
  resume_record: MasterRunRecord | None = None
  resume_is_alive: Callable[[], bool] | None = None


# Per-session currently-processing work item. Read by queued_user_event_ids so
# startup replay can skip user messages this process already owns — the
# restart-reconcile exclusion must be per-event, never per-session, or a
# message queued behind a running one would be replayed.
_current_items: dict[str, _WorkItem] = {}
