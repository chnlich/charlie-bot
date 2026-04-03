"""ClaudeCodeBackend — concrete AgentBackend wrapping the Claude Code CLI."""

import structlog

from src.agents.backends.base import AgentBackend

log = structlog.get_logger()

BASE_COMMAND: list[str] = [
    "claude",
    "-p",
    "--output-format",
    "stream-json",
    "--verbose",
    "--dangerously-skip-permissions",
]


class ClaudeCodeBackend(AgentBackend):
  """Runs a Claude Code CLI subprocess and streams NDJSON events as dicts."""

  def __init__(self, *, model=None, effort=None, **kwargs):
    super().__init__(model=model, **kwargs)
    self._effort = effort
    self._cmd: list[str] = list(BASE_COMMAND)
    if self._model:
      self._cmd += ["--model", self._model]
    if self._extra_flags:
      self._cmd += self._extra_flags
    # TODO: re-enable when Claude Code version supports --effort
    # if self._effort:
    #   self._cmd += ["--effort", self._effort]

  def _build_command(self, prompt: str) -> list[str]:
    return self._cmd + [prompt]
