"""Discord-owned session metadata models."""

import pydantic


class DiscordOrigin(pydantic.BaseModel):
  """Discord thread a session was summoned from; set at creation, never mutated."""
  guild_id: str
  parent_channel_id: str
  thread_id: str


class DiscordSessionFields(pydantic.BaseModel):
  discord_origin: DiscordOrigin | None = None
  discord_watermark_id: str | None = None
