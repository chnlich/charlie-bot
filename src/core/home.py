"""The profile-home and claude-login-directory resolution: the one place that
reads ``CHARLIEBOT_HOME`` and the Claude login-dir derivations.

Every state path derives from :func:`charliebot_home_dir`. The memory CLI
imports from here directly: its store root is a pure derivation of the home,
so resolving it must not drag the config model stack (src.core.config's
pydantic chain, ~180 ms of the M98 CLI wall) into a fresh process. Other
config-importing readers reach these names through the src.core.config
re-export.
"""

import os
from pathlib import Path

CHARLIEBOT_HOME_ENV = "CHARLIEBOT_HOME"

# Claude Code's login-directory env var, a cross-process wire contract: the server
# writes it onto a cc-claude child (claude_code._prepare_env, the tmux spawn in
# src/cli/claude_sub.py), the pool strips any inherited value where it pinned the
# directory itself (master_cc_run, claude_compaction.compaction_env), and the
# in-process readers in src.core.config.claude_config_dir, tui/_claude_config_path,
# and claude_sub read it back. One spelling everywhere. It lives beside the profile
# home so the worker binary's launch path (src.cli.claude_sub) resolves it without
# the config model stack.
CLAUDE_CONFIG_DIR_ENV_VAR = "CLAUDE_CONFIG_DIR"

# The OAuth credential filename inside a login directory: claude-sub snapshots it
# into the session-only config overlay, the account pool reads it for health, and
# the usage provider derives its per-account path from it. It lives beside the
# login-dir names so the claude-sub launch resolves it without the account pool's
# pydantic models.
CREDENTIALS_FILE = ".credentials.json"


def default_claude_dir() -> Path:
  """The default claude login directory (``~/.claude``), read from HOME on every call.

  The terminal fallback of :func:`src.core.config.claude_config_dir`'s order and
  the root the cold-storage, autonamer, tui, and claude-sub readers re-derive per
  call, so those honor a redirected HOME (tests isolate stores that way); the
  tally layer freezes an import-time copy in ``token_tally.DEFAULT_CLAUDE_DIR``.
  """
  return Path.home() / ".claude"


# The resolved home and its string form, per raw ``CHARLIEBOT_HOME`` value plus
# ``HOME`` (``""`` raw is the default home, and a ``~`` value derives from
# HOME). The env values are the process's profile identity, fixed for the
# process life, while resolve() is a per-component symlink walk and
# ``Path.home()``/``str(Path)`` re-parse the path — per-request-fingerprint
# work on every call if repeated. Both public readers serve the same cached
# entry, so a caller comparing its home against the default sees one answer.
_home_cache: dict[tuple[str, str], tuple[Path, str]] = {}


def _home_cached(raw: str) -> tuple[Path, str]:
  """The home for *raw* (``""`` is the default) as ``(Path, str)``, resolved once per env pair."""
  key = (raw, os.environ.get("HOME", ""))
  cached = _home_cache.get(key)
  if cached is None:
    home = Path(raw).expanduser().resolve() if raw else Path.home() / ".charliebot"
    cached = (home, str(home))
    _home_cache[key] = cached
  return cached


def _resolve_home() -> tuple[Path, str]:
  """The validated profile home as ``(Path, str)``."""
  raw = os.environ.get(CHARLIEBOT_HOME_ENV, "").strip()
  if raw and not raw.startswith(("~", "/")):
    raise ValueError(f"{CHARLIEBOT_HOME_ENV} must be an absolute path or start with '~'; got {raw!r}")
  return _home_cached(raw)


def default_charliebot_home() -> Path:
  """The state directory used when ``CHARLIEBOT_HOME`` is unset."""
  return _home_cached("")[0]


def charliebot_home_dir() -> Path:
  """Return the state directory this process belongs to (its profile).

  ``CHARLIEBOT_HOME`` selects the profile: unset or empty gives the default
  ``~/.charliebot``. This is the only place that resolves the home path; every
  other path is derived from :attr:`CharlieBotConfig.charliebot_home`. The one
  raw read of the variable outside this function is the web terminal's profile
  check (``src/agents/backends/terminal.py``): a tmux pane inherits the tmux
  server's environment rather than this process's, so the terminal checks
  whether a profile is set and passes the resolved home to new panes explicitly.

  A set value must be absolute or start with ``~``. A relative value would be
  resolved against each process's own working directory, silently handing the
  server, the CLI and every worker a different home, so it is rejected here
  instead of surfacing later as a write into the wrong profile.
  """
  return _resolve_home()[0]
