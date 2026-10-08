"""The backend lifecycle hook: what a backend package tells the runtime about one run's processes.

The runtime launches a run as a loop of processes (``src/runtime/launch_loop.py``). A backend with a
login pool, such as the Claude account pool, picks the login before the first process, watches the
events for a quota signal, and starts a next process on another login. A backend without a pool
runs one process per run, which is what the default ``BackendLifecycle`` does.

Vocabulary:

- A *run* is one master turn (``kind="turn"``) or one task run (``kind="task"``).
- A *launch* is the plan for one process of the run: the factory arguments, the conversation to
  resume and, after a relay, the prompt to send.
- A *relay* is the next process of the same run, started after the watch of the previous process
  asked for one.

The registries that backend packages fill from their ``register()`` (the child-process environment edits,
the context limits of a reading kind and the usage resolver of a backend type) live in
``src/runtime/hooks/backend_lifecycle_registration.py``. This module imports no backend module and no heavy
module.
"""

from __future__ import annotations

import dataclasses
from collections.abc import Awaitable, Callable
from datetime import datetime
from typing import TYPE_CHECKING, Any, Literal, Protocol

if TYPE_CHECKING:
  from src.infra.config import CharlieBotConfig
  from src.infra.models import BackendOption, SessionMetadata, ThreadMetadata


class LaunchRefused(Exception):  # noqa: N818  (a refusal is a run outcome, named for what the backend did)
  """The backend cannot start or continue this run. The run ends with this message.

  ``quota_exhausted`` is True when the cause is a spent quota: the run could succeed after the
  quota resets. The runtime copies it onto the error event it writes for the refusal.
  """

  def __init__(self, message: str, *, quota_exhausted: bool) -> None:
    super().__init__(message)
    self.quota_exhausted = quota_exhausted


@dataclasses.dataclass(frozen=True)
class LaunchContext:
  """What the runtime gives the backend for one run: a master turn or a task run.

  ``emit`` writes one event for the user to see. On a turn it persists and broadcasts the event.
  On a task it writes to the run's event log through its open file descriptor and delivers the event
  to the session's successor chain.
  """
  cfg: CharlieBotConfig
  option: BackendOption
  session_meta: SessionMetadata
  kind: Literal["turn", "task"]
  cwd: str
  held_native_id: str | None  # the conversation this run may resume; None starts fresh
  preassigned_native_id: str | None  # task: the id the runtime chose for the conversation its first process opens
  emit: Callable[[dict], Awaitable[None]]
  #   turn: persist and broadcast one session event.
  #   task: write the event to the run's raw log through its fd AND to the session's successor
  #         chain (worker.py binds both).
  record_account: Callable[[str], Awaitable[None]]  # persist the account label on the session
  context_state: Callable[[], Awaitable[tuple[int | None, datetime | None]]]  # (context tokens, last request time)


@dataclasses.dataclass(frozen=True)
class Launch:
  """The plan for one process of a run.

  ``backend_kwargs`` holds the extra factory arguments of this process. Its ``extra_flags`` entry
  lists command-line flags that the runtime places after its own resume flag. ``resume_id`` is the
  conversation this process resumes. ``prompt`` replaces the run's prompt when set: a relay sets
  the continuation prompt. ``account_label`` names the login for the run's log lines.
  """
  backend_kwargs: dict[str, Any]  # extra factory arguments of this process
  resume_id: str | None
  prompt: str | None = None
  account_label: str | None = None


class LaunchWatch(Protocol):
  """Folds the events and the exit of one process into the decision to start a next process."""

  def observe(self, event: dict) -> bool:
    """Record one event of the process. True: stop the process now, at a safe relay point."""
    ...

  def wants_next(self, exit_code: int, stderr: str) -> bool:
    """True: the run continues in a next process, which ``next_launch`` plans."""
    ...


@dataclasses.dataclass(frozen=True)
class ContextLimits:
  """The context limits of one reading kind.

  ``declared_window`` None: the reading itself carries the window. ``compact_reserve`` None: the
  compaction point is unknown.
  """
  declared_window: int | None
  compact_reserve: int | None


class BackendLifecycle:
  """The default serves a backend type with no login pool: one process per run."""

  def continuation_domain(self, option: BackendOption, cfg: CharlieBotConfig) -> str:
    """The domain in which backend options continue each other's conversation. One option is its own domain."""
    return option.id

  async def place(self, ctx: LaunchContext) -> Launch:
    """Plan the first process of the run, or raise ``LaunchRefused``.

    turn: ``held_native_id`` is the conversation this turn may resume. The default resumes it as it is.
    task: ``held_native_id`` is None, so the default starts a fresh conversation.
    """
    return Launch(backend_kwargs={}, resume_id=ctx.held_native_id)

  def watch(self, ctx: LaunchContext, launch: Launch) -> LaunchWatch | None:
    """The watch of the process that ``launch`` plans; None asks for no relay."""
    return None

  async def next_launch(self, ctx: LaunchContext, launch: Launch, native_id: str | None) -> Launch:
    """Plan the next process of the run after ``launch``, or raise ``LaunchRefused``.

    ``native_id`` is the run's conversation id as the processes reported it.
    turn: a refused relay ends the turn with the refusal message in the session chat.
    task: a refused relay ends the run with the refusal message in the run's event log.
    The default is never called: its watch is None.
    """
    raise NotImplementedError

  async def after_round(self, ctx: LaunchContext, native_id: str | None, succeeded: bool) -> None:
    """Clean up after the last process of a round, whether the round succeeded or not."""
    return

  def round_notices(self, option: BackendOption, events: list[dict]) -> list[dict]:
    """The notice events for the user that the finished round's events call for."""
    return []

  # The metadata keys a backend keeps on the session and on the task's thread record. The defaults
  # keep none: a backend type without a login pool has no account label and preassigns no task id.

  account_source = ""  # the name that logs and error events give the account label; "" when the backend keeps none
  account_subject = ""  # the same, as a noun phrase in a message

  def account_label(self, meta: SessionMetadata) -> str | None:
    """The label of the pool login whose transcript store holds the session's conversation; None when unrecorded."""
    return None

  def record_account_label(self, meta: SessionMetadata, label: str | None) -> bool:
    """Record ``label`` on ``meta`` in memory. True when the recorded label changed."""
    return False

  def thread_native_id(self, thread: ThreadMetadata) -> str | None:
    """The conversation id the runtime chose for the task before its first process started; None when none."""
    return None

  def assign_thread_native_id(self, thread: ThreadMetadata, native_id: str | None) -> None:
    """Record on ``thread`` the conversation id that the runtime chose for the task."""
    return
