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
from collections.abc import Callable
from typing import Any

import normalize
import servers
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
    elif relative_dir == "tmp":
      # A headless Chrome profile is named at random and holds per-run browser state: not compared.
      dirnames[:] = [name for name in dirnames if not name.startswith(servers.HEADLESS_PROFILE_PREFIX)]
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


@dataclasses.dataclass(frozen=True)
class EventFieldRule:
  """One event field whose baseline value and pair order vary across Python runs."""

  event_type: str
  match_fields: dict[str, Any]
  field: str
  allowed_baseline_values: tuple[Any, ...]
  required_candidate_value: Any


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
    return [
        _jsonl_line_value(line, lambda value: normalize.normalize_json(value, rules, ctx))
        for line in entry.data.splitlines()
        if line.strip()
    ]
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
    event_field_rules: tuple[tuple[EventFieldRule, EventFieldRule], ...] = (),
    candidate_is_python: bool = False,
) -> list[str]:
  if (left_relative.endswith("/data/chat_events.jsonl") and right_relative.endswith("/data/chat_events.jsonl") and
      left.kind == "file" and right.kind == "file" and event_field_rules):
    return _event_log_diff(
        left_relative, left, right_relative, right, left_ctx, right_ctx, event_field_rules, candidate_is_python)
  left_value = _normalized_file(left_relative, left, left_ctx)
  right_value = _normalized_file(right_relative, right, right_ctx)
  if isinstance(left_value, bytes) and isinstance(right_value, bytes):
    return [] if left_value == right_value else _text_diff(left_value, right_value)
  return json_diff(left_value, right_value)


def _event_matches(event: Any, rule: EventFieldRule) -> bool:
  return isinstance(event, dict) and event.get("type") == rule.event_type and all(
      key in event and not json_diff(event[key], expected) for key, expected in rule.match_fields.items())


def _matching_event_indices(events: list[Any], rule: EventFieldRule) -> list[int]:
  return [index for index, event in enumerate(events) if _event_matches(event, rule)]


def _value_is_allowed(value: Any, allowed: tuple[Any, ...]) -> bool:
  return any(not json_diff(value, expected) for expected in allowed)


def _master_done_index(events: list[Any], run_finished_index: int, run_finished: dict) -> int | None:
  input_ids = run_finished.get("input_event_ids")
  if not isinstance(input_ids, list) or not input_ids:
    return None
  candidates = [
      index for index, event in enumerate(events)
      if isinstance(event, dict) and event.get("type") == "master_done" and event.get("input_event_ids") == input_ids
  ]
  return min(candidates, key=lambda index: abs(index - run_finished_index)) if candidates else None


def _canonicalize_run_finish_order(events: list[Any], run_finished_index: int) -> None:
  finished = events[run_finished_index]
  if not isinstance(finished, dict):
    return
  master_done_index = _master_done_index(events, run_finished_index, finished)
  if master_done_index is None:
    return
  master_done = events[master_done_index]
  first, second = sorted((run_finished_index, master_done_index))
  if events[first].get("type") == "master_done":
    events[first], events[second] = events[second], events[first]
  finished["id"] = "<normalized-run-finished-id>"
  finished.pop("event_index", None)
  master_done["id"] = "<normalized-master-done-id>"
  master_done.pop("event_index", None)


def _jsonl_line_value(line: bytes, normalize_value: Callable[[Any], Any] | None = None) -> Any:
  """One JSONL line's value, passed through normalize_value when json.loads accepts the line. A line that json.loads
  refuses keeps its text under malformed_jsonl_line, which no event rule matches, so it compares as that value."""
  try:
    value = json.loads(line)
  except ValueError:
    return {"malformed_jsonl_line": line.decode("utf-8", errors="surrogateescape")}
  return value if normalize_value is None else normalize_value(value)


def _event_log_diff(
    left_relative: str,
    left: Entry,
    right_relative: str,
    right: Entry,
    left_ctx: normalize.Context,
    right_ctx: normalize.Context,
    event_field_rules: tuple[tuple[EventFieldRule, EventFieldRule], ...],
    candidate_is_python: bool,
) -> list[str]:
  left_events = [_jsonl_line_value(line) for line in left.data.splitlines() if line.strip()]
  right_events = [_jsonl_line_value(line) for line in right.data.splitlines() if line.strip()]
  lines: list[str] = []
  for baseline_rule, candidate_rule in event_field_rules:
    rule_name = "allowed_baseline_values_for_one_event_field"
    baseline_matches = _matching_event_indices(left_events, baseline_rule)
    candidate_matches = _matching_event_indices(right_events, candidate_rule)
    if not baseline_matches and not candidate_matches:
      continue
    if len(baseline_matches) != 1:
      lines.append(f"{rule_name}: baseline matched {len(baseline_matches)} {baseline_rule.event_type} events")
    if len(candidate_matches) != 1:
      lines.append(f"{rule_name}: candidate matched {len(candidate_matches)} {candidate_rule.event_type} events")
    if len(baseline_matches) != 1 or len(candidate_matches) != 1:
      continue
    baseline_index, candidate_index = baseline_matches[0], candidate_matches[0]
    baseline_event = left_events[baseline_index]
    candidate_event = right_events[candidate_index]
    baseline_value = baseline_event.get(baseline_rule.field)
    candidate_value = candidate_event.get(candidate_rule.field)
    baseline_valid = baseline_rule.field in baseline_event and _value_is_allowed(
        baseline_value, baseline_rule.allowed_baseline_values)
    if not baseline_valid:
      lines.append(
          f"{rule_name}: baseline {baseline_rule.event_type}.{baseline_rule.field} "
          f"is {_show(baseline_value)}, outside {_show(list(baseline_rule.allowed_baseline_values))}")
    candidate_valid = candidate_rule.field in candidate_event and (
        _value_is_allowed(candidate_value, candidate_rule.allowed_baseline_values)
        if candidate_is_python else not json_diff(candidate_value, candidate_rule.required_candidate_value))
    if not candidate_valid:
      requirement = "one of " + _show(list(candidate_rule.allowed_baseline_values)) if candidate_is_python else _show(
          candidate_rule.required_candidate_value)
      lines.append(
          f"{rule_name}: candidate {candidate_rule.event_type}.{candidate_rule.field} "
          f"is {_show(candidate_value)}, expected {requirement}")
    if baseline_valid:
      baseline_event.pop(baseline_rule.field)
    if candidate_valid:
      candidate_event.pop(candidate_rule.field)
    _canonicalize_run_finish_order(left_events, baseline_index)
    _canonicalize_run_finish_order(right_events, candidate_index)
  left_rules = normalize.file_rules(left_relative)
  right_rules = normalize.file_rules(right_relative)
  left_value = [normalize.normalize_json(event, left_rules, left_ctx) for event in left_events]
  right_value = [normalize.normalize_json(event, right_rules, right_ctx) for event in right_events]
  return [*lines, *json_diff(left_value, right_value)]


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


@dataclasses.dataclass(frozen=True)
class EventSelector:
  """One chat event that an adjacency rule finds in a log: its type and the fields it must hold."""

  event_type: str
  match_fields: dict[str, Any]


def _selected(event: Any, selector: EventSelector) -> bool:
  return isinstance(event, dict) and event.get("type") == selector.event_type and all(
      key in event and not json_diff(event[key], expected) for key, expected in selector.match_fields.items())


def _adjacent_pairs(events: list[Any], first: EventSelector, second: EventSelector) -> list[tuple[int, int]]:
  """Each (first, second) neighbour pair of the log, in log order, either adjacency order.

  A *first* event pairs with its one neighbour that matches *second*; one with no or two such neighbours pairs with
  none, and a *second* event one pair already claimed pairs with no further *first*.
  """
  seconds_at = {index for index, event in enumerate(events) if _selected(event, second)}
  pairs: list[tuple[int, int]] = []
  claimed: set[int] = set()
  for index, event in enumerate(events):
    if not _selected(event, first):
      continue
    neighbours = [other for other in (index - 1, index + 1) if other in seconds_at and other not in claimed]
    if len(neighbours) == 1:
      claimed.add(neighbours[0])
      pairs.append((index, neighbours[0]))
  return pairs


def _ordered_chat_log(data: bytes, first: EventSelector, second: EventSelector) -> tuple[bytes, dict[str, str]]:
  """The log with each pair written as first, then second, and each pair event's raw id mapped to its placeholder."""
  lines = [line for line in data.splitlines() if line.strip()]
  events = [_jsonl_line_value(line) for line in lines]
  pairs = _adjacent_pairs(events, first, second)
  if not pairs:
    return data, {}
  placed: dict[int, Any] = {}
  for first_index, second_index in pairs:
    lower, upper = sorted((first_index, second_index))
    placed[lower] = events[first_index]
    placed[upper] = events[second_index]
  rewritten = list(lines)
  masks: dict[str, str] = {}
  for position, event in sorted(placed.items()):
    event.pop("event_index", None)
    if "id" in event:
      placeholder = f"<normalized-{event['type']}-id>"
      masks[event["id"]] = placeholder
      event["id"] = placeholder
    rewritten[position] = json.dumps(event).encode()
  return b"".join(line + b"\n" for line in rewritten), masks


def order_adjacent_pair(delta: Delta, first: EventSelector, second: EventSelector) -> Delta:
  """*delta* with each chat log's adjacent (first, second) pair written as first, then second.

  The pair then compares as one order, whichever order the tree wrote it in, for every pair the log holds: a
  multi-turn session's log carries one pair per audited turn. A swap moves each event's position, so event_index is
  dropped from both events. Each event's id is masked wherever the tree names it, in its own line and in every later
  reference such as input_event_ids, so a run record that names the nudge compares as well. A *first* event with no or
  two *second* neighbours keeps its content, and the diff reports it.
  """
  masks: dict[str, str] = {}

  def ordered(entries: dict[str, Entry]) -> dict[str, Entry]:
    result: dict[str, Entry] = {}
    for path, entry in entries.items():
      if entry.kind == "file" and path.endswith("/data/chat_events.jsonl"):
        data, pair_masks = _ordered_chat_log(entry.data, first, second)
        masks.update(pair_masks)
        result[path] = Entry(entry.kind, data)
      else:
        result[path] = entry
    return result

  added = ordered(delta.added)
  changed = ordered(delta.changed)
  return dataclasses.replace(delta, added=_masked_ids(added, masks), changed=_masked_ids(changed, masks))


def order_event_type_streams(data: bytes, event_types: tuple[str, ...]) -> bytes:
  """Move selected events after other events while preserving their combined original order.

  Every event's event_index is omitted because moving selected events changes every event's position.
  """
  lines = [line for line in data.splitlines() if line.strip()]
  events = [_jsonl_line_value(line) for line in lines]
  selected_types = set(event_types)
  selected = []
  remaining = []
  for event in events:
    if isinstance(event, dict):
      event.pop("event_index", None)
    event_type = event.get("type") if isinstance(event, dict) else None
    if isinstance(event_type, str) and event_type in selected_types:
      selected.append(event)
    else:
      remaining.append(event)
  ordered = [*remaining, *selected]
  return b"".join(json.dumps(event).encode() + b"\n" for event in ordered)


def order_event_type_streams_in_delta(delta: Delta, path: str, event_types: tuple[str, ...]) -> Delta:
  """*delta* with one chat log's selected event types grouped in the declared order."""

  def ordered(entries: dict[str, Entry]) -> dict[str, Entry]:
    result: dict[str, Entry] = {}
    for entry_path, entry in entries.items():
      if entry_path == path and entry.kind == "file":
        result[entry_path] = Entry(entry.kind, order_event_type_streams(entry.data, event_types))
      else:
        result[entry_path] = entry
    return result

  return dataclasses.replace(delta, added=ordered(delta.added), changed=ordered(delta.changed))


def _masked_ids(entries: dict[str, Entry], masks: dict[str, str]) -> dict[str, Entry]:
  """*entries* with each file's references to a masked event id replaced by that event's placeholder."""
  result: dict[str, Entry] = {}
  for path, entry in entries.items():
    if entry.kind == "file":
      data = entry.data
      for event_id, placeholder in masks.items():
        data = data.replace(json.dumps(event_id).encode(), json.dumps(placeholder).encode())
      result[path] = Entry(entry.kind, data)
    else:
      result[path] = entry
  return result


def delta_diff(
    left: Delta,
    right: Delta,
    left_ctx: normalize.Context,
    right_ctx: normalize.Context,
    event_field_rules: tuple[tuple[EventFieldRule, EventFieldRule], ...] = (),
    candidate_is_python: bool = False,
) -> list[str]:
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
              left_path, new_left[left_path], right_path, new_right[right_path], left_ctx, right_ctx, event_field_rules,
              candidate_is_python))
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
  if clean.get("fake") == "discord" and clean.get("kind") == "http" and clean.get("method") == "POST" and \
      clean.get("path", "").endswith("/messages"):
    body = clean.get("body")
    if isinstance(body, dict) and "nonce" in body:
      # Each paired home has its own run id, which Discord's nonce hashes.
      clean["body"] = {**body, "nonce": "<discord-post-nonce>"}
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


def status_and_shape_diff(
    route: str,
    left: Response,
    right: Response,
    left_ctx: normalize.Context,
    right_ctx: normalize.Context,
) -> list[str]:
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
  if isinstance(left_detail, list) or isinstance(right_detail, list):
    if not (isinstance(left_detail, list) and isinstance(right_detail, list)):
      lines.append("body detail: a list on one side only")
      return lines
    if len(left_detail) != len(right_detail):
      lines.append(f"body detail: {len(left_detail)} errors != {len(right_detail)} errors")
    for index, (left_item, right_item) in enumerate(zip(left_detail, right_detail)):
      left_shape = {key: left_item.get(key) for key in ("type", "loc")} if isinstance(left_item, dict) else None
      right_shape = {key: right_item.get(key) for key in ("type", "loc")} if isinstance(right_item, dict) else None
      lines.extend(f"body detail[{index}] {line}" for line in json_diff(left_shape, right_shape))
    return lines
  left_object = isinstance(left_body, dict) and "detail" in left_body
  right_object = isinstance(right_body, dict) and "detail" in right_body
  if not (left_object and right_object):
    lines.append("body: both sides must be JSON objects with a detail key")
  return lines


def _response_event_rule_bodies(
    route: str,
    left: Response,
    right: Response,
    left_ctx: normalize.Context,
    right_ctx: normalize.Context,
    event_field_rules: tuple[tuple[EventFieldRule, EventFieldRule], ...],
    candidate_is_python: bool,
) -> tuple[Any, Any, list[str]]:
  """Validate a matching run field carried by a response and omit only that field."""
  if not event_field_rules or not (_is_json(left) and _is_json(right)):
    return _normalized_body(left, route, left_ctx), _normalized_body(right, route, right_ctx), []
  left_bytes = normalize.mask_proxy_message_ids(left.decoded_body(), route)
  right_bytes = normalize.mask_proxy_message_ids(right.decoded_body(), route)
  left_raw, right_raw = json.loads(left_bytes), json.loads(right_bytes)
  lines: list[str] = []
  for baseline_rule, candidate_rule in event_field_rules:
    baseline_matches = isinstance(left_raw, dict) and all(
        key in left_raw and not json_diff(left_raw[key], expected)
        for key, expected in baseline_rule.match_fields.items())
    candidate_matches = isinstance(right_raw, dict) and all(
        key in right_raw and not json_diff(right_raw[key], expected)
        for key, expected in candidate_rule.match_fields.items())
    if not (baseline_matches and candidate_matches and baseline_rule.field in left_raw and
            candidate_rule.field in right_raw):
      continue
    baseline_value = left_raw.get(baseline_rule.field)
    candidate_value = right_raw.get(candidate_rule.field)
    baseline_valid = baseline_rule.field in left_raw and _value_is_allowed(
        baseline_value, baseline_rule.allowed_baseline_values)
    if not baseline_valid:
      lines.append(
          "allowed_baseline_values_for_one_event_field: baseline response "
          f"{baseline_rule.field} is {_show(baseline_value)}, outside "
          f"{_show(list(baseline_rule.allowed_baseline_values))}")
    candidate_valid = candidate_rule.field in right_raw and (
        _value_is_allowed(candidate_value, candidate_rule.allowed_baseline_values)
        if candidate_is_python else not json_diff(candidate_value, candidate_rule.required_candidate_value))
    if not candidate_valid:
      requirement = "one of " + _show(list(candidate_rule.allowed_baseline_values)) if candidate_is_python else _show(
          candidate_rule.required_candidate_value)
      lines.append(
          "allowed_baseline_values_for_one_event_field: candidate response "
          f"{candidate_rule.field} is {_show(candidate_value)}, expected {requirement}")
    if baseline_valid:
      left_raw.pop(baseline_rule.field)
    if candidate_valid:
      right_raw.pop(candidate_rule.field)
  left_body = normalize.normalize_json(left_raw, normalize.response_rules(route), left_ctx)
  right_body = normalize.normalize_json(right_raw, normalize.response_rules(route), right_ctx)
  return left_body, right_body, lines


def response_diff(
    route: str,
    left: Response,
    right: Response,
    left_ctx: normalize.Context,
    right_ctx: normalize.Context,
    compare_mode: str = "full",
    event_field_rules: tuple[tuple[EventFieldRule, EventFieldRule], ...] = (),
    candidate_is_python: bool = False) -> list[str]:
  """Compare full responses or malformed-input status and error locations."""
  if compare_mode == "status_and_shape":
    return status_and_shape_diff(route, left, right, left_ctx, right_ctx)
  if compare_mode != "full":
    raise ValueError(f"unknown HTTP response comparison mode {compare_mode!r}")
  lines: list[str] = []
  if left.status != right.status:
    lines.append(f"status: {left.status} != {right.status}")
  left_headers = normalize.normalize_headers(left.headers, left_ctx, route)
  right_headers = normalize.normalize_headers(right.headers, right_ctx, route)
  lines.extend(
      f"header {name}: {left_headers.get(name)} != {right_headers.get(name)}"
      for name in sorted(left_headers.keys() | right_headers.keys())
      if left_headers.get(name) != right_headers.get(name))
  left_body, right_body, body_rule_lines = _response_event_rule_bodies(
      route, left, right, left_ctx, right_ctx, event_field_rules, candidate_is_python)
  lines.extend(body_rule_lines)
  if isinstance(left_body, bytes) or isinstance(right_body, bytes):
    if left_body != right_body:
      if isinstance(left_body, bytes) and isinstance(right_body, bytes):
        lines.extend(f"body: {line}" for line in _text_diff(left_body, right_body))
      else:
        lines.append("body: JSON on one side only")
  else:
    lines.extend(f"body {line}" for line in json_diff(left_body, right_body))
  return lines
