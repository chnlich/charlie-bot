"""The profile-home resolution: the one place that reads ``CHARLIEBOT_HOME``.

Every state path derives from :func:`charliebot_home_dir`. Local-only verbs
(the memory CLI, the backup paths, slash-command loading) import from here
directly: their directories are pure derivations of the home, so resolving
them must not drag the config model stack (src.core.config's pydantic chain,
~180 ms of the M98 CLI wall) into a fresh process.
"""

import os
from pathlib import Path

CHARLIEBOT_HOME_ENV = "CHARLIEBOT_HOME"

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
  ``~/.charliebot``, so an untouched host behaves exactly as before. This is the
  only place that resolves the home path; every other path is derived
  from :attr:`CharlieBotConfig.charliebot_home`. The one raw read of the variable
  outside this function is the web terminal's profile check
  (``src/agents/backends/terminal.py``): a tmux pane inherits the tmux server's
  environment rather than this process's, so the terminal checks whether a
  profile is set and passes the resolved home to new panes explicitly.

  A set value must be absolute or start with ``~``. A relative value would be
  resolved against each process's own working directory, silently handing the
  server, the CLI and every worker a different home, so it is rejected here
  instead of surfacing later as a write into the wrong profile.
  """
  return _resolve_home()[0]
