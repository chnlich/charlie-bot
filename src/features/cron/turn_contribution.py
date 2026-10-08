"""The cron package's chat renderer for skipped scheduled runs."""

from __future__ import annotations

from collections.abc import Callable

from src.infra import event_types as ET
from src.runtime.hooks import turn_contributions


def _scheduled_run_skipped_msg(ev: dict) -> dict:
  task = ev.get('task', '')
  skipped_at = ev.get('skipped_at', '')
  reason = ev.get('reason', '')
  return {
      'role': 'system',
      'content': f"Scheduled run of '{task}' skipped at {skipped_at}: {reason}",
  }


class CronTurnContribution(turn_contributions.TurnContribution):
  """Render cron-owned events in chat."""

  def event_renderers(self) -> dict[str, Callable[[dict], dict]]:
    return {ET.SCHEDULED_RUN_SKIPPED: _scheduled_run_skipped_msg}


CONTRIBUTION = CronTurnContribution()
