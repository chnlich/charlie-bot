"""The composition root: the one list of packages that register routers, CLI commands, background services and page parts.

Each listed package defines ``register()`` in its ``__init__.py``. Deleting a package's line
removes its routes, commands and services from the server and the CLI, and its static files,
templates and page parts from the pages (``src.runtime.hooks.page_render``).
"""

import importlib
from collections.abc import Iterable

from src.runtime.hooks import page_render

# Registration order is route inclusion order, service start order, home-page card order,
# metadata-slot order and page-part order. The file server's catch-all routes come last among
# the feature lines.
PACKAGES = (
    "src.features.artifacts",
    "src.features.backup",
    "src.features.backend_pool",
    "src.features.chat_threads",
    "src.features.code_server",
    "src.features.cron",
    "src.features.diag",
    "src.features.slack",  # Its registered session keys precede Discord's in the saved metadata.
    "src.features.discord",
    "src.features.web_reply",
    "src.features.host_auth",
    "src.features.diff_view",
    "src.features.explain",
    "src.features.improve",
    "src.features.memory",
    "src.features.ncu",
    "src.features.router",
    "src.features.terminal",
    "src.features.trace",
    "src.features.usage",
    "src.features.voice",
    "src.backends.openai_compatible",
    "src.backends.antigravity",
    "src.backends.charlie_code",
    "src.backends.claude_code",
    "src.backends.codex",
    "src.backends.gemini",
    "src.backends.anthropic_compatible",
    "src.backends.opencode",
    "src.features.files",
)

_registered = False
_registered_packages: set[str] = set()


def _register_packages(packages: Iterable[str]) -> None:
  for package in packages:
    if package not in _registered_packages:
      importlib.import_module(package).register()
      page_render.register_page_package(package)
      _registered_packages.add(package)


def register_cli_commands() -> None:
  """Register feature commands without importing backend type hooks for CLI help."""
  if _registered:
    return
  _register_packages(package for package in PACKAGES if package.startswith("src.features."))


def register_all() -> None:
  """Import each package in PACKAGES and call its register(). Runs once per process; a later call returns at once."""
  global _registered
  if _registered:
    return
  _register_packages(PACKAGES)
  _registered = True
