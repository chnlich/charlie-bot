"""The Slack package's turn contribution: the round-end audit and the ``slack_reply`` chat line."""

from __future__ import annotations

from collections.abc import Callable
from typing import TYPE_CHECKING

from src.features.slack.event_types import SLACK_REPLY
from src.infra.config import CharlieBotConfig
from src.infra.models import SessionMetadata
from src.runtime.hooks import turn_contributions

if TYPE_CHECKING:
  from src.runtime.sessions import SessionManager


def _slack_reply_message(event: dict) -> dict:
  return {
      "role": "system",
      "content": f"Posted to Slack: {event.get('content', '')}",
  }


class SlackTurnContribution(turn_contributions.TurnContribution):
  """Runs the Slack round-end audit after every MASTER_DONE and renders ``slack_reply`` events."""

  async def after_turn(
      self, meta: SessionMetadata, done_event: dict, *, cfg: CharlieBotConfig, sessions: SessionManager) -> None:
    # lazy: slack_listener imports SessionManager from src.runtime.sessions at top level
    from src.features.slack import slack_listener

    await slack_listener.deliver_done(
        meta.id, done_event, cfg, sessions.store, sessions.lifecycle, sessions.events, sessions.successor)

  def event_renderers(self) -> dict[str, Callable[[dict], dict]]:
    return {SLACK_REPLY: _slack_reply_message}


CONTRIBUTION = SlackTurnContribution()
