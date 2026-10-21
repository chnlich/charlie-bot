"""Claude CLI launch vocabulary: the per-run-kind environment and settings."""

import os

from src.infra import models

# CharlieBot sessions must not sync claude.ai connectors at all: the CLI
# announces every unauthorized connector at startup, and its announce-once
# dedup cache (mcp-needs-auth-cache.json) lives in the config directory.
# Every Claude launch path merges this into its single --settings JSON.
DISABLE_CONNECTOR_SETTINGS = {"disableClaudeAiConnectors": True}


def headless_claude_settings(run_kind: models.RunKind) -> dict[str, bool]:
  """The ``--settings`` values for a headless Claude Code process of *run_kind*."""
  settings = {
      **DISABLE_CONNECTOR_SETTINGS,
      "syncClaudeAiSkills": False,  # No run kind uses synced skills.
  }
  if run_kind != "manager_turn":
    # Bundled skills have uses only in manager turns.
    settings["disableBundledSkills"] = True
  return settings


# One spelling per Claude Code variable the declared-window logic touches: the
# default pin, the forward allowlist, and the degradation checks must agree.
AUTO_COMPACT_WINDOW_ENV = "CLAUDE_CODE_AUTO_COMPACT_WINDOW"
AUTOCOMPACT_PCT_OVERRIDE_ENV = "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE"
MAX_CONTEXT_TOKENS_ENV = "CLAUDE_CODE_MAX_CONTEXT_TOKENS"
THINKING_DISPLAY_UPDATES_ENV = "CLAUDE_CODE_THINKING_DISPLAY_UPDATES"

# The waiting rule in prompts/task_base.md sets a 15-minute foreground limit.
# Claude Code's Bash tool defaults to 2 minutes and caps at 10 minutes without
# these variables, so a foreground command the rule allows would be killed early.
# The advisor tool goes uncalled in headless CharlieBot runs, so its tool
# description only spends request tokens.
HEADLESS_CLAUDE_INVARIANT_ENV: dict[str, str] = {
    "CLAUDE_CODE_DISABLE_BACKGROUND_TASKS": "1",
    "BASH_DEFAULT_TIMEOUT_MS": "900000",
    "BASH_MAX_TIMEOUT_MS": "900000",
    "CLAUDE_CODE_DISABLE_ADVISOR_TOOL": "1",
}

# The auto-compact window each run kind declares, unless the host exports one.
# Claude Code compacts at window - min(max_output_tokens, 20000) - 13000.
# A 433000 window puts every kind's compaction point at 400000.
# See CLAUDE_COMPACT_OUTPUT_RESERVE and CLAUDE_COMPACT_CONTEXT_RESERVE below.
DECLARED_WINDOW_BY_RUN_KIND: dict[models.RunKind, int] = {
    "manager_turn": 433_000,
    "work": 433_000,
    "review": 433_000,
    "iteration": 433_000,
    "scheduled_step": 433_000,
}

HEADLESS_CLAUDE_FORWARDED_ENV_NAMES: tuple[str, ...] = (
    AUTOCOMPACT_PCT_OVERRIDE_ENV,
    MAX_CONTEXT_TOKENS_ENV,
    AUTO_COMPACT_WINDOW_ENV,
    "CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT",
)

# The two Claude Code internals subtracted from the declared auto-compact window to
# reach the real compaction point (see the comment above DECLARED_WINDOW_BY_RUN_KIND).
CLAUDE_COMPACT_OUTPUT_RESERVE = 20_000
CLAUDE_COMPACT_CONTEXT_RESERVE = 13_000


def headless_claude_env(run_kind: models.RunKind) -> dict[str, str]:
  """Environment for every headless Claude Code subprocess of a *run_kind* run.

  Layered so a host export beats CharlieBot's per-kind default: invariants first,
  the kind's declared window next, allowlisted host values last.
  """
  env = {**HEADLESS_CLAUDE_INVARIANT_ENV, AUTO_COMPACT_WINDOW_ENV: str(DECLARED_WINDOW_BY_RUN_KIND[run_kind])}
  for name in HEADLESS_CLAUDE_FORWARDED_ENV_NAMES:
    if name in os.environ:
      env[name] = os.environ[name]
  return env
