"""The request-time template engine and the static-asset version token every page render stamps.

The engine's search path and template globals come from the page-render registry
(`src.runtime.hooks.page_render`): web/templates first, then each registered package's `templates/`.
"""

import hashlib
import importlib
import os
import subprocess
from pathlib import Path
from typing import TYPE_CHECKING

from src.infra.buildinfo import read_repo_head_sha
from src.infra.constants import REPO_ROOT
from src.infra.log_once import LazyStructlogLogger
from src.infra.memo import StatSignatureMemo
from src.infra.timeouts import SUBPROCESS_GIT_VERSION_TIMEOUT
from src.runtime.hooks import page_render

if TYPE_CHECKING:
  from fastapi.templating import Jinja2Templates

log = LazyStructlogLogger()


def _get_git_version() -> str:
  """Return git short hash + commit date (e.g. 'bc6b882 · 03-24'), or '' on failure."""
  short_hash = read_repo_head_sha(SUBPROCESS_GIT_VERSION_TIMEOUT)
  if short_hash is None:
    log.warning("git_version_failed")
    return ""
  try:
    commit_date = subprocess.check_output(
        ["git", "log", "-1", "--format=%cd", "--date=format:%m-%d"],
        cwd=REPO_ROOT,
        text=True,
        timeout=SUBPROCESS_GIT_VERSION_TIMEOUT,
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

# Per-directory record of the digest walk, keyed on the directory's own
# (mtime_ns, size): dir path -> (subdirectory names, file names). The
# per-render freshness contract stays with the per-file stat every walk takes
# — a content edit moves the file's own signature, not its directory's — so
# only the directory record (entry create, delete, rename, all of which move
# the directory stat) rides the memo; a moved directory re-scandirs on the
# next walk. Served listings are shared across renders, the no-defensive-copy
# idiom of the sibling memos.
_DIR_LISTING_MEMO_LIMIT = 64
_DIR_LISTINGS: StatSignatureMemo[str, tuple[list[str], list[str]]] = StatSignatureMemo(_DIR_LISTING_MEMO_LIMIT)


def _asset_tree_digest() -> str:
  """Content digest over the served static tree, refreshed per call.

  The ?v= token names the bytes the URL serves, and a working-tree edit can
  land between restarts, so the digest walks the tree (one stat pass over the
  static files) and re-hashes only files whose (mtime_ns, size) signature
  moved; an unchanged walk serves the memoized digest. The per-directory
  entry record rides ``_DIR_LISTINGS``, so a steady render stats each file
  and each directory once and re-scandirs only a directory whose own stat
  moved.
  """
  static_root = REPO_ROOT / "web" / "static"
  if not static_root.is_dir():
    return ""
  pairs: list[tuple[str, int, int]] = []

  # String paths only: the per-file stat rides 48 os.stat(str) calls per
  # render, and a Path division per entry would price the walk past the
  # shape it replaces.
  def walk(dir_path: str, prefix: str) -> None:
    dir_st = os.stat(dir_path)
    cached = _DIR_LISTINGS.fresh(dir_path, dir_st)
    if cached is None:
      subdirs: list[str] = []
      files: list[str] = []
      with os.scandir(dir_path) as entries:
        for entry in entries:
          if entry.is_dir():
            subdirs.append(entry.name)
          elif entry.is_file():
            files.append(entry.name)
      cached = (subdirs, files)
      _DIR_LISTINGS.record(dir_path, dir_st, cached)
    subdirs, files = cached
    for name in subdirs:
      walk(os.path.join(dir_path, name), f"{prefix}{name}/")
    for name in files:
      st = os.stat(os.path.join(dir_path, name))
      pairs.append((f"{prefix}{name}", st.st_mtime_ns, st.st_size))

  walk(str(static_root), "")
  pairs.sort()
  sig = tuple(pairs)
  if _ASSET_DIGEST_STATE["sig"] == sig:
    return str(_ASSET_DIGEST_STATE["digest"])
  old_stats = {rel: (mtime, size) for rel, mtime, size in _ASSET_DIGEST_STATE["sig"]}
  digests: dict[str, bytes] = dict(_ASSET_DIGEST_STATE["digests"])
  for rel, mtime, size in pairs:
    if rel in digests and old_stats.get(rel) == (mtime, size):
      continue
    digests[rel] = hashlib.sha1((static_root / rel).read_bytes()).digest()
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
_templates_instance: Jinja2Templates | None = None


def _template_directories() -> list[Path]:
  """web/templates, then the `templates/` directory of each registered package, in registration order."""
  directories = [REPO_ROOT / "web" / "templates"]
  directories.extend(
      Path(importlib.import_module(package).__file__).parent / "templates"
      for package in page_render.template_packages())
  return directories


def _assert_unique_template_names(directories: list[Path]) -> None:
  """Raise when two directories hold one template name: the loader would serve the first and hide the second."""
  holder: dict[str, Path] = {}
  for directory in directories:
    for path in sorted(directory.rglob("*.html")):
      name = path.relative_to(directory).as_posix()
      if name in holder:
        raise ValueError(f"template {name!r} is in both {holder[name]} and {directory}")
      holder[name] = directory


def templates() -> Jinja2Templates:
  """The request-time template engine, built on first use and reused after."""
  global _templates_instance
  if _templates_instance is None:
    from fastapi import templating

    directories = _template_directories()
    _assert_unique_template_names(directories)
    engine = templating.Jinja2Templates(directory=[str(directory) for directory in directories])
    for name, (module, attr) in page_render.template_globals().items():
      engine.env.globals[name] = getattr(importlib.import_module(module), attr)
    _templates_instance = engine
  return _templates_instance
