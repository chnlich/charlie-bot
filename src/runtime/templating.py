"""The request-time template engine, the page parts of each slot, and the static-asset version token every page render stamps.

The engine's search path, its template globals and the page parts come from the page-render registry
(`src.runtime.hooks.page_render`): web/templates first, then each page package's `templates/`. The served
static trees are web/static and each page package's `static/`.
"""

import hashlib
import importlib
import os
import pathlib
import subprocess
from typing import TYPE_CHECKING

from src.infra import buildinfo, constants, log_once, timeouts
from src.runtime.hooks import page_render

if TYPE_CHECKING:
  from fastapi import templating

log = log_once.LazyStructlogLogger()


def _get_git_version() -> str:
  """Return git short hash + commit date (e.g. 'bc6b882 · 03-24'), or '' on failure."""
  short_hash = buildinfo.read_repo_head_sha(timeouts.SUBPROCESS_GIT_VERSION_TIMEOUT)
  if short_hash is None:
    log.warning("git_version_failed")
    return ""
  try:
    commit_date = subprocess.check_output(
        ["git", "log", "-1", "--format=%cd", "--date=format:%m-%d"],
        cwd=constants.REPO_ROOT,
        text=True,
        timeout=timeouts.SUBPROCESS_GIT_VERSION_TIMEOUT,
    ).strip()
  except (OSError, subprocess.SubprocessError):
    log.warning("git_version_failed")
    return ""
  return f"{short_hash} · {commit_date}"


# The two git subprocesses behind the version run only when a page renders the
# token or the footer; the server import floor (docs/perf_baseline.md@5175adf09 M99)
# depends on them staying off it.
_GIT_VERSION: str | None = None


def git_version() -> str:
  """The module's git version, computed on first use and memoized."""
  global _GIT_VERSION
  if _GIT_VERSION is None:
    _GIT_VERSION = _get_git_version()
  return _GIT_VERSION


# Content half of the ?v= asset token, keyed on the walk-instant signature
# tuple: a file that moves after the walk keys the older digest and the next
# render's walk re-hashes it, the same pre-read rule the file memos keep.
# ``digests`` carries each file's own sha1 so a change re-reads only the moved
# files; ``digest`` is the combined hex the token appends.
_ASSET_DIGEST_STATE: dict[str, tuple | dict[str, bytes] | str] = {
    "sig": (),
    "digests": {},
    "digest": "",
}


def package_directory(package: str) -> pathlib.Path:
  """src/<kind>/<name> for the package "src.<kind>.<name>"."""
  return constants.REPO_ROOT.joinpath(*package.split("."))


def page_package_static() -> list[tuple[str, pathlib.Path]]:
  """("<kind>/<name>", its static/ directory) for each page package, in registration order.

  The first item is the URL path under /static/ that serves the directory.
  """
  return [
      (package.removeprefix("src.").replace(".", "/"), package_directory(package) / "static")
      for package in page_render.page_packages()
  ]


def _asset_tree_digest() -> str:
  """Content digest over the served static trees, refreshed per call.

  The ?v= token names the bytes the URL serves, and a working-tree edit can
  land between restarts, so the digest walks web/static and each page
  package's static/ (one stat pass over the static files) and re-hashes only
  files whose (mtime_ns, size) signature moved; an unchanged walk serves the
  memoized digest. Each file is keyed by its URL path under /static/. The walk
  scans with os.scandir so each entry answers is_file from the directory
  record and stats once.
  """
  static_root = constants.REPO_ROOT / "web" / "static"
  if not static_root.is_dir():
    return ""
  pairs: list[tuple[str, int, int]] = []
  paths: dict[str, pathlib.Path] = {}

  def walk(dir_path: pathlib.Path, prefix: str) -> None:
    with os.scandir(dir_path) as entries:
      for entry in entries:
        rel = f"{prefix}{entry.name}"
        if entry.is_dir():
          walk(pathlib.Path(entry.path), f"{rel}/")
        elif entry.is_file():
          st = entry.stat()
          pairs.append((rel, st.st_mtime_ns, st.st_size))
          paths[rel] = pathlib.Path(entry.path)

  walk(static_root, "")
  # Most packages have no static/ directory; a fast-forward can add one at any time.
  for url_path, directory in page_package_static():
    if directory.is_dir():
      walk(directory, f"{url_path}/")
  pairs.sort()
  sig = tuple(pairs)
  if _ASSET_DIGEST_STATE["sig"] == sig:
    return str(_ASSET_DIGEST_STATE["digest"])
  old_stats = {rel: (mtime, size) for rel, mtime, size in _ASSET_DIGEST_STATE["sig"]}
  digests: dict[str, bytes] = dict(_ASSET_DIGEST_STATE["digests"])
  for rel, mtime, size in pairs:
    if rel in digests and old_stats.get(rel) == (mtime, size):
      continue
    digests[rel] = hashlib.sha1(paths[rel].read_bytes()).digest()
  combined = hashlib.sha1()
  for rel, _, _ in pairs:
    combined.update(rel.encode())
    combined.update(b"\0")
    combined.update(digests[rel])
  value = combined.hexdigest()[:12]
  _ASSET_DIGEST_STATE["sig"] = sig
  _ASSET_DIGEST_STATE["digests"] = digests
  _ASSET_DIGEST_STATE["digest"] = value
  return value


def static_asset_version() -> str:
  """Cache-bust token for static assets: the runtime git version plus the served tree's content digest."""
  git_part = git_version().replace(" · ", "-").replace(" ", "-")
  return f"{git_part}-{_asset_tree_digest()}"


# jinja2 + fastapi.templating ride every page render (~19 ms of the M99 server
# import floor, marginal over the already-loaded fastapi) and no import-time
# path touches a template, so the engine builds on first render.
_templates_instance: templating.Jinja2Templates | None = None


def _template_directories() -> list[pathlib.Path]:
  """web/templates, then the `templates/` directory of each page package, in registration order.

  A package without a templates/ directory adds a path that holds no template until a fast-forward creates it.
  """
  directories = [constants.REPO_ROOT / "web" / "templates"]
  directories.extend(package_directory(package) / "templates" for package in page_render.page_packages())
  return directories


def page_parts(slot: str) -> list[str]:
  """Template names of the slot's page parts that exist on disk now, in registration order.

  A page places a slot with {% for part in page_parts('<slot>') %}{% include part %}{% endfor %}. A slot name
  outside page_render.PAGE_SLOTS raises ValueError, so a misspelled slot fails the render.
  """
  if slot not in page_render.PAGE_SLOTS:
    raise ValueError(f"page slot {slot!r} is not one of {page_render.PAGE_SLOTS}")
  parts = []
  for package in page_render.page_packages():
    name = f"{package.rpartition('.')[2]}/page_{slot}.html"
    if (package_directory(package) / "templates" / name).is_file():
      parts.append(name)
  return parts


def _assert_unique_template_names(directories: list[pathlib.Path]) -> None:
  """Raise when two directories hold one template name: the loader would serve the first and hide the second."""
  holder: dict[str, pathlib.Path] = {}
  for directory in directories:
    for path in sorted(directory.rglob("*.html")):
      name = path.relative_to(directory).as_posix()
      if name in holder:
        raise ValueError(f"template {name!r} is in both {holder[name]} and {directory}")
      holder[name] = directory


def templates() -> templating.Jinja2Templates:
  """The request-time template engine, built on first use and reused after."""
  global _templates_instance
  if _templates_instance is None:
    from fastapi import templating

    directories = _template_directories()
    _assert_unique_template_names(directories)
    engine = templating.Jinja2Templates(directory=[str(directory) for directory in directories])
    engine.env.globals["page_parts"] = page_parts
    for name, (module, attr) in page_render.template_globals().items():
      engine.env.globals[name] = getattr(importlib.import_module(module), attr)
    _templates_instance = engine
  return _templates_instance
