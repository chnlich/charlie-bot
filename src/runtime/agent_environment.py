"""The environment the server hands every agent process.

``scripts/start-server.sh`` starts the server with ``uv run``, which activates the
project venv: it sets ``VIRTUAL_ENV`` and ``UV_RUN_RECURSION_DEPTH`` and puts the
venv's bin directory first on PATH. Every agent process copies the server's
``os.environ``, and agents run commands in other checkouts. uv 0.12 takes its
install target from that activation in any directory: ``uv run --active``,
``uv sync --active`` and ``uv pip install`` target ``$VIRTUAL_ENV``, and
``uv pip install`` falls back to a venv interpreter it finds on PATH. One such
command in a worktree re-points the server venv's editable install at it.

The server therefore removes the activation once at start and puts
``ENTRY_POINT_DIR`` on PATH in place of the venv bin. That directory holds one
shim per ``[project.scripts]`` entry, each exec'ing the same-named script in
``$CHARLIEBOT_VENV_BIN``, so ``charliebot`` and ``claude-sub`` resolve by name
while no venv interpreter is left on PATH.
"""

import os
import pathlib
import sys
from collections.abc import Mapping

from src.runtime.agent_process import base

# This file sits at src/runtime/, so the shims are the sibling agent_entry_points/
# directory - derived from the module location, never a host path literal.
ENTRY_POINT_DIR = pathlib.Path(__file__).resolve().parent / "agent_entry_points"

VENV_BIN_ENV_VAR = "CHARLIEBOT_VENV_BIN"

# The variables `uv run` sets to activate the project venv (uv 0.12).
_ACTIVATION_VARS = ("VIRTUAL_ENV", "UV_RUN_RECURSION_DEPTH")


def agent_environment(env: Mapping[str, str], venv_bin: pathlib.Path) -> dict[str, str]:
  """Return a copy of *env* without the activation of the venv whose bin is *venv_bin*.

  The activation variables are dropped, and so is every PATH entry resolving to
  *venv_bin*, duplicates and symlinked spellings included. ``ENTRY_POINT_DIR``
  goes first on PATH and ``CHARLIEBOT_VENV_BIN`` names *venv_bin* for the shims.
  Every other variable keeps its value; *env* itself is not modified.
  """
  result = {name: value for name, value in env.items() if name not in _ACTIVATION_VARS}
  real_venv_bin = os.path.realpath(venv_bin)
  kept = [entry for entry in env["PATH"].split(os.pathsep) if os.path.realpath(entry) != real_venv_bin]
  result["PATH"] = os.pathsep.join(kept)
  base.prepend_path_dir(result, str(ENTRY_POINT_DIR))
  result[VENV_BIN_ENV_VAR] = str(venv_bin)
  return result


def apply_agent_environment() -> None:
  """Replace ``os.environ`` with :func:`agent_environment` for the venv this interpreter runs.

  Raises RuntimeError when the interpreter is not in a venv: the bin directory
  to strip would then be a system directory.
  """
  if sys.prefix == sys.base_prefix:
    raise RuntimeError(f"the CharlieBot server must run from a venv; sys.prefix {sys.prefix} is the base interpreter")
  env = agent_environment(os.environ, pathlib.Path(sys.prefix) / "bin")
  os.environ.clear()
  os.environ.update(env)
