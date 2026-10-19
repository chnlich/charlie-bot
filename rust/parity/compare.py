"""Comparison of the two servers: their responses and the files each step changed in their trees.

Both sides pass through normalize.py first; every difference that survives normalization is a parity
difference. JSON compares structurally (key order is free, value types are not); every other body and file
compares as bytes.
"""

import base64
import collections
import dataclasses
import difflib
import gzip
import json
import os
import pathlib
import time
from typing import Any

import normalize
import yaml

# Lines one difference listing prints before it summarizes the rest.
MAX_LISTED = 25
VALUE_WIDTH = 160
SETTLE_QUIET_SECONDS = 0.3
SETTLE_TIMEOUT_SECONDS = 15.0

# -- JSON -------------------------------------------------------------------


def _show(value: Any) -> str:
  text = json.dumps(value, ensure_ascii=False, sort_keys=True)
  return text if len(text) <= VALUE_WIDTH else text[:VALUE_WIDTH] + "…"


def json_diff(left: Any, right: Any, path: str = "$") -> list[str]:
  """Every structural difference between two JSON values, one line each, addressed by a JSON path."""
  if isinstance(left, dict) and isinstance(right, dict):
    lines: list[str] = []
    for key in sorted(left.keys() | right.keys()):
      child = f"{path}.{key}"
      if key not in right:
        lines.append(f"{child}: only in baseline: {_show(left[key])}")
      elif key not in left:
        lines.append(f"{child}: only in candidate: {_show(right[key])}")
      else:
        lines.extend(json_diff(left[key], right[key], child))
    return lines
  if isinstance(left, list) and isinstance(right, list):
    lines = []
    for index in range(min(len(left), len(right))):
      lines.extend(json_diff(left[index], right[index], f"{path}[{index}]"))
    if len(left) != len(right):
      longer, side = (left, "baseline") if len(left) > len(right) else (right, "candidate")
      extra = longer[min(len(left), len(right)):]
      lines.append(f"{path}: length {len(left)} != {len(right)}; extra in {side}: {_show(extra)}")
    return lines
  # bool is an int subclass and 1 == 1.0: a value differs when its JSON type does.
  if type(left) is not type(right) or left != right:
    return [f"{path}: {_show(left)} != {_show(right)}"]
  return []


def _first_difference(left: str, right: str) -> str:
  """Both lines around their first differing column."""
  column = next(
      (index for index, (a, b) in enumerate(zip(left, right, strict=False)) if a != b), min(len(left), len(right)))
  start = max(0, column - VALUE_WIDTH // 2)
  return (
      f"col {column + 1}: …{left[start:column + VALUE_WIDTH // 2]!r} != "
      f"…{right[start:column + VALUE_WIDTH // 2]!r}")


def _text_diff(left: bytes, right: bytes) -> list[str]:
  """The differing lines of two bodies; a changed line shows the window around its first differing column."""
  try:
    left_lines, right_lines = left.decode("utf-8").splitlines(), right.decode("utf-8").splitlines()
  except UnicodeDecodeError:
    return [f"binary content differs ({len(left)} vs {len(right)} bytes)"]
  lines: list[str] = []
  matcher = difflib.SequenceMatcher(a=left_lines, b=right_lines, autojunk=False)
  for tag, left_start, left_end, right_start, right_end in matcher.get_opcodes():
    if tag == "equal":
      continue
    if tag == "replace" and left_end - left_start == right_end - right_start:
      lines.extend(
          f"line {left_start + offset + 1} "
          f"{_first_difference(left_lines[left_start + offset], right_lines[right_start + offset])}"
          for offset in range(left_end - left_start))
      continue
    lines.extend(
        f"line {left_start + offset + 1} only in baseline: {left_lines[left_start + offset][:VALUE_WIDTH]!r}"
        for offset in range(left_end - left_start))
    lines.extend(
        f"line {right_start + offset + 1} only in candidate: {right_lines[right_start + offset][:VALUE_WIDTH]!r}"
        for offset in range(right_end - right_start))
  if not lines:
    return [f"content differs only in line endings or a trailing newline ({len(left)} vs {len(right)} bytes)"]
  return lines


def capped(lines: list[str]) -> list[str]:
  if len(lines) <= MAX_LISTED:
    return lines
  return [*lines[:MAX_LISTED], f"… {len(lines) - MAX_LISTED} more"]


# -- trees ------------------------------------------------------------------


@dataclasses.dataclass(frozen=True)
class Entry:
  kind: str  # "dir", "file", "symlink" or "other"
  data: bytes  # a file's content or a link's target; empty for the other kinds


Snapshot = dict[str, Entry]


class TreeChangingError(Exception):
  """A file vanished between listing and reading: the server is still writing."""


def snapshot(root: pathlib.Path, skip: tuple[str, ...]) -> Snapshot:
  """Every directory, file and link beneath *root*, by tree-relative path; top-level *skip* entries left out."""
  entries: Snapshot = {}
  for dirpath, dirnames, filenames in os.walk(root):
    relative_dir = os.path.relpath(dirpath, root)
    if relative_dir == ".":
      dirnames[:] = [name for name in dirnames if name not in skip]
      filenames = [name for name in filenames if name not in skip]
    dirnames.sort()
    for name in list(dirnames):
      path = os.path.join(dirpath, name)
      relative = os.path.normpath(os.path.join(relative_dir, name))
      if os.path.islink(path):
        dirnames.remove(name)
        entries[relative] = Entry("symlink", os.readlink(path).encode())
      else:
        entries[relative] = Entry("dir", b"")
    for name in sorted(filenames):
      path = os.path.join(dirpath, name)
      relative = os.path.normpath(os.path.join(relative_dir, name))
      try:
        if os.path.islink(path):
          entries[relative] = Entry("symlink", os.readlink(path).encode())
        elif os.path.isfile(path):
          entries[relative] = Entry("file", pathlib.Path(path).read_bytes())
        else:
          entries[relative] = Entry("other", b"")
      except FileNotFoundError as error:
        raise TreeChangingError(relative) from error
  return entries


def prepare_context(
    entries: Snapshot,
    ctx: normalize.Context,
    extra_files: tuple[tuple[str, bytes], ...] = (),
) -> None:
  """Assign minted UUID names from sorted tree entries and explicitly compared harness files."""
  values = [(path, entry.kind, entry.data) for path, entry in entries.items()]
  values.extend((path, "file", data) for path, data in extra_files)
  normalize.prepare_snapshot(values, ctx)


def settle_all(roots: list[pathlib.Path], skip: tuple[str, ...]) -> list[Snapshot]:
  """Each tree once two rounds of snapshots SETTLE_QUIET_SECONDS apart agree; raises past SETTLE_TIMEOUT_SECONDS.

  A server may finish a request's writes after it answers, so a step's file changes are read only once every
  tree has been still for one quiet interval.
  """
  deadline = time.monotonic() + SETTLE_TIMEOUT_SECONDS
  previous: list[Snapshot] | None = None
  while time.monotonic() < deadline:
    try:
      current = [snapshot(root, skip) for root in roots]
    except TreeChangingError:
      previous = None
    else:
      if current == previous:
        return current
      previous = current
    time.sleep(SETTLE_QUIET_SECONDS)
  raise RuntimeError(f"{[str(root) for root in roots]} kept changing for {SETTLE_TIMEOUT_SECONDS}s")


@dataclasses.dataclass(frozen=True)
class Delta:
  """What one step changed in one tree: path → new entry (None for a removed path)."""

  added: dict[str, Entry]
  removed: frozenset[str]
  changed: dict[str, Entry]


def delta(before: Snapshot, after: Snapshot) -> Delta:
  return Delta(
      added={
          path: entry for path, entry in after.items() if path not in before
      },
      removed=frozenset(path for path in before if path not in after),
      changed={
          path: entry for path, entry in after.items() if path in before and before[path] != entry
      },
  )


def _normalized_file(relative: str, entry: Entry, ctx: normalize.Context) -> Any:
  """A file's content as compared: JSON, JSONL and YAML parsed under normalize.py's rules, anything else as bytes."""
  if entry.kind != "file":
    return {"kind": entry.kind, "data": normalize.normalize_text(entry.data.decode(errors="surrogateescape"), ctx)}
  rules = normalize.file_rules(relative)
  if relative.endswith(".json"):
    return normalize.normalize_json(json.loads(entry.data), rules, ctx)
  if relative.endswith(".jsonl"):
    return [normalize.normalize_json(json.loads(line), rules, ctx) for line in entry.data.splitlines() if line.strip()]
  if relative.endswith((".yaml", ".yml")):
    return normalize.normalize_json(yaml.safe_load(entry.data), rules, ctx)
  return normalize.normalize_text(entry.data.decode("utf-8", errors="surrogateescape"), ctx).encode(
      "utf-8", errors="surrogateescape")


def _content_diff(
    left_relative: str,
    left: Entry,
    right_relative: str,
    right: Entry,
    left_ctx: normalize.Context,
    right_ctx: normalize.Context,
) -> list[str]:
  left_value = _normalized_file(left_relative, left, left_ctx)
  right_value = _normalized_file(right_relative, right, right_ctx)
  if isinstance(left_value, bytes) and isinstance(right_value, bytes):
    return [] if left_value == right_value else _text_diff(left_value, right_value)
  return json_diff(left_value, right_value)


def _normalized_path_map(paths: Any, ctx: normalize.Context) -> dict[str, str]:
  return {normalize.normalize_path(path, ctx): path for path in paths}


def tree_diff(left: Snapshot, right: Snapshot, left_ctx: normalize.Context, right_ctx: normalize.Context) -> list[str]:
  """Every difference between two whole trees (the fixture determinism check)."""
  lines: list[str] = []
  left_paths = _normalized_path_map(left, left_ctx)
  right_paths = _normalized_path_map(right, right_ctx)
  for path in sorted(left_paths.keys() | right_paths.keys()):
    if path not in right_paths:
      lines.append(f"{path}: only in baseline")
    elif path not in left_paths:
      lines.append(f"{path}: only in candidate")
    else:
      left_path, right_path = left_paths[path], right_paths[path]
      lines.extend(
          f"{path}: {line}"
          for line in _content_diff(left_path, left[left_path], right_path, right[right_path], left_ctx, right_ctx))
  return lines


def delta_diff(left: Delta, right: Delta, left_ctx: normalize.Context, right_ctx: normalize.Context) -> list[str]:
  """Every difference between what one step changed in each tree."""
  lines: list[str] = []
  for label, left_paths, right_paths in (
      ("added", left.added, right.added),
      ("removed", dict.fromkeys(left.removed), dict.fromkeys(right.removed)),
      ("changed", left.changed, right.changed),
  ):
    left_names = _normalized_path_map(left_paths, left_ctx)
    right_names = _normalized_path_map(right_paths, right_ctx)
    lines.extend(f"file {label} by baseline only: {path}" for path in sorted(left_names.keys() - right_names.keys()))
    lines.extend(f"file {label} by candidate only: {path}" for path in sorted(right_names.keys() - left_names.keys()))
  for new_left, new_right in ((left.added, right.added), (left.changed, right.changed)):
    left_names = _normalized_path_map(new_left, left_ctx)
    right_names = _normalized_path_map(new_right, right_ctx)
    for path in sorted(left_names.keys() & right_names.keys()):
      left_path, right_path = left_names[path], right_names[path]
      lines.extend(
          f"file {path}: {line}" for line in _content_diff(
              left_path, new_left[left_path], right_path, new_right[right_path], left_ctx, right_ctx))
  return lines


def describe_delta(change: Delta) -> str:
  """"+added -removed ~changed" path counts, or "" when the step changed nothing."""
  if not (change.added or change.removed or change.changed):
    return ""
  return f"files +{len(change.added)} -{len(change.removed)} ~{len(change.changed)}"


def _record_value(value: Any, ctx: normalize.Context) -> Any:
  if isinstance(value, bytes):
    return {"$bytes_base64": base64.b64encode(value).decode("ascii")}
  if isinstance(value, list):
    return [_record_value(item, ctx) for item in value]
  if isinstance(value, dict):
    return {key: _record_value(item, ctx) for key, item in value.items()}
  if isinstance(value, str):
    return normalize.normalize_text(value, ctx)
  return value


def _normalized_record(record: dict, route: str, ctx: normalize.Context) -> dict:
  clean = {key: value for key, value in record.items() if key != "_index"}
  if "headers" in clean:
    clean["headers"] = normalize.request_headers(
        clean["fake"], clean["headers"], ctx, websocket=clean["kind"] == "ws_handshake")
  if "message" in clean and isinstance(clean["message"], dict):
    clean["message"] = normalize.normalize_json(clean["message"], normalize.ws_frame_rules(route), ctx)
  return _record_value(clean, ctx)


def recording_diff(
    left: list[dict],
    right: list[dict],
    route: str,
    left_ctx: normalize.Context,
    right_ctx: normalize.Context,
) -> list[str]:
  """Compare fake HTTP requests as a multiset and each WebSocket connection's traffic in order."""

  def compared(records: list[dict], ctx: normalize.Context) -> tuple[list[dict], dict[tuple[str, str], list[dict]]]:
    http_records = []
    websocket: dict[tuple[str, str], list[dict]] = {}
    for record in records:
      if record.get("kind") == "refused":
        continue
      normalized = _normalized_record(record, route, ctx)
      if normalized.get("kind") == "http":
        http_records.append(normalized)
      elif normalized.get("kind") in ("ws_handshake", "ws_message"):
        if normalized.get("kind") == "ws_message" and normalize.ignored_fake_message(normalized):
          continue
        key = (str(normalized.get("fake")), str(normalized.get("connection")))
        websocket.setdefault(key, []).append(normalized)
    return http_records, websocket

  left_http, left_ws = compared(left, left_ctx)
  right_http, right_ws = compared(right, right_ctx)
  left_counts = collections.Counter(json.dumps(record, sort_keys=True, ensure_ascii=False) for record in left_http)
  right_counts = collections.Counter(json.dumps(record, sort_keys=True, ensure_ascii=False) for record in right_http)
  lines = []
  for record in sorted(left_counts.keys() | right_counts.keys()):
    left_count, right_count = left_counts[record], right_counts[record]
    if left_count != right_count:
      lines.append(f"fake HTTP request count {left_count} != {right_count}: {record}")
  for key in sorted(left_ws.keys() | right_ws.keys()):
    left_frames, right_frames = left_ws.get(key, []), right_ws.get(key, [])
    lines.extend(f"fake WebSocket {key[0]} connection {key[1]} {line}" for line in json_diff(left_frames, right_frames))
  return lines


# -- responses --------------------------------------------------------------


@dataclasses.dataclass(frozen=True)
class Response:
  status: int
  headers: list[tuple[str, str]]
  body: bytes

  def header(self, name: str) -> str | None:
    for key, value in self.headers:
      if key.lower() == name:
        return value
    return None

  def decoded_body(self) -> bytes:
    """The body with its content coding removed."""
    coding = self.header("content-encoding")
    if coding is None or coding == "identity":
      return self.body
    if coding == "gzip":
      return gzip.decompress(self.body)
    raise ValueError(f"unsupported content-encoding {coding!r}")

  def json(self) -> Any:
    return json.loads(self.decoded_body())


def _is_json(response: Response) -> bool:
  media_type = (response.header("content-type") or "").split(";")[0].strip().lower()
  return media_type == "application/json" or media_type.endswith("+json")


def _normalized_body(response: Response, route: str, ctx: normalize.Context) -> Any:
  body = response.decoded_body()
  if not body:
    return body
  body = normalize.mask_proxy_message_ids(body, route)
  if _is_json(response):
    return normalize.normalize_json(json.loads(body), normalize.response_rules(route), ctx)
  return normalize.normalize_body_bytes(body, route, ctx)


def response_diff(
    route: str, left: Response, right: Response, left_ctx: normalize.Context,
    right_ctx: normalize.Context) -> list[str]:
  """Every difference between the two responses: status, compared headers, then the body."""
  lines: list[str] = []
  if left.status != right.status:
    lines.append(f"status: {left.status} != {right.status}")
  left_headers = normalize.normalize_headers(left.headers, left_ctx, route)
  right_headers = normalize.normalize_headers(right.headers, right_ctx, route)
  lines.extend(
      f"header {name}: {left_headers.get(name)} != {right_headers.get(name)}"
      for name in sorted(left_headers.keys() | right_headers.keys())
      if left_headers.get(name) != right_headers.get(name))
  left_body = _normalized_body(left, route, left_ctx)
  right_body = _normalized_body(right, route, right_ctx)
  if isinstance(left_body, bytes) or isinstance(right_body, bytes):
    if left_body != right_body:
      if isinstance(left_body, bytes) and isinstance(right_body, bytes):
        lines.extend(f"body: {line}" for line in _text_diff(left_body, right_body))
      else:
        lines.append("body: JSON on one side only")
  else:
    lines.extend(f"body {line}" for line in json_diff(left_body, right_body))
  return lines


def status_and_shape_diff(
    route: str, left: Response, right: Response, left_ctx: normalize.Context,
    right_ctx: normalize.Context) -> list[str]:
  """The malformed-input comparison: the status, then each 422 error's type and loc, nothing else.

  For a body whose ``detail`` is a list, it compares the list length and each item's ``type`` and ``loc``; the
  error text may differ. For any other body, it only requires both sides to be JSON objects with a ``detail``
  key, or both to be non-JSON.
  """
  lines: list[str] = []
  if left.status != right.status:
    lines.append(f"status: {left.status} != {right.status}")
  left_body = _normalized_body(left, route, left_ctx)
  right_body = _normalized_body(right, route, right_ctx)
  if isinstance(left_body, bytes) or isinstance(right_body, bytes):
    if isinstance(left_body, bytes) != isinstance(right_body, bytes):
      lines.append("body: JSON on one side only")
    return lines
  left_detail = left_body.get("detail") if isinstance(left_body, dict) else None
  right_detail = right_body.get("detail") if isinstance(right_body, dict) else None
  if isinstance(left_detail, list) and isinstance(right_detail, list):
    if len(left_detail) != len(right_detail):
      lines.append(f"body detail: {len(left_detail)} errors != {len(right_detail)} errors")
    for index, (left_item, right_item) in enumerate(zip(left_detail, right_detail)):
      left_shape = {key: left_item.get(key) for key in ("type", "loc")} if isinstance(left_item, dict) else None
      right_shape = {key: right_item.get(key) for key in ("type", "loc")} if isinstance(right_item, dict) else None
      lines.extend(f"body detail[{index}] {line}" for line in json_diff(left_shape, right_shape))
    return lines
  left_object = isinstance(left_body, dict) and "detail" in left_body
  right_object = isinstance(right_body, dict) and "detail" in right_body
  if left_object != right_object:
    lines.append("body: one side is a JSON object with a detail key, the other side is not")
  return lines
