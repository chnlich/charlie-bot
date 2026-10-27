"""The composition root: the one list of packages that register CLI commands, config sections and runtime hooks.

Each listed package defines ``register()`` in its ``__init__.py``. Deleting a package's line
removes its commands and hooks from the CLI, and its page packages from the page registry
(``src.runtime.hooks.page_render``). The Rust server (``rust/crates/app/server``) holds its own package list.
"""

import importlib
from collections.abc import Iterable

from src.runtime.hooks import page_render

# Registration order is metadata-slot order and turn-contribution order.
PACKAGES = (
    "src.features.artifacts",
    "src.features.backup",
    "src.features.backend_pool",
    "src.features.chat_threads",
    "src.features.code_server",
    "src.features.cron",
    "src.features.slack",  # Its registered session keys precede Discord's in the saved metadata.
    "src.features.discord",
    "src.features.disk_sweep",
    "src.features.web_reply",
    "src.features.improve",
    "src.features.memory",
    "src.features.router",
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
