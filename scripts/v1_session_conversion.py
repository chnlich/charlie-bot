#!/usr/bin/env python3
"""Convert the v1 sessions of one CharlieBot home into task-tree manager roots.

A v1 session has metadata ``profile`` None (schema_version 1). The conversion
gives each one the facts a manager root carries, so the runtime can drop its
v1 branches. The server and every other writer of the home must be stopped:
``apply`` and ``rollback`` hold the home writer fence and refuse while a
server holds it.

  uv run python scripts/v1_session_conversion.py dry-run  --home HOME
  uv run python scripts/v1_session_conversion.py apply    --home HOME
  uv run python scripts/v1_session_conversion.py rollback --home HOME

``--home`` is required. The script never reads CHARLIEBOT_HOME and has no
default home.

Selection: every directory under ``HOME/sessions/`` whose ``metadata.json``
has ``profile`` None. Sessions with a profile and everything under a
session's ``threads/`` stay untouched.

``apply`` converts each selected session in this order, so a crash between two
steps leaves a state that the next run completes:

  1. Append one ``task_imported`` event to ``data/chat_events.jsonl``. It lists
     no pending input, so the session's earlier inputs become history.
  2. When the session is archived, append one ``task_closed`` event with
     outcome ``archived``.
  3. Rewrite ``metadata.json`` with schema_version 2, profile ``manager`` and
     ``created_by_event`` naming the ``task_imported`` event. Every other key
     keeps its value and the file keeps its serialization.

Both event ids derive from the session id, so a rerun finds an appended event
and appends nothing twice. The conversion writes no ``native_prompt_hash``:
the session's next turn starts a fresh backend conversation behind the reset
note, because a v2 turn continues a conversation only on a matching hash.

``apply`` writes ``HOME/migrations/v1_conversion.json`` before it touches a
session and finishes it last, so a rollback after an interrupted apply still
knows every session the run reached. ``rollback`` restores each metadata file
first and removes the appended events second, because a manager root without
its ``task_imported`` event would read the whole old history as pending input.
Events written after the conversion stay.

``dry-run`` writes nothing. It prints the counts, the sessions it would
convert, the sessions it cannot parse, and the v1 sessions whose last user
input has no ``master_done`` after it. When a finished receipt exists it also
reads the conversion back: ``apply`` ends with the same readback.
"""

from __future__ import annotations

import argparse
import dataclasses
import datetime
import json
import os
import pathlib
import shutil
import subprocess
import sys
import uuid

import orjson

from src.infra import event_types as ET
from src.runtime import chat_events, control_events, home_writer_fence, task_sessions

RECEIPT_RELATIVE_PATH = pathlib.Path("migrations") / "v1_conversion.json"
ROLLED_BACK_RELATIVE_PATH = pathlib.Path("migrations") / "v1_conversion.rolled_back.json"
RECEIPT_KIND = "v1_session_conversion/receipt/v1"
RECEIPT_COMPLETE = "complete"
RECEIPT_IN_PROGRESS = "in_progress"

# The request id the conversion's archived close fact binds to, the same role
# archive_subtree's per-call request id plays for its close facts.
CONVERSION_REQUEST_ID = "v1-session-conversion"

CONVERTED_PROFILE = "manager"
CONVERTED_SCHEMA_VERSION = 2

# The keys the conversion sets, in the order the metadata model declares them.
CONVERTED_KEYS = ("schema_version", "profile", "created_by_event")

# The serializations metadata.json files carry: the server writes indent 2
# with UTF-8 text, and older writers wrote one line. A session whose file
# matches none of them is not converted, because a rollback could not restore
# its bytes.
SERIALIZATIONS: dict[str, dict] = {
    "compact_ascii": {
        "indent": None,
        "ensure_ascii": True
    },
    "compact_utf8": {
        "indent": None,
        "ensure_ascii": False
    },
    "indent2_utf8": {
        "indent": 2,
        "ensure_ascii": False
    },
}
TRAILING_NEWLINE_SUFFIX = "+newline"


def import_event_id(session_id: str) -> str:
  """The event id the one ``task_imported`` fact of a converted session binds to."""
  return str(uuid.uuid5(control_events.TASK_ID_NAMESPACE, f"task-imported:{session_id}"))


def close_event_id(session_id: str) -> str:
  """The event id the archived ``task_closed`` fact of a converted session binds to."""
  return control_events.stable_close_event_id(session_id, CONVERSION_REQUEST_ID)


def appended_event_ids(session_id: str, was_archived: bool) -> list[str]:
  """The ids the conversion appends to one session's log, in append order."""
  return [import_event_id(session_id), *([close_event_id(session_id)] if was_archived else [])]


# ---------------------------------------------------------------------------
# Reading one home
# ---------------------------------------------------------------------------


@dataclasses.dataclass
class LogScan:
  """What one pass over a session's ``chat_events.jsonl`` found.

  ``error`` is the first thing that stops the log from being parsed or
  appended to; the other fields are valid only when it is None. ``found_ids``
  holds the watched ids that occur in the log. ``tail`` holds every event from
  the first watched ``task_imported`` event to the end of the log.
  ``last_input_unanswered`` is true when the last real user message has no
  ``master_done`` after it.
  """
  error: str | None = None
  found_ids: set[str] = dataclasses.field(default_factory=set)
  tail: list[dict] = dataclasses.field(default_factory=list)
  last_input_unanswered: bool = False


def scan_log(path: pathlib.Path, watched_ids: frozenset[str], import_id: str) -> LogScan:
  """Parse every line of the log at *path*; a missing log is an empty one.

  Blank lines are skipped, as the server skips them. A line that is not a JSON
  object, or a final line without its newline, is an error: an append behind
  a torn line would corrupt the appended event too.
  """
  scan = LogScan()
  if not path.exists():
    return scan
  collecting = False
  last_user_seen = False
  answered = False
  with path.open("rb") as stream:
    for number, line in enumerate(stream, 1):
      stripped = line.strip()
      if not stripped:
        continue
      try:
        event = orjson.loads(stripped)
      except orjson.JSONDecodeError as e:
        scan.error = f"{path}: line {number} is not valid JSON: {e}"
        return scan
      if not isinstance(event, dict):
        scan.error = f"{path}: line {number} is not a JSON object"
        return scan
      event_id = event.get("id")
      if event_id in watched_ids:
        scan.found_ids.add(event_id)
      if event_id == import_id:
        collecting = True
      if collecting:
        scan.tail.append(event)
      if ET.is_real_user_message(event):
        last_user_seen = True
        answered = False
      elif event.get("type") == ET.MASTER_DONE and last_user_seen:
        answered = True
  if path.stat().st_size and not _ends_with_newline(path):
    scan.error = f"{path}: the last line has no newline"
    return scan
  scan.last_input_unanswered = last_user_seen and not answered
  return scan


def _ends_with_newline(path: pathlib.Path) -> bool:
  with path.open("rb") as stream:
    stream.seek(-1, os.SEEK_END)
    return stream.read(1) == b"\n"


@dataclasses.dataclass
class SessionScan:
  """One session directory as read: its metadata object and its log scan."""
  directory: pathlib.Path
  meta: dict | None = None
  serialization: str | None = None
  error: str | None = None
  log: LogScan = dataclasses.field(default_factory=LogScan)

  @property
  def session_id(self) -> str:
    return self.directory.name

  @property
  def is_v1(self) -> bool:
    return self.meta is not None and self.meta.get("profile") is None

  @property
  def is_archived(self) -> bool:
    return self.meta is not None and self.meta.get("status") == "archived"


def scan_session(directory: pathlib.Path, *, with_log: bool) -> SessionScan:
  """Read one session directory; every parse failure lands in ``error``.

  The log is parsed only when *with_log* is true: the conversion reads the log
  of a v1 session and of a session its receipt names, nothing else.
  """
  scan = SessionScan(directory=directory)
  try:
    text = (directory / "metadata.json").read_text(encoding="utf-8")
    meta = json.loads(text)
  except (OSError, ValueError) as e:
    scan.error = f"metadata.json cannot be read: {e}"
    return scan
  if not isinstance(meta, dict):
    scan.error = "metadata.json is not a JSON object"
    return scan
  if meta.get("id") != directory.name:
    scan.error = f"metadata id {meta.get('id')!r} does not match the directory name"
    return scan
  if meta.get("profile") is None and meta.get("schema_version", 1) != 1:
    scan.error = f"schema_version {meta.get('schema_version')!r} without a profile"
    return scan
  scan.meta = meta
  scan.serialization = detect_serialization(text, meta)
  if scan.is_v1 and scan.serialization is None:
    scan.error = "metadata.json matches no serialization the conversion can restore byte for byte"
    return scan
  if scan.is_v1 and not chat_events.chat_events_path(directory).parent.is_dir():
    scan.error = "the session has no data directory"
    return scan
  if with_log:
    ids = appended_event_ids(directory.name, was_archived=True)
    scan.log = scan_log(chat_events.chat_events_path(directory), frozenset(ids), ids[0])
    scan.error = scan.log.error
  return scan


def sessions_dir_of(home: pathlib.Path) -> pathlib.Path:
  return home / "sessions"


def scan_home(
    home: pathlib.Path,
    *,
    log_session_ids: frozenset[str] = frozenset(),
    read_all_logs: bool = False,
) -> list[SessionScan]:
  """Every session directory of the home, in id order.

  A session directory holds a ``metadata.json``. An unpublished create's
  staging directory (``.task-*.tmp``) is never a session. The log is parsed
  for each v1 session, each id in *log_session_ids*, or every session when
  *read_all_logs* is true.
  """
  scans: list[SessionScan] = []
  for entry in sorted(sessions_dir_of(home).iterdir()):
    if not entry.is_dir() or (entry.name.startswith(".task-") and entry.name.endswith(".tmp")):
      continue
    if not (entry / "metadata.json").exists():
      continue
    scan = scan_session(entry, with_log=read_all_logs)
    if scan.meta is not None and not read_all_logs and (scan.is_v1 or entry.name in log_session_ids):
      scan = scan_session(entry, with_log=True)
    scans.append(scan)
  return scans


@dataclasses.dataclass(frozen=True)
class Census:
  """The counts the receipt and the readback compare: archived means the stored status."""
  total: int
  archived: int
  v1: int


def census_of(scans: list[SessionScan]) -> Census:
  parsed = [s for s in scans if s.meta is not None]
  return Census(
      total=len(parsed), archived=sum(1 for s in parsed if s.is_archived), v1=sum(1 for s in parsed if s.is_v1))


# ---------------------------------------------------------------------------
# Writing: appends, metadata, receipt
# ---------------------------------------------------------------------------


def detect_serialization(text: str, meta: dict) -> str | None:
  """The name of the serialization that reproduces *text* from *meta* exactly, else None."""
  for name, options in SERIALIZATIONS.items():
    rendered = json.dumps(meta, **options)
    if text == rendered:
      return name
    if text == rendered + "\n":
      return name + TRAILING_NEWLINE_SUFFIX
  return None


def render_metadata(meta: dict, serialization: str) -> str:
  name = serialization.removesuffix(TRAILING_NEWLINE_SUFFIX)
  text = json.dumps(meta, **SERIALIZATIONS[name])
  return text + "\n" if serialization.endswith(TRAILING_NEWLINE_SUFFIX) else text


def replace_atomically(path: pathlib.Path, data: bytes) -> None:
  """Publish *data* at *path* through a durable temp file and ``os.replace``."""
  path.parent.mkdir(parents=True, exist_ok=True)
  temporary = path.with_name(f".{path.name}.{uuid.uuid4().hex}.tmp")
  try:
    with temporary.open("wb") as stream:
      stream.write(data)
      stream.flush()
      os.fsync(stream.fileno())
    if path.exists():
      shutil.copymode(path, temporary)
    os.replace(temporary, path)
  except BaseException:
    temporary.unlink(missing_ok=True)
    raise


def append_event(session_dir: pathlib.Path, event: dict) -> None:
  """Append one event line to the session's log and make it durable before returning."""
  path = chat_events.chat_events_path(session_dir)
  line = (json.dumps(event, ensure_ascii=False) + "\n").encode("utf-8")
  fd = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o666)
  try:
    view = memoryview(line)
    while view:
      view = view[os.write(fd, view):]
    os.fsync(fd)
  finally:
    os.close(fd)


def build_import_event(session_id: str) -> dict:
  return control_events.build_control_event(
      ET.TASK_IMPORTED,
      actor=control_events.ACTOR_SYSTEM,
      source_session_id=session_id,
      event_id=import_event_id(session_id),
      pending_inputs=[],
  )


def build_close_event(session_id: str) -> dict:
  """The archived close fact in the shape ``TaskTreeManager.archive_subtree`` writes."""
  return control_events.build_control_event(
      ET.TASK_CLOSED,
      actor=control_events.ACTOR_SYSTEM,
      source_session_id=session_id,
      event_id=close_event_id(session_id),
      request_id=CONVERSION_REQUEST_ID,
      outcome="archived",
      summary="archived before the v1 session conversion",
      result_refs=[],
      run_ids=[],
      report_to=None,
  )


def write_converted_metadata(scan: SessionScan, entry: dict) -> None:
  """Step 3: the metadata rewrite. Only the three converted keys change."""
  meta = dict(scan.meta)
  meta["schema_version"] = CONVERTED_SCHEMA_VERSION
  meta["profile"] = CONVERTED_PROFILE
  meta["created_by_event"] = {"session_id": scan.session_id, "event_id": import_event_id(scan.session_id)}
  text = render_metadata(meta, entry["metadata_serialization"])
  replace_atomically(scan.directory / "metadata.json", text.encode("utf-8"))


def convert_session(scan: SessionScan, entry: dict) -> None:
  """Convert one v1 session: the two appends, then the metadata, each skipped when already done."""
  if import_event_id(scan.session_id) not in scan.log.found_ids:
    append_event(scan.directory, build_import_event(scan.session_id))
  if entry["was_archived"] and close_event_id(scan.session_id) not in scan.log.found_ids:
    append_event(scan.directory, build_close_event(scan.session_id))
  write_converted_metadata(scan, entry)


def receipt_path(home: pathlib.Path) -> pathlib.Path:
  return home / RECEIPT_RELATIVE_PATH


def load_receipt(home: pathlib.Path) -> dict | None:
  path = receipt_path(home)
  if not path.exists():
    return None
  receipt = json.loads(path.read_text(encoding="utf-8"))
  if receipt.get("kind") != RECEIPT_KIND:
    raise ValueError(f"{path} is not a v1 conversion receipt")
  return receipt


def write_receipt(home: pathlib.Path, receipt: dict) -> None:
  replace_atomically(receipt_path(home), (json.dumps(receipt, indent=2, ensure_ascii=False) + "\n").encode("utf-8"))


def source_sha() -> str:
  """The git HEAD of the checkout this script runs from."""
  here = pathlib.Path(__file__).resolve().parent
  result = subprocess.run(["git", "-C", str(here), "rev-parse", "HEAD"], check=True, capture_output=True, text=True)
  return result.stdout.strip()


def utc_now_iso() -> str:
  return datetime.datetime.now(datetime.UTC).isoformat()


# ---------------------------------------------------------------------------
# Readback
# ---------------------------------------------------------------------------


def readback(home: pathlib.Path, receipt: dict) -> list[str]:
  """Compare the home with its receipt; the returned list holds one line per mismatch.

  The counts of sessions and archived sessions equal the receipt's before
  counts and no v1 session remains. Each converted session carries the
  converted metadata, and the events from its ``task_imported`` fact through
  its last appended fact fold to state ``archived`` when it was archived and
  ``open`` otherwise, with no pending input. Events after the conversion are
  later facts and stay out of the fold.
  """
  problems: list[str] = []
  scans = scan_home(home, log_session_ids=frozenset(e["session_id"] for e in receipt["converted"]))
  problems.extend(f"{s.session_id}: {s.error}" for s in scans if s.error is not None)
  census = census_of(scans)
  counts = receipt["counts"]
  for name, now, before in (("total sessions", census.total, counts["total_sessions"]),
                            ("archived sessions", census.archived, counts["archived_sessions"])):
    if now != before:
      problems.append(f"{name}: {now} now, {before} before the conversion")
  if census.v1 != 0:
    problems.append(f"{census.v1} v1 session(s) remain")
  by_id = {s.session_id: s for s in scans}
  for entry in receipt["converted"]:
    problems.extend(_readback_entry(by_id.get(entry["session_id"]), entry))
  return problems


def _readback_entry(scan: SessionScan | None, entry: dict) -> list[str]:
  session_id = entry["session_id"]
  if scan is None:
    return [f"{session_id}: the session directory is gone"]
  if scan.meta is None:
    return []  # already reported as a parse error
  problems: list[str] = []
  expected_ref = {"session_id": session_id, "event_id": import_event_id(session_id)}
  if (scan.meta.get("schema_version"), scan.meta.get("profile"),
      scan.meta.get("created_by_event")) != (CONVERTED_SCHEMA_VERSION, CONVERTED_PROFILE, expected_ref):
    problems.append(f"{session_id}: metadata is not the converted shape")
  if scan.is_archived != entry["was_archived"]:
    problems.append(f"{session_id}: stored status moved since the conversion")
  last_id = entry["appended_event_ids"][-1]
  ids = [e.get("id") for e in scan.log.tail]
  if last_id not in ids:
    problems.append(f"{session_id}: the log lacks its appended event {last_id}")
    return problems
  facts = task_sessions._fold_task_events(task_sessions._TaskFacts(), scan.log.tail[:ids.index(last_id) + 1], 0)
  expected_state = "archived" if entry["was_archived"] else "open"
  if facts.task_state != expected_state:
    problems.append(f"{session_id}: folds to {facts.task_state}, expected {expected_state}")
  if facts.input_candidates:
    problems.append(f"{session_id}: folds with {len(facts.input_candidates)} pending input(s)")
  return problems


# ---------------------------------------------------------------------------
# Subcommands
# ---------------------------------------------------------------------------


def resolve_home(raw: str) -> pathlib.Path:
  """The home named by ``--home``; it must hold a ``sessions/`` directory."""
  home = pathlib.Path(raw).expanduser().resolve()
  if not sessions_dir_of(home).is_dir():
    raise SystemExit(f"preflight failed: {sessions_dir_of(home)} is not a directory")
  return home


def print_scan_errors(scans: list[SessionScan]) -> int:
  errors = [s for s in scans if s.error is not None]
  if errors:
    print(f"cannot parse {len(errors)} session(s):")
    for scan in errors:
      print(f"  {scan.session_id}: {scan.error}")
  return len(errors)


def format_census(label: str, census: Census) -> str:
  return f"{label}: total={census.total} archived={census.archived} v1={census.v1}"


def cmd_dry_run(home: pathlib.Path) -> int:
  scans = scan_home(home, read_all_logs=True)
  print(format_census("sessions", census_of(scans)))
  v1 = [s for s in scans if s.is_v1 and s.error is None]
  print(f"would convert {len(v1)} session(s):")
  for scan in v1:
    print(f"  {scan.session_id}  {'archived' if scan.is_archived else 'active  '}  {scan.meta.get('name')!r}")
  unparseable = print_scan_errors(scans)
  unanswered = [s for s in v1 if s.log.last_input_unanswered]
  print(
      f"{len(unanswered)} v1 session(s) whose last user input has no master_done after it "
      "(informational: the conversion lists no pending input):")
  for scan in unanswered:
    print(f"  {scan.session_id}")
  fence = home_writer_fence.probe_writer_fence(home)
  print(f"writer fence: {'HELD' if fence['exclusive_holder_alive'] else 'free'} ({fence['lock_path']})")
  receipt = load_receipt(home)
  problems: list[str] = []
  if receipt is None:
    print("receipt: none")
  elif receipt["status"] != RECEIPT_COMPLETE:
    print(f"receipt: {receipt['status']} with {len(receipt['converted'])} planned session(s); apply finishes it")
  else:
    problems = readback(home, receipt)
    print(
        f"receipt: complete, {len(receipt['converted'])} converted session(s); readback "
        f"{'OK' if not problems else 'MISMATCH'}")
    for problem in problems:
      print(f"  {problem}")
  return 1 if unparseable or problems else 0


def plan_receipt(home: pathlib.Path, sha: str, previous: dict | None, v1: list[SessionScan], census: Census) -> dict:
  """The receipt this apply works from: the interrupted run's entries first, then the new ones.

  An entry of an earlier run keeps its recorded old values, since its session
  no longer shows them.
  """
  entries = {e["session_id"]: e for e in (previous or {"converted": []})["converted"]}
  for scan in v1:
    if scan.session_id in entries:
      continue
    entries[scan.session_id] = {
        "session_id": scan.session_id,
        "was_archived": scan.is_archived,
        "appended_event_ids": appended_event_ids(scan.session_id, scan.is_archived),
        "log_existed": chat_events.chat_events_path(scan.directory).exists(),
        "old_schema_version": scan.meta.get("schema_version", 1),
        "old_profile": scan.meta.get("profile"),
        "old_values": {
            k: scan.meta[k] for k in CONVERTED_KEYS if k in scan.meta
        },
        "added_metadata_keys": [k for k in CONVERTED_KEYS if k not in scan.meta],
        "metadata_serialization": scan.serialization,
    }
  counts = previous["counts"] if previous is not None else {
      "total_sessions": census.total,
      "archived_sessions": census.archived,
      "v1_before": census.v1,
      "v1_after": None,
  }
  return {
      "kind": RECEIPT_KIND,
      "status": RECEIPT_IN_PROGRESS,
      "home": str(home),
      "source_sha": sha,
      "started_at": previous["started_at"] if previous is not None else utc_now_iso(),
      "finished_at": None,
      "counts": counts,
      "converted": list(entries.values()),
  }


def cmd_apply(home: pathlib.Path) -> int:
  try:
    fence = home_writer_fence.acquire_home_writer_fence(home, purpose="v1 session conversion apply")
  except (home_writer_fence.HomeWriterActiveError, home_writer_fence.FencePathRefusalError) as e:
    print(f"apply refused: {e}", file=sys.stderr)
    return 1
  with fence:
    sha = source_sha()  # the receipt pins this checkout before the first session write
    scans = scan_home(home)
    if print_scan_errors(scans):
      print("apply refused: fix the sessions above, then run apply again", file=sys.stderr)
      return 1
    census = census_of(scans)
    v1 = [s for s in scans if s.is_v1]
    previous = load_receipt(home)
    if previous is not None and previous["status"] == RECEIPT_COMPLETE:
      if v1:
        print(f"apply refused: {receipt_path(home)} is complete yet {len(v1)} v1 session(s) remain", file=sys.stderr)
        return 1
      print(f"nothing to convert; {receipt_path(home)} is complete")
      return _report_readback(readback(home, previous))
    receipt = plan_receipt(home, sha, previous, v1, census)
    write_receipt(home, receipt)  # the plan lands before any session changes
    entries = {e["session_id"]: e for e in receipt["converted"]}
    print(format_census("before", census))
    for number, scan in enumerate(v1, 1):
      convert_session(scan, entries[scan.session_id])
      print(f"converted {number}/{len(v1)} {scan.session_id}")
    after = census_of(scan_home(home))
    receipt["status"] = RECEIPT_COMPLETE
    receipt["finished_at"] = utc_now_iso()
    receipt["counts"]["v1_after"] = after.v1
    write_receipt(home, receipt)
    print(format_census("after", after))
    return _report_readback(readback(home, receipt))


def _report_readback(problems: list[str]) -> int:
  if problems:
    print(f"readback MISMATCH ({len(problems)}):")
    for problem in problems:
      print(f"  {problem}")
    return 1
  print("readback OK")
  return 0


def rollback_metadata(scan: SessionScan, entry: dict) -> bool:
  """Restore one session's metadata to its pre-conversion values; true when it wrote the file."""
  meta = dict(scan.meta)
  if meta.get("profile") is None:
    return False  # restored by an earlier rollback
  expected_ref = {"session_id": scan.session_id, "event_id": import_event_id(scan.session_id)}
  if meta.get("profile") != CONVERTED_PROFILE or meta.get("created_by_event") != expected_ref:
    raise ValueError(f"{scan.session_id}: metadata no longer shows this conversion's shape")
  for key in CONVERTED_KEYS:
    if key in entry["added_metadata_keys"]:
      del meta[key]
    else:
      meta[key] = entry["old_values"][key]
  replace_atomically(
      scan.directory / "metadata.json",
      render_metadata(meta, entry["metadata_serialization"]).encode("utf-8"))
  return True


def rollback_events(session_dir: pathlib.Path, event_ids: list[str], *, log_existed: bool) -> int:
  """Remove the lines whose event id is in *event_ids* from the session's log; the count removed.

  A log the conversion created and left empty is deleted with its lines.
  """
  path = chat_events.chat_events_path(session_dir)
  if not path.exists():
    return 0
  needles = [i.encode("utf-8") for i in event_ids]
  wanted = set(event_ids)
  removed = 0
  temporary = path.with_name(f".{path.name}.{uuid.uuid4().hex}.tmp")
  try:
    with path.open("rb") as source, temporary.open("wb") as target:
      for line in source:
        if any(n in line for n in needles) and orjson.loads(line).get("id") in wanted:
          removed += 1
          continue
        target.write(line)
      target.flush()
      os.fsync(target.fileno())
    if removed:
      shutil.copymode(path, temporary)
      os.replace(temporary, path)
  finally:
    temporary.unlink(missing_ok=True)
  if not log_existed and path.stat().st_size == 0:
    path.unlink()
  return removed


def cmd_rollback(home: pathlib.Path) -> int:
  try:
    fence = home_writer_fence.acquire_home_writer_fence(home, purpose="v1 session conversion rollback")
  except (home_writer_fence.HomeWriterActiveError, home_writer_fence.FencePathRefusalError) as e:
    print(f"rollback refused: {e}", file=sys.stderr)
    return 1
  with fence:
    receipt = load_receipt(home)
    if receipt is None:
      print(f"rollback refused: {receipt_path(home)} does not exist", file=sys.stderr)
      return 1
    failures: list[str] = []
    for entry in receipt["converted"]:
      directory = sessions_dir_of(home) / entry["session_id"]
      scan = scan_session(directory, with_log=False) if (directory / "metadata.json").exists() else None
      if scan is None or scan.meta is None:
        failures.append(f"{entry['session_id']}: {'directory is gone' if scan is None else scan.error}")
        continue
      try:
        restored = rollback_metadata(scan, entry)  # first: a v1 session may keep stray events, a root may not lose one
        removed = rollback_events(directory, entry["appended_event_ids"], log_existed=entry["log_existed"])
      except ValueError as e:
        failures.append(str(e))
        continue
      if restored and removed != len(entry["appended_event_ids"]):
        failures.append(
            f"{entry['session_id']}: {removed} of {len(entry['appended_event_ids'])} appended event(s) "
            "found in the live log; the rest left it since the conversion")
        continue
      print(
          f"rolled back {entry['session_id']}: metadata {'restored' if restored else 'already v1'}, "
          f"{removed} event(s) removed")
    if failures:
      print(f"rollback incomplete ({len(failures)}):", file=sys.stderr)
      for failure in failures:
        print(f"  {failure}", file=sys.stderr)
      return 1
    os.replace(receipt_path(home), home / ROLLED_BACK_RELATIVE_PATH)
    print(f"rollback complete; receipt moved to {home / ROLLED_BACK_RELATIVE_PATH}")
    return 0


def build_parser() -> argparse.ArgumentParser:
  parser = argparse.ArgumentParser(description="Convert the v1 sessions of one CharlieBot home into manager roots.")
  commands = parser.add_subparsers(dest="command", required=True)
  for name, summary in (("dry-run", "report the conversion and write nothing"),
                        ("apply", "convert every v1 session (holds the home writer fence)"),
                        ("rollback", "undo the conversion recorded in the receipt (holds the home writer fence)")):
    command = commands.add_parser(name, help=summary, description=summary)
    command.add_argument("--home", required=True, help="the CHARLIEBOT_HOME directory to convert")
  return parser


def main(argv: list[str] | None = None) -> int:
  args = build_parser().parse_args(argv)
  home = resolve_home(args.home)
  if args.command == "dry-run":
    return cmd_dry_run(home)
  if args.command == "apply":
    return cmd_apply(home)
  if args.command == "rollback":
    return cmd_rollback(home)
  raise AssertionError(f"unreachable command {args.command!r}")


if __name__ == "__main__":
  sys.exit(main())
