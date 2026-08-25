"""ClaudeCodeBackend — concrete AgentBackend wrapping the Claude Code CLI."""

import asyncio
import os
import signal
from pathlib import Path

import structlog

from src.agents.backends.base import AgentBackend
from src.core.process import kill_process_group

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
# CharlieBot-chosen defaults, applied only when the host has not set the variable.
# Claude Code compacts at window - min(max_output_tokens, 20000) - 13000, so declaring
# a 433000 window puts the compaction point at 400000 tokens (433000 = 400000 + 13000
# + 20000) instead of ~967000 under the model's full 1M window. The 13000 and 20000
# terms are Claude Code internals: if a CLI upgrade changes them the compaction point
# drifts silently and this constant has to be recomputed.
HEADLESS_CLAUDE_DEFAULT_ENV: dict[str, str] = {
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "433000",
}
HEADLESS_CLAUDE_FORWARDED_ENV_NAMES: tuple[str, ...] = (
    "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE",
    "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
    "CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT",
)

# The two Claude Code internals subtracted from the declared auto-compact window to
# reach the real compaction point (see comment above HEADLESS_CLAUDE_DEFAULT_ENV).
CLAUDE_COMPACT_OUTPUT_RESERVE = 20_000
CLAUDE_COMPACT_CONTEXT_RESERVE = 13_000


def headless_claude_env() -> dict[str, str]:
  """Environment for every headless Claude Code subprocess.

  Layered so a host export beats CharlieBot's own default: invariants first,
  CharlieBot defaults next, allowlisted host values last.
  """
  env = {**HEADLESS_CLAUDE_INVARIANT_ENV, **HEADLESS_CLAUDE_DEFAULT_ENV}
  for name in HEADLESS_CLAUDE_FORWARDED_ENV_NAMES:
    if name in os.environ:
      env[name] = os.environ[name]
  return env


def claude_model_env(model: str) -> dict[str, str]:
  """Every Claude Code model-selection variable pinned to one model.

  Claude Code reads the main model, the opus/sonnet/haiku slot defaults, and the
  subagent model from separate variables. A backend that selects the model by env
  (no --model flag) sets them all, or an unset slot falls back to a CLI default.
  """
  return {
      "ANTHROPIC_MODEL": model,
      "ANTHROPIC_DEFAULT_OPUS_MODEL": model,
      "ANTHROPIC_DEFAULT_SONNET_MODEL": model,
      "ANTHROPIC_DEFAULT_HAIKU_MODEL": model,
      "CLAUDE_CODE_SUBAGENT_MODEL": model,
  }


def headless_claude_declared_window() -> tuple[int, int | None]:
  """Return ``(declared_window, compact_point | None)`` for a headless Claude Code run.

  Reads the same environment ``headless_claude_env()`` gives the CLI, so a host
  export of ``CLAUDE_CODE_AUTO_COMPACT_WINDOW`` overrides CharlieBot's default and
  the declared window follows it. Computed per call — never cached in a module
  constant — because the host environment can change between calls.

  ``declared_window`` is the auto-compact window the CLI advertises. ``compact_point``
  is ``declared_window − OUTPUT_RESERVE − CONTEXT_RESERVE`` (with the 433000 default
  this is 400000); the caller derives the real compaction point from the effective
  ``context_full`` (``min(model contextWindow, declared_window)``) using the same
  constants. Degrades loudly, never silently:

  - unparseable / non-positive window: ``declared_window`` falls back to the 433000
    default and ``compact_point`` is derived normally; one ``log.warning`` names the
    responsible variable.
  - a forwarded-but-unmodelled override (``CLAUDE_AUTOCOMPACT_PCT_OVERRIDE``,
    ``CLAUDE_CODE_MAX_CONTEXT_TOKENS``) is present: ``declared_window`` is the parsed
    window and ``compact_point`` is ``None`` (the caller then reports no compaction
    line); one ``log.warning`` names the responsible variable.
  """
  env = headless_claude_env()
  raw_window = env.get(
      "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
      HEADLESS_CLAUDE_DEFAULT_ENV["CLAUDE_CODE_AUTO_COMPACT_WINDOW"],
  )
  default_window = int(HEADLESS_CLAUDE_DEFAULT_ENV["CLAUDE_CODE_AUTO_COMPACT_WINDOW"])

  try:
    window = int(raw_window)
    if window <= 0:
      raise ValueError(f"non-positive window: {raw_window!r}")
  except (ValueError, TypeError):
    log.warning(
        "claude_declared_window_unparseable_window",
        variable="CLAUDE_CODE_AUTO_COMPACT_WINDOW",
        window=raw_window,
    )
    window = default_window

  for override_name in ("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE", "CLAUDE_CODE_MAX_CONTEXT_TOKENS"):
    if override_name in env:
      log.warning(
          "claude_declared_window_degraded",
          variable=override_name,
          reason="forwarded but semantics not modelled; returning declared window without compaction point",
      )
      return window, None

  return window, window - CLAUDE_COMPACT_OUTPUT_RESERVE - CLAUDE_COMPACT_CONTEXT_RESERVE


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

  async def one_shot_text(self, prompt: str, system_prompt: str, *, timeout: float) -> str:
    """Generate text via `claude -p` in text mode, using this backend's model.

    Print mode with tools disabled: no session persistence, prompt on stdin,
    plain-text stdout. The process group is killed on timeout.
    """
    proc = await asyncio.create_subprocess_exec(
        "claude",
        "-p",
        "--output-format",
        "text",
        "--no-session-persistence",
        "--model",
        self._model,
        "--system-prompt",
        system_prompt,
        "--disallowed-tools",
        "Bash,Read,Write,Edit,Glob,Grep,Agent",
        stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        start_new_session=True,
    )
    try:
      stdout, stderr = await asyncio.wait_for(proc.communicate(input=prompt.encode()), timeout=timeout)
    except asyncio.TimeoutError:
      kill_process_group(proc.pid, signal.SIGKILL)
      raise
    if proc.returncode != 0:
      raise RuntimeError(f"claude CLI failed (exit {proc.returncode}): {stderr.decode().strip()}")
    return stdout.decode().strip()
