"""Which memory share a session's agent processes run in.

The operator's ``resource_class`` mark is persisted only on the session it was
set on. Resolution walks that session's task-parent chain at each spawn and
takes the nearest ``"repair"`` mark, the session's own mark first; everything
else is ``"agent"``. Nothing is written on descendants, so clearing a parent's
mark moves the descendants' next spawns back to the agents' share.

The walk reads ``metadata.json`` directly rather than a cached view: a spawn
must see a mark the operator just cleared, and the chain is short.
"""

import asyncio
import json
import pathlib

from src.infra import config, models
from src.runtime import run_identity


def resolve_sync(session_id: str, sessions_dir: pathlib.Path) -> models.ResourceClass:
  """The nearest repair mark on *session_id*'s task-parent chain, or ``"agent"``."""
  seen: set[str] = set()
  current = session_id
  while True:
    if current in seen:
      raise ValueError(f"task-parent cycle while resolving resource class for {session_id}: {current}")
    seen.add(current)
    path = sessions_dir / current / run_identity.SESSION_METADATA_NAME
    try:
      meta = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError as exc:
      raise FileNotFoundError(
          f"session {current} metadata is missing while resolving resource class for {session_id}") from exc
    marked_class = meta.get("resource_class", "agent")
    if marked_class == "repair":
      return "repair"
    if marked_class != "agent":
      raise ValueError(f"session {current} has invalid resource_class {marked_class!r}")
    parent = meta.get("task_parent_id")
    if parent is None:
      return "agent"
    if not isinstance(parent, str) or not parent:
      raise ValueError(f"session {current} has invalid task_parent_id {parent!r}")
    current = parent


async def resolve(session_id: str) -> models.ResourceClass:
  """The spawn-time class for *session_id*, read off the loop from the task tree's metadata files."""
  return await asyncio.to_thread(resolve_sync, session_id, config.get_config().sessions_dir)
