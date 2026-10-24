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
  """One Waiting on you item: a decision or an action, its context, and the link to where the user answers it.

  ``since`` and ``since_exact`` are written by the server. ``since`` is the time the item first appeared or a lower
  bound on it, and ``since_exact`` is false when ``since`` is that lower bound. An item stored before these fields
  existed carries neither; the status read fills them from the status time.
  """
  model_config = pydantic.ConfigDict(extra="forbid")

  text: Text
  # The line's state, why the decision comes now, the options and their consequences, and the evidence links. An
  # item stored before the field existed serializes without it, so the stored shape stays as it was.
  context: Text | None = pydantic.Field(default=None, exclude_if=lambda value: value is None)
  link: str | None = None
  since: models.UtcDatetime | None = pydantic.Field(default=None, exclude_if=lambda value: value is None)
  since_exact: bool | None = pydantic.Field(default=None, exclude_if=lambda value: value is None)


class WebStatus(pydantic.BaseModel):
  """The session status: Goal, Now and Waiting on you, and the time the last set stored them."""
  goal: Text | None = None
  now: Text | None = None
  waiting: list[WaitingItem] = pydantic.Field(default_factory=list)
  updated_at: models.UtcDatetime = pydantic.Field(default_factory=models.utc_now)


class WebReplyMetadata(pydantic.BaseModel):
  """The top-level metadata key the web reply package owns: the session status, None until the first set."""
  web_status: WebStatus | None = None
