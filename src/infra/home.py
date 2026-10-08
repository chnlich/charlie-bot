"""The profile-home and claude-login-directory resolution: the one place that
reads ``CHARLIEBOT_HOME`` and the Claude login-dir derivations.

Every state path derives from :func:`charliebot_home_dir`. The memory CLI
imports from here directly: its store root is a pure derivation of the home,
so resolving it must not drag the config model stack (src.infra.config's
pydantic chain, ~180 ms of the M98 CLI wall) into a fresh process. Other
config-importing readers reach these names through the src.infra.config
re-export.
"""

import os
import pathlib

CHARLIEBOT_HOME_ENV = "CHARLIEBOT_HOME"

# Claude Code's login-directory env var, a cross-process wire contract: the server
# writes it onto a cc-claude child (claude_code._prepare_env), the pool strips
# any inherited value where it pinned the directory itself (master_cc_run,
# claude_compaction.compaction_env), and the in-process reader
# src.infra.config.claude_config_dir reads it back. One spelling everywhere.
# It lives beside the profile home so it resolves without the config model stack.
CLAUDE_CONFIG_DIR_ENV_VAR = "CLAUDE_CONFIG_DIR"

# The OAuth credential filename inside a login directory: the account pool reads
# it for health, and the usage provider derives its per-account path from it.
# It lives beside the login-dir names.
CREDENTIALS_FILE = ".credentials.json"


def default_claude_dir() -> pathlib.Path:
  """The default claude login directory (``~/.claude``), read from HOME on every call.

  The terminal fallback of :func:`src.infra.config.claude_config_dir`'s order and
  the root the cold-storage reader re-derives per call, so it honors a
  redirected HOME (tests isolate stores that way).
  """
  return pathlib.Path.home() / ".claude"


def default_opencode_db() -> pathlib.Path:
  """The default opencode database (``~/.local/share/opencode/opencode.db``), read from HOME on every call.

  The usage logs and the cold-storage sweep both read it, so its one spelling lives here.
  """
  return pathlib.Path.home() / ".local/share/opencode/opencode.db"


# The resolved home and its string form, per raw ``CHARLIEBOT_HOME`` value plus
# ``HOME`` (``""`` raw is the default home, and a ``~`` value derives from
# HOME). The env values are the process's profile identity, fixed for the
# process life, while resolve() is a per-component symlink walk and
# ``pathlib.Path.home()``/``str(pathlib.Path)`` re-parse the path — per-request-fingerprint
# work on every call if repeated. Both public readers serve the same cached
# entry, so a caller comparing its home against the default sees one answer.
_home_cache: dict[tuple[str, str], tuple[pathlib.Path, str]] = {}


def _home_cached(raw: str) -> tuple[pathlib.Path, str]:
  """The home for *raw* (``""`` is the default) as ``(pathlib.Path, str)``, resolved once per env pair."""
  key = (raw, os.environ.get("HOME", ""))
  cached = _home_cache.get(key)
  if cached is None:
    home = pathlib.Path(raw).expanduser().resolve() if raw else pathlib.Path.home() / ".charliebot"
    cached = (home, str(home))
    _home_cache[key] = cached
  return cached


def _resolve_home() -> tuple[pathlib.Path, str]:
  """The validated profile home as ``(pathlib.Path, str)``."""
  raw = os.environ.get(CHARLIEBOT_HOME_ENV, "").strip()
  if raw and not raw.startswith(("~", "/")):
    raise ValueError(f"{CHARLIEBOT_HOME_ENV} must be an absolute path or start with '~'; got {raw!r}")
  return _home_cached(raw)


def default_charliebot_home() -> pathlib.Path:
  """The state directory used when ``CHARLIEBOT_HOME`` is unset."""
  return _home_cached("")[0]


def charliebot_home_dir() -> pathlib.Path:
  """Return the state directory this process belongs to (its profile).

  ``CHARLIEBOT_HOME`` selects the profile: unset or empty gives the default
  ``~/.charliebot``. This is the only place that resolves the home path; every
  other path is derived from :attr:`CharlieBotConfig.charliebot_home`. The one
  raw read of the variable outside this function belongs to the code that
  spawns tmux panes: a pane inherits the tmux server's environment rather than
  this process's, so that code checks whether a profile is set and passes the
  resolved home to new panes explicitly.

  A set value must be absolute or start with ``~``. A relative value would be
  resolved against each process's own working directory, silently handing the
  server, the CLI and every worker a different home, so it is rejected here
  instead of surfacing later as a write into the wrong profile.
  """
  return _resolve_home()[0]


def file_fingerprint(name: str) -> tuple[float, int]:
  """The ``(mtime, size)`` reload cache key over one file in the profile home.

  Size comes from the same stat call and costs nothing extra; it catches
  mtime-preserving writes (``cp -p``, ``touch -r``, two writes inside one second
  on a coarse-resolution filesystem) that an mtime-only key would miss silently.
  A content change that preserves both mtime and size is deliberately not
  covered. A missing file stats to a sentinel rather than raising.

  This is the per-request path (the auth middleware's ``get_config``), so the
  stat stays on raw strings and ``os`` calls: per-call ``Path`` allocation and
  ``resolve`` measured ~130 µs of the ~150 µs middleware floor on the live
  corpus, against ~10 µs of unavoidable fresh stats.
  """
  try:
    st = os.stat(os.path.join(_resolve_home()[1], name))
  except OSError:
    return (0.0, 0)
  return (st.st_mtime, st.st_size)
