"""The LaTeX package's turn contribution: a turn that edits the .tex file proposes the edit instead of keeping it."""

from __future__ import annotations

import asyncio

from src.features.latex import event_types, latex
from src.infra import config, log_once, models
from src.runtime import session_events
from src.runtime.hooks import turn_contributions

log = log_once.LazyStructlogLogger()


class LatexTurnContribution(turn_contributions.TurnContribution):
  """Snapshots the .tex file when a turn's input is queued and compares it when the turn ends.

  The snapshot lives in :mod:`src.features.latex.latex`; ``check_tex_changed`` finds nothing to compare
  when no snapshot was taken, so a turn queued while the file was absent proposes nothing.
  """

  async def before_turn(self, meta: models.SessionMetadata, cfg: config.CharlieBotConfig) -> None:
    latex.clear_snapshot()
    if latex.get_tex_path().exists():
      await asyncio.to_thread(latex.snapshot_tex)

  async def after_turn(self, meta: models.SessionMetadata, done_event: dict, *, cfg: config.CharlieBotConfig) -> None:
    if not latex.has_snapshot():
      return
    proposal = await asyncio.to_thread(latex.check_tex_changed)
    if proposal:
      await session_events.events().persist_and_broadcast(meta.id, {"type": event_types.TEX_EDIT_PROPOSED})
      log.info(event_types.TEX_EDIT_PROPOSED, session=meta.id)
    else:
      latex.clear_snapshot()


CONTRIBUTION = LatexTurnContribution()
