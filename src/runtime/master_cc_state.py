"""Per-session master-run state — run queues, consumer tasks, live backends, and work items."""

import asyncio
import dataclasses
import datetime
import zoneinfo
from collections.abc import Awaitable, Callable

from src.infra import config, models
from src.runtime.agent_process import base

# Per-session FIFO queue for serializing run_message calls.
_session_queues: dict[str, asyncio.Queue] = {}

# One consumer task per session — drains the queue sequentially.
_session_consumers: dict[str, asyncio.Task] = {}

# Per-session running backend reference for external cancellation.
_active_procs: dict[str, base.AgentBackend] = {}


@dataclasses.dataclass
class TaskRunBinding:
  """The v2 task-tree Run one work item executes (data only, no behavior).

  When set, the turn is a v2 manager_turn Run: the consumer records the
  turn's outcome on the Run through the adapter's callbacks — never a
  ``SessionMetadata.master_run`` — and the transport dir is the Run's own
  directory (raw log, stderr log, cursor, launch text all live there).
  """

  session_id: str
  run_id: str
  transport_dir: str
  # True when this launch deliberately starts a fresh native context (the
  # effective instruction hash or backend identity changed): the consumer
  # resumes nothing and clears the stale anchor at spawn.
  fresh_native_context: bool = False


@dataclasses.dataclass
class _WorkItem:
  """All arguments needed to execute a single CC run, plus a future for the result."""
  cfg: config.CharlieBotConfig
  session_meta: models.SessionMetadata
  user_content: str
  callbacks: models.SessionCallbacks
  is_voice: bool
  auto_trigger: bool
  backend_option: models.BackendOption | None
  extra_claude_flags: list[str] | None
  should_check_tex: bool
  future: asyncio.Future
  # True only on the scheduled-session weekly-recycle path that deliberately
  # clears the anchor; suppresses the resume-anchor-missing pre-flight alarm.
  expect_fresh_session: bool = False
  # Chat events this turn answers, in arrival order: one event on every
  # single-input turn, the whole batch on a merged one. Persisted into
  # master_run so restart reconcile excludes exactly the answered events from
  # replay, and carried on MASTER_DONE for the Slack round audit.
  user_event_ids: list[str] = dataclasses.field(default_factory=list)
  # The enqueueing entry point's declared input type, an INPUT_EVENT_TYPES
  # member (src/runtime/session_dispatch.py); the enqueue asserts the membership
  # and a merged batch's per-part headers render it. None only on items that
  # are not legacy input delivery at all (a restart re-attach or a v2 Run) --
  # those never take part in a batch.
  input_event_type: str | None = None
  # Local wall-clock moment this item entered its session queue: the received
  # time a merged batch's header stamps each part with. The default_factory
  # stamps construction (direct-seeded items); _enqueue_work_item re-stamps so
  # the value is the enqueue moment.
  received_at: datetime.datetime = dataclasses.field(
      default_factory=lambda: datetime.datetime.now(zoneinfo.ZoneInfo(config.HOUSE_TIMEZONE)))
  # Structured attachment refs from the user message, handed to backend.run;
  # the opencode backend turns image refs into prompt file parts.
  uploaded_files: list[dict] | None = None
  # Set for re-attach items enqueued by startup reconcile: follow a recorded
  # live turn's raw log instead of spawning a new process.
  resume_record: models.MasterRunRecord | None = None
  resume_is_alive: Callable[[], bool] | None = None
  # Prebuilt managed instructions (the v2 task snapshot's joined text). When
  # set, the turn delivers exactly these bytes through the backend's
  # system-instruction seam and never runs the legacy v1 instruction builder —
  # no second memory/project/PM injection.
  task_instructions: str | None = None
  # v2 task-tree binding plus the adapter's spawn/finish hooks. When task_run
  # is set the turn records pid/pid_start and its terminal outcome on the Run
  # (task_execution closures), merges extra_env into the child environment,
  # and never writes a SessionMetadata.master_run.
  task_run: TaskRunBinding | None = None
  on_task_spawn: Callable[[int, str | None], Awaitable[None]] | None = None
  on_task_finish: Callable[[str | None, int, dict], Awaitable[None]] | None = None
  extra_env: dict[str, str] | None = None


# Per-session currently-processing work item. Read by queued_user_event_ids so
# startup replay can skip inputs this process already owns — the
# restart-reconcile exclusion must be per-event, never per-session, or an
# input queued behind a running one would be replayed. Read by
# running_user_event_ids so Slack reply binding uses the running round's
# identity without passing through the session metadata cache.
_current_items: dict[str, _WorkItem] = {}


def running_user_event_ids(session_id: str) -> list[str]:
  """Chat event ids the session's currently-running round answers, in arrival order.

  Empty covers both "no round is running in this process" and "the running
  round was not started by a chat event (worker wake)". Unlike
  ``queued_user_event_ids`` this never mixes in queued items: reply binding
  means the running round only, and a set carries no order to pick the newest
  Slack-bearing input from.
  """
  item = _current_items.get(session_id)
  return list(item.user_event_ids) if item is not None else []
