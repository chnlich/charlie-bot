"""Backlog's config models, registered in this package's ``register()``.

``BacklogConfig`` is the ``backlog:`` config section. ``ImprovementLoopConfig`` is the ``loop:``
section of a scheduled task, registered with the loop action.
"""

import os

import pydantic


class BacklogRepoConfig(pydantic.BaseModel):
  """A single backlog repo entry: label + path."""

  model_config = pydantic.ConfigDict(extra='forbid')

  label: str
  path: str


class BacklogConfig(pydantic.BaseModel):
  """``backlog:`` section: the repos the backlog panel lists."""

  model_config = pydantic.ConfigDict(extra='forbid')

  # Backlog panel
  repos: list[BacklogRepoConfig] = []

  @pydantic.model_validator(mode="after")
  def _expand_tilde(self) -> BacklogConfig:
    """Expand ``~`` in each backlog repo path."""
    for entry in self.repos:
      entry.path = os.path.expanduser(entry.path)
    return self


class ImprovementLoopConfig(pydantic.BaseModel):
  """Declarative config for an improvement-loop cron task."""

  backlog: str  # relative path within repo, e.g. 'backlog/backlog.yaml'
  role: str  # agent role description
  scope_files: list[str]  # files/dirs agent may modify
  id_prefix: str = ''  # e.g. 'D' for D-001, empty for plain 001
  language: str = 'en'  # 'en' or 'zh-CN'
  max_pending: int = 10
  stale_timeout_hours: float = 1.0
  state_files: list[str] = []  # extra files to read before acting
  verify: list[str] = []  # shell commands to run after implementing
  scan_prompt: str = ''  # module-specific instructions for health scan step
  idea_prompt: str = ''  # what to think about when generating new ideas
  extra_rules: list[str] = []  # module-specific rules appended to prompt
