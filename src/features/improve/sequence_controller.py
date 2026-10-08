"""Sequence controller for interrupted improve loops.

Improve Run owner references are loop directory paths. ``owner_prefix`` is
``"improve:"``, so those references do not select this controller for replay.
``redrive`` raises if a lookup is ever routed here because a restart never
resumes an improve loop. The controller binds no sessions, claims no session
ownership, and adds no listing fields.
"""

from __future__ import annotations

from datetime import datetime
from typing import TYPE_CHECKING

from src.runtime.hooks.sequence_controllers import SequenceController

if TYPE_CHECKING:
  from collections.abc import Iterable

  from src.infra.config import CharlieBotConfig
  from src.infra.models import SessionMetadata
  from src.runtime.hooks.sequence_controllers import SequenceBinding
  from src.runtime.task_sessions import TaskTreeManager


class ImproveSequenceController(SequenceController):
  owner_prefix = "improve:"

  async def redrive(self, session_id: str, tree: TaskTreeManager, cfg: CharlieBotConfig) -> None:
    raise RuntimeError("improve iteration replay is not supported")

  async def reconcile_interrupted(self, cfg: CharlieBotConfig, tree: TaskTreeManager) -> None:
    from src.features.improve import improve_sequence

    await improve_sequence.reconcile_interrupted_sequences(cfg, tree)

  def binding(self, session_id: str) -> SequenceBinding | None:
    return None

  def owns_session(self, meta: SessionMetadata) -> bool:
    return False

  def listing_fields(self, session_ids: Iterable[str], now_utc: datetime) -> dict[str, dict]:
    return {}
