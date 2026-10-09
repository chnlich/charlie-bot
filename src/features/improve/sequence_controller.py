"""Sequence controller for improve loops.

The controller owns every Run whose ``sequence_ref.kind`` is ``"improve"``:
``launch_context`` renders the iteration's task/input context, and recovery
never resumes a loop (the restart boundary). ``after_run`` and
``recover_run`` run the loop's close step
(:func:`src.features.improve.improve_sequence.close_ended_loop_child`): once
the loop has ended and no launched iteration still owes its terminal fact,
the step closes the loop's worker child through the common completion owner.
``redrive`` raises if a replay is ever routed here. The controller binds no
sessions, claims no session ownership, and adds no listing fields.
"""

from __future__ import annotations

from datetime import datetime
from pathlib import Path
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
    """An iteration's durable finish re-runs its loop's close step.

    The loop's own controller task owns progression while it lives; this is
    the call point that closes the worker child when the loop has already
    ended (or died) with this Run's terminal fact landing last. The step
    returns immediately while the loop can still progress.
    """
    await _close_loop_for_run(run, tree, cfg)

  async def recover_run(
      self, session_id: str, run: RunRecord, outcome: str | None, tree: TaskTreeManager, cfg: CharlieBotConfig) -> bool:
    """Recovery runs the same close step; the loop itself is never resumed.

    Covers the windows ``after_run`` cannot: a stop-requested iteration whose
    interrupted finish recovery writes directly, and a crash between
    run_finished and after_run. Returns False either way — the close step is
    idempotent by the close request id, so nothing here counts a follow-up.
    """
    await _close_loop_for_run(run, tree, cfg)
    return False

  async def reconcile_interrupted(self, cfg: CharlieBotConfig, tree: TaskTreeManager) -> None:
    await improve_sequence.reconcile_interrupted_sequences(cfg, tree)

  def binding(self, session_id: str) -> SequenceBinding | None:
    return None

  def owns_session(self, meta: SessionMetadata) -> bool:
    return False

  def listing_fields(self, session_ids: Iterable[str], now_utc: datetime) -> dict[str, dict]:
    return {}


async def _close_loop_for_run(run: RunRecord, tree: TaskTreeManager, cfg: CharlieBotConfig) -> None:
  """Run the owning loop's close step, located through the Run's sequence_ref.

  The owner_ref is the loop directory (``<sessions>/<session>/loops/<id>``),
  the shape ``improve_sequence.loop_owner_ref`` writes; the manager session
  id and the loop id read back out of it.
  """
  seq = run.sequence_ref
  assert seq is not None  # the runtime only routes sequence Runs here
  loop_dir = Path(seq.owner_ref)
  await improve_sequence.close_ended_loop_child(tree, cfg, loop_dir.parent.parent.name, int(loop_dir.name))
