"""Page-render registry: packages register their page parts, template globals and home-page cards.

Each registration holds strings. `src.runtime.templating` builds the template engine from them on the
first page render, so this module imports nothing heavy.

A page package keeps its page parts at fixed paths inside its directory src/<kind>/<name>/:

    static/                              served at /static/<kind>/<name>/ and counted in the asset version
    templates/                           joins the template search path, after web/templates
    templates/<name>/page_<slot>.html    included where a page places the slot, for each slot in PAGE_SLOTS

The page reads the part files from disk at each render, so a part that a fast-forward adds or deletes takes
effect without a restart.
"""

from __future__ import annotations

from src.runtime.hooks import registration

# The places a page leaves for package parts. index.html places the first seven, the diff page places
# diff_toolbar, and the home page places home_notes.
PAGE_SLOTS = (
    "head", "tab_buttons", "below_header", "tab_panels", "input_buttons", "modals", "scripts", "diff_toolbar",
    "home_notes")

# Registration order is the order of the lists: template directories search in it, page parts and cards list in it.
_PAGE_PACKAGES: list[str] = []
_TEMPLATE_GLOBALS: dict[str, tuple[str, str]] = {}  # name -> (module, attr)
_HOME_CARDS: list[dict[str, str]] = []


def register_page_package(package: str) -> None:
  """The package's static files, templates and page parts join the pages, after those of earlier registrations."""
  _PAGE_PACKAGES.append(package)


def register_template_global(name: str, module: str, *, attr: str) -> None:
  """Every template sees name as getattr(import_module(module), attr): a zero-argument callable that a template calls at render.

  A second registration of one name raises ValueError.
  """
  if name in _TEMPLATE_GLOBALS:
    raise registration.already_registered("template global", name, _TEMPLATE_GLOBALS[name][0])
  _TEMPLATE_GLOBALS[name] = (module, attr)


def register_home_card(name: str, url: str, description: str) -> None:
  """The /home page lists this destination card after the app's own cards, in registration order."""
  _HOME_CARDS.append({"name": name, "url": url, "description": description})


def page_packages() -> tuple[str, ...]:
  """Page package names in registration order."""
  return tuple(_PAGE_PACKAGES)


def template_globals() -> dict[str, tuple[str, str]]:
  """name -> (module, attr)."""
  return dict(_TEMPLATE_GLOBALS)


def home_cards() -> tuple[dict[str, str], ...]:
  """{"name", "url", "description"} in registration order."""
  return tuple(dict(card) for card in _HOME_CARDS)
