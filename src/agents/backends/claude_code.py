"""ClaudeCodeBackend — concrete AgentBackend wrapping the Claude Code CLI."""

import asyncio
import os
import re
import signal
from collections.abc import Mapping
from pathlib import Path

import structlog

from src.agents.backends.base import SKIP_PERMISSIONS_FLAG, AgentBackend
from src.core import event_types as ET
from src.core.process import kill_process_group

log = structlog.get_logger()

BASE_COMMAND: list[str] = [
    "claude",
    "-p",
    "--output-format",
    "stream-json",
    "--verbose",
    SKIP_PERMISSIONS_FLAG,
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
# One spelling per Claude Code variable the declared-window logic touches: the
# default pin, the forward allowlist, and the degradation checks must agree.
AUTO_COMPACT_WINDOW_ENV = "CLAUDE_CODE_AUTO_COMPACT_WINDOW"
AUTOCOMPACT_PCT_OVERRIDE_ENV = "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE"
MAX_CONTEXT_TOKENS_ENV = "CLAUDE_CODE_MAX_CONTEXT_TOKENS"

# CharlieBot-chosen defaults, applied only when the host has not set the variable.
# Claude Code compacts at window - min(max_output_tokens, 20000) - 13000, so declaring
# a 433000 window puts the compaction point at 400000 tokens (433000 = 400000 + 13000
# + 20000) instead of ~967000 under the model's full 1M window. The 13000 and 20000
# terms are Claude Code internals: if a CLI upgrade changes them the compaction point
# drifts silently and this constant has to be recomputed.
HEADLESS_CLAUDE_DEFAULT_ENV: dict[str, str] = {
    AUTO_COMPACT_WINDOW_ENV: "433000",
}
HEADLESS_CLAUDE_FORWARDED_ENV_NAMES: tuple[str, ...] = (
    AUTOCOMPACT_PCT_OVERRIDE_ENV,
    MAX_CONTEXT_TOKENS_ENV,
    AUTO_COMPACT_WINDOW_ENV,
    "CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT",
)

# The two Claude Code internals subtracted from the declared auto-compact window to
# reach the real compaction point (see comment above HEADLESS_CLAUDE_DEFAULT_ENV).
CLAUDE_COMPACT_OUTPUT_RESERVE = 20_000
CLAUDE_COMPACT_CONTEXT_RESERVE = 13_000

# Usage resolution re-derives the declared window per call while the environment a
# degradation warning reports is fixed for the process's life, so the first sighting
# of each reported shape is the whole alarm and every repeat re-fires it.
_DECLARED_WINDOW_WARNINGS_SEEN: set[tuple[str, str, tuple[tuple[str, str], ...]]] = set()


def _warn_declared_window_once(event: str, *, variable: str, **fields: str) -> None:
  """Log one declared-window degradation per reportable shape per process.

  A caller relies on at most one line per (event, variable, logged fields): the
  first sighting carries the full signal and a later call on unchanged environment
  repeats a fired alarm. The key is exactly the fields the line logs, so an edit
  that swaps one bad setting for another earns one new line, and nothing outside
  the log statement can drift the key away from what was reported.
  """
  key = (event, variable, tuple(sorted(fields.items())))
  if key in _DECLARED_WINDOW_WARNINGS_SEEN:
    return
  _DECLARED_WINDOW_WARNINGS_SEEN.add(key)
  log.warning(event, variable=variable, **fields)


def _reset_declared_window_warnings_for_tests() -> None:
  """Clear the warn-once registry, restoring the process-start state."""
  _DECLARED_WINDOW_WARNINGS_SEEN.clear()


def claude_supervisor_env(env: Mapping[str, str]) -> dict[str, str]:
  """Environment for a supervisor process whose children run Claude Code.

  Three pins travel together for every supervisor (master, worker): the
  inherited ``CLAUDECODE`` marker is stripped — a child ``claude`` refuses to
  launch when it detects a parent session — ``CLAUDE_CODE_DISABLE_AUTO_MEMORY``
  stays pinned, so a child's auto-memory writes stay off and CharlieBot's own
  memory store remains the only one, and an inherited ``CHARLIEBOT_SESSION_ID``
  is stripped, so only the id a caller writes afterwards travels on: a master
  gets its own session's id (see ``master_cc_run._build_master_env``) and a
  worker gets none, whatever session's environment started the server.
  Returns a copy; the argument is not mutated.
  """
  out = dict(env)
  out.pop("CLAUDECODE", None)
  out.pop("CHARLIEBOT_SESSION_ID", None)
  out["CLAUDE_CODE_DISABLE_AUTO_MEMORY"] = "1"
  return out


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
    default and ``compact_point`` is derived normally; one ``log.warning`` per
    process names the responsible variable.
  - a forwarded-but-unmodelled override (``CLAUDE_AUTOCOMPACT_PCT_OVERRIDE``,
    ``CLAUDE_CODE_MAX_CONTEXT_TOKENS``) is present: ``declared_window`` is the parsed
    window and ``compact_point`` is ``None`` (the caller then reports no compaction
    line); one ``log.warning`` per process names the responsible variable.
  """
  env = headless_claude_env()
  raw_window = env.get(
      AUTO_COMPACT_WINDOW_ENV,
      HEADLESS_CLAUDE_DEFAULT_ENV[AUTO_COMPACT_WINDOW_ENV],
  )
  default_window = int(HEADLESS_CLAUDE_DEFAULT_ENV[AUTO_COMPACT_WINDOW_ENV])

  try:
    window = int(raw_window)
    if window <= 0:
      raise ValueError(f"non-positive window: {raw_window!r}")
  except (ValueError, TypeError):
    _warn_declared_window_once(
        "claude_declared_window_unparseable_window",
        variable=AUTO_COMPACT_WINDOW_ENV,
        window=raw_window,
    )
    window = default_window

  for override_name in (AUTOCOMPACT_PCT_OVERRIDE_ENV, MAX_CONTEXT_TOKENS_ENV):
    if override_name in env:
      _warn_declared_window_once(
          "claude_declared_window_degraded",
          variable=override_name,
          value=env[override_name],
          reason="forwarded but semantics not modelled; returning declared window without compaction point",
      )
      return window, None

  return window, window - CLAUDE_COMPACT_OUTPUT_RESERVE - CLAUDE_COMPACT_CONTEXT_RESERVE


# The CLI's synthetic assistant events (errors, injected notices) carry this
# sentinel as message.model; they name no real model and never count as one.
_SYNTHETIC_MODEL = "<synthetic>"

# One trailing 8-digit date segment, e.g. claude-haiku-4-5-20251001. Stripped
# from both sides before the family comparison so a dated basename and its
# bare spelling land in the same family.
_TRAILING_DATE_SEGMENT = re.compile(r"-\d{8}$")


def _family_membership(served: str, configured: str) -> bool:
  """Whether *served* and *configured* name models of the same Claude family.

  Both sides first lose one trailing 8-digit date segment
  (claude-haiku-4-5-20251001 -> claude-haiku-4-5), then the shorter name must
  be the longer name or its prefix at a complete dash-segment boundary:
  claude-fable-5 is a segment prefix of claude-fable-5-1 (same family), while
  claude-opus-4-8 and claude-opus-5 are segment prefixes of neither
  (different families). Stripping any trailing numeric segment instead would
  cut the main healthy spelling claude-fable-5 down to claude-fable and
  misjudge it as out-of-family.
  """
  served_stripped = _TRAILING_DATE_SEGMENT.sub("", served)
  configured_stripped = _TRAILING_DATE_SEGMENT.sub("", configured)
  shorter, longer = sorted((served_stripped, configured_stripped), key=len)
  return longer == shorter or longer.startswith(f"{shorter}-")


def out_of_family_served_models(events: list[dict], configured_model: str | None) -> list[str]:
  """Models outside *configured_model*'s family that authored a round's visible reply.

  Pure detector over parsed (projected) event dicts. The author of the visible
  reply is a main-chain assistant event — no ``parent_tool_use_id`` — carrying
  at least one ``{"type": "text"}`` content block, the blocks the aggregator
  merges into the rendered reply text; thinking and tool_use blocks never
  qualify, and only ``message.model`` is read (never modelUsage, whose healthy
  key set routinely includes haiku). Events with a missing/null model or the
  CLI's ``<synthetic>`` sentinel are skipped. Returns the raw model names in
  first-appearance order, deduplicated. Empty when every text-authoring model
  is in-family, and when *configured_model* is empty — a family backend with
  no model pin never produces a notice.
  """
  if not configured_model:
    return []
  served: list[str] = []
  seen: set[str] = set()
  for event in events:
    if event.get("parent_tool_use_id"):
      continue
    if event.get("type") != ET.ASSISTANT:
      continue
    message = event.get("message")
    if not isinstance(message, dict):
      continue
    model = message.get("model")
    if not model or model == _SYNTHETIC_MODEL:
      continue
    blocks = message.get("content")
    if not isinstance(blocks, list):
      continue
    if not any(isinstance(block, dict) and block.get("type") == "text" for block in blocks):
      continue
    if _family_membership(model, configured_model):
      continue
    if model not in seen:
      seen.add(model)
      served.append(model)
  return served


class ClaudeCodeBackend(AgentBackend):
  """Runs a Claude Code CLI subprocess and streams NDJSON events as dicts."""

  def __init__(
      self,
      *,
      model: str | None = None,
      effort: str | None = None,
      cli_binary: str | None = None,
      fast_mode: bool = False,
      claude_session_id: str | None = None,
      claude_config_dir: str | None = None,
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
    self._write_instructions_file(cwd, "CLAUDE.md", "claude_code_wrote_claude_md")

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
    except TimeoutError:
      kill_process_group(proc.pid, signal.SIGKILL)
      raise
    if proc.returncode != 0:
      raise RuntimeError(f"claude CLI failed (exit {proc.returncode}): {stderr.decode().strip()}")
    return stdout.decode().strip()


class AnthropicEndpointBackend(ClaudeCodeBackend):
  """Claude Code pointed at a non-Anthropic Anthropic-compatible endpoint by env.

  The endpoint URL and bearer token ride ANTHROPIC_BASE_URL / ANTHROPIC_AUTH_TOKEN,
  and claude_model_env pins every model-selection variable to one model, so the
  ``claude`` binary must not see a ``--model`` flag (it would override the env).
  Subclasses own where the endpoint and credential come from.
  """

  def __init__(self, *, base_url: str, auth_token: str, model: str, **kwargs):
    self._base_url = base_url
    self._auth_token = auth_token
    self._env_model = model
    super().__init__(model=None, **kwargs)

  def _prepare_env(self, env: dict) -> dict:
    return {
        **super()._prepare_env(env),
        "ANTHROPIC_BASE_URL": self._base_url,
        "ANTHROPIC_AUTH_TOKEN": self._auth_token,
        **claude_model_env(self._env_model),
    }
