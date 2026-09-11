"""Raw-event shaping for the chat message pipeline: upload normalization and stable history order.

The aggregation pipeline's input half: ``src.core.message_aggregator`` and
``src.core.message_projection`` import from this module at module level, so it
must not import the aggregator back. The event-construction twins
(``build_user_event`` and friends) and the session-view assembly stay in
``src.api.message_utils``.
"""

from src.core import event_types as ET

_ATTACHED_FILES_MARKER = "\n\n[Attached files]\n"


def _filename_from_path(path: str) -> str:
  """Display filename for an attachment path.

  Both attachment spellings — the structured ``uploaded_files`` refs and the
  legacy footer lines — must derive the same filename for the same path, so
  the derivation lives here. A path with no final segment ("", "/") comes
  back unchanged.
  """
  return path.replace("\\", "/").rstrip("/").split("/")[-1] or path


def serialize_uploaded_files(uploaded_files: list[object] | None) -> list[dict]:
  """Convert uploaded-file models or dicts into JSON-serializable dicts."""
  serialized: list[dict] = []
  for uploaded_file in uploaded_files or []:
    if hasattr(uploaded_file, "model_dump"):
      serialized.append(uploaded_file.model_dump(mode="json", exclude_none=True))
    elif isinstance(uploaded_file, dict):
      serialized.append(uploaded_file)
    elif isinstance(uploaded_file, str):
      serialized.append({"filename": _filename_from_path(uploaded_file), "path": uploaded_file})
    else:
      raise TypeError(f"Unsupported uploaded file payload: {type(uploaded_file)!r}")
  return serialized


def strip_attached_files_block(content: str) -> tuple[str, list[dict]]:
  """Split a legacy attachment footer from user-visible content."""
  if _ATTACHED_FILES_MARKER not in content:
    return content, []

  body, marker, attachments_block = content.rpartition(_ATTACHED_FILES_MARKER)
  if not marker:
    return content, []

  uploaded_files: list[dict] = []
  for line in attachments_block.splitlines():
    if not line.startswith("- "):
      return content, []
    path = line[2:].strip()
    if not path:
      return content, []
    uploaded_files.append({"filename": _filename_from_path(path), "path": path})

  return body, uploaded_files


def normalize_user_message_event(ev: dict) -> dict:
  """Return display content + structured uploads for a raw user event."""
  content = ev.get("content", "")
  uploaded_files = serialize_uploaded_files(ev.get("uploaded_files"))
  if uploaded_files:
    return {"content": content, "uploaded_files": uploaded_files}

  stripped_content, legacy_files = strip_attached_files_block(content)
  return {"content": stripped_content, "uploaded_files": legacy_files}


def _stable_history_projection(events: list[dict]) -> list[tuple[int, dict]]:
  """Move queued users behind completed OpenCode runs without changing source events."""
  complete_intervals: list[tuple[int, int]] = []
  interval_start: int | None = None
  for idx, event in enumerate(events):
    if event.get("type") is None and event.get("session_id"):
      interval_start = idx
    elif event.get("type") == ET.MASTER_DONE and interval_start is not None:
      complete_intervals.append((interval_start, idx))
      interval_start = None

  deferred_indices: set[int] = set()
  deferred_by_end: dict[int, list[tuple[int, dict]]] = {}
  for start, end in complete_intervals:
    deferred: list[tuple[int, dict]] = []
    for idx in range(start + 1, end):
      event = events[idx]
      if event.get("type") != ET.USER:
        continue
      if "message" in event and "content" not in event:
        continue
      if normalize_user_message_event(event)["content"].startswith("/"):
        continue
      deferred.append((idx, event))
    if deferred:
      deferred_by_end[end] = deferred
      deferred_indices.update(idx for idx, _ in deferred)

  projected: list[tuple[int, dict]] = []
  for idx, event in enumerate(events):
    if idx not in deferred_indices:
      projected.append((idx, event))
    projected.extend(deferred_by_end.get(idx, []))
  return projected


def stable_closed_prefix_len(events: list[dict]) -> int:
  """Largest prefix length whose stable-history order is final under appends.

  ``_stable_history_projection`` defers queued users only within one completed
  run interval, so once the prefix ends outside every open interval no later
  append can reorder it; the interval rule mirrors that function's own
  ``interval_start``/``MASTER_DONE`` scan and both must move together.
  """
  interval_open = False
  closed = 0
  for idx, event in enumerate(events):
    if event.get("type") is None and event.get("session_id"):
      interval_open = True
    elif event.get("type") == ET.MASTER_DONE and interval_open:
      interval_open = False
    if not interval_open:
      closed = idx + 1
  return closed
