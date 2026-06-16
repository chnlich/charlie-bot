"""ClaudeCodeBackend — concrete AgentBackend wrapping the Claude Code CLI."""

from pathlib import Path

import structlog

from src.agents.backends.base import AgentBackend
from src.core import event_types as ET

log = structlog.get_logger()

BASE_COMMAND: list[str] = [
    "claude",
    "-p",
    "--output-format",
    "stream-json",
    "--verbose",
    "--dangerously-skip-permissions",
    # Disable Claude Code scheduling/monitoring tools that are unsafe in CharlieBot
    # headless one-shot mode. In -p mode, scheduling tools are no-ops, and Monitor
    # can create false recall expectations after external waits.
    # Workers should use CharlieBot's schedule_trigger mechanism instead.
    "--disallowed-tools",
    "Monitor,ScheduleWakeup,CronCreate,CronDelete,CronList",
]

# Subscription mode (cli_binary='claude-sub') drives an interactive `claude` TUI in
# tmux and cannot answer arrow-key menus. Additionally disallow the tools that raise
# such menus so the model emits plain-text choices instead of deadlocking the session.
SUBSCRIPTION_DISALLOWED_TOOLS = "AskUserQuestion,ExitPlanMode"


class ClaudeCodeBackend(AgentBackend):
  """Runs a Claude Code CLI subprocess and streams NDJSON events as dicts."""

  def __init__(self, *, model=None, effort=None, cli_binary=None, fast_mode=False, **kwargs):
    super().__init__(model=model, **kwargs)
    self._effort = effort
    self._fast_mode = fast_mode
    self._cmd: list[str] = list(BASE_COMMAND)
    if cli_binary:
      self._cmd[0] = cli_binary
      if cli_binary == "claude-sub":
        self._cmd += ["--disallowed-tools", SUBSCRIPTION_DISALLOWED_TOOLS]
    if self._model:
      self._cmd += ["--model", self._model]
    if self._effort:
      self._cmd += ["--effort", self._effort]
    if self._fast_mode:
      self._cmd += ["--settings", '{"fastMode":true}']
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

  def translate_event(self, event: dict) -> list[dict]:
    """Pass Claude Code stream-json events through and surface thinking blocks.

    Claude Code emits extended thinking as assistant message content blocks of
    type ``thinking`` (``{"type": "thinking", "thinking": "..."}``). These
    blocks are invisible to text extraction, so we translate them into
    standalone ``thinking`` events that the UI can render.
    """
    results: list[dict] = []
    results.append(event)
    if event.get("type") != ET.ASSISTANT:
      return results
    msg = event.get("message") or {}
    blocks = msg.get("content") or []
    if not isinstance(blocks, list):
      return results
    for block in blocks:
      if not isinstance(block, dict):
        continue
      if block.get("type") != "thinking":
        continue
      thinking_text = block.get("thinking") or block.get("text", "")
      if thinking_text:
        results.append({"type": ET.THINKING, "content": str(thinking_text)})
    return results
