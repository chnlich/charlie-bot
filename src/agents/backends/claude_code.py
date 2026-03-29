"""ClaudeCodeBackend — concrete AgentBackend wrapping the Claude Code CLI."""

from pathlib import Path

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
    if self._effort:
      self._cmd += ["--effort", self._effort]

  def _prepare_cwd(self, cwd: str) -> None:
    """Write CLAUDE.md into the cwd so Claude Code auto-detects it."""
    if not self._instructions_content:
      return
    claude_md = Path(cwd) / "CLAUDE.md"
    claude_md.write_text(self._instructions_content, encoding="utf-8")
    log.debug("claude_code_wrote_claude_md", path=str(claude_md))

  def _build_command(self, prompt: str) -> list[str]:
    return self._cmd + [prompt]
