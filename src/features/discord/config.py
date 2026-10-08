"""The ``discord:`` config section model, registered in this package's ``register()``."""

from typing import Any

from pydantic import BaseModel, ConfigDict, model_validator


class DiscordConfig(BaseModel):
  """``discord:`` section: the summon entrypoint's account map."""

  model_config = ConfigDict(extra='forbid')

  # Discord summon entrypoint
  allowed_users: dict[str, str] = {}  # Discord user id -> the person that account belongs to; empty = nobody

  @model_validator(mode="before")
  @classmethod
  def _reject_legacy_allow_list(cls, data: Any) -> Any:
    """Reject the retired ``allowed_user_ids`` list, naming its successor.

    The id -> person map replaced the plain id list: who asks is judged by the
    person an account maps to, so a bare id list carries too little. No
    compatibility path reads the old key.
    """
    if isinstance(data, dict) and "allowed_user_ids" in data:
      raise ValueError(
          "discord.allowed_user_ids is retired: move each allowed Discord user id into "
          "discord.allowed_users as <user id>: <the person that account belongs to>")
    return data
