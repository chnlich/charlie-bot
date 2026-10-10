"""The Claude login directories: where the Claude CLI keeps a login's credentials and transcripts.

``CLAUDE_CONFIG_DIR_ENV_VAR`` (src/infra/home.py) states the cross-process contract of the
``CLAUDE_CONFIG_DIR`` variable: the server writes it onto a cc-claude child, the pool strips an
inherited value where it pinned the directory itself, and ``claude_config_dir`` reads it back.
"""

import os
import pathlib

from src.infra import home

# The OAuth credential filename inside a login directory: the account pool reads
# it for health, and the usage provider derives its per-account path from it.
CREDENTIALS_FILE = ".credentials.json"


def default_claude_dir() -> pathlib.Path:
  """The default claude login directory (``~/.claude``), read from HOME on every call.

  The terminal fallback of :func:`claude_config_dir`'s order and the root the cold-storage
  sweep re-derives per call, so it honors a redirected HOME (tests isolate stores that way).
  """
  return pathlib.Path.home() / ".claude"


def claude_config_dir() -> pathlib.Path:
  """Resolve the CLAUDE_CONFIG_DIR a cc-claude process will use.

  Single source of truth for the resolution order: ``$CLAUDE_CONFIG_DIR``
  first, then ``~/.claude``. Both the API backend-switch guard and the
  runtime resume resolver call this — do not restate the order anywhere
  else. A pool account's pinned ``config_dir`` rides the ``CLAUDE_CONFIG_DIR``
  value the backend sets on the process environment, never this call.
  """
  env_dir = os.environ.get(home.CLAUDE_CONFIG_DIR_ENV_VAR)
  if env_dir:
    return pathlib.Path(env_dir).expanduser()
  return default_claude_dir()
