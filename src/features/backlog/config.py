"""The ``backlog:`` config section model, registered in this package's ``register()``."""

import os

from pydantic import BaseModel, ConfigDict, model_validator


class BacklogRepoConfig(BaseModel):
  """A single backlog repo entry: label + path."""

  model_config = ConfigDict(extra='forbid')

  label: str
  path: str


class BacklogConfig(BaseModel):
  """``backlog:`` section: the repos the backlog panel lists."""

  model_config = ConfigDict(extra='forbid')

  # Backlog panel
  repos: list[BacklogRepoConfig] = []

  @model_validator(mode="after")
  def _expand_tilde(self) -> BacklogConfig:
    """Expand ``~`` in each backlog repo path."""
    for entry in self.repos:
      entry.path = os.path.expanduser(entry.path)
    return self
