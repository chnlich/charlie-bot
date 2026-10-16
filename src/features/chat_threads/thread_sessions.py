"""The thread-session predicate.

A thread session answers one Slack or Discord thread: its metadata carries that
platform's origin field, set at summon creation and never mutated. The
chat-threads turn contribution (which rule file follows prompts/master.md) and
the sidebar's chat-thread subtree rule (src/runtime/scheduled_sessions.py)
classify the session through :func:`is_thread_session`, so the test has one
definition.
"""

from src.infra import metadata_slots, models


def is_thread_session(meta: models.SessionMetadata) -> bool:
  """True when *meta* carries a Slack or Discord thread origin."""
  slack = metadata_slots.fields_of(meta, "slack")
  discord = metadata_slots.fields_of(meta, "discord")
  return slack.slack_origin is not None or discord.discord_origin is not None
