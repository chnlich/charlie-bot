"""Slack-owned session metadata models."""

import pydantic


class SlackOrigin(pydantic.BaseModel):
  """Slack thread a session was summoned from; set at creation, never mutated."""
  team_id: str
  channel_id: str
  thread_ts: str


class SlackSessionFields(pydantic.BaseModel):
  slack_origin: SlackOrigin | None = None
  slack_watermark_ts: str | None = None
