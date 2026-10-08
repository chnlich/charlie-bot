"""Discord-owned session metadata models."""

from pydantic import BaseModel


class DiscordOrigin(BaseModel):
  """Discord thread a session was summoned from; set at creation, never mutated."""
  guild_id: str
  parent_channel_id: str
  thread_id: str


class DiscordSessionFields(BaseModel):
  discord_origin: DiscordOrigin | None = None
  discord_watermark_id: str | None = None
