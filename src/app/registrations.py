"""The composition root: the one list of packages that register routers, CLI commands and background services.

Each listed package defines ``register()`` in its ``__init__.py``. Deleting a package's line
removes its routes, commands and services from the server and the CLI.
"""

import importlib

# Registration order is route inclusion order and service start order. The file server's
# catch-all routes come last among the feature lines.
PACKAGES = (
    "src.features.artifacts",
    "src.features.backlog",
    "src.features.backup",
    "src.features.code_server",
    "src.features.cron",
    "src.features.diag",
    "src.features.diff_view",
    "src.features.discord",
    "src.features.host_auth",
    "src.features.improve",
    "src.features.latex",
    "src.features.memory",
    "src.features.remote_launch",
    "src.features.session_tree_preview",
    "src.features.slack",
    "src.features.storage",
    "src.features.terminal",
    "src.features.usage",
    "src.features.voice",
    "src.backends.openai_compatible",
    "src.features.files",
)

_registered = False


def register_all() -> None:
  """Import each package in PACKAGES and call its register(). Runs once per process; a later call returns at once."""
  global _registered
  if _registered:
    return
  for package in PACKAGES:
    importlib.import_module(package).register()
  _registered = True
