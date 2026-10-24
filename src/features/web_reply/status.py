"""A web session's status: the Goal, Now and Waiting on you items that ``charliebot web status`` sets and reads.

A set replaces the items its file names, stores the status in the ``web_status`` metadata field, records one
``web_status`` event and tells every ancestor page. Waiting items need non-blank context before either the unread query
or the set checks digests. A set and a confirm refuse an item link to an older version of a registered plan or to a
closed plan before anything else runs. The set stores items only after a matching ``web_ask_check`` digest. The server
writes each item's ``since`` and ``since_exact``, keeping the start time of the item an update continues. The set's
return holds the stored status plus ``now_chars`` and ``now_over_budget``; the stored status and event keep neither. A
confirm records one ``web_status_confirmed`` event and cold reads nothing. A read lists open waiting descendants and
open children with a Now, both newest first, with every waiting item's start time filled in.

The cold-read side keeps one digest of a Waiting on you item. The unread query and set gate use the same digest and
``web_ask_check`` event lookup.
"""

import asyncio
import datetime
import hashlib
import json

import pydantic

from src.features.web_reply import metadata, plan_links, reply
from src.infra import event_types as ET
from src.infra import log_once, metadata_slots, models
from src.runtime import file_urls, session_events, session_store, streaming, task_sessions

logger = log_once.LazyStructlogLogger()

# The owner that the web reply package registers its metadata under (see the package's register()).
OWNER = "web_reply"

# The status items in the order a set lists them and the status line names them.
ITEMS = ("goal", "now", "waiting")

# The label of each item in the event's content.
LABELS = {"goal": "Goal", "now": "Now", "waiting": "Waiting on you"}

# The non-persisted frame an ancestor's page receives when a descendant sets its status.
SUBTREE_NOTICE_TYPE = "web_status_subtree"

# The Now budget the set's now_over_budget flag measures; an over-budget Now still stores.
NOW_BUDGET_CHARS = 200

# The content of the one web_ask_check event a cold read records.
ASK_CHECK_CONTENT = "Cold read of a Waiting on you item"
# The event's wire keys: the item digest, the answering backend and the raw answer.
ITEM_SHA256 = "item_sha256"
BACKEND = "backend"
ANSWER = "answer"


class StatusPatch(pydantic.BaseModel):
  """The JSON object of ``charliebot web status set``. An item that the object leaves out keeps its stored value."""
  model_config = pydantic.ConfigDict(extra="forbid")

  goal: metadata.Text | None = None
  now: metadata.Text | None = None
  waiting: list[metadata.WaitingItem] = pydantic.Field(default_factory=list)


def _error_summary(exc: pydantic.ValidationError) -> str:
  return "; ".join(f"{'.'.join(str(part) for part in err['loc'])}: {err['msg']}" for err in exc.errors())


def _refuse_server_fields(raw: dict) -> None:
  """Refuse a set file whose waiting item carries a field that only the server writes."""
  waiting = raw.get("waiting")
  if not isinstance(waiting, list):
    return
  for raw_item in waiting:
    if not isinstance(raw_item, dict):
      continue
    written = [name for name in ("since", "since_exact") if name in raw_item]
    if written:
      raise reply.WebReplyError(
          422, f"Waiting item fields {', '.join(written)} are written by the server; remove them from the file")


def parse_patch(text: str, scan_context: file_urls.ScanContext) -> StatusPatch:
  """The valid status patch in *text*; WebReplyError 422 for invalid items, no named item or a dead file link."""
  try:
    raw = json.loads(text)
  except json.JSONDecodeError as exc:
    raise reply.WebReplyError(422, f"Status file is not JSON: {exc.msg}") from exc
  if not isinstance(raw, dict):
    raise reply.WebReplyError(422, "Status file is not a JSON object")
  _refuse_server_fields(raw)
  try:
    patch = StatusPatch.model_validate(raw)
  except pydantic.ValidationError as exc:
    raise reply.WebReplyError(422, f"Status file is invalid: {_error_summary(exc)}") from exc
  if not patch.model_fields_set:
    raise reply.WebReplyError(422, "Status file names no item: give goal, now or waiting")
  findings: list[file_urls.FileLink] = []
  if patch.goal is not None:
    findings.extend(file_urls.scan_file_links(patch.goal, scan_context))
  if patch.now is not None:
    findings.extend(file_urls.scan_file_links(patch.now, scan_context))
  for item in patch.waiting:
    for value in (item.text, item.context, item.link):
      if value is not None:
        findings.extend(file_urls.scan_file_links(value, scan_context))
  dead_links = [finding for finding in findings if finding.dead]
  if dead_links:
    raise reply.WebReplyError(422, file_urls.dead_links_detail(dead_links))
  return patch


def effective_waiting(status: metadata.WebStatus) -> list[metadata.WaitingItem]:
  """The status's waiting items with ``since`` and ``since_exact`` filled in.

  An item stored before the fields existed has no start time of its own; the status's ``updated_at`` is the lower
  bound, so the item reads with ``since_exact`` false.
  """
  items: list[metadata.WaitingItem] = []
  for item in status.waiting:
    since = status.updated_at if item.since is None else item.since
    exact = False if item.since_exact is None else item.since_exact
    if item.since == since and item.since_exact == exact:
      items.append(item)
    else:
      items.append(item.model_copy(update={"since": since, "since_exact": exact}))
  return items


def status_payload(status: metadata.WebStatus) -> dict:
  """The status as the API and the CLI return it, with each waiting item's start time filled in."""
  payload = status.model_dump(mode="json")
  payload["waiting"] = [item.model_dump(mode="json") for item in effective_waiting(status)]
  return payload


def _require_current_plan_links(
    items: list[metadata.WaitingItem], scan_context: file_urls.ScanContext) -> plan_links.PlanLinks:
  links = plan_links.PlanLinks(scan_context.charliebot_home / "sessions")
  stale = links.stale_links(items, scan_context)
  if stale:
    raise reply.WebReplyError(409, plan_links.stale_detail(stale))
  return links


def _assign_since(
    new_items: list[metadata.WaitingItem], stored_items: list[metadata.WaitingItem], links: plan_links.PlanLinks,
    now: datetime.datetime) -> list[metadata.WaitingItem]:
  """The new items with each one's start time kept from the stored item it continues.

  An item whose text equals a stored item's text keeps that item's time. The remaining items pair in order within
  groups of one link target: a registered plan lineage, another link, or no link at all.
  """
  available: list[metadata.WaitingItem | None] = list(stored_items)
  assigned: list[metadata.WaitingItem | None] = [None] * len(new_items)
  for index, item in enumerate(new_items):
    match = next(
        (slot for slot, stored in enumerate(available) if stored is not None and stored.text == item.text), None)
    if match is None:
      continue
    stored = available[match]
    assert stored is not None
    available[match] = None
    assigned[index] = item.model_copy(update={"since": stored.since, "since_exact": stored.since_exact})
  new_groups: dict[str, list[int]] = {}
  stored_groups: dict[str, list[int]] = {}
  for index, item in enumerate(new_items):
    if assigned[index] is None:
      new_groups.setdefault(links.target(item), []).append(index)
  for slot, stored in enumerate(available):
    if stored is not None:
      stored_groups.setdefault(links.target(stored), []).append(slot)
  for target, indices in new_groups.items():
    paired = stored_groups.get(target, [])
    for position, index in enumerate(indices):
      if position < len(paired):
        stored = available[paired[position]]
        assert stored is not None
        assigned[index] = new_items[index].model_copy(update={"since": stored.since, "since_exact": stored.since_exact})
      else:
        assigned[index] = new_items[index].model_copy(update={"since": now, "since_exact": True})
  result = [item for item in assigned if item is not None]
  assert len(result) == len(new_items)
  return result


async def set_status(
    session_id: str, text: str, store: session_store.SessionStore, events: session_events.SessionEvents,
    tree: task_sessions.TaskTreeManager, *, scan_context: file_urls.ScanContext) -> dict:
  """Store the items that *text* names as the session's status and return the stored status with the Now length.

  Refusals raise WebReplyError before anything is stored or recorded. A stale plan link refuses the set before the
  cold read. A stored set records one web_status event and sends the ancestor notice to every ancestor's page. The
  return adds ``now_chars`` and ``now_over_budget`` to the stored status; the event and the stored metadata keep
  neither.
  """
  meta = await reply.require_web_session(store, session_id)
  patch = parse_patch(text, scan_context)
  _require_waiting_context(patch)
  stored = metadata_slots.fields_of(meta, OWNER).web_status
  base = metadata.WebStatus() if stored is None else stored
  stored_waiting = effective_waiting(base)
  prospective = patch.waiting if "waiting" in patch.model_fields_set else stored_waiting
  links = _require_current_plan_links(prospective, scan_context)
  if "waiting" in patch.model_fields_set:
    unread = _unread_items(patch.waiting, await _read_item_digests(session_id, events))
    if unread:
      names = "; ".join(item.text for item in unread)
      raise reply.WebReplyError(409, f"Waiting on you items need a cold read: {names}")
  now = models.utc_now()
  waiting = (
      _assign_since(patch.waiting, stored_waiting, links, now)
      if "waiting" in patch.model_fields_set else stored_waiting)
  named = {item: getattr(patch, item) for item in ITEMS if item in patch.model_fields_set}
  status = base.model_copy(update={**named, "waiting": waiting, "updated_at": now})
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
  now_chars = 0 if status.now is None else len(status.now)
  return {**payload, "now_chars": now_chars, "now_over_budget": now_chars > NOW_BUDGET_CHARS}


async def confirm_status(
    session_id: str, store: session_store.SessionStore, events: session_events.SessionEvents, *,
    scan_context: file_urls.ScanContext) -> dict:
  """Confirm that the stored status needs no change and return it.

  A stale plan link in a stored item refuses the confirmation with 409 and names the newest file. A confirmation
  records one web_status_confirmed event and cold reads nothing.
  """
  meta = await reply.require_web_session(store, session_id)
  stored = metadata_slots.fields_of(meta, OWNER).web_status
  status = metadata.WebStatus() if stored is None else stored
  _require_current_plan_links(effective_waiting(status), scan_context)
  await events.persist_and_broadcast(session_id, {
      "type": ET.WEB_STATUS_CONFIRMED,
      "content": "Status confirmed",
  })
  logger.info("web_status_confirmed", session=session_id)
  return status_payload(status)


def item_sha256(item: metadata.WaitingItem) -> str:
  """The digest that names *item* as it stands: its three fields and the bytes of the host file its link names.

  The linked path contributes bytes only when it names a file; directories and missing paths contribute no bytes.
  """
  payload = json.dumps(
      {
          "text": item.text,
          "context": item.context,
          "link": item.link
      }, sort_keys=True, ensure_ascii=False)
  digest = hashlib.sha256(payload.encode("utf-8"))
  path = file_urls.host_file(item.link)
  if path is not None and path.is_file():
    digest.update(path.read_bytes())
  return digest.hexdigest()


def parse_item(raw: object) -> metadata.WaitingItem:
  """The Waiting on you item that *raw* holds; WebReplyError 422 when it is not one."""
  try:
    return metadata.WaitingItem.model_validate(raw)
  except pydantic.ValidationError as exc:
    raise reply.WebReplyError(422, f"Waiting item is invalid: {_error_summary(exc)}") from exc


def _require_waiting_context(patch: StatusPatch) -> None:
  """Refuse a waiting item without context before its digest is checked."""
  for item in patch.waiting:
    if item.context is None:
      raise reply.WebReplyError(422, f"Waiting item needs context: {item.text}")


async def record_ask_check(
    session_id: str, raw_item: object, backend: str, answer: str, store: session_store.SessionStore,
    events: session_events.SessionEvents) -> dict:
  """Record one cold read of *raw_item* and return the item's digest.

  Refusals raise WebReplyError before anything is recorded: 404 an unknown session, 409 a thread session or a worker,
  422 an invalid item.
  """
  await reply.require_web_session(store, session_id)
  item = parse_item(raw_item)
  digest = item_sha256(item)
  await events.persist_and_broadcast(
      session_id, {
          "type": ET.WEB_ASK_CHECK,
          "content": ASK_CHECK_CONTENT,
          ITEM_SHA256: digest,
          BACKEND: backend,
          ANSWER: answer,
      })
  logger.info("web_ask_check_recorded", session=session_id, backend=backend)
  return {ITEM_SHA256: digest}


async def unread_waiting(
    session_id: str, text: str, store: session_store.SessionStore, events: session_events.SessionEvents, *,
    scan_context: file_urls.ScanContext) -> dict:
  """The waiting items of the status file *text* that no web_ask_check event of the session names.

  The file is parsed and validated as a set parses it, including the waiting-item context check; refusals match the
  set's. Nothing is stored or recorded.
  """
  meta = await reply.require_web_session(store, session_id)
  patch = parse_patch(text, scan_context)
  _require_waiting_context(patch)
  stored = metadata_slots.fields_of(meta, OWNER).web_status
  prospective = patch.waiting if "waiting" in patch.model_fields_set else ([] if stored is None else stored.waiting)
  _require_current_plan_links(prospective, scan_context)
  unread = _unread_items(patch.waiting, await _read_item_digests(session_id, events))
  return {"unread": [item.model_dump(mode="json") for item in unread]}


async def _read_item_digests(session_id: str, events: session_events.SessionEvents) -> set[str | None]:
  """The item digests of every web_ask_check event in *session_id*."""
  loaded = await asyncio.to_thread(events.load_chat_events_sync, session_id)
  return {ev.get(ITEM_SHA256) for ev in loaded if ev.get("type") == ET.WEB_ASK_CHECK}


def _unread_items(items: list[metadata.WaitingItem], read_digests: set[str | None]) -> list[metadata.WaitingItem]:
  """The items whose current digest is absent from *read_digests*."""
  return [item for item in items if item_sha256(item) not in read_digests]


async def _open_statuses(
    session_ids: list[str], store: session_store.SessionStore, tree: task_sessions.TaskTreeManager,
    archived: set[str]) -> list[tuple[str, models.SessionMetadata, metadata.WebStatus]]:
  """The sessions among *session_ids* that are not archived, have an open task, and hold a web status."""
  listed: list[tuple[str, models.SessionMetadata, metadata.WebStatus]] = []
  for session_id in session_ids:
    child = await store.get_session(session_id)
    if child is None:
      continue  # deleted between the index read and this one
    child_status = metadata_slots.fields_of(child, OWNER).web_status
    if child_status is None:
      continue
    if child.status == models.SessionStatus.ARCHIVED or session_id in archived:
      continue
    if tree.task_state(session_id) != "open":
      continue
    listed.append((session_id, child, child_status))
  return listed


async def read_status(session_id: str, store: session_store.SessionStore, tree: task_sessions.TaskTreeManager) -> dict:
  """The session's status, the Waiting on you items of its open descendants and the Now of its open children.

  A descendant qualifies when its status holds at least one waiting item. A child qualifies when its status holds a
  Now. Both groups need the session not archived and the task open, and both list newest first.
  """
  meta = await reply.require_web_session(store, session_id)
  stored = metadata_slots.fields_of(meta, OWNER).web_status
  archived = await tree.derived_archived_ids()
  rows: list[tuple[datetime.datetime, dict]] = []
  descendant_ids = await tree.descendant_ids(session_id)
  for descendant_id, child, child_status in await _open_statuses(descendant_ids, store, tree, archived):
    if not child_status.waiting:
      continue
    payload = status_payload(child_status)
    row = {
        "session_id": descendant_id,
        "name": child.name,
        "updated_at": payload["updated_at"],
        "waiting": payload["waiting"],
    }
    rows.append((child_status.updated_at, row))
  rows.sort(key=lambda entry: entry[0], reverse=True)
  children: list[tuple[datetime.datetime, dict]] = []
  for child_id, child, child_status in await _open_statuses(await tree.child_ids(session_id), store, tree, archived):
    if child_status.now is None:
      continue
    payload = child_status.model_dump(mode="json")
    row = {
        "session_id": child_id,
        "name": child.name,
        "now": payload["now"],
        "updated_at": payload["updated_at"],
    }
    children.append((child_status.updated_at, row))
  children.sort(key=lambda entry: entry[0], reverse=True)
  return {
      "session_id": session_id,
      "web_status": None if stored is None else status_payload(stored),
      "descendants": [row for _, row in rows],
      "children": [row for _, row in children],
  }
