"""Claude CLI launch vocabulary: permission flags, argv assembly, headless env.

Stdlib-only by contract: the claude-sub worker binary imports this module on
every launch (the M108 floor, docs/perf_baseline.md), so nothing here may
reach the backend ABC, the config model stack, or pydantic. Consumers read
these names from three homes: base (src.agents.backends.base) re-exports the
two permission flags for its established import path, claude_code
(src.agents.backends.claude_code) imports the env vocabulary in-file and is
the path session_usage, claude_compaction, and the session-usage tests read
it through, and every other reader imports from here.
"""

import os

# The flag that suppresses the CLI's interactive permission prompt. Its
# spelling is fixed by the vendor CLI contract, not by this repo, so every
# Claude-compatible launcher here (claude headless/TUI, claude-sub, agy,
# opencode) must pass the same literal.
SKIP_PERMISSIONS_FLAG = "--dangerously-skip-permissions"

# Settings-side companion of SKIP_PERMISSIONS_FLAG: with the flag passed, an
# interactive launch still pops a one-time dangerous-mode confirmation unless
# this key is set. Same vendor-fixed spelling, so the claude TUI and
# claude-sub both pin the same dict.
SKIP_PERMISSIONS_SETTINGS = {"skipDangerousModePermissionPrompt": True}

# CharlieBot sessions must not sync claude.ai connectors at all: the CLI
# announces every unauthorized connector at startup, and its announce-once
# dedup cache (mcp-needs-auth-cache.json) lives in the config directory —
# claude-sub gives each session a fresh CLAUDE_CONFIG_DIR, so the cache never
# carries over and the announcement would replay every launch. Every Claude
# launch path merges this into its single --settings JSON.
DISABLE_CONNECTOR_SETTINGS = {"disableClaudeAiConnectors": True}

# The tool-deny flag's spelling is vendor-fixed like SKIP_PERMISSIONS_FLAG's; the
# CLI also accepts a camelCase alias, which only the claude-sub parser mirrors.
DISALLOWED_TOOLS_FLAG = "--disallowed-tools"


def build_claude_argv(
    session_id: str,
    resume: bool,
    *,
    settings: str,
    plugin_dir: str | None = None,
    model: str | None = None,
    effort: str | None = None,
    disallowed_tools: list[str] | None = None,
    prompt: str | None = None,
) -> list[str]:
  """Assemble the `claude` CLI launch argv shared by the interactive launchers.

  *settings* is the pre-serialized ``--settings`` JSON value. *resume*
  selects ``--resume`` over ``--session-id``. *plugin_dir*, *model*,
  *effort*, *disallowed_tools*, and *prompt* are appended only when
  provided; an empty *prompt* still gets its ``--`` separator.
  """
  argv = [
      "claude",
      "--settings",
      settings,
      SKIP_PERMISSIONS_FLAG,
  ]
  if plugin_dir is not None:
    argv.extend(["--plugin-dir", plugin_dir])
  argv.extend(["--resume" if resume else "--session-id", session_id])
  if model:
    argv.extend(["--model", model])
  if effort:
    argv.extend(["--effort", effort])
  # Collapse every incoming entry into one comma-joined value: the launched `claude`
  # reliably honors a single disallowed-tools flag, not repeated ones.
  if disallowed_tools:
    argv.extend([DISALLOWED_TOOLS_FLAG, ",".join(disallowed_tools)])
  if prompt is not None:
    # Claude Code 2.1.212 accepts `--` and treats the following value as the prompt,
    # even when it starts with '-'.  tmux respawn-pane passes these argv entries
    # directly to Claude; it does not invoke a shell for the command after the target.
    argv.extend(["--", prompt])
  return argv


# One spelling per Claude Code variable the declared-window logic touches: the
# default pin, the forward allowlist, and the degradation checks must agree.
AUTO_COMPACT_WINDOW_ENV = "CLAUDE_CODE_AUTO_COMPACT_WINDOW"
AUTOCOMPACT_PCT_OVERRIDE_ENV = "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE"
MAX_CONTEXT_TOKENS_ENV = "CLAUDE_CODE_MAX_CONTEXT_TOKENS"

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
