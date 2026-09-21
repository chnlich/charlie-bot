"""The profile's secrets file and the one hot-reload cache every file-backed reload mounts.

Split off src/core/config so the CLI chains that read only credentials (the
request contract's auth header) and the CLI base-url cache keep the config
model stack (pydantic + yaml models, ~150 ms of the M97 wall) out of their
process: this module imports stdlib plus the light home/log/yaml helpers only.
config.py re-exports the public names, so every established import path keeps
working; new CLI-side readers import from here.
"""

import os
from collections.abc import Callable
from pathlib import Path
from typing import Generic, TypeVar

from src.core.home import _resolve_home, charliebot_home_dir
from src.core.log_once import LazyStructlogLogger, WarnOnceRegistry
from src.core.yaml_utils import load_yaml

T = TypeVar("T")

log = LazyStructlogLogger()

# The profile's secrets file, named once so the backup's exclusion
# (src/core/backup.py) cannot drift from the loader's path.
CREDENTIALS_FILENAME = "credentials.yaml"


def _install_replace(current: T | None, fresh: T) -> T:
  """Drop the previous value and adopt the fresh one."""
  return fresh


class _HotReloadCache(Generic[T]):
  """One file-backed cache that reloads through a loader when the file's fingerprint moves.

    ``get(loader)`` re-runs *loader* only when the fingerprint differs from both
    the cached value's and the last failure's; the surrounding bookkeeping is the
    one state machine every hot-reload cache shares:

    - a failed reload keeps the previous value and logs one warning per error
      string per process (the key is exactly the field the line logs); the
      failed fingerprint is recorded, so the same broken corpus pays no parse
      and no line until it moves — the freshness rule the successful path
      follows, applied to failure;
    - a reload with nothing cached re-raises: with no fallback the raise is what
      surfaces the broken file, so no failed fingerprint is recorded;
    - a successful reload installs, clears the failed fingerprint, and re-arms
      the registry: a later relapse is a new onset and earns one new line.
    """

  def __init__(
      self,
      fingerprint: Callable[[], tuple[float, int]],
      event: str,
      install: Callable[[T | None, T], T],
  ) -> None:
    self._fingerprint = fingerprint
    self._event = event
    self._install = install
    self.value: T | None = None
    self._mtime: tuple[float, int] | None = None
    self.failed_mtime: tuple[float, int] | None = None
    self.seen = WarnOnceRegistry()

  def reset(self) -> None:
    """Forget the cached value and every fingerprint and warning state."""
    self.value = None
    self._mtime = None
    self.failed_mtime = None
    self.seen.clear()

  def seed(self, value: T) -> None:
    """Install *value* as if freshly loaded, stamped with the current fingerprint."""
    self.value = value
    self._mtime = self._fingerprint()

  def get(self, loader: Callable[[], T]) -> T:
    """Return the cached value, reloading through *loader* when the fingerprint moves."""
    fingerprint = self._fingerprint()
    if self.value is None or fingerprint not in (self._mtime, self.failed_mtime):
      try:
        fresh = loader()
      except Exception as error:
        self.seen.log(log.warning, self._event, str(error), error=str(error))
        if self.value is None:
          raise
        # Only a fallback value makes the failed fingerprint meaningful: with
        # none, the raise above ends the process.
        self.failed_mtime = fingerprint
      else:
        self.value = self._install(self.value, fresh)
        self._mtime = fingerprint
        self.failed_mtime = None
        # The reported failure state ended: a later relapse is a new onset and
        # earns one new line.
        self.seen.clear()
    return self.value


def _file_fingerprint(name: str) -> tuple[float, int]:
  """The ``(mtime, size)`` reload cache key over one file in the profile home.

    Size comes from the same stat call and costs nothing extra; it catches
    mtime-preserving writes (``cp -p``, ``touch -r``, two writes inside one second
    on a coarse-resolution filesystem) that an mtime-only key would miss silently.
    A content change that preserves both mtime and size is deliberately not
    covered. A missing file stats to a sentinel rather than raising.

    This is the per-request path (the auth middleware's ``get_config``), so the
    stat stays on raw strings and ``os`` calls: per-call ``Path`` allocation and
    ``resolve`` measured ~130 µs of the ~150 µs middleware floor on the live
    corpus, against ~10 µs of unavoidable fresh stats.
    """
  try:
    st = os.stat(os.path.join(_resolve_home()[1], name))
  except OSError:
    return (0.0, 0)
  return (st.st_mtime, st.st_size)


class Credentials:
  """One profile's ``credentials.yaml``: ``section -> key -> scalar`` secret values.

    ``path`` is the file the sections were read from; ``sections`` maps each
    top-level section to its scalar values (strings or integers, ``None``
    dropped). Deliberately outside :class:`CharlieBotConfig`: the structure
    file never carries secrets, so nothing holding a config can leak one.
    :meth:`get` answers "is it set"; :meth:`require` turns a missing value
    into a :class:`ValueError` naming the key path and the file it is missing
    from. A plain class, not a dataclass: the CLI verbs that read credentials
    are fresh processes, and the ``dataclasses`` import pulls ``inspect``
    (~9 ms of the M97 verb wall) for machinery no consumer calls.
  """

  __slots__ = ("path", "sections")

  def __init__(self, path: Path, sections: dict[str, dict[str, str | int]]) -> None:
    self.path = path
    self.sections = sections

  def get(self, section: str, key: str) -> str | int | None:
    """Return the value under *section*/*key*, or None when it is unset."""
    return self.sections.get(section, {}).get(key)

  def require(self, section: str, key: str) -> str | int:
    """Return the value under *section*/*key*, raising :class:`ValueError` when it is unset."""
    value = self.get(section, key)
    if value is None:
      raise ValueError(f"credentials.{section}.{key} is not set in {self.path}")
    return value


def load_credentials() -> Credentials:
  """Load this profile's ``credentials.yaml``, the secrets file split out of ``config.yaml``.

    A missing file loads as empty sections. The document must be a mapping whose
    values are mappings whose values are strings or integers; a ``None`` value
    counts as unset and is dropped. Any other shape raises :class:`ValueError`
    naming the offending path as ``credentials.<section>`` or
    ``credentials.<section>.<key>``. Section and key names are never validated:
    any name loads.
    """
  path = charliebot_home_dir() / CREDENTIALS_FILENAME
  data = load_yaml(path, default={})
  if data is None:
    data = {}
  if not isinstance(data, dict):
    raise ValueError(f"credentials must be a mapping of sections: {path}")
  sections: dict[str, dict[str, str | int]] = {}
  for section, keys in data.items():
    if not isinstance(keys, dict):
      raise ValueError(f"credentials.{section} must be a mapping of keys: {path}")
    entry: dict[str, str | int] = {}
    for key, value in keys.items():
      if value is None:
        continue
      if not isinstance(value, (str, int)):
        raise ValueError(f"credentials.{section}.{key} must be a string or integer: {path}")
      entry[key] = value
    sections[section] = entry
  return Credentials(path=path, sections=sections)


def _credentials_fingerprint() -> tuple[float, int]:
  """The reload cache key over ``credentials.yaml``: :func:`_file_fingerprint` on it."""
  return _file_fingerprint(CREDENTIALS_FILENAME)


_credentials_cache = _HotReloadCache(
    fingerprint=_credentials_fingerprint, event="credentials_reload_failed", install=_install_replace)


def get_credentials() -> Credentials:
  """Return the process-wide credentials, refreshed when ``credentials.yaml`` changes.

    Independent of :func:`get_config`: the reload key is ``credentials.yaml``'s
    ``(mtime, size)`` (see :func:`_credentials_fingerprint`), and the cached
    :class:`Credentials` is replaced wholesale — its consumers read per call and
    hold no instance references. A failed reload keeps the previous value and
    logs one warning per onset; with nothing cached yet the error propagates.
    """
  return _credentials_cache.get(load_credentials)


def configured_access_key() -> str:
  """Return the ``charliebot.access_key`` credential, or "" when it is unset.

    One home of the read every access-key gate repeats; an empty value means
    every gate passes unauthenticated readers through.
    """
  return str(get_credentials().get("charliebot", "access_key") or "")
