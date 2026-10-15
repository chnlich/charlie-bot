"""The Slack package's turn contribution: the round-end audit, the thread reply contract, and the ``slack_reply`` chat line."""

from __future__ import annotations

from collections.abc import Callable

from src.features.chat_threads import thread_entry
from src.features.slack import event_types, slack_listener
from src.infra import config, metadata_slots, models
from src.runtime import session_events, session_lifecycle, session_store, session_successor, task_prompts
from src.runtime.hooks import turn_contributions


def _slack_reply_message(event: dict) -> dict:
  return {
      "role": "system",
      "content": f"Posted to Slack: {event.get('content', '')}",
  }


class SlackTurnContribution(turn_contributions.TurnContribution):
  """Runs the Slack round-end audit after every MASTER_DONE, carries the thread reply contract, and renders ``slack_reply`` events."""

  async def after_turn(self, meta: models.SessionMetadata, done_event: dict, *, cfg: config.CharlieBotConfig) -> None:
    await slack_listener.deliver_done(
        meta.id, done_event, cfg, session_store.store(), session_lifecycle.lifecycle(), session_events.events(),
        session_successor.successor())

  def instruction_segments(self, meta: models.SessionMetadata, kind: str,
                           cfg: config.CharlieBotConfig) -> list[task_prompts.RuleSegment]:
    """The thread reply contract for a manager round of a Slack thread session.

    Only a manager round of a session whose origin is this platform's gets it
    (the summon line stays one line because the contract rides here once);
    every other session or kind gets none.
    """
    if kind not in task_prompts.MANAGER_KINDS:
      return []
    origin = metadata_slots.fields_of(meta, "slack").slack_origin
    if origin is None:
      return []
    platform = slack_listener.SLACK
    text = thread_entry.thread_reply_contract(platform, slack_listener.thread_contract_part(origin), cfg)
    return [thread_entry.thread_reply_segment(platform, text)]

  def event_renderers(self) -> dict[str, Callable[[dict], dict]]:
    return {event_types.SLACK_REPLY: _slack_reply_message}


CONTRIBUTION = SlackTurnContribution()
