"""Slack-owned session metadata models."""

from pydantic import BaseModel


class SlackOrigin(BaseModel):
  """Slack thread a session was summoned from; set at creation, never mutated."""
  team_id: str
  channel_id: str
  thread_ts: str


class SlackSessionFields(BaseModel):
  slack_origin: SlackOrigin | None = None
  slack_watermark_ts: str | None = None
