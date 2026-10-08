"""The LaTeX package's turn contribution: a turn that edits the .tex file proposes the edit instead of keeping it."""

from __future__ import annotations

import asyncio
from typing import TYPE_CHECKING

from src.features.latex import latex
from src.features.latex.event_types import TEX_EDIT_PROPOSED
from src.infra.config import CharlieBotConfig
from src.infra.log_once import LazyStructlogLogger
from src.infra.models import SessionMetadata
from src.runtime.hooks import turn_contributions

if TYPE_CHECKING:
  from src.runtime.sessions import SessionManager

log = LazyStructlogLogger()


class LatexTurnContribution(turn_contributions.TurnContribution):
  """Snapshots the .tex file when a turn's input is queued and compares it when the turn ends.

  The snapshot lives in :mod:`src.features.latex.latex`; ``check_tex_changed`` finds nothing to compare
  when no snapshot was taken, so a turn queued while the file was absent proposes nothing.
  """

  async def before_turn(self, meta: SessionMetadata, cfg: CharlieBotConfig) -> None:
    latex.clear_snapshot()
    if latex.get_tex_path().exists():
      await asyncio.to_thread(latex.snapshot_tex)

  async def after_turn(
      self, meta: SessionMetadata, done_event: dict, *, cfg: CharlieBotConfig, sessions: SessionManager) -> None:
    if not latex.has_snapshot():
      return
    proposal = await asyncio.to_thread(latex.check_tex_changed)
    if proposal:
      await sessions.events.persist_and_broadcast(meta.id, {"type": TEX_EDIT_PROPOSED})
      log.info(TEX_EDIT_PROPOSED, session=meta.id)
    else:
      latex.clear_snapshot()


CONTRIBUTION = LatexTurnContribution()
