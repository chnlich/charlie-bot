"""Sequence controller for interrupted improve loops.

The controller owns every Run whose ``sequence_ref.kind`` is ``"improve"``:
``launch_context`` renders the iteration's task/input context, and recovery
never resumes a loop (the restart boundary) — ``after_run`` does nothing and
``recover_run`` returns False, so an iteration Run is left exactly as its
durable facts hold it. ``redrive`` raises if a replay is ever routed here.
The controller binds no sessions, claims no session ownership, and adds no
listing fields.
"""

from __future__ import annotations

from datetime import datetime
from typing import TYPE_CHECKING

from src.features.improve import improve_sequence
from src.runtime.hooks.sequence_controllers import SequenceController
from src.runtime.task_errors import TaskInvalidError

if TYPE_CHECKING:
  from collections.abc import Iterable

  from src.infra.config import CharlieBotConfig
  from src.infra.models import RunRecord, SessionMetadata
  from src.runtime.hooks.sequence_controllers import SequenceBinding
  from src.runtime.task_sessions import TaskTreeManager


class ImproveSequenceController(SequenceController):
  sequence_kind = "improve"

  async def redrive(self, session_id: str, tree: TaskTreeManager, cfg: CharlieBotConfig) -> None:
    raise RuntimeError("improve iteration replay is not supported")

  async def launch_context(
      self, meta: SessionMetadata, run: RunRecord, launch_text: str, cfg: CharlieBotConfig) -> str | None:
    """One iteration's context: shared-worktree bindings + the loop position.

    The controller composes the description (live goal, optional plan,
    previous summaries) and passes it as the launch text; the sequence_ref
    pins the shared worktree facts. A Run whose sequence_ref is not this
    controller's kind is a registration bug and fails loudly.
    """
    seq = run.sequence_ref
    if seq is None or seq.kind != self.sequence_kind:
      raise TaskInvalidError(
          f"iteration run {run.id} carries no improve sequence_ref; the controller "
          "that registered it is broken")
    return improve_sequence.iteration_context(cfg, meta, run, seq, launch_text)

  async def after_run(self, session_id: str, run: RunRecord, tree: TaskTreeManager, cfg: CharlieBotConfig) -> None:
    """An iteration's finish carries no delivery chain of its own.

    The loop's own controller task observes the terminal fact and owns
    progression (judge, next launch, the one final report).
    """
    return

  async def recover_run(
      self, session_id: str, run: RunRecord, outcome: str | None, tree: TaskTreeManager, cfg: CharlieBotConfig) -> bool:
    """An improve loop is never resumed: recovery leaves every iteration Run alone."""
    return False

  async def reconcile_interrupted(self, cfg: CharlieBotConfig, tree: TaskTreeManager) -> None:
    await improve_sequence.reconcile_interrupted_sequences(cfg, tree)

  def binding(self, session_id: str) -> SequenceBinding | None:
    return None

  def owns_session(self, meta: SessionMetadata) -> bool:
    return False

  def listing_fields(self, session_ids: Iterable[str], now_utc: datetime) -> dict[str, dict]:
    return {}
