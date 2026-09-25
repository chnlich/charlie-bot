"""NDJSON (newline-delimited JSON) file utilities."""

import asyncio
import json
import mmap
import os
from collections.abc import Callable, Iterable, Iterator
from contextlib import contextmanager
from itertools import islice
from pathlib import Path
from typing import Any, BinaryIO

import orjson

from src.core.log_once import LazyStructlogLogger
from src.core.memo import StatSignatureMemo

log = LazyStructlogLogger()

_COUNT_CHUNK_SIZE = 1024 * 1024

# Both tail readers bound their trailing reads to this one window.
_TAIL_WINDOW_SIZE = 512 * 1024

# Bound on _count_memo in files, not bytes: the chat tail page (archived
# sessions) and the event-count callers cycle one live file per view, so a
# small cap covers every concurrently viewed session.
_COUNT_MEMO_LIMIT = 64

# path -> line count, and (path, limit) -> (events, total, has_more). The
# count entry's signature is the pre-scan stat: an append during the scan keys
# the count under the older signature, which no later stat can match, so the
# entry is never served stale — the next call re-stats, misses, and recounts;
# a post-scan signature could instead key a stale count under bytes the scan
# never reached, and that entry would serve until the file changed again.
# Chat event files only append; their atomic archive rewrites replace the
# whole file. Tail entries share their event dicts with every caller;
# consumers must treat them as read-only.
_count_memo: StatSignatureMemo[Path, int] = StatSignatureMemo(_COUNT_MEMO_LIMIT)
_tail_memo: StatSignatureMemo[tuple[Path, int], tuple[list[dict], int, bool]] = StatSignatureMemo(_COUNT_MEMO_LIMIT)


def _count_lines(f: BinaryIO) -> int:
  """Count the lines remaining in an open binary file, reading it once from
  the current position. The count matches Python's file-iteration contract
  (a final line without a trailing newline counts), which the tail reader's
  ``total_line_count`` feeds into global ordinal math. The SIMD count is ~4x
  ``bytes.count`` on the production host (~3 GB/s vs ~0.7 GB/s measured)."""
  # numpy rides this call site (the M99 server import floor): the module
  # imports on the sessions chain every server start pulls, and its ~90 ms
  # load serves only this one SIMD count.
  import numpy as np
  total = 0
  last_byte = b""
  while chunk := f.read(_COUNT_CHUNK_SIZE):
    total += int(np.count_nonzero(np.frombuffer(chunk, dtype=np.uint8) == 0x0A))
    last_byte = chunk[-1:]
  if last_byte and last_byte != b"\n":
    total += 1
  return total


# The one skip label for the readers that consume a whole file or an in-memory
# line stream. The windowed readers (tail, tail_parseable, range) name their
# own windows, so a debug log still says which reader skipped the line.
PARSE_SKIP_LOG_EVENT = "ndjson_parse_skip"

# orjson names its invalid-UTF-8 class with this message prefix at every
# position (it validates the whole input's encoding upfront, so the error
# reads column 1 wherever the bad byte sits). The errors="replace" repair
# below can rescue only that class: replace rewrites invalid UTF-8 sequences
# and nothing else, so a structural failure (control character in string,
# truncation, bad literal) survives the replaced decode unchanged and a
# re-parse of it doubles the scan a giant failed line pays (measured on a
# 2.1 GB failed line: one scan 5.6 s, the repair round trip 8.0 s more).
_UTF8_ERROR_PREFIX = "str is not valid UTF-8"


def parse_ndjson_line(
    line: str | bytes | bytearray | memoryview, *, log_event: str, log_fields: dict[str, Any]) -> dict | None:
  """Parse one line under the NDJSON reader skip contract, or None when the line skips.

  The one definition of the NDJSON reader skip contract: an empty or
  whitespace-only line is invisible, and a line the parser rejects logs
  *log_event* (plus *log_fields* and the parse error) at debug level and
  answers None. The parse
  rides the raw line — orjson ignores surrounding whitespace, so no strip copy
  runs — and a bytes line orjson rejects as invalid UTF-8 gets one
  errors="replace" decode before the verdict: a torn multibyte char parses as
  U+FFFD, hard corruption skips as malformed. A structural rejection (control
  character, truncation, bad literal) skips without the repair pass — replace
  rewrites only invalid UTF-8 sequences, so the re-parse cannot change its
  verdict (see _UTF8_ERROR_PREFIX). A memoryview line parses zero-copy (the raw
  bytes stay shared with the caller's read buffer); its replace fallback
  copies once, on the utf-8-class parse-failed path only. The parser is
  orjson, ~2x stdlib json.loads per line measured on the live corpora; orjson
  rejects the stdlib json NaN/Infinity extensions and float literals that
  overflow a double (those lines skip as malformed), and ints at or beyond
  2**64 parse as float where stdlib keeps exact precision.
  """
  if not line:
    return None
  if isinstance(line, memoryview):
    # bytes.isspace() answers from the first byte on a real line; mirror that
    # without a full copy and pay the probe copy only on a line that starts
    # with whitespace.
    if line[0] in b" \t\n\r\v\f" and bytes(line).isspace():
      return None
  elif line.isspace():
    # str, bytes, and bytearray all carry isspace.
    return None
  try:
    return orjson.loads(line)
  except ValueError as e:
    if not str(e).startswith(_UTF8_ERROR_PREFIX):
      log.debug(log_event, error=str(e), **log_fields)
      return None
    if isinstance(line, memoryview):
      line = line.tobytes()
    if not isinstance(line, bytes):
      log.debug(log_event, error=str(e), **log_fields)
      return None
  try:
    return orjson.loads(line.decode("utf-8", errors="replace"))
  except ValueError as e:
    log.debug(log_event, error=str(e), **log_fields)
    return None


def iter_ndjson_events(lines: Iterable[str | bytes], *, log_event: str, log_fields: dict[str, Any]) -> Iterator[dict]:
  """Yield the JSON objects parsed from *lines*, skipping blank and malformed lines.

  Rides :func:`parse_ndjson_line`, the one definition of the reader skip
  contract. Lazy, so first-match and early-stop readers terminate without
  reading the rest.
  """
  for raw_line in lines:
    event = parse_ndjson_line(raw_line, log_event=log_event, log_fields=log_fields)
    if event is not None:
      yield event


def iter_ndjson_events_containing(path: Path, needle: bytes, *, log_event: str,
                                  log_fields: dict[str, Any]) -> Iterator[dict]:
  """Yield parsed events whose raw line contains *needle*, in file order, lazily.

  The needle proof is the whole skip: a line whose bytes lack *needle* cannot
  carry the value it names, so only a hit's enclosing line parses and the scan
  rides one C-level find over the mapping instead of a per-line Python loop —
  a whole-file scan pays memchr's byte rate, not the parse's. Blank lines
  contain no non-empty needle and skip; a hit line that fails to parse follows
  the shared reader skip contract (logged via *log_event*, answered None).
  A missing file yields nothing. *needle* must be non-empty: an empty needle
  matches at every offset and the scan would never advance, so a caller
  passing one is a bug — raised.
  """
  if not needle:
    raise ValueError("needle must be non-empty")
  if not path.exists():
    return
  with open(path, "rb") as f, _mapped_lines(path, f) as (mm, size):
    if mm is None:
      return
    view: memoryview | None = None
    pos = 0
    find = mm.find
    try:
      while True:
        hit = find(needle, pos)
        if hit < 0:
          return
        # The hit's enclosing physical line; a second needle occurrence in the
        # same line maps to the yielded line, so the cursor jumps past the
        # whole line and never yields one event twice.
        line_start = mm.rfind(b"\n", 0, hit) + 1
        line_end = find(b"\n", hit)
        if line_end < 0:
          line_end = size
        if view is None:
          view = memoryview(mm)
        event = parse_ndjson_line(view[line_start:line_end], log_event=log_event, log_fields=log_fields)
        if event is not None:
          yield event
        pos = line_end + 1
    finally:
      del view


def parse_ndjson_file(path: Path) -> list[dict]:
  """Sync read+parse an NDJSON file. Skips blank/malformed lines."""
  return parse_ndjson_events(path, log_event=PARSE_SKIP_LOG_EVENT, log_fields={})


def parse_ndjson_events(path: Path, *, log_event: str, log_fields: dict[str, Any]) -> list[dict]:
  """Whole-file read+parse with the caller's skip label and fields.

  One zero-copy mmap pass: lines are memoryview slices of the mapping riding
  :func:`parse_ndjson_line`'s memoryview contract, so a cold whole-file parse
  pays no per-line decode or re-encode copy. The line domain is ``\n`` — the
  same domain :func:`count_ndjson_lines` and the tail readers count, so the
  whole-file parse and the windowed readers disagree on nothing. The mapping
  is safe against the writers because they only ever append (an archive
  rewrite publishes through ``os.replace`` onto a new inode); a writer that
  truncated a mapped file would SIGBUS the parse instead.
  """
  if not path.exists():
    return []
  # The read-everything consumer skips iter_ndjson_events's generator layer:
  # every line is read, so the laziness the wrapper exists for is pure
  # per-line resume cost here. The skip rules stay in parse_ndjson_line.
  with open(path, "rb") as f, _mapped_lines(path, f) as (mm, size):
    if mm is None:
      return []
    return [
        event for line in _iter_mmap_lines(mm, size)
        if (event := parse_ndjson_line(line, log_event=log_event, log_fields=log_fields)) is not None
    ]


@contextmanager
def _mapped_lines(path: Path, f: BinaryIO) -> Iterator[tuple[mmap.mmap | None, int]]:
  """Map *f* read-only and yield (mapping, size), closing the mapping on exit.

  An empty file yields (None, 0) — mmap refuses a zero-length mapping. An
  aborting parse can leave its last line view in an unwinding traceback's
  frames; the buffer protocol frees the mapping when that frame dies, so the
  explicit close is best-effort and the deferred case is logged, not fatal.
  """
  size = os.fstat(f.fileno()).st_size
  mm = None if size == 0 else mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ)
  try:
    yield mm, size
  finally:
    if mm is not None:
      try:
        mm.close()
      except BufferError:
        log.debug("ndjson_mmap_close_deferred", path=str(path))


def _iter_mmap_lines(mm: mmap.mmap, size: int) -> Iterator[memoryview]:
  """Yield the mapping's physical lines as zero-copy views, in file order.

  Lines split on ``\n`` (a final line without its trailing newline still
  yields), the same domain the count and tail readers count.
  """
  view = memoryview(mm)
  pos = 0
  find = mm.find
  while True:
    nl = find(b"\n", pos)
    if nl < 0:
      if pos < size:
        yield view[pos:]
      return
    yield view[pos:nl]
    pos = nl + 1


def count_ndjson_lines(path: Path) -> int:
  """Return the number of persisted NDJSON lines without parsing JSON.

  Memoized on the file's (mtime_ns, size): a repeat call over an unchanged
  file pays one stat and zero file bytes. A missing file answers 0 fresh
  every call — it has no signature to memoize, matching the sibling memo
  policy in plans.py.
  """
  if not path.exists():
    return 0
  st = path.stat()
  memoized = _count_memo.fresh(path, st)
  if memoized is not None:
    return memoized
  with open(path, "rb") as f:
    total = _count_lines(f)
  _count_memo.record(path, st, total)
  return total


def parse_ndjson_tail(path: Path, limit: int) -> tuple[list[dict], int, bool]:
  """Read the last *limit* lines from an NDJSON file using seek-from-end.

  Returns (events, total_line_count, has_more). The whole page memoizes on
  (mtime_ns, size): the chat tail page is re-requested per view of an
  unchanged file (SPA switches, re-materializations), where a repeat pays
  one stat and zero file bytes; an append re-reads the count and window.
  """
  if not path.exists():
    return [], 0, False
  st = path.stat()
  memoized = _tail_memo.fresh((path, limit), st)
  if memoized is not None:
    return memoized

  total = count_ndjson_lines(path)
  if total == 0:
    return [], 0, False

  has_more = total > limit
  take = min(limit, total)
  if take == 0:
    return [], total, has_more

  with open(path, "rb") as f:
    f.seek(0, 2)
    file_size = f.tell()
    tail_lines: list[bytes] | None = None
    if file_size > _TAIL_WINDOW_SIZE:
      f.seek(file_size - _TAIL_WINDOW_SIZE)
      window = f.read()
      split_lines = window.split(b"\n")

      complete_lines = split_lines[1:]
      if complete_lines and complete_lines[-1] == b"":
        complete_lines = complete_lines[:-1]
      window_lines = [line for line in complete_lines if line.strip()]

      if len(window_lines) >= take:
        tail_lines = window_lines[-take:]
    if tail_lines is None:
      # The file fits the window, or the window holds fewer than *take* lines:
      # the whole-file read is the one fallback, so its skip-and-take contract
      # lives here exactly once.
      f.seek(0)
      tail_lines = [line for line in f.read().split(b"\n") if line.strip()][-take:]

  events = list(iter_ndjson_events(tail_lines, log_event="ndjson_tail_parse_skip", log_fields={}))

  _tail_memo.record((path, limit), st, (events, total, has_more))
  return events, total, has_more


# Bound, in bytes, on the head probe a head-provable filter reads in the
# from-the-end walk. Every event type the repo writes proves within it; a
# longer name defeats the proof and parses, the conservative direction the
# filter's contract already takes for every shape it cannot read.
_HEAD_PROOF_BYTES = 256


def _parse_mapped_line(
    mm: mmap.mmap, start: int, end: int, *, log_event: str, log_fields: dict[str, Any]) -> dict | None:
  """Parse the mapping's [start, end) bytes under the reader skip contract.

  The view lives only in this frame, so a walk that stops early never leaves
  a mapping-pinning view in the walker's own frame and the mapping closes.
  """
  return parse_ndjson_line(memoryview(mm)[start:end], log_event=log_event, log_fields=log_fields)


def iter_ndjson_events_from_end(
    path: Path,
    *,
    log_event: str,
    log_fields: dict[str, Any],
    parse_filter: Callable[[bytes], bool] | None = None) -> Iterator[dict]:
  """Yield the JSON objects parsed from *path*, newest line first.

  Same skip contract as :func:`iter_ndjson_events` (an empty or
  whitespace-only line is invisible, a line the parser rejects logs and
  yields nothing); *parse_filter* rides it with the same raw-line contract.
  One backward scan over a read-only mapping: each line is the zero-copy view
  between the newlines :func:`mmap.mmap.rfind` finds, so a consumer that
  stops early never scans the bytes past its answer, and a line a
  head-provable filter rejects costs one bounded head probe instead of the
  line's bytes. The mapping is safe against the writers because they only
  ever append (an archive rewrite publishes through ``os.replace`` onto a new
  inode); a writer that truncated a mapped file would SIGBUS the walk — the
  same ground :func:`parse_ndjson_events`' mapping stands on. A missing file
  yields nothing.
  """
  if not path.exists():
    return
  # A head fragment decides only a head-provable filter: a plain callable
  # keyed on bytes beyond the head would misread a bounded probe, so the walk
  # hands it whole lines only.
  head_filter = parse_filter if isinstance(parse_filter, HeadProvableFilter) else None
  with open(path, "rb") as f, _mapped_lines(path, f) as (mm, size):
    if mm is None:
      return
    rfind = mm.rfind
    pos = size
    while pos > 0:
      nl = rfind(b"\n", 0, pos)
      start = 0 if nl < 0 else nl + 1
      if start == pos:
        # The empty segment a trailing (or doubled) newline leaves: invisible
        # under the skip contract, so no filter verdict can change it.
        pos = 0 if nl < 0 else nl
        continue
      if parse_filter is None:
        event = _parse_mapped_line(mm, start, pos, log_event=log_event, log_fields=log_fields)
        if event is not None:
          yield event
      elif head_filter is not None:
        head = bytes(memoryview(mm)[start:min(start + _HEAD_PROOF_BYTES, pos)])
        if head_filter(head):
          event = _parse_mapped_line(mm, start, pos, log_event=log_event, log_fields=log_fields)
          if event is not None:
            yield event
      else:
        line = bytes(memoryview(mm)[start:pos])
        if parse_filter(line):
          event = parse_ndjson_line(line, log_event=log_event, log_fields=log_fields)
          if event is not None:
            yield event
      pos = 0 if nl < 0 else nl


class HeadProvableFilter:
  """A parse_filter whose False answers are provable from a line's head bytes
  alone.

  The from-the-end walker hands one of these a bounded head probe so a
  rejected giant line costs the probe instead of the line's bytes; a plain
  callable keyed on bytes beyond the head would misread the probe, so the
  walker extends that trust to this type only and feeds every other filter
  whole lines.
  """

  __slots__ = ("_keep",)

  def __init__(self, keep: Callable[[bytes], bool]) -> None:
    self._keep = keep

  def __call__(self, raw_line: bytes) -> bool:
    return self._keep(raw_line)


def type_line_filter(types: frozenset[str]) -> HeadProvableFilter:
  """A :func:`iter_ndjson_events_from_end` parse_filter keeping only lines whose event
  type is in *types*.

  The proof is the line's head: every writer in this repo serializes each key
  once and leads with ``type`` (orjson dict order; the stdlib-era shapes the
  corpora still carry do the same), so a line opening ``{"type"`` names its
  event in that first value, and a value outside *types* cannot match a
  consumer keyed on those types. Every other shape — a foreign leading key,
  whitespace before the object, a value the head walk cannot read — returns
  True and parses: the filter skips only what it can prove. The from-the-end
  walk may pass a bounded head probe (the line's first
  ``_HEAD_PROOF_BYTES`` bytes); the probe opens with the line's own first
  bytes, so the proof reads identically, and a probe without the closing
  quote parses.
  """

  def keep(raw_line: bytes) -> bool:
    if not raw_line.startswith(b'{"type"'):
      return True
    rest = raw_line[7:].lstrip(b" \t")
    if not rest.startswith(b":"):
      return True
    rest = rest[1:].lstrip(b" \t")
    if not rest.startswith(b'"'):
      return True
    end = rest.find(b'"', 1)
    if end < 0:
      return True
    return rest[1:end].decode("utf-8", errors="replace") in types

  return HeadProvableFilter(keep)


def parse_ndjson_tail_parseable(path: Path, limit: int) -> list[dict]:
  """Return the last *limit* parseable events of an NDJSON file, in file order.

  Same result as ``parse_ndjson_file(path)[-limit:]`` — blank and malformed
  lines are skipped and never count toward *limit* — but reads only as many
  trailing bytes as the limit needs (the from-the-end walk of
  :func:`iter_ndjson_events_from_end`, stopped by *limit*). Callers that must
  see every line (exact prefixes, global ordinals) keep ``parse_ndjson_file``;
  this reader is for the "last N of whatever parsed" budget the worker-summary
  readers carry. A missing file returns ``[]`` and *limit* <= 0 returns
  ``[]``.
  """
  if limit <= 0:
    return []
  collected = list(
      islice(iter_ndjson_events_from_end(path, log_event="ndjson_tail_parseable_skip", log_fields={}), limit))
  collected.reverse()
  return collected


def parse_ndjson_range(path: Path, start: int, end: int) -> tuple[list[dict], bool]:
  """Read NDJSON lines in range [start, end) by line index.

  Returns (events, has_more) where has_more is True when start > 0.
  """
  if not path.exists():
    return [], False
  with open(path, "rb") as f, _mapped_lines(path, f) as (mm, size):
    if mm is None:
      return [], start > 0
    events = list(
        iter_ndjson_events(
            islice(_iter_mmap_lines(mm, size), start, end),
            log_event="ndjson_range_parse_skip",
            log_fields={},
        ))
  return events, start > 0


def write_all(fd: int, data: bytes) -> None:
  """Write all of *data* to *fd*: a short write keeps going, never a torn line."""
  view = memoryview(data)
  while view:
    view = view[os.write(fd, view):]


def _append_ndjson_sync(path: Path, line: str) -> None:
  """One open(O_APPEND)+write+close per append.

  The handle is opened per call on purpose: O_APPEND re-resolves the path, so
  an append after an atomic archive rewrite (or a recreate) lands on the new
  file, never behind a stale handle pointing at a swapped-out inode. The loop
  keeps the io stack's write-all contract: a short write keeps going instead
  of publishing a torn line.
  """
  fd = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o666)
  try:
    write_all(fd, line.encode("utf-8"))
    os.fdatasync(fd)  # durable before close: a hard VM kill must not leave a size-without-data NUL hole
  finally:
    os.close(fd)


async def append_ndjson(path: Path, data: dict) -> None:
  """Async-append a single JSON line to an NDJSON file."""
  path.parent.mkdir(parents=True, exist_ok=True)
  await asyncio.to_thread(_append_ndjson_sync, path, json.dumps(data) + "\n")


def append_ndjson_sync(path: Path, data: dict) -> None:
  """Synchronous form of :func:`append_ndjson` (same durability contract).

  The migration apply runs its fact appends inside one asyncio loop; the
  offline callers that hold no loop use this form instead of nesting event
  loops. One open(O_APPEND)+write+fsync per append, exactly as the async form.
  """
  _append_ndjson_sync(path, json.dumps(data) + "\n")
