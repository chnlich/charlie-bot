"""Plan-registry lookup for a Waiting on you item's file links.

A link to a registered plan version names its plan lineage and version. The check refuses a link to a version below
the lineage's newest version or to a closed lineage. The since pairing groups items of one lineage together, so a
rewording or a newer version keeps the item's start time.
"""

import dataclasses
import json
import pathlib
import posixpath
import re

from src.features.web_reply import metadata
from src.runtime import file_urls

# A bare file-server path inside prose: the context and text fields carry these without a Markdown target.
_MOUNT_PATH = re.compile(re.escape(file_urls.FILE_SERVER_MOUNTS[0]) + r"/[^\s<>\"'`)]*")


@dataclasses.dataclass(frozen=True)
class Binding:
  """One item link bound to a registered plan version."""

  session_id: str
  plan_id: int
  version: int
  newest_version: int
  newest_file: pathlib.Path
  closed: str | None

  @property
  def stale(self) -> bool:
    """Whether the link names an older version than the newest or a version of a closed lineage."""
    return self.closed is not None or self.version < self.newest_version


@dataclasses.dataclass(frozen=True)
class StaleLink:
  """One item link the registry refuses with the newest file the user should read instead."""

  link: str
  binding: Binding


class PlanLinks:
  """Read the plan registries that item links name, once per session for one status command."""

  def __init__(self, sessions_dir: pathlib.Path) -> None:
    self._sessions_dir = sessions_dir
    self._root = sessions_dir.resolve()
    self._registries: dict[str, list[dict] | None] = {}

  def _relative(self, path: pathlib.Path) -> tuple[str, str] | None:
    """The session id and session-relative posix path that *path* names, or None outside the sessions directory."""
    try:
      relative = path.resolve().relative_to(self._root)
    except ValueError:
      return None
    parts = relative.parts
    if len(parts) < 2:
      return None
    return parts[0], posixpath.normpath("/".join(parts[1:]))

  def _registry(self, session_id: str) -> list[dict] | None:
    """The plans of *session_id*, or None when its registry is missing."""
    if session_id not in self._registries:
      path = self._sessions_dir / session_id / "plans.json"
      try:
        data = json.loads(path.read_text(encoding="utf-8"))
      except FileNotFoundError:
        self._registries[session_id] = None
        return None
      self._registries[session_id] = data["plans"]
    return self._registries[session_id]

  def binding(self, path: pathlib.Path) -> Binding | None:
    """The registered plan version that *path* names, or None for any other file."""
    relative = self._relative(path)
    if relative is None:
      return None
    session_id, file_relative = relative
    plans = self._registry(session_id)
    if plans is None:
      return None
    for plan in plans:
      versions = plan["versions"]
      matched = next((ver for ver in versions if posixpath.normpath(ver["file"]) == file_relative), None)
      if matched is None:
        continue
      newest = max(versions, key=lambda ver: ver["v"])
      closed = plan.get("closed")
      return Binding(
          session_id=session_id,
          plan_id=plan["id"],
          version=matched["v"],
          newest_version=newest["v"],
          newest_file=self._sessions_dir / session_id / newest["file"],
          closed=None if closed is None else closed["as"],
      )
    return None

  def target(self, item: metadata.WaitingItem) -> str:
    """The pairing target of *item*: its plan lineage, its other link, or the one group for items without a link."""
    link = item.link
    if link is None:
      return "no-link"
    path = file_urls.host_file(link)
    binding = None if path is None else self.binding(path)
    if binding is not None:
      return f"plan:{binding.session_id}:{binding.plan_id}"
    return f"link:{link}"

  def _links(self, value: str | None, scan_context: file_urls.ScanContext) -> list[tuple[str, pathlib.Path]]:
    """The link text and local file path of each file link *value* names."""
    if value is None:
      return []
    links: list[tuple[str, pathlib.Path]] = []
    for finding in file_urls.scan_file_links(value, scan_context):
      if finding.live is None or finding.path is None:
        continue
      links.append((finding.link, finding.path))
    for match in _MOUNT_PATH.finditer(value):
      link = file_urls.LINK_TRAILING_PUNCTUATION_RE.sub("", match.group(0))
      path = file_urls.host_file(link)
      if path is not None:
        links.append((link, path))
    return links

  def stale_links(self, items: list[metadata.WaitingItem], scan_context: file_urls.ScanContext) -> list[StaleLink]:
    """Every stale registered-plan link in the text, context and link of *items*."""
    stale: list[StaleLink] = []
    seen: set[tuple[str, str, int]] = set()
    for item in items:
      for value in (item.text, item.context, item.link):
        links = self._links(value, scan_context)
        if value == item.link:
          path = file_urls.host_file(value)
          if path is not None:
            links.append((value, path))
        for link, path in links:
          binding = self.binding(path)
          if binding is None or not binding.stale:
            continue
          key = (link, binding.session_id, binding.plan_id)
          if key in seen:
            continue
          seen.add(key)
          stale.append(StaleLink(link=link, binding=binding))
    return stale


def stale_detail(stale: list[StaleLink]) -> str:
  """The 409 detail that names each stale link and its lineage's newest file."""
  lines = ["Stale plan links; nothing was stored."]
  for entry in stale:
    binding = entry.binding
    if binding.closed is None:
      reason = f"plan {binding.plan_id} version {binding.version} is older than version {binding.newest_version}"
    else:
      reason = (
          f"plan {binding.plan_id} is closed ({binding.closed}); its newest version is version "
          f"{binding.newest_version}")
    lines.append(f"- {entry.link}: {reason} at {binding.newest_file}")
  return "\n".join(lines)
