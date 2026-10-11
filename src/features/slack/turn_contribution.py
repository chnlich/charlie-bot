"""The Slack package's turn contribution: the round-end audit and the ``slack_reply`` chat line."""

from __future__ import annotations

from collections.abc import Callable

from src.features.slack import event_types, slack_listener
from src.infra import config, models
from src.runtime import session_events, session_lifecycle, session_store, session_successor
from src.runtime.hooks import turn_contributions


def _slack_reply_message(event: dict) -> dict:
  return {
      "role": "system",
      "content": f"Posted to Slack: {event.get('content', '')}",
  }


class SlackTurnContribution(turn_contributions.TurnContribution):
  """Runs the Slack round-end audit after every MASTER_DONE and renders ``slack_reply`` events."""

  async def after_turn(self, meta: models.SessionMetadata, done_event: dict, *, cfg: config.CharlieBotConfig) -> None:
    await slack_listener.deliver_done(
        meta.id, done_event, cfg, session_store.store(), session_lifecycle.lifecycle(), session_events.events(),
        session_successor.successor())

  def event_renderers(self) -> dict[str, Callable[[dict], dict]]:
    return {event_types.SLACK_REPLY: _slack_reply_message}


CONTRIBUTION = SlackTurnContribution()
