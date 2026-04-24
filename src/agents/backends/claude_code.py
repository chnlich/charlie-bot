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
    # Disable scheduling tools that only work in interactive Claude Code sessions.
    # In -p (one-shot) mode these tools are no-ops, but the model may still attempt
    # to use them (e.g. "I'll check back later"), creating false promises.
    # Workers should use CharlieBot's schedule_trigger mechanism instead.
    "--disallowed-tools",
    "ScheduleWakeup,CronCreate,CronDelete,CronList",
]


class ClaudeCodeBackend(AgentBackend):
  """Runs a Claude Code CLI subprocess and streams NDJSON events as dicts."""

  def __init__(self, *, model=None, **kwargs):
    super().__init__(model=model, **kwargs)
    self._cmd: list[str] = list(BASE_COMMAND)
    if self._model:
      self._cmd += ["--model", self._model]
    if self._extra_flags:
      self._cmd += self._extra_flags

  def _prepare_cwd(self, cwd: str) -> None:
    """Write CLAUDE.md into the cwd so Claude Code auto-detects it."""
    if not self._instructions_content:
      return
    claude_md = Path(cwd) / "CLAUDE.md"
    claude_md.write_text(self._instructions_content, encoding="utf-8")
    log.debug("claude_code_wrote_claude_md", path=str(claude_md))

  def _build_command(self, prompt: str) -> list[str]:
    return self._cmd + ["--", prompt]
