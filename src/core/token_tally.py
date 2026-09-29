"""Parse every agent log on this host into the usage ledger's records.

This module is the ledger's writing side (src/core/usage_ledger.py is the reading and
aggregation side): it reads each source's logs, turns every usage-bearing line into a
UsageRecord, and hands the records to the ledger, whose rows outlive the logs they were
parsed from. The entry points are ``capture_local`` (this host's own sources, the call the
page's sweep gate, the scheduler and the CLI share) and ``capture_usage`` (the sources the
caller names); captures run in the collector, so every parse or read failure raises
instead of noting-and-continuing.

Sources, all local logs (no vendor usage API is called):
  Claude Code  <config_dir>/projects/**/*.jsonl   assistant message.usage + message.model
  Codex        ~/.codex/sessions/**/*.jsonl       token_count events, model from turn_context
  opencode     ~/.local/share/opencode/opencode.db   table message, JSON data.tokens + modelID
  charlie-bot  ~/.charliebot/sessions/*/threads/*/data/events.jsonl thread result events,
               ~/.charliebot/sessions/*/data/master_runs/*/agent.raw.ndjson master-run captures,
               ~/.charliebot/sessions/*/data/runs/*/agent.raw.ndjson Run captures

Record ids — the ledger upserts on them, so a re-parsed file re-writes what it stored
before, a message seen twice counts once, and an updated source row moves its record:
  claude:<message id, falling back to requestId, then uuid>  per response. Claude Code
      replays history verbatim on resume and fork — about half of all usage lines on this
      host — so responses dedupe on the id, within a file and across config dirs alike
  codex:<session id>:<event index>  per token_count event. The session id comes off the
      rollout-*.jsonl file name (its last five dash-separated segments) or, for any other
      name, the path relative to its home. Subagent threads inherit the parent's
      cumulative total_token_usage, so per-request last_token_usage is summed instead
  opencode:<message id>  per contributing db row; zero-token rows contribute nothing
  thread:<session>/<thread>/<i>  per charlie-bot thread result event
  master:<session>/<run>  per master-run capture's trailing result
  run:<run id>  per Run capture, at most one

The fallback rule — the charlie-bot source and the CLI sources describe overlapping runs,
and a thread's or run's CLI log can disappear (history pruned) after the capture:
  NATIVE     usage only the charlie-bot log holds: CLC thread results, master-run
             captures, and a CLC Run's trailing result line. No sessions — nothing can
             restate it.
  FALLBACK   a codex-, cc-claude- or opencode-type thread or run, whose CLI log may still
             exist. The record carries the session ids its log names, and the ledger's
             any-match exclusion retires it the moment any of those ids has a NATIVE
             record of its own. A codex-type thread is admitted only when none of the
             session ids its event log carries matches a rollout-*.jsonl file name still
             under the Codex homes — any match skips the whole thread, which keeps a
             partially-pruned multi-session thread from being counted twice. A fallback
             candidate with no ids cannot key that exclusion and contributes nothing.

Cache — one JSON document of per-file Claude, Codex and charlie-bot contributions (the
gigabyte-scale, hundred-megabyte-scale and many-small-files sources), so a capture
re-parses only the files that changed. An entry stores one file's parsed records under
its stat signature ([mtime_ns, size]); ``lookup_sig`` serves an entry only while the
caller's signature matches and copies the hit into the next document, ``store_sig`` adds
fresh scans there, and the saved document holds only files seen this run — deleted logs
drop out without a separate sweep. These logs only grow by appends, so a moved file first
tries the append-tail fast path: the sha256 of the final window at the last parsed offset
(the boundary guard) must re-hash equal and end on a newline before only the appended
lines parse; a replaced, truncated or mid-line prefix fails the check and re-parses whole.
"""

from __future__ import annotations

import bisect
import datetime as dt
import hashlib
import json
import os
import re
import socket
import sqlite3
from collections import defaultdict
from collections.abc import Callable, Iterator
from pathlib import Path
from typing import BinaryIO, TypeVar

import orjson

from src.core import event_types as ET
from src.core.codex_usage import (
    CODEX_SESSION_META,
    CODEX_TOKEN_COUNT,
    CODEX_TURN_CONTEXT,
    DEFAULT_CODEX_HOME,
    codex_token_count_payload,
)
from src.core.config import default_claude_dir, get_config
from src.core.constants import (
    USAGE_SOURCE_CHARLIE_BOT,
    USAGE_SOURCE_CLAUDE_CODE,
    USAGE_SOURCE_CODEX,
    USAGE_SOURCE_OPENCODE,
    BackendType,
)
from src.core.json_utils import atomic_write_stream
from src.core.runs import DATA_DIR_NAME, MASTER_RUNS_DIR_NAME, RAW_LOG_NAME, RUN_METADATA_NAME, RUNS_DIR_NAME
from src.core.threads import EVENTS_LOG_NAME, METADATA_NAME, THREADS_DIR_NAME
from src.core.usage_ledger import RecordKind, UsageLedger, UsageRecord

DEFAULT_CLAUDE_DIR = default_claude_dir()
DEFAULT_OPENCODE_DB = Path.home() / ".local/share/opencode/opencode.db"


def discover_homes(claude_default: Path, codex_default: Path) -> tuple[dict[str, Path], dict[str, Path]]:
  """Claude config dirs and Codex homes from config.yaml plus the on-disk defaults.

  Reading the account list keeps a newly added pool account in the capture without an edit here;
  the default is always included, and codex always runs from its default home.
  """
  cfg = get_config()
  claude: set[Path] = {claude_default}
  for account in cfg.accounts.claude:
    claude.add(Path(account.config_dir).expanduser())
  codex: set[Path] = {codex_default}

  claude_map = {_account_label(p, ".claude"): p for p in sorted(claude) if (p / "projects").is_dir()}
  codex_map = {_account_label(p, ".codex"): p for p in sorted(codex) if (p / "sessions").is_dir()}
  return claude_map, codex_map


def _account_label(path: Path, stem: str) -> str:
  """Derive an account label from a dir name: the suffix after the ``<stem>-`` prefix.

  The provider default dir (``path.name == stem``) is labelled ``work (default)``; a custom
  dir ``.claude-ext-1`` (stem ``.claude``) reads as ``ext-1``. Parallel to ``src/api/ext_usage.py``'s
  account labels but not identical -- that one labels the default ``main`` and takes every other
  label verbatim from the configured pool; core must not import the api layer, so the derivation
  is restated here.
  """
  if path.name == stem:
    return "work (default)"
  return path.name.removeprefix(stem + "-")


class TallyCache:
  """Per-file parsed contributions keyed by file signature, persisted as one JSON document.

  ``lookup_sig`` serves an entry only while the caller's signature matches and copies the
  hit into the next document; ``store_sig`` adds fresh scans there. The saved document
  therefore holds only files seen this run — deleted logs drop out without a separate sweep.
  """

  SCHEMA_VERSION = 3

  def __init__(self, sources: dict[str, dict[str, dict]]) -> None:
    self._sources = sources
    self._next: dict[str, dict[str, dict]] = defaultdict(dict)

  @classmethod
  def load(cls, path: Path, notes: list[str]) -> TallyCache:
    """Read the persisted document; an unreadable or stale-schema file starts a cold cache.

    Version 1 and 2 documents still serve: their entries carry the same records under
    older shapes, and the first store rewrites them in the current one.
    """
    try:
      doc = orjson.loads(path.read_bytes())
    except FileNotFoundError:
      doc = None
    except (OSError, ValueError) as exc:
      notes.append(f"Tally cache: unreadable {path} ({exc}); rebuilt from the logs")
      doc = None
    if not isinstance(doc, dict) or doc.get("version") not in (1, 2, cls.SCHEMA_VERSION):
      return cls({})
    return cls(doc.get("sources", {}))

  def save(self, path: Path) -> None:
    """Persist the next document atomically when it moved, creating the cache directory."""
    if self._next == self._sources:
      return
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = orjson.dumps({"version": self.SCHEMA_VERSION, "sources": self._next})
    atomic_write_stream(path, lambda stream: stream.write(payload))

  def lookup_sig(self, source: str, key: str, sig: list) -> dict | None:
    """The cached entry for *key* when its stored signature equals *sig*, else None.

    *sig* is the caller's own proof — one file's stat pair from the walk that
    produced *key*, or a source-level signature like the opencode db's.
    """
    entry = self._sources.get(source, {}).get(key)
    if entry is None:
      return None
    if entry.get("sig") != sig:
      return None
    self._next[source][key] = entry
    return entry

  def prev(self, source: str, key: str) -> dict | None:
    """The persisted entry for *key* under whatever signature it last parsed.

    The append-tail fast path's prefix candidate: the file moved, or ``lookup_sig`` would
    have served it. The tail parse proves the prefix from the entry's own guard before
    trusting any of it.
    """
    return self._sources.get(source, {}).get(key)

  def store_sig(self, source: str, key: str, entry: dict) -> None:
    """Record one freshly scanned contribution for the next document (``lookup_sig``'s key)."""
    self._next[source][key] = entry


def _unreadable_note(notes: list[str], source: str, label: str, exc: object) -> None:
  """Append the walk's unreadable note for one path; the single home of its wording."""
  notes.append(f"{source}: unreadable {label}: {exc}")


def _walk_error_hook(notes: list[str], source: str, label: str, root_name: str) -> Callable[[OSError], None]:
  """The os.walk onerror hook turning an unreadable directory into a per-account note."""

  def _onerror(exc: OSError) -> None:
    if not isinstance(exc, FileNotFoundError):
      _unreadable_note(notes, source, f"{label}/{root_name}", exc)

  return _onerror


# The claude+codex walk's per-directory listing memo: (dirpath, suffixes) ->
# ((mtime_ns, size), (subdir paths, candidate file paths)). The candidate file names are
# stored suffix-filtered, so *suffixes* rides the memo key. Paths are absolute so a memo hit
# joins nothing. Entries are bounded by the historical directory set of the walked trees; a
# subtree that stops being walked leaves its entries until the process restarts.
_jsonl_dir_memo: dict[tuple[str, tuple[str, ...]], tuple[tuple[int, int], tuple[list[str], list[str]]]] = {}

_MemoKey = TypeVar("_MemoKey")
_MemoValue = TypeVar("_MemoValue")


def _memoized_listing(
    memo: dict[_MemoKey, tuple[tuple[int, int], _MemoValue]],
    lookup_key: _MemoKey,
    dirpath: str,
    scan: Callable[[], _MemoValue],
) -> _MemoValue:
  """Serve *scan*'s listing through *memo*, validated by the directory's own stat pair.

  A remembered listing costs one stat to validate; a miss re-scandirs. A vanished directory
  drops its memo entry and raises FileNotFoundError; any other read failure raises OSError
  for the caller to note. The stored pair is ((mtime_ns, size), value): one stat validates a
  remembered listing, since an entry's create, delete or rename moves the containing
  directory's own mtime_ns, while a file append moves only the file's mtime, which the
  walk's per-file stat takes every pass.
  """
  try:
    st = os.stat(dirpath)
    stat_key = (st.st_mtime_ns, st.st_size)
  except OSError:
    memo.pop(lookup_key, None)
    raise
  hit = memo.get(lookup_key)
  if hit is not None and hit[0] == stat_key:
    return hit[1]
  try:
    value = scan()
  except OSError:
    memo.pop(lookup_key, None)
    raise
  memo[lookup_key] = (stat_key, value)
  return value


def _jsonl_listing(dirpath: str, suffixes: tuple[str, ...]) -> tuple[list[str], list[str]]:
  """The directory's subdirectory paths and suffix-matching file paths, memoized on the
  directory's own stat pair (``_memoized_listing`` owns the stat-and-fail contract)."""

  def scan() -> tuple[list[str], list[str]]:
    subdirs: list[str] = []
    files: list[str] = []
    with os.scandir(dirpath) as scandir:
      for entry in scandir:
        if entry.is_dir():
          if not entry.is_symlink():
            subdirs.append(entry.path)
        elif entry.name.endswith(suffixes):
          files.append(entry.path)
    return subdirs, files

  return _memoized_listing(_jsonl_dir_memo, (dirpath, suffixes), dirpath, scan)


def _iter_jsonl_stats(root: Path, notes: list[str], source: str,
                      label: str) -> Iterator[tuple[str, os.stat_result | None, str | None]]:
  """Yield ``(path, stat, error)`` for every ``.jsonl`` file under *root*, recording
  a note when a directory is unreadable.

  ``Path.rglob`` swallows ``PermissionError`` while walking (shell-glob semantics), so an
  unreadable directory would vanish silently instead of surfacing. ``os.walk``'s ``onerror`` hook
  gets the error instead, which becomes a per-account note; a missing directory is not an error
  here (``discover_homes`` already filters those out for the real on-disk layout). Paths are
  plain strings carrying each file's stat, so the serve walk pays one syscall per file and
  never builds a Path per entry. Each
  directory's listing is memoized on the directory's own stat pair (``_jsonl_listing``), so a
  repeat walk over an unchanged tree pays one stat per directory and one per candidate file.
  """
  hook = _walk_error_hook(notes, source, label, root.name)
  stack = [str(root)]
  while stack:
    dirpath = stack.pop()
    try:
      subdirs, files = _jsonl_listing(dirpath, (".jsonl",))
    except OSError as exc:
      hook(exc)
      continue
    stack.extend(subdirs)
    for path in files:
      try:
        yield path, os.stat(path, follow_symlinks=True), None
      except OSError as exc:
        yield path, None, repr(exc)


# The charlie-bot walk's per-directory listing memo: dirpath -> ((mtime_ns, size),
# [subdir path]). Only subdirectories are listed: the walk's sole consumer stats candidate
# files one level below these directories, and an entry's dir-ness changes only through a
# parent-directory rename the stat pair catches. Paths are absolute so a memo hit joins
# nothing. Entries are bounded by the historical directory set of the sessions tree; a
# subtree that stops being listed leaves its entries until the process restarts.
_charliebot_dir_memo: dict[str, tuple[tuple[int, int], list[str]]] = {}


def _charliebot_listing(dirpath: str) -> list[str]:
  """The directory's subdirectory paths, memoized on the directory's own stat pair
  (``_memoized_listing`` owns the stat-and-fail contract)."""

  def scan() -> list[str]:
    with os.scandir(dirpath) as scandir:
      return [entry.path for entry in scandir if entry.is_dir() and not entry.is_symlink()]

  return _memoized_listing(_charliebot_dir_memo, dirpath, dirpath, scan)


# The walk's per-kind (kind, container under the session dir, candidate file name), built
# once at import: the per-session loop re-entered it ~1k times per capture, re-joining the
# two constant paths each time. Candidate and container paths are entry.path (os.scandir's
# absolute form, never ending in the separator) plus one separator plus a relative
# constant, so concatenation replaces os.path.join's case analysis at the walk's ~19k
# per-capture call sites — the walk's largest Python slice after the stat syscalls.
_WALK_SEP = os.sep
_WALK_KINDS = (
    ("thread", THREADS_DIR_NAME, os.path.join(DATA_DIR_NAME, EVENTS_LOG_NAME)),
    ("master", os.path.join(DATA_DIR_NAME, MASTER_RUNS_DIR_NAME), RAW_LOG_NAME),
)


def _iter_charliebot_logs(sessions: Path,
                          notes: list[str]) -> Iterator[tuple[str, str, os.stat_result | None, str | None]]:
  """Yield ``(kind, path, stat, error)`` over the charlie-bot corpus: every session directory's
  thread event logs (``threads/*/data/events.jsonl``, kind ``"thread"``) and master raw
  captures (``data/master_runs/*/agent.raw.ndjson``, kind ``"master"``).

  Both file names are pinned by their writers — a thread's event log is always
  ``events.jsonl`` under its ``data/`` (threads.thread_events_log_path) and a run's capture always
  ``agent.raw.ndjson`` (runs.RAW_LOG_NAME) — so the walk lists only the three levels whose
  entries it must discover (the sessions root, each ``threads/``, each ``data/master_runs/``,
  each memoized on the directory's own stat pair by ``_charliebot_listing``) and stats each
  candidate file directly every pass. The listed levels do not move when a deep file appears
  or disappears (that moves only the file's own containing directory, which the walk never
  lists), so the fresh per-candidate stat is the only thing that can see it; one stat per
  candidate plus one per discovered directory is the walk's floor, and the deeper directories
  (thread dirs, their ``data/``, run dirs) are never listed or statted at all.
  A missing sessions root or session subtree is an empty corpus, not an error — the same
  contract _iter_jsonl_stats runs under; anything else unreadable becomes a note, and a
  candidate stat failing with anything but an absent file yields its error row.
  """
  try:
    session_listing = _charliebot_listing(str(sessions))
  except OSError as exc:
    if not isinstance(exc, FileNotFoundError):
      _unreadable_note(notes, USAGE_SOURCE_CHARLIE_BOT, str(sessions), exc)
    return
  for session_path in session_listing:
    for kind, container, name in _WALK_KINDS:
      try:
        listing = _charliebot_listing(session_path + _WALK_SEP + container)
      except OSError as exc:
        if not isinstance(exc, FileNotFoundError):
          _unreadable_note(notes, USAGE_SOURCE_CHARLIE_BOT, f"{session_path}/{container}", exc)
        continue
      for entry_path in listing:
        path = entry_path + _WALK_SEP + name
        try:
          yield kind, path, os.stat(path), None
        except FileNotFoundError:
          continue
        except OSError as exc:
          yield kind, path, None, repr(exc)


def _walk_charliebot(sessions: Path, notes: list[str]) -> list[tuple[str, str, int | None, int | None, str | None]]:
  """The charlie-bot corpus in one pass: ``(kind, path, mtime_ns, size, error)`` per file,
  the stat pair None on an unreadable file. ``capture_charliebot`` consumes the rows, so a
  capture walks the corpus once."""
  rows: list[tuple[str, str, int | None, int | None, str | None]] = []
  for kind, path, st, error in _iter_charliebot_logs(sessions, notes):
    if st is None:
      rows.append((kind, path, None, None, error))
    else:
      rows.append((kind, path, st.st_mtime_ns, st.st_size, None))
  return rows


# The append-tail fast path's prefix proof window: the guard hashes this many final
# prefix bytes, and a tail round re-hashes the same window before trusting the prefix.
_TAIL_WINDOW = 8192

# Read size per chunk the line splitter consumes. One C-level find scan per marker hands the
# fold only marker lines; a per-line Python membership test would pay every line instead.
_PARSE_CHUNK = 1 << 22


def _parse_lines(fh: BinaryIO, markers: tuple[bytes, ...]) -> tuple[list[dict], int]:
  """Parse the marker lines from *fh*'s current position to EOF.

  Returns (objects, consumed byte offset), objects in file order. Only complete lines parse:
  a trailing fragment without its newline is left for the carry until the round whose read
  covers it whole. The consumed offset is every complete line's byte span — which is what the
  read paid. A marker hit in the trailing fragment waits in the carry for the next chunk; an
  unparseable marker line is dropped.
  """
  objects: list[dict] = []
  consumed = 0
  # The carry holds exactly the current unterminated line between rounds: the chunk append
  # copies only the fresh bytes and the consumed prefix drops once per round, so a
  # multi-hundred-MB line costs one append pass per chunk instead of the per-round
  # re-concat and from-zero re-scan that paid O(line^2 / chunk) on the gigabyte raw logs
  # (a ~500 MB line read 96 s where the same bytes pass once in ~1 s). The newline scans
  # ride the fresh region; the marker pass runs once per newline round over the round's
  # complete region, carried partial included — once per line's life, still linear.
  carry = bytearray()
  hits: list[int] = []
  while True:
    chunk = fh.read(_PARSE_CHUNK)
    if not chunk:
      break
    plen = len(carry)
    carry += chunk
    cut = carry.rfind(b"\n", plen)
    if cut == -1:
      continue
    hits.clear()
    for marker in markers:
      i = carry.find(marker, 0, cut + 1)
      while i != -1:
        hits.append(i)
        i = carry.find(marker, i + 1, cut + 1)
    starts: set[int] = set()
    if hits:
      # A hit's line start is the partial's own first byte (the carry holds no newline
      # before the fresh region) or the last fresh newline before it — both from one
      # ascending newline walk, never a from-zero rfind per hit.
      fresh_nls: list[int] = []
      pos = carry.find(b"\n", plen)
      while pos != -1 and pos <= cut:
        fresh_nls.append(pos)
        pos = carry.find(b"\n", pos + 1)
      for i in hits:
        j = bisect.bisect_left(fresh_nls, i)
        starts.add(0 if j == 0 else fresh_nls[j - 1] + 1)
    for start in sorted(starts):
      end = carry.find(b"\n", start)
      line = bytes(carry[start:end + 1])
      try:
        objects.append(orjson.loads(line))
      except ValueError:
        # orjson rejects invalid UTF-8, NaN/Infinity, and >8-byte float overflow; the
        # stdlib replace-decode restores the tolerant parse those lines had before.
        try:
          objects.append(json.loads(line.decode("utf-8", errors="replace")))
        except ValueError:
          continue
    consumed += cut + 1
    del carry[:cut + 1]
  return objects, consumed


def _prefiltered_jsonl(path: str, markers: tuple[bytes, ...]) -> tuple[list, list[dict], int]:
  """Parse one jsonl into the objects whose raw line carries any *markers* substring.

  Returns (signature, objects, consumed byte offset), objects in file order. The signature is
  taken before the read: a concurrent append mid-read then necessarily outdates the stored
  sig and the next lookup re-scans, so a partial or extended read can never be served later
  as if complete. *consumed* is the offset the parse actually stopped at — the end of the
  last complete line — which the append-tail fast path continues from; it can sit past the
  signature's size when the writer appended mid-read. An unparseable line is dropped.
  """
  st = os.stat(path)
  with open(path, "rb") as fh:
    objects, consumed = _parse_lines(fh, markers)
  return [st.st_mtime_ns, st.st_size], objects, consumed


def _boundary_guard(path: str, end: int) -> list | None:
  """Hash the file's final *end*-bounded window, the append-tail fast path's prefix proof.

  Returns [window, hexdigest] or None when the window cannot be read (a file truncated below
  its own parsed end): a None guard sends every later round down the full parse.
  """
  window = min(_TAIL_WINDOW, end)
  try:
    with open(path, "rb") as fh:
      fh.seek(end - window)
      raw = fh.read(window)
  except OSError:
    return None
  if len(raw) != window:
    return None
  return [window, hashlib.sha256(raw).hexdigest()]


def _tail_parse(path: str, entry: dict, markers: tuple[bytes, ...]) -> tuple[list[dict], list, int] | None:
  """Parse the lines appended since *entry*'s parse, or None when the prefix is unproven.

  Returns (objects, signature, new consumed offset). The prefix proof is the entry's own
  guard: the stored window must re-hash equal and end on a newline (the stored offset only
  ever follows a complete line, so a mid-line boundary — a replaced or truncated prefix —
  fails the check), and the file must have grown past the parsed offset with no mtime
  rewind. The signature is taken before the read, the same contract _prefiltered_jsonl runs
  under. The trailing partial line stays unparsed; the round whose tail covers it whole
  parses it.
  """
  sig, guard, end = entry.get("sig"), entry.get("guard"), entry.get("end")
  if not sig or not guard or not isinstance(end, int):
    return None
  st = os.stat(path)
  if st.st_size <= end or st.st_mtime_ns < sig[0]:
    return None
  window = guard[0]
  with open(path, "rb") as fh:
    fh.seek(end - window)
    prefix = fh.read(window)
    if len(prefix) != window or prefix[-1:] != b"\n" or hashlib.sha256(prefix).hexdigest() != guard[1]:
      return None
    fh.seek(end)
    objects, consumed = _parse_lines(fh, markers)
  return objects, [st.st_mtime_ns, st.st_size], end + consumed


def _usage_counts(usage: dict) -> list[int]:
  """A record row's four usage counts; a missing or null usage key counts as 0."""
  return [
      usage.get(ET.USAGE_INPUT_TOKENS, 0) or 0,
      usage.get(ET.USAGE_CACHE_CREATION_INPUT_TOKENS, 0) or 0,
      usage.get(ET.USAGE_CACHE_READ_INPUT_TOKENS, 0) or 0,
      usage.get(ET.USAGE_OUTPUT_TOKENS, 0) or 0,
  ]


def _claude_records(recs: list[dict], seen: set) -> list[list]:
  """Fold prefiltered Claude records into ledger records, deduped against *seen*.

  *seen* carries the keys already counted — the empty set on a full parse, the cached
  records' keys on an append-tail round.
  """
  records: list[list] = []
  for rec in recs:
    msg = rec.get("message")
    if not isinstance(msg, dict):
      continue
    usage, model = msg.get("usage"), msg.get("model")
    if not isinstance(usage, dict) or not model or model == "<synthetic>":
      continue
    key = msg.get("id") or rec.get("requestId") or rec.get("uuid")
    if key in seen:
      continue
    seen.add(key)
    records.append([key, model, rec.get("timestamp"), *_usage_counts(usage)])
  return records


_CLAUDE_MARKERS = (b'"usage"',)


def _claude_file_contribution(path: str, prev: dict | None = None) -> tuple[dict, int]:
  """Parse one Claude Code jsonl into its cache entry; return (entry, bytes read).

  *prev* is the file's cached entry under an older signature; when the guard proves the
  prefix unchanged, only the appended tail parses and the cached records ride forward.
  """
  if prev is not None:
    tail = _tail_parse(path, prev, _CLAUDE_MARKERS)
    if tail is not None:
      recs, sig, end = tail
      records = _claude_records(recs, {rec[0] for rec in prev["records"]})
      entry = {"sig": sig, "records": prev["records"] + records, "end": end}
      entry["guard"] = _boundary_guard(path, end)
      return entry, end - prev["end"]
  sig, recs, end = _prefiltered_jsonl(path, _CLAUDE_MARKERS)
  entry = {"sig": sig, "records": _claude_records(recs, set()), "end": end}
  entry["guard"] = _boundary_guard(path, end)
  return entry, end


# One source walk's rows: account -> (path, mtime_ns, size, error) per file, the stat pair
# None with the error string on an unreadable file. The shape of _walk_jsonl_logs's return,
# which _walk_source takes pre-walked.
_JsonlRows = dict[str, list[tuple[str, int | None, int | None, str | None]]]


def _walk_jsonl_logs(
    sub: str,
    homes: dict[str, Path],
    source: str,
    notes: list[str],
) -> _JsonlRows:
  """Walk every home's *sub* tree once: ``account -> [(path, mtime_ns, size, error)]`` per
  file, the stat pair None with the error string on an unreadable file. ``_walk_source``
  consumes the rows, so a capture walks each tree once — the charlie-bot walk's one-pass
  contract (``_walk_charliebot``)."""
  rows: dict[str, list[tuple[str, int | None, int | None, str | None]]] = {}
  for account, home in homes.items():
    per_home: list[tuple[str, int | None, int | None, str | None]] = []
    for path, st, error in _iter_jsonl_stats(home / sub, notes, source, account):
      per_home.append((path, st.st_mtime_ns, st.st_size, None) if st is not None else (path, None, None, error))
    rows[account] = per_home
  return rows


def _walk_source(
    notes: list[str],
    source: str,
    cache_key: str,
    homes: dict[str, Path],
    rows_by_account: _JsonlRows,
    cache: TallyCache | None,
    parse: Callable,
) -> list[tuple[str, str, dict | None, bool]]:
  """Serve every walked log file, cache hits and parse misses; returns one row per file —
  (path, account, entry or None on a failed parse, cache-hit flag).

  The rows arrive pre-walked (``_walk_jsonl_logs``), so the serve pays no directory listing
  at all — one cache-gated lookup per file, a parse only on a miss.
  """
  walked: list[tuple[str, str, dict | None, bool]] = []
  for account in homes:
    for path, mtime_ns, size, error in rows_by_account[account]:
      if mtime_ns is None:
        _unreadable_note(notes, source, f"{account}/{os.path.basename(path)}", error)
        walked.append((path, account, None, False))
        continue
      entry = (cache.lookup_sig(cache_key, path, [mtime_ns, size]) if cache is not None else None)
      hit = entry is not None
      if entry is None:
        prev = cache.prev(cache_key, path) if cache is not None else None
        try:
          entry, _nbytes = parse(path, prev)
        except OSError as exc:
          _unreadable_note(notes, source, f"{account}/{os.path.basename(path)}", exc)
          walked.append((path, account, None, False))
          continue
        if cache is not None:
          cache.store_sig(cache_key, path, entry)
      walked.append((path, account, entry, hit))
  return walked


_CODEX_MARKERS = tuple(f'"{name}"'.encode() for name in (CODEX_SESSION_META, CODEX_TURN_CONTEXT, CODEX_TOKEN_COUNT))


def _codex_records(recs: list[dict], model: str | None, records: list[list]) -> str | None:
  """Fold prefiltered Codex records into ledger records, appending to *records*; returns
  the trailing model context. *model* is the context in force at the first record — None
  on a full parse, the cached entry's trailing context on an append-tail round;
  session_meta and turn_context records update it in file order, and every token_count row
  resolves against the context at its own line. Subagent threads inherit the parent's
  cumulative total_token_usage, so per-request last_token_usage is summed instead.
  """
  for rec in recs:
    if rec.get("type") in (CODEX_SESSION_META, CODEX_TURN_CONTEXT):
      model = (rec.get("payload") or {}).get("model") or model
      continue
    payload = codex_token_count_payload(rec)
    if payload is None:
      continue
    last = (payload.get("info") or {}).get("last_token_usage") or {}
    cached = last.get("cached_input_tokens", 0) or 0
    fresh = (last.get("input_tokens", 0) or 0) - cached
    out = last.get("output_tokens", 0) or 0
    records.append([model or "unknown", rec.get("timestamp"), fresh, cached, out])
  return model


def _codex_file_contribution(path: str, prev: dict | None = None) -> tuple[dict, int]:
  """Parse one Codex rollout jsonl into its cache entry; return (entry, bytes read).

  *prev* is the file's cached entry under an older signature; when the guard proves the
  prefix unchanged, only the appended tail parses and the cached records ride forward with
  the model context the prefix settled.
  """
  if prev is not None:
    tail = _tail_parse(path, prev, _CODEX_MARKERS)
    if tail is not None:
      recs, sig, end = tail
      records: list[list] = []
      model = _codex_records(recs, prev.get("model_ctx"), records)
      entry = {"sig": sig, "records": prev["records"] + records, "model_ctx": model, "end": end}
      entry["guard"] = _boundary_guard(path, end)
      return entry, end - prev["end"]
  sig, recs, end = _prefiltered_jsonl(path, _CODEX_MARKERS)
  # The model context opens at the file's first declared model, so a token_count
  # preceding the first turn_context still carries it.
  model = next(
      (
          (rec.get("payload") or {}).get("model")
          for rec in recs
          if rec.get("type") in (CODEX_SESSION_META, CODEX_TURN_CONTEXT) and (rec.get("payload") or {}).get("model")),
      None,
  )
  records = []
  model = _codex_records(recs, model, records)
  entry = {"sig": sig, "records": records, "model_ctx": model, "end": end}
  entry["guard"] = _boundary_guard(path, end)
  return entry, end


# ---------------------------------------------------------------------------
# charlie-bot source: this host's own thread event logs and master raw captures
# ---------------------------------------------------------------------------

# Every record the parser reads (thread result events, the bare session-id event, the
# claude-style init envelope, master-run context/result lines) serializes its type as a
# quoted literal with a space after the colon — charlie-bot writes json.dumps defaults —
# so the substring filter cannot skip a record the full parse would see; it only skips
# parsing irrelevant lines. The claude CLI's own stream (no space) never matches.
_CHARLIEBOT_THREAD_MARKERS = (b'"type": "result"', b'"session_id"')
_CHARLIEBOT_MASTER_MARKERS = (b'"type": "context"', b'"type": "result"')

# Account label for a master-run capture whose context model matches no charlie-code
# backend in config.yaml (a retired backend's master runs, or an ad-hoc model).
_CLC_MASTER_ACCOUNT = "clc-master"


def _bare_model(model: str) -> str:
  """The row name for a backend config model: the last path segment ("openai/zai-org/
  GLM-5.3-Flash" -> "GLM-5.3-Flash"). Page rows carry the bare model name."""
  return model.rsplit("/", 1)[-1]


def _backend_registry() -> dict[str, object]:
  """config.yaml's backend options by id. Re-read per capture: a backend added or retired
  reclassifies a moved file on its next parse without touching any cached parse."""
  return {opt.id: opt for opt in get_config().backends.options}


# Backend id type prefixes with the fallback disposition of an id off config (a retired id):
# the id rule (BackendsConfig in src/core/config.py) keeps the prefix on every id, so an id
# that left config still names its backend type.
_BACKEND_ID_PREFIXES = (("charlie-code-", "include"), ("codex-", "codex"), ("claude-", "skip"), ("opencode-", "skip"))


def _thread_row_model(meta: dict, registry: dict) -> str:
  """The row name for one thread: the model the thread's metadata recorded (bare), else the
  backend's config model (bare) while the id is registered, else the id minus its type
  prefix ("codex-gpt-5.6-sol-personal" -> "gpt-5.6-sol-personal"), which only a retired id
  whose thread recorded no model reaches. The recorded model leads because an id names a
  family and keeps its name across version bumps (the id rule on BackendsConfig in
  src/core/config.py): the config model is today's version, and reading it first would
  move a thread's history onto every later version."""
  model = meta.get("model")
  if model:
    return _bare_model(model)
  backend = meta.get("backend")
  if not backend:
    return "unknown"
  opt = registry.get(backend)
  if opt is not None and opt.model:
    return _bare_model(opt.model)
  prefix = next((p for p, _verdict in _BACKEND_ID_PREFIXES if backend.startswith(p)), "")
  return backend.removeprefix(prefix)


def _thread_metadata(path: str) -> dict | None:
  """The thread's ``{backend, model}`` pair from its metadata.json, or None when the file is
  absent. Any other read/parse failure propagates: the capture raises, same contract as an
  unreadable log file."""
  meta_path = Path(path).parent.parent / METADATA_NAME
  try:
    with open(meta_path, "rb") as fh:
      meta = orjson.loads(fh.read())
  except FileNotFoundError:
    return None
  if not isinstance(meta, dict):
    raise ValueError(f"{meta_path}: metadata is not an object")
  return {"backend": meta.get("backend"), "model": meta.get("model")}


def _thread_records(objects: list[dict], meta: dict | None, registry: dict) -> tuple[list[list], list[str]]:
  """(records, session ids) from prefiltered thread event lines.

  Each result event folds into ``[model, backend id, ts, input, cache write, cache read,
  output]`` — the envelope's four usage numbers verbatim (missing keys are 0; a CLC result
  carries no cache fields, a codex result's input arrives with its cached reads included in
  the same field the envelope names). Session ids come off every line carrying one at top
  level: the codex translation emits one session-adopt event per thread.started (the typed
  ``session_attached`` signal, or its bare pre-typed spelling in older logs), and
  the claude-style init envelope embeds its own — both key the fallback's exclusion.
  """
  if meta is None:
    return [], []
  model = _thread_row_model(meta, registry)
  backend = meta.get("backend") or ""
  records: list[list] = []
  ids: list[str] = []
  for obj in objects:
    if obj.get("type") == ET.RESULT:
      usage = obj.get("usage") or {}
      records.append([model, backend, obj.get("timestamp"), *_usage_counts(usage)])
    elif obj.get("session_id"):
      sid = obj["session_id"]
      if isinstance(sid, str):
        ids.append(sid)
  return records, sorted(set(ids))


def _thread_contribution(path: str, registry: dict, prev: dict | None) -> tuple[dict, int]:
  """Parse one thread event log into its cache entry; return (entry, bytes read).

  *prev* is the file's cached entry under an older signature; when the guard proves the
  prefix unchanged, only the appended tail parses, the cached records ride forward, and the
  classification they were parsed under rides with them (metadata's backend/model are
  write-once). A first parse reads metadata.json beside the log; a thread without one yields
  an entry with no records and meta None, which contributes no records.
  """
  if prev is not None:
    tail = _tail_parse(path, prev, _CHARLIEBOT_THREAD_MARKERS)
    if tail is not None:
      objects, sig, end = tail
      meta = prev["meta"]
      records, ids = _thread_records(objects, meta, registry)
      entry = {
          "sig": sig,
          "records": prev["records"] + records,
          "ids": sorted(set(prev["ids"]) | set(ids)),
          "meta": meta,
          "end": end,
      }
      entry["guard"] = _boundary_guard(path, end)
      return entry, end - prev["end"]
  meta = _thread_metadata(path)
  sig, objects, end = _prefiltered_jsonl(path, _CHARLIEBOT_THREAD_MARKERS)
  records, ids = _thread_records(objects, meta, registry)
  entry = {"sig": sig, "records": records, "ids": ids, "meta": meta, "end": end}
  entry["guard"] = _boundary_guard(path, end)
  return entry, end


def _master_records(objects: list[dict],
                    path: str,
                    registry: dict,
                    model: str | None = None) -> tuple[list[list], str | None]:
  """The capture's (records, trailing context model): one record per trailing result event,
  at most one.

  A CLC-shaped capture self-identifies with ``type: context`` lines that carry the model;
  its trailing ``type: result`` line carries the run's usage (no cache fields — they stay
  0). *model* is the prefix's trailing context on an append-tail round, where the tail
  carries no context line of its own. The timestamp is the master_runs directory name, the
  run's recorded start time: stable across re-parses, unlike the file mtime a growing log
  keeps moving. Captures without any context model (the claude CLI's own stream, already
  covered by the Claude Code source) or without a trailing result (a run killed mid-turn)
  contribute nothing.
  """
  last = None
  for obj in objects:
    if obj.get("type") == "context" and obj.get("model"):
      model = obj["model"]
    elif obj.get("type") == ET.RESULT:
      last = obj
  if model is None or last is None:
    return [], model
  usage = last.get("usage") or {}
  opt = next((o for o in registry.values() if o.type == BackendType.CHARLIE_CODE and o.model == model), None)
  account = opt.id if opt is not None else _CLC_MASTER_ACCOUNT
  ts = Path(path).parts[-2]  # the master_runs/<started_at> directory name
  return ([[_bare_model(model), account, ts, *_usage_counts(usage)]], model)


def _master_contribution(path: str, registry: dict, prev: dict | None = None) -> tuple[dict, int]:
  """Parse one master raw capture into its cache entry; return (entry, bytes read).

  Same append-tail contract as the thread leg: the cached record rides forward unless the
  tail carries a newer result line, which replaces it (the entry always describes the file's
  trailing result).
  """
  if prev is not None:
    tail = _tail_parse(path, prev, _CHARLIEBOT_MASTER_MARKERS)
    if tail is not None:
      objects, sig, end = tail
      records, model = _master_records(objects, path, registry, prev.get("model_ctx"))
      entry = {
          "sig": sig,
          "records": records or prev["records"],
          "model_ctx": model,
          "end": end,
      }
      entry["guard"] = _boundary_guard(path, end)
      return entry, end - prev["end"]
  sig, objects, end = _prefiltered_jsonl(path, _CHARLIEBOT_MASTER_MARKERS)
  records, model = _master_records(objects, path, registry)
  entry = {"sig": sig, "records": records, "model_ctx": model, "end": end}
  entry["guard"] = _boundary_guard(path, end)
  return entry, end


def _classify_backend(backend: str, registry: dict) -> str | None:
  """One thread or run backend id's disposition: ``"include"`` (charlie-code type or
  prefix — usage only the charlie-bot log holds), ``"codex"``/``"skip"`` (both captured as
  fallback records keyed on session ids), or None when neither the registry nor the id
  prefix can name the type."""
  opt = registry.get(backend)
  btype = str(opt.type) if opt is not None else None
  if btype is None:
    for prefix, verdict in _BACKEND_ID_PREFIXES:
      if backend.startswith(prefix):
        return verdict
    return None
  if btype == BackendType.CHARLIE_CODE:
    return "include"
  if btype == BackendType.CODEX:
    return "codex"
  return "skip"


# The row prefilter: a blob without a tokens key cannot carry token counts. ASCII-case-
# insensitive, mirroring SQLite's LIKE folding — str.lower would mismatch marks SQLite
# leaves distinct.
_OPENCODE_TOKENS_LIKE = re.compile(r'"[tT][oO][kK][eE][nN][sS]"').search


def _opencode_row(row: tuple) -> list | None:
  """Ledger record for one projected message row, or None when it contributes nothing."""
  model, provider, created, in_fresh, output, total, cache_write, cache_read = row
  if not (in_fresh or output or total):
    return None
  model = model or "unknown"
  if model.startswith("/"):
    model = f"{Path(model).name} ({provider})"
  ts = dt.datetime.fromtimestamp(created / 1000, dt.UTC).isoformat() if isinstance(created, (int, float)) else None
  return [model, provider or "unknown", ts, in_fresh or 0, cache_write or 0, cache_read or 0, output or 0]


def _strict_json_constant(name: str) -> None:
  """Reject the NaN/Infinity literals json.loads admits but SQLite's json_valid rejects."""
  raise ValueError(f"invalid JSON constant: {name}")


def _opencode_row_data(data: str) -> list | None:
  """One message row's ledger record from its data blob, or None when it contributes nothing.

  The same filter chain the db's own gate would apply: the tokens prefilter, a strict JSON
  parse (the NaN/Infinity literals json.loads admits but SQLite's json_valid rejects), and
  the assistant role; the record is then ``_opencode_row`` over the same eight projections.
  """
  if _OPENCODE_TOKENS_LIKE(data) is None:
    return None
  try:
    obj = json.loads(data, parse_constant=_strict_json_constant)
  except (ValueError, RecursionError):
    return None
  if not isinstance(obj, dict) or obj.get("role") != "assistant":
    return None
  tokens = obj.get("tokens")
  tokens = tokens if isinstance(tokens, dict) else {}
  cache = tokens.get("cache")
  cache = cache if isinstance(cache, dict) else {}
  created = obj.get("time")
  created = created.get("created") if isinstance(created, dict) else None
  return _opencode_row(
      (
          obj.get("modelID"), obj.get("providerID"), created, tokens.get("input"), tokens.get("output"),
          tokens.get("total"), cache.get("write"), cache.get("read")))


# ---------------------------------------------------------------------------
# usage ledger capture: the Claude Code and Codex jsonl, and the charlie-bot corpus, copied
# into the SQLite ledger
# ---------------------------------------------------------------------------


def capture_jsonl_sources(
    ledger: UsageLedger,
    host: str,
    claude_homes: dict[str, Path],
    codex_homes: dict[str, Path],
    cache: TallyCache | None,
) -> dict[str, int]:
  """Copy the Claude Code and Codex jsonl usage into the SQLite usage ledger, so the page's
  rows survive deletion of the source logs (see the ledger's module docstring).

  The serve rides the shared walk and cache-gated parse (``cache`` is the caller's
  TallyCache or None); a parsed file whose content signature the ledger already recorded for
  this host (``captured_sigs``, read once per call) is skipped, and every other parsed file
  is written atomically with that signature. Each record dedupes on its record_id — Claude
  on the response's message id, Codex on the file session and event index — so a re-captured
  file re-upserts what it stored before and a message replayed into a second config dir
  counts once. A parse failure raises: the capture runs in the collector, not the page
  load, so nothing here notes-and-continues.

  Returns the records written per source label.
  """
  written: dict[str, int] = {USAGE_SOURCE_CLAUDE_CODE: 0, USAGE_SOURCE_CODEX: 0}
  captured = ledger.captured_sigs(host)
  notes: list[str] = []

  rows = _walk_jsonl_logs("projects", claude_homes, USAGE_SOURCE_CLAUDE_CODE, notes)
  walked = _walk_source(notes, USAGE_SOURCE_CLAUDE_CODE, "claude", claude_homes, rows, cache, _claude_file_contribution)
  for path, account, entry, _hit in walked:
    if entry is None:
      continue
    sig = f"{entry['sig'][0]}:{entry['sig'][1]}"
    if captured.get(path) == sig:
      continue
    stem = Path(path).stem
    records = [
        UsageRecord(
            record_id=f"claude:{key}",
            kind=RecordKind.NATIVE,
            source=USAGE_SOURCE_CLAUDE_CODE,
            model=model,
            account=account,
            ts=ts or "",
            in_fresh=in_fresh,
            cache_write=cache_write,
            cache_read=cache_read,
            output=output,
            sessions=(stem,)) for key, model, ts, in_fresh, cache_write, cache_read, output in entry["records"]
    ]
    written[USAGE_SOURCE_CLAUDE_CODE] += ledger.record_file(host, path, sig, records)

  rows = _walk_jsonl_logs("sessions", codex_homes, USAGE_SOURCE_CODEX, notes)
  walked = _walk_source(notes, USAGE_SOURCE_CODEX, "codex", codex_homes, rows, cache, _codex_file_contribution)
  for path, account, entry, _hit in walked:
    if entry is None:
      continue
    sig = f"{entry['sig'][0]}:{entry['sig'][1]}"
    if captured.get(path) == sig:
      continue
    name = os.path.basename(path)
    if name.startswith("rollout-") and name.endswith(".jsonl"):
      # The session id the file name carries: its last five dash-separated segments.
      sid = "-".join(name[len("rollout-"):-len(".jsonl")].rsplit("-", 5)[-5:])
    else:
      sid = os.path.relpath(path, str(codex_homes[account]))
    records = [
        UsageRecord(
            record_id=f"codex:{sid}:{i}",
            kind=RecordKind.NATIVE,
            source=USAGE_SOURCE_CODEX,
            model=model,
            account=account,
            ts=ts or "",
            in_fresh=in_fresh,
            cache_write=0,
            cache_read=cache_read,
            output=output,
            sessions=(sid,)) for i, (model, ts, in_fresh, cache_read, output) in enumerate(entry["records"])
    ]
    written[USAGE_SOURCE_CODEX] += ledger.record_file(host, path, sig, records)
  return written


def capture_charliebot(ledger: UsageLedger, host: str, sessions_dir: Path, cache: TallyCache | None) -> int:
  """Copy the charlie-bot thread event logs and master-run captures into the SQLite usage
  ledger, so a thread's result totals survive deletion of its own event log (see the
  ledger's module docstring).

  The serve rides the shared walk and cache-gated parse (``cache`` is the caller's
  TallyCache or None); a parsed file whose stat signature the ledger already recorded for
  this host (``captured_sigs``, read once per call) is skipped, and every other parsed file
  is recorded with that signature — also one whose content yields no records, so an empty
  thread is never re-parsed. A thread with no metadata.json and one whose
  backend id cannot be classified contribute no records here; an unreadable file or a parse
  failure raises: the capture runs in the collector, not the page load.

  The record kind states the inclusion rules for the ledger:
    master capture  one NATIVE record, no sessions — a master run has no CLI log behind it.
    "include" thread  NATIVE, no sessions — the charlie-bot thread log is the only home the
      usage has, so nothing can restate it.
    "codex" / "skip" thread  FALLBACK carrying the thread's session ids: the CLI log behind
      it may still exist, and the ledger's any-match exclusion retires the fallback the
      moment any of those ids has a NATIVE record of its own — which also covers Codex
      rollouts pruned from disk once the surviving ones are captured. A thread with no
      ids cannot key that exclusion and contributes nothing.

  Returns the records written.
  """
  registry = _backend_registry()
  captured = ledger.captured_sigs(host)
  notes: list[str] = []
  written = 0
  for kind, path, mtime_ns, size, error in _walk_charliebot(sessions_dir, notes):
    if mtime_ns is None:
      raise OSError(f"charlie-bot: unreadable {path}: {error}")
    sig = f"{mtime_ns}:{size}"
    if captured.get(path) == sig:
      continue
    entry = cache.lookup_sig(USAGE_SOURCE_CHARLIE_BOT, path, [mtime_ns, size]) if cache is not None else None
    if entry is None:
      prev = cache.prev(USAGE_SOURCE_CHARLIE_BOT, path) if cache is not None else None
      parse = _thread_contribution if kind == "thread" else _master_contribution
      entry, _nbytes = parse(path, registry, prev)
      if cache is not None:
        cache.store_sig(USAGE_SOURCE_CHARLIE_BOT, path, entry)
    records: list[UsageRecord]
    if kind == "master":
      parts = Path(path).parts
      records = [
          UsageRecord(
              record_id=f"master:{parts[-5]}/{parts[-2]}",
              kind=RecordKind.NATIVE,
              source=USAGE_SOURCE_CHARLIE_BOT,
              model=model,
              account=account,
              ts=ts,
              in_fresh=in_fresh,
              cache_write=cache_write,
              cache_read=cache_read,
              output=output) for model, account, ts, in_fresh, cache_write, cache_read, output in entry["records"]
      ]
    else:
      records = []
      meta = entry["meta"]
      backend = meta.get("backend") if meta is not None else None
      verdict = _classify_backend(backend, registry) if backend else None
      # A codex- or skip-verdict thread without ids cannot key the any-match exclusion: an
      # unexcludable fallback would double count once the CLI log is captured.
      if verdict == "include" or (verdict is not None and entry["ids"]):
        native = verdict == "include"
        parts = Path(path).parts
        sessions = () if native else tuple(entry["ids"])
        records = [
            UsageRecord(
                record_id=f"thread:{parts[-5]}/{parts[-3]}/{i}",
                kind=RecordKind.NATIVE if native else RecordKind.FALLBACK,
                source=USAGE_SOURCE_CHARLIE_BOT,
                model=model,
                account=account,
                ts=ts or "",
                in_fresh=in_fresh,
                cache_write=cache_write,
                cache_read=cache_read,
                output=output,
                sessions=sessions)
            for i, (model, account, ts, in_fresh, cache_write, cache_read, output) in enumerate(entry["records"])
        ]
    written += ledger.record_file(host, path, sig, records)
  return written


# The Run raw stream's prefilter markers: the CLC worker's json.dumps spelling
# ("type": "result", with the space its writer defaults to), the claude CLI's compact
# spelling ("type":"result" — its result line carries no type key until deep into the
# line), the Codex stream's turn.completed and thread.started, and the session_id the
# claude stream's init and result lines carry at top level. A line is parsed only when it
# can contribute usage or an exclusion key.
_RUNS_MARKERS = (
    b'"type": "result"',
    b'"type":"result"',
    b'"turn.completed"',
    b'"session_id"',
    b'"thread.started"',
)


def _iter_run_logs(sessions_dir: Path) -> Iterator[str]:
  """Yield every Run raw capture's path: ``<session>/data/runs/<run id>/agent.raw.ndjson``.

  Both directory levels list through ``_charliebot_listing``; a session without a
  ``data/runs`` directory — most of them, the sessions tree predating Run records — is
  empty, and a run directory that launched no raw log skips. Every other read failure
  raises: the capture runs in the collector, not the page load.
  """
  try:
    session_dirs = _charliebot_listing(str(sessions_dir))
  except FileNotFoundError:
    return
  for session_dir in session_dirs:
    runs_dir = _WALK_SEP.join((session_dir, DATA_DIR_NAME, RUNS_DIR_NAME))
    try:
      run_dirs = _charliebot_listing(runs_dir)
    except FileNotFoundError:
      continue
    for run_dir in run_dirs:
      run_log = run_dir + _WALK_SEP + RAW_LOG_NAME
      try:
        os.stat(run_log)
      except FileNotFoundError:
        continue
      yield run_log


def _run_metadata(run_dir: str) -> dict:
  """The run directory's metadata.json document.

  A missing file or a non-object document raises: the Run record is written at
  registration, before any raw line lands, so a capture that cannot read it is a corpus
  bug, not a skippable shape.
  """
  meta_path = Path(run_dir).parent / RUN_METADATA_NAME
  with open(meta_path, "rb") as fh:
    meta = orjson.loads(fh.read())
  if not isinstance(meta, dict):
    raise ValueError(f"{meta_path}: metadata is not an object")
  return meta


def _last_run_result(objects: list[dict]) -> dict | None:
  """The raw stream's last ``type == "result"`` object, or None when it carries none."""
  return next((obj for obj in reversed(objects) if obj.get("type") == ET.RESULT), None)


def _codex_turn_sums(objects: list[dict]) -> tuple[int, int, int] | None:
  """(in_fresh, cache_read, output) summed over the raw stream's ``turn.completed`` lines,
  or None when it carries none — a run with no usage line contributes no record. Each
  turn's input arrives with its cached reads inside the same field, so per turn the
  in_fresh arm subtracts them back out."""
  in_fresh = cache_read = output = 0
  seen = False
  for obj in objects:
    if obj.get("type") != "turn.completed":
      continue
    seen = True
    usage = obj.get("usage") or {}
    cached = usage.get("cached_input_tokens", 0) or 0
    in_fresh += (usage.get("input_tokens", 0) or 0) - cached
    cache_read += cached
    output += usage.get("output_tokens", 0) or 0
  return (in_fresh, cache_read, output) if seen else None


def _cc_claude_backend(backend: str, registry: dict) -> bool:
  """Whether a backend id names a cc-claude backend: its config type, or — off config (a
  retired id) — the type prefix the id rule keeps on every id."""
  opt = registry.get(backend)
  return opt.type == BackendType.CC_CLAUDE if opt is not None else backend.startswith("claude-")


def _run_session_ids(objects: list[dict], meta: dict) -> tuple[str, ...]:
  """A fallback run's session ids, sorted and deduplicated: metadata's native_session_id
  (absent on many runs), every string top-level ``session_id`` in the raw stream (the
  claude stream's init and result lines), and every ``thread.started`` object's
  ``thread_id`` (the Codex stream). These key the ledger's any-match exclusion, so the
  fallback retires the moment any of the run's CLI logs is captured."""
  ids: set[str] = set()
  native = meta.get("native_session_id")
  if isinstance(native, str) and native:
    ids.add(native)
  for obj in objects:
    sid = obj.get("session_id")
    if isinstance(sid, str) and sid:
      ids.add(sid)
    if obj.get("type") == "thread.started":
      tid = obj.get("thread_id")
      if isinstance(tid, str) and tid:
        ids.add(tid)
  return tuple(sorted(ids))


def _run_record(objects: list[dict], meta: dict, registry: dict, backend: str, run_id: str) -> UsageRecord | None:
  """The Run directory's one ledger record, or None when nothing is storable.

  The backend verdict picks the usage arm:
    include (CLC)     the raw stream's trailing result line, native — the run log is the
      only home the usage has. Its input arrives with the cached reads included in one
      field (``cached_tokens``), so in_fresh subtracts them back out.
    codex             the turn.completed lines' summed usage, fallback.
    skip + cc-claude  the trailing result line through the Claude envelope keys
      (``_usage_counts``), fallback — the CLI's own transcript restates it while it exists.
  Any other backend, a run with no usage line of its verdict's shape, and a fallback run
  with no session id to key the any-match exclusion on contribute nothing — the file still
  records as captured, so it is never re-parsed.
  """
  verdict = _classify_backend(backend, registry)
  if verdict == "include":
    last = _last_run_result(objects)
    if last is None:
      return None
    usage = last.get("usage") or {}
    input_tokens = usage.get("input_tokens", 0) or 0
    cached = usage.get("cached_tokens", 0) or 0
    in_fresh, cache_write, cache_read = input_tokens - cached, 0, cached
    output = usage.get("output_tokens", 0) or 0
    kind = RecordKind.NATIVE
    sessions: tuple[str, ...] = ()
  else:
    if verdict == "codex":
      sums = _codex_turn_sums(objects)
      if sums is None:
        return None
      in_fresh, cache_read, output = sums
      cache_write = 0
    elif _cc_claude_backend(backend, registry):
      last = _last_run_result(objects)
      if last is None:
        return None
      in_fresh, cache_write, cache_read, output = _usage_counts(last.get("usage") or {})
    else:
      return None
    sessions = _run_session_ids(objects, meta)
    if not sessions:
      return None
    kind = RecordKind.FALLBACK
  row_meta = {"backend": backend, "model": meta.get("model")}
  return UsageRecord(
      record_id=f"run:{run_id}",
      kind=kind,
      source=USAGE_SOURCE_CHARLIE_BOT,
      model=_thread_row_model(row_meta, registry),
      account=backend,
      ts=meta.get("started_at") or "",
      in_fresh=in_fresh,
      cache_write=cache_write,
      cache_read=cache_read,
      output=output,
      sessions=sessions)


def capture_runs(ledger: UsageLedger, host: str, sessions_dir: Path) -> int:
  """Copy the Run directories' raw captures into the SQLite usage ledger, so a run's totals
  survive deletion of its own raw log (see the ledger's module docstring).

  The corpus is ``<session>/data/runs/<run id>/agent.raw.ndjson`` — the execution record
  the Run workers write. One record per run at most,
  deduped on the run id; the backend verdict in the run's metadata picks the usage arm (see
  ``_run_record``). Each candidate is stat'ed before anything opens it: its
  ``st_mtime_ns:st_size`` pair is the signature, and one ``captured_sigs`` (read once per
  call) already holds for this host skips the file outright — no log read, no
  metadata.json — so an unchanged corpus pays one stat per run. Otherwise the file parses
  as before and records the parse-time signature (taken before the read, so an append
  mid-read is seen on the next capture) — also one whose content yields no record, so a
  result-less run is never re-parsed. A missing or unreadable metadata.json raises: the
  capture runs in the collector, not the page load.

  Returns the records written.
  """
  registry = _backend_registry()
  captured = ledger.captured_sigs(host)
  written = 0
  for path in _iter_run_logs(sessions_dir):
    st = os.stat(path)
    sig = f"{st.st_mtime_ns}:{st.st_size}"
    if captured.get(path) == sig:
      continue
    sig_pair, objects, _end = _prefiltered_jsonl(path, _RUNS_MARKERS)
    sig = f"{sig_pair[0]}:{sig_pair[1]}"
    meta = _run_metadata(path)
    backend = meta.get("backend")
    record = _run_record(objects, meta, registry, backend, Path(path).parent.name) if backend else None
    written += ledger.record_file(host, path, sig, [record] if record is not None else [])
  return written


# ---------------------------------------------------------------------------
# usage ledger capture: the opencode db, and the one capture entry point
# ---------------------------------------------------------------------------

# The whole message table's gate, one probe query: the four aggregates sign the db for the
# capture, and the max time_updated is the floor a re-read starts from. Every insert and
# every time_updated bump carries the write's own wall-clock ms (drizzle $onUpdate), so a
# row the last capture has not seen sits above that max unless the clock stepped backward.
_OPENCODE_PROBE_SQL = (
    "select count(*), coalesce(sum(time_updated), 0), "
    "coalesce(max(time_updated), 0), coalesce(max(rowid), 0) from message")

# The probe scans the whole message table (5.7 GB db, ~80 ms warm and multi-second disk-cold),
# so it re-runs only when the db files moved since the probe whose signature it recorded: a
# durable commit appends a WAL frame or rewrites the main db, moving one of the two
# (size, mtime_ns) pairs, while a reader touching only the -shm sidecar moves neither. The
# gate lives in the ledger's capture_gates table keyed by the caller's path spelling (the same
# string the captured_files key uses), so a fresh process — every server restart's first page
# load — reuses the stored probe instead of paying the scan again.


def _opencode_db_stats(db: Path) -> tuple[tuple[int, int], tuple[int, int] | None]:
  """The main db's and -wal sidecar's (size, mtime_ns) pairs — the gate's change proof."""
  main = db.stat()
  try:
    wal_stat = db.with_name(db.name + "-wal").stat()
    wal = (wal_stat.st_size, wal_stat.st_mtime_ns)
  except FileNotFoundError:
    wal = None
  return (main.st_size, main.st_mtime_ns), wal


def capture_opencode(ledger: UsageLedger, host: str, db: Path) -> int:
  """Copy the opencode db's message-table usage into the SQLite usage ledger, so the page's
  rows survive deletion of the db itself (see the ledger's module docstring).

  The db opens read-only (mode=ro) — the capture never writes to it. The whole message
  table signs with the probe aggregates ``_OPENCODE_PROBE_SQL`` projects; a db whose
  signature the ledger already recorded for this host is skipped, and the probe itself is
  skipped while the db files sit unchanged since the probe that recorded it (the gate stored
  in the ledger's capture_gates table). Otherwise every row at or above the previous
  capture's max time_updated is re-read
  through ``_opencode_row_data`` and each contributing row upserts on ``opencode:<message
  id>``: an updated row moves its ledger record, a deleted row leaves the stored rows
  untouched (the ledger contains no DELETE). A missing db contributes nothing; a parse or
  read failure raises, because the capture runs in the collector, not the page load.

  Returns the records written.
  """
  if not db.exists():
    return 0
  db_key = str(db)
  main_pair, wal_pair = _opencode_db_stats(db)
  stored = ledger.captured_gate(host, db_key)
  sig: str | None = None
  if stored is not None and stored[0] == (main_pair, wal_pair):
    sig = stored[1]  # the files are byte-still since that probe, so its aggregates hold
  con: sqlite3.Connection | None = None
  try:
    if sig is None:
      con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
      sig = ":".join(map(str, con.execute(_OPENCODE_PROBE_SQL).fetchone()))
      # The gate is written before the rows it prices: a capture that dies between the two
      # leaves the probe's signature absent from captured_files, so the next capture still
      # re-reads those rows from the captured floor — the gate never hides uncaptured rows.
      ledger.record_gate(host, db_key, main_pair, wal_pair, sig)
    captured = ledger.captured_sigs(host).get(db_key)
    if captured == sig:
      return 0
    if con is None:
      con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    floor = int(captured.split(":")[2]) if captured is not None else 0
    records: list[UsageRecord] = []
    for mid, session_id, _time_updated, data in con.execute(
        "select id, session_id, time_updated, data from message where time_updated >= ?", (floor,)):
      rec = _opencode_row_data(data)
      if rec is None:
        continue
      records.append(
          UsageRecord(
              record_id=f"opencode:{mid}",
              kind=RecordKind.NATIVE,
              source=USAGE_SOURCE_OPENCODE,
              model=rec[0],
              account=rec[1],
              ts=rec[2] or "",
              in_fresh=rec[3],
              cache_write=rec[4],
              cache_read=rec[5],
              output=rec[6],
              sessions=(session_id,)))
  finally:
    if con is not None:
      con.close()
  return ledger.record_file(host, str(db), sig, records)


def capture_usage(
    ledger: UsageLedger,
    *,
    host: str,
    claude_homes: dict[str, Path],
    codex_homes: dict[str, Path],
    opencode_db: Path | None,
    sessions_dir: Path | None,
    cache_path: Path | None,
) -> dict[str, int]:
  """Capture every source the caller names into the ledger — the one entry point the page's
  sweep gate, the cron capture and the CLI share (see the ledger's module docstring).

  The serve rides one cache document: it loads the TallyCache from *cache_path* (None
  captures cacheless) and saves it back after the sources run, so callers pointing several
  captures at one cache path pay each parse once.
  ``capture_jsonl_sources`` always runs; the opencode db is captured when *opencode_db* is
  given; the charlie-bot threads and the run directories when *sessions_dir* is. Every
  error raises: the capture runs in the collector, not the page load.

  Returns the records written per source label; the charlie-bot label sums the thread and
  run captures, whose records carry that source.
  """
  notes: list[str] = []
  cache = TallyCache.load(cache_path, notes) if cache_path is not None else None
  written = capture_jsonl_sources(ledger, host, claude_homes, codex_homes, cache)
  if opencode_db is not None:
    written[USAGE_SOURCE_OPENCODE] = capture_opencode(ledger, host, opencode_db)
  if sessions_dir is not None:
    written[USAGE_SOURCE_CHARLIE_BOT] = (
        capture_charliebot(ledger, host, sessions_dir, cache) + capture_runs(ledger, host, sessions_dir))
  if cache is not None:
    cache.save(cache_path)
  return written


def capture_local(ledger: UsageLedger) -> dict[str, int]:
  """Capture this host's own sources with this host's defaults: the discovered Claude
  config dirs and Codex homes, the default opencode db, the config's session tree, and a
  tally cache under ``cache/usage_capture/`` — a directory of its own, so the capture's
  document never shares a path with any other component's cache.
  """
  claude_homes, codex_homes = discover_homes(DEFAULT_CLAUDE_DIR, DEFAULT_CODEX_HOME)
  return capture_usage(
      ledger,
      host=socket.gethostname(),
      claude_homes=claude_homes,
      codex_homes=codex_homes,
      opencode_db=DEFAULT_OPENCODE_DB,
      sessions_dir=get_config().sessions_dir,
      cache_path=get_config().charliebot_home / "cache" / "usage_capture" / "tally.json",
  )
