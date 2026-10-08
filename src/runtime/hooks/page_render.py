"""Page-render registry: packages register template directories, template globals and home-page cards.

Each registration holds strings. `src.runtime.templating` builds the template engine from them on the
first page render, so this module imports nothing heavy.
"""

from __future__ import annotations

# Registration order is the order of the lists: template directories search in it, cards list in it.
_TEMPLATE_PACKAGES: list[str] = []
_TEMPLATE_GLOBALS: dict[str, tuple[str, str]] = {}  # name -> (module, attr)
_HOME_CARDS: list[dict[str, str]] = []


def register_templates(package: str) -> None:
  """The directory `templates/` inside package joins the template search path, after web/templates."""
  _TEMPLATE_PACKAGES.append(package)


def register_template_global(name: str, module: str, *, attr: str) -> None:
  """Every template sees name as getattr(import_module(module), attr): a zero-argument callable that a template calls at render.

  A second registration of one name raises ValueError.
  """
  if name in _TEMPLATE_GLOBALS:
    raise ValueError(f"template global {name!r} is already registered by {_TEMPLATE_GLOBALS[name][0]}")
  _TEMPLATE_GLOBALS[name] = (module, attr)


def register_home_card(name: str, url: str, description: str) -> None:
  """The /home page lists this destination card after the app's own cards, in registration order."""
  _HOME_CARDS.append({"name": name, "url": url, "description": description})


def template_packages() -> tuple[str, ...]:
  """Package names in registration order."""
  return tuple(_TEMPLATE_PACKAGES)


def template_globals() -> dict[str, tuple[str, str]]:
  """name -> (module, attr)."""
  return dict(_TEMPLATE_GLOBALS)


def home_cards() -> tuple[dict[str, str], ...]:
  """{"name", "url", "description"} in registration order."""
  return tuple(dict(card) for card in _HOME_CARDS)
