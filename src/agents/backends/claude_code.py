"""ClaudeCodeBackend — concrete AgentBackend wrapping the Claude Code CLI."""

import os
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
    # Disable Claude Code tools that are unsafe in CharlieBot headless one-shot mode.
    # Besides scheduling/monitoring (scheduling tools are no-ops in -p mode, and Monitor
    # can create false recall expectations after external waits), this also blocks Claude
    # Code's subagent dispatch (Agent), workflow (Workflow), task system (Task*), and agent
    # comms (SendMessage/ListAgents) so the model routes async/background work through
    # CharlieBot's schedule-trigger / delegate mechanism instead.
    "--disallowed-tools",
    "Monitor,ScheduleWakeup,CronCreate,CronDelete,CronList,Agent,Workflow,TaskCreate,TaskGet,TaskUpdate,TaskList,TaskStop,TaskOutput,SendMessage,ListAgents",
]

# Subscription mode (cli_binary='claude-sub') drives an interactive `claude` TUI in
# tmux and cannot answer arrow-key menus. Additionally disallow the tools that raise
# such menus so the model emits plain-text choices instead of deadlocking the session.
SUBSCRIPTION_DISALLOWED_TOOLS = "AskUserQuestion,ExitPlanMode"

HEADLESS_CLAUDE_INVARIANT_ENV: dict[str, str] = {
    "CLAUDE_CODE_DISABLE_BACKGROUND_TASKS": "1",
}
HEADLESS_CLAUDE_FORWARDED_ENV_NAMES: tuple[str, ...] = (
    "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE",
    "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
)


def headless_claude_env() -> dict[str, str]:
  env = dict(HEADLESS_CLAUDE_INVARIANT_ENV)
  for name in HEADLESS_CLAUDE_FORWARDED_ENV_NAMES:
    if name in os.environ:
      env[name] = os.environ[name]
  return env


class ClaudeCodeBackend(AgentBackend):
  """Runs a Claude Code CLI subprocess and streams NDJSON events as dicts."""

  def __init__(
      self,
      *,
      model=None,
      effort=None,
      cli_binary=None,
      fast_mode=False,
      claude_session_id=None,
      claude_config_dir=None,
      **kwargs):
    super().__init__(model=model, **kwargs)
    self._effort = effort
    self._fast_mode = fast_mode
    self._claude_config_dir = str(Path(claude_config_dir).expanduser()) if claude_config_dir else None
    self._cmd: list[str] = list(BASE_COMMAND)
    if cli_binary:
      self._cmd[0] = cli_binary
      if cli_binary == "claude-sub":
        self._cmd += ["--disallowed-tools", SUBSCRIPTION_DISALLOWED_TOOLS]
    if claude_session_id:
      self._cmd += ["--session-id", claude_session_id]
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

  def _prepare_env(self, env: dict) -> dict:
    out = {**env, **headless_claude_env()}
    if self._claude_config_dir:
      out["CLAUDE_CONFIG_DIR"] = self._claude_config_dir
    return out

  def _build_command(self, prompt: str) -> list[str]:
    return list(self._cmd)

  def _stdin_prompt(self, prompt: str) -> str | None:
    return prompt
