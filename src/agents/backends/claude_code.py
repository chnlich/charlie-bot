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

  def __init__(self, **kwargs):
    model = kwargs.pop("model", None)
    instructions_content = kwargs.pop("instructions_content", None)
    extra_flags = kwargs.pop("extra_flags", None)
    super().__init__(
        model=model,
        instructions_content=instructions_content,
        extra_flags=extra_flags,
        **kwargs)
    self._cmd: list[str] = list(BASE_COMMAND)
    if model:
      self._cmd += ["--model", model]
    if extra_flags:
      self._cmd += extra_flags

  def _prepare_cwd(self, cwd: str) -> None:
    """Write CLAUDE.md into the cwd so Claude Code auto-detects it."""
    if not self._instructions_content:
      return
    claude_md = Path(cwd) / "CLAUDE.md"
    claude_md.write_text(self._instructions_content, encoding="utf-8")
    log.debug("claude_code_wrote_claude_md", path=str(claude_md))

  def _build_command(self, prompt: str) -> list[str]:
    return self._cmd + [prompt]
