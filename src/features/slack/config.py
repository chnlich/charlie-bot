"""The ``slack:`` config section model, registered in this package's ``register()``."""

from pydantic import BaseModel, ConfigDict


class SlackConfig(BaseModel):
  """``slack:`` section: the summon entrypoint's user allow-list."""

  model_config = ConfigDict(extra='forbid')

  # Slack summon entrypoint
  allowed_user_ids: list[str] = []  # Slack user ids allowed to summon; empty = nobody
