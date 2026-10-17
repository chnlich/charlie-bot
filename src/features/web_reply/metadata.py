"""Metadata fields owned by the web reply package: the session status that a web session's agent keeps."""

from typing import Annotated

import pydantic

from src.infra import models


def _reject_blank(value: str) -> str:
  if not value.strip():
    raise ValueError("must not be blank")
  return value


# A status item's text: a string with at least one non-space character.
Text = Annotated[str, pydantic.AfterValidator(_reject_blank)]


class WaitingItem(pydantic.BaseModel):
  """One Waiting on you item: a decision or an action, and the link to the place where the user answers it."""
  model_config = pydantic.ConfigDict(extra="forbid")

  text: Text
  link: str | None = None


class WebStatus(pydantic.BaseModel):
  """The session status: Goal, Now and Waiting on you, and the time the last set stored them."""
  goal: Text | None = None
  now: Text | None = None
  waiting: list[WaitingItem] = pydantic.Field(default_factory=list)
  updated_at: models.UtcDatetime = pydantic.Field(default_factory=models.utc_now)


class WebReplyMetadata(pydantic.BaseModel):
  """The top-level metadata key the web reply package owns: the session status, None until the first set."""
  web_status: WebStatus | None = None
