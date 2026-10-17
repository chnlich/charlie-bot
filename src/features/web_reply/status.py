"""A web session's status: the Goal, Now and Waiting on you items that ``charliebot web status`` sets and reads.

A set replaces the items its file names, stores the status in the ``web_status`` metadata field, records one
``web_status`` event and tells every ancestor page. A read lists the open descendants whose status holds Waiting on you
items, newest first.
"""

import datetime
import json

import pydantic

from src.features.web_reply import metadata, reply
from src.infra import event_types as ET
from src.infra import log_once, metadata_slots, models
from src.runtime import session_events, session_store, streaming, task_sessions

logger = log_once.LazyStructlogLogger()

# The owner that the web reply package registers its metadata under (see the package's register()).
OWNER = "web_reply"

# The status items in the order a set lists them and the status line names them.
ITEMS = ("goal", "now", "waiting")

# The label of each item in the event's content.
LABELS = {"goal": "Goal", "now": "Now", "waiting": "Waiting on you"}

# The non-persisted frame an ancestor's page receives when a descendant sets its status.
SUBTREE_NOTICE_TYPE = "web_status_subtree"


class StatusPatch(pydantic.BaseModel):
  """The JSON object of ``charliebot web status set``. An item that the object leaves out keeps its stored value."""
  model_config = pydantic.ConfigDict(extra="forbid")

  goal: metadata.Text | None = None
  now: metadata.Text | None = None
  waiting: list[metadata.WaitingItem] = pydantic.Field(default_factory=list)


def _error_summary(exc: pydantic.ValidationError) -> str:
  return "; ".join(f"{'.'.join(str(part) for part in err['loc'])}: {err['msg']}" for err in exc.errors())


def parse_patch(text: str) -> StatusPatch:
  """The patch that the JSON *text* holds; WebReplyError 422 when it is not an object of valid items or names none."""
  try:
    raw = json.loads(text)
  except json.JSONDecodeError as exc:
    raise reply.WebReplyError(422, f"Status file is not JSON: {exc.msg}") from exc
  if not isinstance(raw, dict):
    raise reply.WebReplyError(422, "Status file is not a JSON object")
  try:
    patch = StatusPatch.model_validate(raw)
  except pydantic.ValidationError as exc:
    raise reply.WebReplyError(422, f"Status file is invalid: {_error_summary(exc)}") from exc
  if not patch.model_fields_set:
    raise reply.WebReplyError(422, "Status file names no item: give goal, now or waiting")
  return patch


async def set_status(
    session_id: str, text: str, store: session_store.SessionStore, events: session_events.SessionEvents,
    tree: task_sessions.TaskTreeManager) -> dict:
  """Store the items that *text* names as the session's status and return the stored status.

  Refusals raise WebReplyError before anything is stored or recorded. A stored set records one web_status event and
  sends the ancestor notice to every ancestor's page.
  """
  meta = await reply.require_web_session(store, session_id)
  patch = parse_patch(text)
  stored = metadata_slots.fields_of(meta, OWNER).web_status
  base = metadata.WebStatus() if stored is None else stored
  named = {item: getattr(patch, item) for item in ITEMS if item in patch.model_fields_set}
  status = base.model_copy(update={**named, "updated_at": models.utc_now()})
  await tree.update_slot_fields(session_id, OWNER, web_status=status)
  payload = status.model_dump(mode="json")
  changed = list(named)
  await events.persist_and_broadcast(
      session_id, {
          "type": ET.WEB_STATUS,
          "content": "Status updated: " + ", ".join(LABELS[item] for item in changed),
          ET.WEB_STATUS: payload,
          "web_status_changed": changed,
      })
  for ancestor_id in await tree.ancestor_ids(session_id):
    await streaming.streaming_manager.broadcast(
        streaming.session_channel(ancestor_id), {
            "type": SUBTREE_NOTICE_TYPE,
            "source_session_id": session_id
        })
  logger.info("web_status_set", session=session_id, changed=changed)
  return payload


async def read_status(session_id: str, store: session_store.SessionStore, tree: task_sessions.TaskTreeManager) -> dict:
  """The session's status and the Waiting on you items of its open descendants, newest first.

  A descendant qualifies when its status holds at least one waiting item, its session is not archived, and its task is
  open.
  """
  meta = await reply.require_web_session(store, session_id)
  stored = metadata_slots.fields_of(meta, OWNER).web_status
  archived = await tree.derived_archived_ids()
  rows: list[tuple[datetime.datetime, dict]] = []
  for descendant_id in await tree.descendant_ids(session_id):
    child = await store.get_session(descendant_id)
    if child is None:
      continue  # deleted between the index read and this one
    child_status = metadata_slots.fields_of(child, OWNER).web_status
    if child_status is None or not child_status.waiting:
      continue
    if child.status == models.SessionStatus.ARCHIVED or descendant_id in archived:
      continue
    if tree.task_state(descendant_id) != "open":
      continue
    payload = child_status.model_dump(mode="json")
    row = {
        "session_id": descendant_id,
        "name": child.name,
        "updated_at": payload["updated_at"],
        "waiting": payload["waiting"],
    }
    rows.append((child_status.updated_at, row))
  rows.sort(key=lambda entry: entry[0], reverse=True)
  return {
      "session_id": session_id,
      "web_status": None if stored is None else stored.model_dump(mode="json"),
      "descendants": [row for _, row in rows],
  }
