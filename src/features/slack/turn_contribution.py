"""The Slack package's turn contribution: the round-end audit, the thread reply contract, and the ``slack_reply`` chat line."""

from __future__ import annotations

from src.features.chat_threads import turn_contribution
from src.features.slack import metadata, slack_listener


class SlackTurnContribution(turn_contribution.ThreadTurnContribution):
  """Runs the Slack round-end audit after every MASTER_DONE, carries the thread reply contract, and renders ``slack_reply`` events."""

  PLATFORM = slack_listener.SLACK
  LISTENER = slack_listener

  def contract_part(self, origin: metadata.SlackOrigin) -> str:
    return slack_listener.thread_contract_part(origin)


CONTRIBUTION = SlackTurnContribution()
