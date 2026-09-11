"""Tally token usage per model across every agent log on this host.

One model is one row: the same model served by several subscriptions (Claude config dirs, Codex
homes) is merged, with the per-account split kept as a secondary breakdown on each row.

Sources, all local logs (no vendor usage API is called):
  Claude Code  <config_dir>/projects/**/*.jsonl   assistant message.usage + message.model
  Codex        ~/.codex/sessions/**/*.jsonl       token_count events, model from turn_context
  opencode     ~/.local/share/opencode/opencode.db   table message, JSON data.tokens + modelID

Two accounting traps this handles:
  1. Claude Code replays history verbatim on resume and fork, so responses are deduped on
     message.id (falling back to requestId, then uuid). About half of all usage lines on this
     host are replays.
  2. Codex subagent threads inherit the parent's cumulative total_token_usage, so per-request
     last_token_usage is summed instead; total_token_usage only cross-checks root sessions.

Cache — one JSON document of per-file Claude and Codex contributions (the gigabyte-scale and
hundred-megabyte-scale sources) plus the opencode db's whole contribution, so a page load
re-parses only the sources that changed. On top of that document, an in-process aggregate
memo holds the merged Claude+Codex partial of the last collect, keyed on the walk signature:
the home pairs, every log file's (path, mtime_ns, size), and the walk's own error strings.
A hit serves the sums, spans and notes without replaying a single cached record; misses to
the full path follow the per-file cache's own visibility contract: appends, deletes, renames
and directory-permission changes all move the signature, while a file-level chmod that
leaves (mtime_ns, size) untouched keeps serving the cached parse. The memo is order-safe
under an unstable walk order: cross-file dedupe arbitrates verbatim replays, which carry
identical token values, so first-wins cannot move a sum. The opencode db stays outside that
corpus memo: its WAL sidecar moves under plain serve traffic, and the miss path is incremental
per message row (see row memo below). Its document entry re-stores only when the row memo
moved: a WAL write over unchanged rows keeps the stored entry, so the document rewrite waits
for a real contribution change.
One more memo sits above both: the whole-tally memo, keyed on the walk signature, the opencode
db signature its rows were read at, and the row memo's change epoch, holds the built rows and
notes of the last collect. Two hits serve the tally without loading the persisted document,
replaying a record, or re-reading a row: the db signature still matching means no write landed
since the read (a stat-only check); a moved signature whose row memo the scan just proved
unchanged means the WAL wrote rows the tally never reads — the epoch counts row-memo changes,
so an unchanged epoch re-serves the rows and re-signs them at the scan's own signature. Only
memos built from a scan carry that epoch proof; rows served from the persisted document sign
into the key by signature alone. The walk signature itself recurses scandir entries carrying
their own stat — one syscall per jsonl — and pays it on every collect, hit or miss.
Vocabulary (opencode row memo):
  key         ``(message id, time_updated)`` of one row in the db's message table. opencode
              (drizzle ORM, ``$onUpdate(() => Date.now())`` on the column) bumps time_updated
              to epoch ms on every write, insert and upsert alike, so the pair identifies the
              row's content: a collect re-reads only rows whose pair moved since the previous
              collect and drops ids absent from a ``select id, time_updated`` pass (cascade
              deletes). A terminal pair of writes to one row inside one millisecond can carry
              the same time_updated; the second write then never reaches the memo. Only
              finish metadata (error/completed) rides such writes on observed opencode write
              paths — token fields change exactly once, at the step-finish write that starts
              the pair — so the three projected token buckets the tally sums cannot go stale,
              and any later write to the row re-reads it.
  partial     the opencode source's accumulated buckets plus its contributing-record count,
              kept against the rows the last merge served: a scan-path merge rebuilds it from
              the row memo's fold (in lockstep with the memo), an entry-served merge adopts
              or rebuilds it from the entry it serves, and every merge ends with the partial
              describing exactly the rows that merge folded.
Vocabulary:
  signature   ``[mtime_ns, size]`` for a log file; a file re-scans whole whenever either value
              moves. The opencode db signs as ``[mtime_ns, size, wal_sig]`` with ``wal_sig`` the
              ``-wal`` sidecar's ``[mtime_ns, size]`` or None — a WAL-mode write grows the
              sidecar without touching the main file, so the main file pair alone cannot see it
  end         the byte offset a file's parse actually stopped at — after its last complete
              line, which can sit past the signature's size when the writer appended mid-read
  guard       the append-tail fast path's prefix proof: the sha256 of the file's final
              ``_TAIL_WINDOW`` bytes at the parsed offset. These jsonl logs only grow by
              appends, so a tail round re-hashes that window (and requires the boundary
              newline) before parsing only the appended lines; a replaced, truncated or
              mid-line prefix fails the check and re-parses whole
  entry       a source's parsed contribution: ``{"sig", "records", "dupes", "end", "guard"}``
              for a Claude file (dupes is the within-file replay count), ``{"sig", "records",
              "check", "model_ctx", "is_root", "final_total", "walked", "end", "guard"}`` for a
              Codex file (check is the root-session self-check pair ``[walked, final_total]``
              or None; the tail round carries the model context, rootness and self-check state
              the prefix settled), ``{"sig", "rows", "partial"}`` for the opencode db
   rows       the opencode db's per-row map, ``{message id: [time_updated, record or None]}`` —
              the row memo's persisted form. A process restart rebuilds the row memo from it
              and diffs one key pass against the live table, so the restart-cold collect
              fetches only rows that moved since the document was written instead of
              re-reading every data blob
   records    Claude: ``[key, model, ts, in_fresh, cache_write, cache_read, output]`` per
               response, replay-deduped within the file; Codex: ``[model, ts, in_fresh,
               cache_read, output]`` per token_count event, model resolved by file position;
               opencode v1 entries: ``[model, account, ts, in_fresh, cache_write, cache_read,
               output]`` per assistant message with token counts (v2 entries carry the same
               records as the values of ``rows``)
Cross-file replay dedupe happens at merge (first record wins in walk order), which composed
with within-file first-wins gives exactly the global first-wins a cacheless scan computes.

The merged Claude+Codex buckets are themselves incremental per file (source partials):
  partial     one file's contribution to the merged buckets: the bucket deltas its records
              added (post dedupe), the (source, model) span they covered, its record and
              within-file-dupe counts, and per replay key the copy count its records carry
  key counts  corpus-wide per-key copy counts plus, per key, the contributing file, its
              record values (first fold wins; an earlier-walked newcomer takes the credit, as
              a fresh fold credits the first carrier) and the copy holders. A contributing
              file that moves while a copy survives elsewhere hands the contribution to an
              orphan pool anchored at the earliest-walked surviving holder — verbatim replays
              carry identical token values, so only the account label changes. Each round
              releases the dead partials and key copies, re-folds only those files, and the
              partial sums always equal a fresh fold of the current corpus.
 """

from __future__ import annotations

import datetime as dt
import hashlib
import json
import os
import re
import sqlite3
import time
from collections import defaultdict
from collections.abc import Callable, Iterator
from dataclasses import dataclass, field
from pathlib import Path
from typing import BinaryIO, NamedTuple

import orjson

from src.core import event_types as ET
from src.core.codex_usage import CODEX_EVENT_MSG, CODEX_SESSION_META, CODEX_TOKEN_COUNT, CODEX_TURN_CONTEXT
from src.core.config import get_config
from src.core.json_utils import atomic_write_stream

DEFAULT_CLAUDE_DIR = Path.home() / ".claude"
DEFAULT_CODEX_HOME = Path.home() / ".codex"
DEFAULT_OPENCODE_DB = Path.home() / ".local/share/opencode/opencode.db"

FIELDS = ("in_fresh", "cache_write", "cache_read", "output", "calls")


@dataclass(frozen=True)
class AccountRow:
  """Usage for one subscription account of a model."""

  name: str
  calls: int
  output: int
  total: int


@dataclass(frozen=True)
class ModelRow:
  """Usage for one model, with the merge key and the per-account split."""

  model: str
  source: str
  calls: int
  in_fresh: int
  cache_write: int
  cache_read: int
  output: int
  total: int
  first: str
  last: str
  accounts: list[AccountRow]


@dataclass(frozen=True)
class TokenTally:
  """Result of one full collection, with per-source self-check notes."""

  rows: list[ModelRow]
  notes: list[str]
  elapsed_s: float
  scanned_bytes: int


@dataclass
class _Tally:
  """Mutable accumulator shared while a collection is in flight."""

  by_model: defaultdict = field(default_factory=lambda: defaultdict(lambda: dict.fromkeys(FIELDS, 0)))
  by_account: defaultdict = field(default_factory=lambda: defaultdict(lambda: dict.fromkeys(FIELDS, 0)))
  span: defaultdict = field(default_factory=lambda: defaultdict(lambda: [None, None]))
  notes: list[str] = field(default_factory=list)
  scanned_bytes: int = 0

  def add(self, source: str, model: str, account: str, ts: str | None, **vals: int) -> None:
    for tgt in (self.by_model[(source, model)], self.by_account[(source, model, account)]):
      for key, value in vals.items():
        tgt[key] += value
      tgt["calls"] += 1
    if ts:
      span = self.span[(source, model)]
      span[0] = ts if span[0] is None or ts < span[0] else span[0]
      span[1] = ts if span[1] is None or ts > span[1] else span[1]


class _SourceAggregate(NamedTuple):
  """The merged Claude+Codex partial of one collect; see the module docstring for the memo."""

  by_model: dict
  by_account: dict
  span: dict
  notes: list

  @classmethod
  def snapshot(cls, t: _Tally, notes_from: int) -> _SourceAggregate:
    """Copy the source partial out of the accumulator; notes before ``notes_from`` are not its."""
    return cls(
        by_model={
            k: dict(v) for k, v in t.by_model.items()
        },
        by_account={
            k: dict(v) for k, v in t.by_account.items()
        },
        span={
            k: tuple(v) for k, v in t.span.items()
        },
        notes=list(t.notes[notes_from:]),
    )

  def apply(self, t: _Tally) -> None:
    """Merge a snapshot into a fresh accumulator, copying so later adds never alias the memo."""
    for tgt, src in ((t.by_model, self.by_model), (t.by_account, self.by_account)):
      for key, val in src.items():
        tgt[key] = dict(val)
    for key, val in self.span.items():
      t.span[key] = list(val)
    t.notes.extend(self.notes)


def discover_homes(claude_default: Path, codex_default: Path) -> tuple[dict[str, Path], dict[str, Path]]:
  """Claude config dirs and Codex homes from config.yaml plus the on-disk defaults.

  Reading the account list keeps a newly added pool account in the tally without an edit here;
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
  account labels but not identical -- that one labels the default ``main`` and strips a leading
  dot from every basename; core must not import the api layer, so the derivation is restated here.
  """
  if path.name == stem:
    return "work (default)"
  return path.name.removeprefix(stem + "-")


class TallyCache:
  """Per-file tally contributions keyed by file signature, persisted as one JSON document.

  ``lookup_sig`` serves an entry only while the caller's signature matches and copies the hit
  into the next document; ``store``/``store_sig`` add fresh scans there. The saved document
  therefore holds only files seen this run — deleted logs drop out without a separate sweep.
  """

  SCHEMA_VERSION = 2

  def __init__(self, sources: dict[str, dict[str, dict]]) -> None:
    self._sources = sources
    self._next: dict[str, dict[str, dict]] = defaultdict(dict)

  @classmethod
  def load(cls, path: Path, notes: list[str]) -> TallyCache:
    """Read the persisted document; an unreadable or stale-schema file starts a cold cache.

    Version 1 documents (records-only opencode entries) still serve: their entries fall back
    to the replay paths, and the first save rewrites them in the current shape.
    """
    try:
      doc = orjson.loads(path.read_bytes())
    except FileNotFoundError:
      doc = None
    except (OSError, ValueError) as exc:
      notes.append(f"Tally cache: unreadable {path} ({exc}); rebuilt from the logs")
      doc = None
    if not isinstance(doc, dict) or doc.get("version") not in (1, cls.SCHEMA_VERSION):
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

  def store(self, source: str, path: Path, entry: dict) -> None:
    """Record one freshly scanned contribution for the next document."""
    self._next[source][str(path)] = entry

  def store_sig(self, source: str, key: str, entry: dict) -> None:
    """``store``'s sibling for a caller that already carries the str key (``lookup_sig``)."""
    self._next[source][key] = entry


def _walk_error_hook(t: _Tally, source: str, label: str, root_name: str) -> Callable[[OSError], None]:
  """The os.walk onerror hook turning an unreadable directory into a per-account note."""

  def _onerror(exc: OSError) -> None:
    if not isinstance(exc, FileNotFoundError):
      t.notes.append(f"{source}: unreadable {label}/{root_name}: {exc}")

  return _onerror


def _iter_jsonl_stats(root: Path, t: _Tally, source: str,
                      label: str) -> Iterator[tuple[str, os.stat_result | None, str | None]]:
  """Yield ``(path, stat, error)`` for every ``*.jsonl`` under *root*, recording a note when a
  directory is unreadable.

  ``Path.rglob`` swallows ``PermissionError`` while walking (shell-glob semantics), so an
  unreadable directory would vanish silently instead of surfacing. ``os.walk``'s ``onerror`` hook
  gets the error instead, which becomes a per-account note; a missing directory is not an error
  here (``discover_homes`` already filters those out for the real on-disk layout). Paths are
  plain strings carrying each file's stat, so both consumers — the corpus signature and the
  per-file serve walk — pay one syscall per file and never build a Path per entry.
  """
  hook = _walk_error_hook(t, source, label, root.name)
  stack = [str(root)]
  while stack:
    dirpath = stack.pop()
    try:
      scandir = os.scandir(dirpath)
    except OSError as exc:
      hook(exc)
      continue
    with scandir:
      for entry in scandir:
        if entry.is_dir():
          if not entry.is_symlink():
            stack.append(entry.path)
        elif entry.name.endswith(".jsonl"):
          try:
            yield entry.path, entry.stat(follow_symlinks=True), None
          except OSError as exc:
            yield entry.path, None, repr(exc)


def _corpus_signature(claude_homes: dict[str, Path], codex_homes: dict[str, Path]) -> tuple:
  """Walk signature of the Claude+Codex corpus: home pairs, every jsonl's stat pair, and the
  walk's own error strings. Any corpus or permission move changes the tuple."""
  sig = []
  for source, homes, sub in (("Claude Code", claude_homes, "projects"), ("Codex", codex_homes, "sessions")):
    probe = _Tally()
    entries = []
    for label, home in homes.items():
      per_home = []
      for path, st, error in _iter_jsonl_stats(home / sub, probe, source, label):
        per_home.append((path, st.st_mtime_ns, st.st_size) if st is not None else (path, None, error))
      entries.append((label, tuple(sorted(per_home))))
    home_pairs = tuple(sorted((label, str(path)) for label, path in homes.items()))
    sig.append((source, home_pairs, tuple(entries), tuple(probe.notes)))
  return tuple(sig)


# The aggregate memo pair (walk signature, partial); see the module docstring.
_aggregate_memo: tuple[tuple, _SourceAggregate] | None = None

# The whole-tally memo: ((walk signature, opencode db signature, row epoch, scan-sourced),
# rows, notes) of the last collect; see the module docstring for the key's contract. The db
# half of the key is the signature the rows were read at, not a pre-walk lookup value; the
# epoch is the row memo's change count as of the build, and only a scan-built memo carries a
# proof an epoch comparison can honor. Served tallies copy out of the stored containers via
# ``_materialize_rows``.
_tally_memo: tuple[tuple, list[dict], list[str]] | None = None

# Per-row memo for the opencode message table: db path -> {message id: (time_updated, record
# or None)}; see the module docstring for the key's contract.
_opencode_row_memos: dict[str, dict[str, tuple[int, list | None]]] = {}

# Per-db change count of the row memo: the epoch a scan-built whole-tally memo keys its rows
# on. It advances exactly when a scan moves the memo (a row landed, moved, or vanished), so
# an equal epoch proves the memo's records — and the rows built from them — still current.
_opencode_row_epochs: dict[str, int] = {}

# Per-db proof aggregates of the row memo's last full scan: (row count, sum of time_updated).
# The pair is a strictly weaker proof than the key scan's per-id diff: every single-row move
# changes it — an insert or delete moves the count, and a moved row rewrites time_updated so
# the sum changes with it, while a data-only rewrite with an unchanged time_updated is
# invisible to the key scan itself — but a multi-row coincidence whose count and sum both net
# to zero (a delete and an insert landing in the same millisecond, the only window where the
# inserted row's time_updated can equal the deleted row's last write) dodges the probe where
# the per-id diff would see it, and that wrong serve stands until the next proof miss
# re-scans. The one same-row shape both miss is a terminal pair of writes inside one
# millisecond, the class the row memo vocabulary documents above. Equal aggregates skip the
# per-row key read.
_OPENCODE_PROBE_SQL = "select count(*), coalesce(sum(time_updated), 0) from message"
_opencode_probes: dict[str, tuple[int, int]] = {}

# Per-db opencode partial of the last merge, corresponding to the row memo's current state
# (buckets by model and account, per-model spans, contributing-record count). A scan that
# moves the memo reports the per-row deltas; the next merge adjusts these buckets by them
# instead of replaying every record. Absent means the next merge must replay.
_opencode_partials: dict[str, _OpencodePartial | None] = {}


class _OpencodePartial(NamedTuple):
  """The opencode source's accumulated buckets, kept adjacent to the row memo it sums."""

  by_model: dict
  by_account: dict
  span: dict
  count: int


def _snapshot_opencode_partial(t: _Tally, count: int) -> _OpencodePartial:
  """Copy the opencode source's buckets out of the accumulator. The stored partial must never
  alias a served tally's containers, so every bucket copies."""
  return _OpencodePartial(
      by_model={
          k: dict(v) for k, v in t.by_model.items() if k[0] == "opencode"
      },
      by_account={
          k: dict(v) for k, v in t.by_account.items() if k[0] == "opencode"
      },
      span={
          k: tuple(v) for k, v in t.span.items() if k[0] == "opencode"
      },
      count=count)


def _partial_to_doc(partial: _OpencodePartial) -> dict:
  """The partial's persisted form. The bucket keys are tuples in memory; the document nests
  them by model (then account) so the JSON encoding stays collision-free without a separator
  convention model names would have to honor."""
  accounts: dict[str, dict] = {}
  for (_, model, account), bucket in partial.by_account.items():
    accounts.setdefault(model, {})[account] = bucket
  return {
      "by_model": {
          model: bucket for (_, model), bucket in partial.by_model.items()
      },
      "by_account": accounts,
      "span": {
          model: list(pair) for (_, model), pair in partial.span.items()
      },
      "count": partial.count,
  }


def _partial_from_doc(doc: object) -> _OpencodePartial | None:
  """The stored partial back as buckets, or None when the entry carries none (a v1 entry, or
  a document from before the field existed). Absent means the next merge replays instead of
  adjusting — the same contract an in-process partial absence follows."""
  if not isinstance(doc, dict):
    return None
  return _OpencodePartial(
      by_model={
          ("opencode", model): bucket for model, bucket in doc["by_model"].items()
      },
      by_account={
          ("opencode", model, account): bucket for model, buckets in doc["by_account"].items()
          for account, bucket in buckets.items()
      },
      span={
          ("opencode", model): tuple(pair) for model, pair in doc["span"].items()
      },
      count=doc["count"])


def _entry_records(entry: dict) -> list:
  """The entry's records under either entry shape: v2's ``rows`` map values, or the v1
  ``records`` list the first save rewrites."""
  rows = entry.get("rows")
  if rows is not None:
    return [row[1] for row in rows.values() if row[1] is not None]
  return entry["records"]


class _OpencodeScan(NamedTuple):
  """One row-memo advance. ``ok`` is False when the db is absent or sqlite-unreadable
  (``error`` carries the message the collect note needs). ``deltas`` carries the scan's
  per-row record moves as (old, new) record pairs — (None, new) for a row that landed,
  (old, None) for one that vanished; None means the memo was cold and the caller replays
  whole."""

  sig: list | None
  epoch: int
  nbytes: int
  ok: bool
  error: str | None
  deltas: list | None = None


# Per-cache-path in-process document memo: the parsed per-file entry maps of the document
# this process last loaded or saved. The multi-MB JSON re-parses on every changed round
# otherwise; entries are immutable once stored (stores replace, never mutate), so rounds
# share the maps and each save adopts the round's next-document state as the new memo.
_tally_cache_docs: dict[str, dict[str, dict[str, dict]]] = {}

# Whether the cache document's opencode entry holds this db's current row memo. A probe hit
# proves the rows unchanged since the entry was stored, so re-storing it would only re-sign
# the document — a multi-MB rewrite for a WAL signature the next write stales anyway — and
# the entry keeps its stored signature, leaving the save's equality check to skip the
# rewrite. Any scan that changes the row memo sets False, forcing the next cached merge to
# re-store current records.
_opencode_doc_synced: dict[str, bool] = {}


def _reset_aggregate_memo() -> None:
  """Drop the collection's process-wide memos (test isolation)."""
  global _aggregate_memo, _tally_memo
  _aggregate_memo = None
  _tally_memo = None
  _opencode_row_memos.clear()
  _opencode_row_epochs.clear()
  _opencode_partials.clear()
  _opencode_probes.clear()
  _tally_cache_docs.clear()
  _opencode_doc_synced.clear()
  _source_partials.clear()
  _claude_key_counts.clear()
  _claude_key_records.clear()
  _claude_key_loc.clear()
  _claude_key_holders.clear()
  _claude_orphan.clear()


# The append-tail fast path's prefix proof window: the guard hashes this many final
# prefix bytes, and a tail round re-hashes the same window before trusting the prefix.
_TAIL_WINDOW = 8192

# Read size per chunk the line splitter consumes. One C-level find scan per marker hands the
# fold only marker lines; a per-line Python membership test would pay every line instead.
_PARSE_CHUNK = 1 << 22


def _parse_lines(fh: BinaryIO, markers: tuple[bytes, ...]) -> tuple[list[dict], int, int]:
  """Parse the marker lines from *fh*'s current position to EOF.

  Returns (objects, bytes read, consumed byte offset), objects in file order. Only complete
  lines parse: a trailing fragment without its newline is left for the round whose read
  covers it whole, and the bytes count is the consumed offset — every complete line's bytes,
  which is what the read paid. A marker hit in the trailing fragment waits in the remainder
  for the next chunk; an unparseable marker line is dropped.
  """
  objects: list[dict] = []
  consumed = 0
  remainder = b""
  while True:
    chunk = fh.read(_PARSE_CHUNK)
    if not chunk:
      break
    data = remainder + chunk
    cut = data.rfind(b"\n")
    if cut == -1:
      remainder = data
      continue
    remainder = data[cut + 1:]
    consumed += cut + 1
    starts = set()
    for marker in markers:
      i = data.find(marker)
      while i != -1:
        if i <= cut:
          starts.add(data.rfind(b"\n", 0, i) + 1)
        i = data.find(marker, i + 1)
    for start in sorted(starts):
      try:
        objects.append(json.loads(data[start:data.find(b"\n", start) + 1].decode("utf-8", errors="replace")))
      except ValueError:
        continue
  return objects, consumed, consumed


def _prefiltered_jsonl(path: str, markers: tuple[bytes, ...]) -> tuple[list, list[dict], int, int]:
  """Parse one jsonl into the objects whose raw line carries any *markers* substring.

  Returns (signature, objects, bytes read, consumed byte offset), objects in file order. The
  signature is taken before the read: a concurrent append mid-read then necessarily outdates
  the stored sig and the next lookup re-scans, so a partial or extended read can never be
  served later as if complete. *consumed* is the offset the parse actually stopped at — the
  end of the last complete line — which the append-tail fast path continues from; it can sit
  past the signature's size when the writer appended mid-read. Every line counts toward the
  bytes; an unparseable line is dropped.
  """
  st = os.stat(path)
  with open(path, "rb") as fh:
    objects, nbytes, consumed = _parse_lines(fh, markers)
  return [st.st_mtime_ns, st.st_size], objects, nbytes, consumed


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


def _tail_parse(path: str, entry: dict, markers: tuple[bytes, ...]) -> tuple[list[dict], int, list, int] | None:
  """Parse the lines appended since *entry*'s parse, or None when the prefix is unproven.

  Returns (objects, bytes read, signature, new consumed offset). The prefix proof is the
  entry's own guard: the stored window must re-hash equal and end on a newline (the stored
  offset only ever follows a complete line, so a mid-line boundary — a replaced or truncated
  prefix — fails the check), and the file must have grown past the parsed offset with no
  mtime rewind. The signature is taken before the read, the same contract
  _prefiltered_jsonl runs under. The trailing partial line stays unparsed; the round whose
  tail covers it whole parses it.
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
    objects, nbytes, consumed = _parse_lines(fh, markers)
  return objects, nbytes, [st.st_mtime_ns, st.st_size], end + consumed


def _claude_records(recs: list[dict], seen: set) -> tuple[list[list], int]:
  """Fold prefiltered Claude records into tally rows, deduped against *seen*.

  Returns (records, within-file dupe count). *seen* carries the keys already counted —
  the empty set on a full parse, the cached records' keys on an append-tail round.
  """
  records: list[list] = []
  dupes = 0
  for rec in recs:
    msg = rec.get("message")
    if not isinstance(msg, dict):
      continue
    usage, model = msg.get("usage"), msg.get("model")
    if not isinstance(usage, dict) or not model or model == "<synthetic>":
      continue
    key = msg.get("id") or rec.get("requestId") or rec.get("uuid")
    if key in seen:
      dupes += 1
      continue
    seen.add(key)
    records.append(
        [
            key, model,
            rec.get("timestamp"),
            usage.get(ET.USAGE_INPUT_TOKENS, 0) or 0,
            usage.get(ET.USAGE_CACHE_CREATION_INPUT_TOKENS, 0) or 0,
            usage.get(ET.USAGE_CACHE_READ_INPUT_TOKENS, 0) or 0,
            usage.get(ET.USAGE_OUTPUT_TOKENS, 0) or 0
        ])
  return records, dupes


_CLAUDE_MARKERS = (b'"usage"',)


def _claude_file_contribution(path: str, prev: dict | None = None) -> tuple[dict, int]:
  """Parse one Claude Code jsonl into its cache entry; return (entry, bytes read).

  *prev* is the file's cached entry under an older signature; when the guard proves the
  prefix unchanged, only the appended tail parses and the cached records ride forward.
  """
  if prev is not None:
    tail = _tail_parse(path, prev, _CLAUDE_MARKERS)
    if tail is not None:
      recs, nbytes, sig, end = tail
      records, dupes = _claude_records(recs, {rec[0] for rec in prev["records"]})
      entry = {"sig": sig, "records": prev["records"] + records, "dupes": prev.get("dupes", 0) + dupes, "end": end}
      entry["guard"] = _boundary_guard(path, end)
      return entry, nbytes
  sig, recs, nbytes, end = _prefiltered_jsonl(path, _CLAUDE_MARKERS)
  records, dupes = _claude_records(recs, set())
  entry = {"sig": sig, "records": records, "dupes": dupes, "end": end}
  entry["guard"] = _boundary_guard(path, end)
  return entry, nbytes


class _FilePartial(NamedTuple):
  """One log file's contribution to the merged source buckets (see the module docstring).
  ``keys`` (Claude only) counts, per replay key, every copy the file's records carry, so a
  release can decrement the corpus counts exactly."""

  by_model: dict
  by_account: dict
  spans: dict
  n_records: int
  entry_dupes: int
  keys: dict | None


# Per-file partials keyed (source, account, path); the shared Claude replay-key state; and the
# orphan pool of contributions whose file moved while a copy survives elsewhere (key -> [record
# values] — the anchor resolves at merge time).
_source_partials: dict[tuple[str, str, str], _FilePartial] = {}
_claude_key_counts: dict[str, int] = {}
_claude_key_records: dict[str, list] = {}
_claude_key_loc: dict[str, tuple] = {}
_claude_key_holders: dict[str, set] = {}
_claude_orphan: dict[str, list] = {}
_ORPHAN = ("", "")  # _claude_key_loc sentinel: the contribution lives in _claude_orphan


def _add_record(t: _Tally, source: str, account: str, rec: list) -> None:
  t.add(source, rec[1], account, rec[2], in_fresh=rec[3], cache_write=rec[4], cache_read=rec[5], output=rec[6])


def _transfer_credit(key: str, old_loc: tuple, new_loc: tuple, new_account: str, fold: _Tally) -> None:
  """Move a replay key's contribution to an earlier-walked carrier — a fresh fold credits the
  first carrier in walk order, so a file appearing before the credited one takes the credit."""
  source, old_account, _ = old_loc
  rec = _claude_key_records[key]
  old = _source_partials[old_loc]
  vals = {"in_fresh": rec[3], "cache_write": rec[4], "cache_read": rec[5], "output": rec[6]}
  for map_key in ((source, rec[1], old_account), (source, rec[1])):
    buckets = old.by_account if len(map_key) == 3 else old.by_model
    bucket = buckets[map_key]
    for f, v in vals.items():
      bucket[f] -= v
    bucket["calls"] -= 1
    if not any(bucket.values()):
      del buckets[map_key]
  _add_record(fold, source, new_account, rec)
  _claude_key_loc[key] = new_loc
  _claude_key_holders[key].discard(new_loc)
  _claude_key_holders[key].add(old_loc)


def _fold_file_partial(source: str, account: str, path_key: tuple, entry: dict, order: dict) -> _FilePartial:
  """Fold one file's records into a fresh partial, registering every replay-key copy with the
  corpus counts; Claude dedupes corpus-wide (first fold wins), Codex always contributes."""
  fold = _Tally()
  keys = None
  if source == "Claude Code":
    keys = {}
    for rec in entry["records"]:
      key = rec[0]
      keys[key] = keys.get(key, 0) + 1
      prev = _claude_key_counts.get(key, 0)
      _claude_key_counts[key] = prev + 1
      if prev:
        _claude_key_holders.setdefault(key, set()).add(path_key)
        loc = _claude_key_loc[key]
        if loc != _ORPHAN and order[path_key] < order[loc]:
          _transfer_credit(key, loc, path_key, account, fold)
      else:
        _claude_key_records[key] = rec
        _claude_key_loc[key] = path_key
        _add_record(fold, source, account, rec)
  else:
    for model, ts, in_fresh, cache_read, output in entry["records"]:
      fold.add(source, model, account, ts, in_fresh=in_fresh, cache_read=cache_read, output=output)
  return _FilePartial(fold.by_model, fold.by_account, fold.span, len(entry["records"]), entry.get("dupes", 0), keys)


def _release_partial(path_key: tuple, partial: _FilePartial) -> None:
  """Retract one file's replay-key copies from the corpus counts; a contribution whose file
  moves survives via a surviving copy (the orphan pool), the last copy drops it."""
  if partial.keys is None:
    return
  for key, copies in partial.keys.items():
    remaining = _claude_key_counts[key] - copies
    if remaining:
      _claude_key_counts[key] = remaining
      holders = _claude_key_holders.get(key)
      if holders is not None:
        holders.discard(path_key)
        if not holders:
          del _claude_key_holders[key]
      if _claude_key_loc[key] == path_key:
        if not holders:
          raise AssertionError(f"released replay key {key!r} with copies but no surviving holder")
        _claude_key_loc[key] = _ORPHAN
        _claude_orphan[key] = _claude_key_records[key]
    else:
      loc = _claude_key_loc.pop(key)
      del _claude_key_counts[key], _claude_key_records[key]
      _claude_key_holders.pop(key, None)
      if loc == _ORPHAN:
        del _claude_orphan[key]


def _apply_partial(t: _Tally, partial: _FilePartial) -> None:
  """Merge one surviving partial's buckets and span into the in-flight tally."""
  for src, tgt_map in ((partial.by_model, t.by_model), (partial.by_account, t.by_account)):
    for key, vals in src.items():
      tgt = tgt_map[key]
      for name in FIELDS:
        tgt[name] += vals[name]
  for key, (lo, hi) in partial.spans.items():
    span = t.span[key]
    span[0] = lo if span[0] is None or lo < span[0] else span[0]
    span[1] = hi if span[1] is None or hi > span[1] else span[1]


def _reconcile_partials(t: _Tally, source: str, walked: list[tuple[str, str, dict | None, bool]], order: dict) -> None:
  """Rebuild the source's merged buckets from the per-file partial state: a partial survives
  only its cache hit with an unchanged account; the rest release, the re-parsed, relabelled
  and brand-new files re-fold, and the tally sums the survivors plus the orphan pool —
  exactly what a fresh fold of the current corpus computes (module docstring)."""
  seen = {w[0]: w for w in walked if w[3] and w[2] is not None}
  for state_key in [k for k in _source_partials if k[0] == source]:
    entry_row = seen.get(state_key[2])
    if entry_row is not None and entry_row[1] == state_key[1]:
      continue
    _release_partial(state_key, _source_partials.pop(state_key))
  for path_str, account, entry, _ in walked:
    if entry is None or (source, account, path_str) in _source_partials:
      continue
    state_key = (source, account, path_str)
    _source_partials[state_key] = _fold_file_partial(source, account, state_key, entry, order)
  for state_key, partial in _source_partials.items():
    if state_key[0] == source:
      _apply_partial(t, partial)
  if source == "Claude Code":
    for key, record in _claude_orphan.items():
      holders = _claude_key_holders.get(key)
      if not holders:
        raise AssertionError(f"orphaned replay key {key!r} with no surviving holder")
      # The earliest-walked surviving holder is the account a fresh scan would credit.
      anchor = min(holders, key=order.__getitem__)
      _add_record(t, source, anchor[1], record)


def _walk_source(
    t: _Tally,
    source: str,
    cache_key: str,
    sub: str,
    homes: dict[str, Path],
    cache: TallyCache | None,
    parse: Callable,
) -> tuple[list[tuple[str, str, dict | None, bool]], dict]:
  """Walk every log file, serving cache hits and parsing misses; returns one row per file —
  (path, account, entry or None on a failed parse, cache-hit flag) — plus the walk order.

  The walk is ``_iter_jsonl_stats``: each file arrives as its str path plus the stat the
  walker took before yielding, so a serve pays one syscall per file and no Path build — the
  same stat pair both proves the cached entry and keys its store. Only a cache miss builds
  anything heavier than dict lookups.
  """
  walked: list[tuple[str, str, dict | None, bool]] = []
  order: dict[tuple, int] = {}
  for account, home in homes.items():
    for path, st, error in _iter_jsonl_stats(home / sub, t, source, account):
      if st is None:
        t.notes.append(f"{source}: unreadable {account}/{os.path.basename(path)}: {error}")
        walked.append((path, account, None, False))
        continue
      entry = (cache.lookup_sig(cache_key, path, [st.st_mtime_ns, st.st_size]) if cache is not None else None)
      hit = entry is not None
      if entry is None:
        prev = cache.prev(cache_key, path) if cache is not None else None
        try:
          entry, nbytes = parse(path, prev)
        except OSError as exc:
          t.notes.append(f"{source}: unreadable {account}/{os.path.basename(path)}: {exc}")
          walked.append((path, account, None, False))
          continue
        t.scanned_bytes += nbytes
        if cache is not None:
          cache.store_sig(cache_key, path, entry)
      state_key = (source, account, path)
      order[state_key] = len(order)
      walked.append((path, account, entry, hit))
  return walked, order


def collect_claude(t: _Tally, homes: dict[str, Path], cache: TallyCache | None) -> None:
  walked, order = _walk_source(t, "Claude Code", "claude", "projects", homes, cache, _claude_file_contribution)
  _reconcile_partials(t, "Claude Code", walked, order)
  n_records = sum(p.n_records for k, p in _source_partials.items() if k[0] == "Claude Code")
  entry_dupes = sum(p.entry_dupes for k, p in _source_partials.items() if k[0] == "Claude Code")
  distinct = len(_claude_key_counts)
  t.notes.append(
      f"Claude Code: {distinct:,} unique API responses over {len(homes)} config dirs, "
      f"{entry_dupes + n_records - distinct:,} replayed lines skipped")


# Every record the tally reads (session_meta, turn_context, token_count) serializes
# its type as a quoted literal in the raw line, so the substring filter cannot skip a
# record the full parse would see; it only skips parsing irrelevant lines.
_CODEX_MARKERS = tuple(f'"{name}"'.encode() for name in (CODEX_SESSION_META, CODEX_TURN_CONTEXT, CODEX_TOKEN_COUNT))


def _codex_records(recs: list[dict], model: str | None, records: list[list]) -> tuple[int, int, str | None]:
  """Fold prefiltered Codex records into token_count rows, appending to *records*.

  Returns (walked sum, final_total high-water, trailing model context). *model* is the
  context in force at the first record — None on a full parse, the cached entry's trailing
  context on an append-tail round; session_meta and turn_context records update it in file
  order, and every token_count row resolves against the context at its own line.
  """
  walked = 0
  final_total = 0
  for rec in recs:
    payload = rec.get("payload") or {}
    if rec.get("type") in (CODEX_SESSION_META, CODEX_TURN_CONTEXT):
      model = payload.get("model") or model
      continue
    if rec.get("type") != CODEX_EVENT_MSG or payload.get("type") != CODEX_TOKEN_COUNT:
      continue
    info = payload.get("info") or {}
    last, total = info.get("last_token_usage") or {}, info.get("total_token_usage") or {}
    final_total = max(final_total, total.get("total_tokens", 0) or 0)
    cached = last.get("cached_input_tokens", 0) or 0
    fresh = (last.get("input_tokens", 0) or 0) - cached
    out = last.get("output_tokens", 0) or 0
    walked += cached + fresh + out
    records.append([model or "unknown", rec.get("timestamp"), fresh, cached, out])
  return walked, final_total, model


def _codex_file_contribution(path: str, prev: dict | None = None) -> tuple[dict, int]:
  """Parse one Codex rollout jsonl into its cache entry; return (entry, bytes read).

  *prev* is the file's cached entry under an older signature; when the guard proves the
  prefix unchanged, only the appended tail parses and the cached records ride forward with
  the model context, is_root and self-check state the prefix settled.
  """
  if prev is not None:
    tail = _tail_parse(path, prev, _CODEX_MARKERS)
    if tail is not None:
      recs, nbytes, sig, end = tail
      records: list[list] = []
      walked, final_total, model = _codex_records(recs, prev.get("model_ctx"), records)
      total_walked = prev.get("walked", 0) + walked
      total_final = max(prev.get("final_total", 0), final_total)
      is_root = prev.get("is_root", False)
      entry = {
          "sig": sig,
          "records": prev["records"] + records,
          "check": [total_walked, total_final] if total_final and is_root else None,
          "model_ctx": model,
          "is_root": is_root,
          "final_total": total_final,
          "walked": total_walked,
          "end": end
      }
      entry["guard"] = _boundary_guard(path, end)
      return entry, nbytes
  sig, recs, nbytes, end = _prefiltered_jsonl(path, _CODEX_MARKERS)
  meta = next((rec for rec in recs if rec.get("type") == CODEX_SESSION_META), None)
  mp = (meta or {}).get("payload") or {}
  source = json.dumps(mp.get("source") or {})
  is_root = not (mp.get("forked_from_id") or mp.get("parent_thread_id") or "subagent" in source)
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
  walked, final_total, model = _codex_records(recs, model, records)
  entry = {
      "sig": sig,
      "records": records,
      "check": [walked, final_total] if final_total and is_root else None,
      "model_ctx": model,
      "is_root": is_root,
      "final_total": final_total,
      "walked": walked,
      "end": end
  }
  entry["guard"] = _boundary_guard(path, end)
  return entry, nbytes


def collect_codex(t: _Tally, homes: dict[str, Path], cache: TallyCache | None) -> None:
  walked, order = _walk_source(t, "Codex", "codex", "sessions", homes, cache, _codex_file_contribution)
  _reconcile_partials(t, "Codex", walked, order)
  check = [tuple(entry["check"]) for _, _, entry, _ in walked if entry is not None and entry["check"] is not None]
  if check:
    w = sum(x for x, _ in check)
    f = sum(y for _, y in check)
    t.notes.append(
        f"Codex: per-request sum {w:,} vs session totals {f:,} ({(w - f) / f * 100:+.2f}% over "
        f"{len(check)} root sessions; /compact resets a session total, so the per-request sum leads)")


# The cold-pass scan projects each matching row's tally fields inside SQLite: json_extract
# in C there beats a Python round trip plus json.loads per row (measured ~4x slower over this
# host's 36k-row message table). json_valid keeps the old json.loads failure mode: the LIKE
# prefilter can match a malformed row, and skipping it must not error the query. Non-object
# tokens project NULLs, dropped by the row filter below. The trailing id/time_updated columns
# seed the row memo; rows the WHERE clause drops are known non-contributors and memoize as
# None without a re-read. _opencode_row_data must project an identical record per row.
_OPENCODE_SCAN_SQL = """
select json_extract(data, '$.modelID'), json_extract(data, '$.providerID'),
       json_extract(data, '$.time.created'),
       json_extract(data, '$.tokens.input'), json_extract(data, '$.tokens.output'),
       json_extract(data, '$.tokens.total'),
       json_extract(data, '$.tokens.cache.write'), json_extract(data, '$.tokens.cache.read'),
       length(data), id, time_updated
from message
where data like '%"tokens"%'
  and json_valid(data)
  and json_extract(data, '$.role') = 'assistant'
"""

# Row keys the incremental path diffs against the memo; a leaf-page scan that never touches
# the data blobs' overflow pages (~0.03 s warm over this host's table).
_OPENCODE_KEYS_SQL = "select id, time_updated from message"

# ASCII-case-insensitive mirror of the scan SQL's LIKE '%"tokens"%' prefilter (SQLite folds
# only A-Z, so str.lower would mismatch marks SQLite leaves distinct).
_OPENCODE_TOKENS_LIKE = re.compile(r'"[tT][oO][kK][eE][nN][sS]"').search


def _opencode_row(row: tuple) -> list | None:
  """Tally record for one projected message row, or None when it contributes nothing."""
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


def _opencode_row_data(data: str) -> tuple[list | None, int]:
  """One message row's (record, bytes counted) from its data blob, as the scan SQL projects.

  Byte-counted exactly when the SQL filter chain (LIKE prefilter, json_valid, assistant role)
  would return the row; the record is then _opencode_row over the same eight projections.
  """
  if _OPENCODE_TOKENS_LIKE(data) is None:
    return None, 0
  try:
    obj = json.loads(data, parse_constant=_strict_json_constant)
  except (ValueError, RecursionError):
    return None, 0
  if not isinstance(obj, dict) or obj.get("role") != "assistant":
    return None, 0
  tokens = obj.get("tokens")
  tokens = tokens if isinstance(tokens, dict) else {}
  cache = tokens.get("cache")
  cache = cache if isinstance(cache, dict) else {}
  created = obj.get("time")
  created = created.get("created") if isinstance(created, dict) else None
  rec = _opencode_row(
      (
          obj.get("modelID"), obj.get("providerID"), created, tokens.get("input"), tokens.get("output"),
          tokens.get("total"), cache.get("write"), cache.get("read")))
  return rec, len(data)


def _opencode_db_signature(db: Path) -> list | None:
  """The db's cache signature: main file stat plus the ``-wal`` sidecar's.

  A WAL-mode write grows the sidecar without touching the main file; a checkpoint rewrites the
  main file and truncates or removes the sidecar. Both moves change the composite, so a change
  the read path could observe always invalidates. None signals a stat failure (no caching).
  """
  try:
    st = db.stat()
  except OSError:
    return None
  try:
    wal = db.with_name(db.name + "-wal").stat()
    wal_sig: list | None = [wal.st_mtime_ns, wal.st_size]
  except OSError:
    wal_sig = None
  return [st.st_mtime_ns, st.st_size, wal_sig]


def _advance_opencode_rows(db: Path, seed: dict | None = None) -> _OpencodeScan:
  """Advance the db's row memo to its message table's current rows, bumping the epoch when any
  row moved. Read-only: the scan never writes. Absent or unreadable dbs advance nothing and
  return ``ok=False``. A warm memo first checks the proof aggregates: unchanged (count, sum)
  proves every row move the aggregates can see is absent and the scan is skipped — a weaker
  proof than the key scan's per-id diff (see the probe comment), traded for not reading
  85k keys on the WAL-noise rounds that are the steady state this gate exists for.

  *seed* is the persisted document's ``rows`` map for this db. A cold memo seeded from it
  skips the whole-blob cold scan: the memo starts at the document's rows and the warm key
  diff fetches only rows that moved since the document was written.
  """
  key = str(db)
  sig = _opencode_db_signature(db)
  try:
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
      memo = _opencode_row_memos.setdefault(key, {})
      # A seeded cold memo has no stored probe to check against, so it skips the gate's probe
      # read and takes the same post-scan probe the cold-memo path computes.
      seeded = not memo and seed is not None
      if seeded:
        memo.update({mid: (row[0], row[1]) for mid, row in seed.items()})
      con.execute("begin")  # one snapshot: the stored proof must describe the scanned state
      probe = None if seeded else (tuple(con.execute(_OPENCODE_PROBE_SQL).fetchone()) if memo else None)
      if probe is not None and _opencode_probes.get(key) == probe:
        con.commit()
        return _OpencodeScan(sig, _opencode_row_epochs.get(key, 0), 0, True, None, [])
      nbytes, deltas = _scan_opencode_rows(con, memo)
      if probe is None:  # cold memo: the scan's snapshot is the state the memo now describes
        probe = tuple(con.execute(_OPENCODE_PROBE_SQL).fetchone())
        _opencode_doc_synced[key] = False
      elif any(old is not None or new is not None for old, new in deltas):
        # (None, None) pairs are non-contributing rows whose key moved; the records the
        # document holds are unchanged, so only a record-bearing move unsyncs the entry.
        _opencode_doc_synced[key] = False
      _opencode_probes[key] = probe
      con.commit()
    finally:
      con.close()
  except sqlite3.Error as exc:
    # A failed scan leaves its memo partially advanced at worst; dropping the partial forces
    # the next merge down the full replay, which rebuilds both from whatever the memo holds.
    # The stored proof describes a state the failed scan never reached, so it drops too.
    _opencode_probes.pop(key, None)
    _opencode_partials[key] = None
    return _OpencodeScan(sig, 0, 0, False, str(exc))
  epoch = _opencode_row_epochs.get(key, 0)
  if deltas:
    epoch += 1
    _opencode_row_epochs[key] = epoch
  return _OpencodeScan(sig, epoch, nbytes, True, None, deltas)


def _replay_opencode_records(t: _Tally, records: list) -> None:
  """Fold a record list into the accumulator (the cold and cache-document paths)."""
  for model, account, ts, in_fresh, cache_write, cache_read, output in records:
    t.add(
        "opencode",
        model,
        account,
        ts,
        in_fresh=in_fresh,
        cache_write=cache_write,
        cache_read=cache_read,
        output=output)


def _merge_opencode(
    t: _Tally,
    db: Path,
    cache: TallyCache | None,
    scan: _OpencodeScan | None,
) -> tuple[list | None, int, bool]:
  """Tally the opencode db into the accumulator. Scans unless ``scan`` already advanced the
  row memo this collect, or the cache document's entry still matches the file. Returns
  (the signature the rows were read at, the row epoch, scan-sourced); the signature is None
  when no signature applies (absent, unstatable, or unreadable db), so the whole-tally memo
  never signs rows it cannot key. A scan-reported delta set with a current partial adjusts
  the source's buckets instead of replaying every record; an entry-served merge adopts the
  partial the same way (the partial's rows are the served entry's — see the entry comment);
  the partial always ends the merge describing the rows that merge served."""
  if not db.exists():
    t.notes.append("opencode: db absent")
    return None, 0, False
  sig = _opencode_db_signature(db)
  key = str(db)
  entry = cache.lookup_sig("opencode", key, sig) if cache is not None and sig is not None else None
  epoch = 0
  from_scan = False
  if entry is not None:
    # The signature is taken before the read and stored with the rows, so an entry can only
    # be served while the file still matches it, and a row move writes the db or its WAL
    # sidecar, which moves that signature — a served entry's records are therefore unchanged
    # since the merge that stored them, so a partial built from those records (by the replay
    # below or a scan-path fold) sums exactly what this merge would fold, and its buckets
    # adopt in place of the per-record fold. The row memo itself is still only ever advanced
    # by scans.
    epoch = _opencode_row_epochs.get(key, 0)
    partial = _opencode_partials.get(key)
    if partial is None:
      # Process start: adopt the entry's stored partial when it carries one, so the served
      # buckets never replay the records; one replay builds the partial only for a v1 entry.
      stored = _partial_from_doc(entry.get("partial"))
      if stored is not None:
        _opencode_partials[key] = stored
        partial = stored
    if partial is None:
      records = _entry_records(entry)
      _replay_opencode_records(t, records)
      count = len(records)
      _opencode_partials[key] = _snapshot_opencode_partial(t, count)
    else:
      count = _adjust_opencode_partial(t, _opencode_row_memos.setdefault(key, {}), partial, [])
    t.notes.append(f"opencode: {count:,} assistant messages with token counts")
    return sig, epoch, from_scan
  if scan is None:
    seed = None
    prev = cache.prev("opencode", key) if cache is not None else None
    if prev is not None:
      seed = prev.get("rows")
      if _opencode_partials.get(key) is None:
        stored = _partial_from_doc(prev.get("partial"))
        if stored is not None:
          _opencode_partials[key] = stored
    scan = _advance_opencode_rows(db, seed)
  if not scan.ok:
    t.notes.append(f"opencode: unreadable db: {scan.error}")
    return None, scan.epoch, False
  memo = _opencode_row_memos[key]
  t.scanned_bytes += scan.nbytes
  epoch = scan.epoch
  from_scan = True
  partial = _opencode_partials.get(key)
  if scan.deltas is None or partial is None:
    records = [rec for _, rec in memo.values() if rec is not None]
    _replay_opencode_records(t, records)
    count = len(records)
  else:
    count = _adjust_opencode_partial(t, memo, partial, scan.deltas)
  # The stored partial must never alias a served tally's containers, so it copies out; the
  # store below persists it, so it lands before the entry is built.
  _opencode_partials[key] = _snapshot_opencode_partial(t, count)
  if cache is not None and sig is not None:
    entry = cache._sources.get("opencode", {}).get(str(db))
    if entry is not None and _opencode_doc_synced.get(key):
      # The probe proved the rows unchanged since this entry was stored: its signature is
      # stale only by WAL writes to rows the tally never reads, and re-signing it would
      # rewrite the multi-MB document for a signature the next WAL write stales anyway.
      cache.store("opencode", db, entry)
    else:
      cache.store(
          "opencode", db, {
              "sig": sig,
              "rows": {
                  mid: (tu, rec) for mid, (tu, rec) in memo.items()
              },
              "partial": _partial_to_doc(_opencode_partials[key]),
          })
      _opencode_doc_synced[key] = True
  t.notes.append(f"opencode: {count:,} assistant messages with token counts")
  return sig, epoch, from_scan


def _adjust_opencode_partial(
    t: _Tally,
    memo: dict[str, tuple[int, list | None]],
    partial: _OpencodePartial,
    deltas: list[tuple[list | None, list | None]],
) -> int:
  """Carry the partial across one scan's row moves: fold every delta out of and into a copy
  of the buckets, re-derive spans a removal invalidated, drop buckets whose last record went
  away, and feed the result into *t*. Returns the contributing-record count. An empty delta
  set is the entry-served merge's adoption: the buckets feed *t* unchanged."""
  by_model = {k: dict(v) for k, v in partial.by_model.items()}
  by_account = {k: dict(v) for k, v in partial.by_account.items()}
  span = {k: tuple(v) for k, v in partial.span.items()}
  rederive: set = set()
  for old, new in deltas:
    for rec, sign in ((old, -1), (new, 1)):
      if rec is None:
        continue
      # Fold one record out (−1) or in (+1); a subtraction touching a span boundary marks
      # the bucket for re-derivation from the row memo.
      model, account, ts, in_fresh, cache_write, cache_read, output = rec
      vals = {"in_fresh": in_fresh, "cache_write": cache_write, "cache_read": cache_read, "output": output}
      for key, bucket in ((("opencode", model), by_model), (("opencode", model, account), by_account)):
        tgt = bucket.setdefault(key, dict.fromkeys(FIELDS, 0)) if sign > 0 else bucket[key]
        for name, value in vals.items():
          tgt[name] += sign * value
        tgt["calls"] += sign
      if ts:
        lo, hi = span.get(("opencode", model), (None, None))
        if sign > 0:
          span[("opencode", model)] = (ts if lo is None or ts < lo else lo, ts if hi is None or ts > hi else hi)
        elif ts == lo or ts == hi:
          rederive.add(("opencode", model))
  for span_key in rederive:
    lo = hi = None
    for _, rec in memo.values():
      if rec is not None and rec[0] == span_key[1] and rec[2]:
        lo = rec[2] if lo is None or rec[2] < lo else lo
        hi = rec[2] if hi is None or rec[2] > hi else hi
    span[span_key] = (lo, hi)
  for key in [k for k, v in by_model.items() if v["calls"] == 0]:
    del by_model[key]
    span.pop(key, None)
  for key in [k for k, v in by_account.items() if v["calls"] == 0]:
    del by_account[key]
  count = partial.count + sum(1 for _, new in deltas if new is not None) \
      - sum(1 for old, _ in deltas if old is not None)
  t.by_model.update({k: dict(v) for k, v in by_model.items()})
  t.by_account.update({k: dict(v) for k, v in by_account.items()})
  t.span.update({k: [lo, hi] for k, (lo, hi) in span.items()})
  return count


def _scan_opencode_rows(
    con: sqlite3.Connection,
    memo: dict[str, tuple[int, list | None]],
) -> tuple[int, list[tuple[list | None, list | None]] | None]:
  """Advance *memo* to the message table's current rows; return the bytes this pass read and
  the per-row record deltas ``(old, new)`` — rows whose ``(id, time_updated)`` key moved, and
  ``(old, None)`` for ids that vanished. With an empty memo the SQL scan projects every
  contributing row (the cold pass, as at a process start), rows it filters out memoize as
  None, and deltas come back None — the caller replays whole. All reads happen before any
  memo write, so a failure mid-scan leaves the memo — and the partial keyed to it —
  untouched.
  """
  live = {mid: tu for mid, tu in con.execute(_OPENCODE_KEYS_SQL)}
  nbytes = 0
  if not memo:
    fresh: dict[str, tuple[int, list | None]] = {}
    for row in con.execute(_OPENCODE_SCAN_SQL):
      nbytes += row[8]
      fresh[row[9]] = (row[10], _opencode_row(row[:8]))
    for mid, tu in live.items():
      if mid not in fresh:
        fresh[mid] = (tu, None)
    memo.update(fresh)
    return nbytes, None
  removed_ids = [mid for mid in memo if mid not in live]
  changed_ids = [mid for mid, tu in live.items() if memo.get(mid, (None,))[0] != tu]
  fetched: list[tuple[str, int, list | None]] = []
  for mid in changed_ids:
    row = con.execute("select data from message where id = ?", (mid,)).fetchone()
    rec, n = (None, 0) if row is None else _opencode_row_data(row[0])
    nbytes += n
    fetched.append((mid, live[mid], rec))
  deltas = [(memo[mid][1] if mid in memo else None, rec) for mid, _, rec in fetched]
  deltas += [(memo[mid][1], None) for mid in removed_ids]
  for mid in removed_ids:
    del memo[mid]
  for mid, tu, rec in fetched:
    memo[mid] = (tu, rec)
  return nbytes, deltas


def _build(t: _Tally) -> list[dict]:
  rows: list[dict] = []
  for (source, model), c in t.by_model.items():
    lo, hi = t.span[(source, model)]
    accts: dict[str, dict] = {a: dict(v) for (s, m, a), v in t.by_account.items() if s == source and m == model}
    total = c["in_fresh"] + c["cache_write"] + c["cache_read"] + c["output"]
    acct_rows = [
        AccountRow(
            name=a,
            calls=v["calls"],
            output=v["output"],
            total=v["in_fresh"] + v["cache_write"] + v["cache_read"] + v["output"],
        ) for a, v in sorted(
            accts.items(),
            key=lambda kv: -(kv[1]["in_fresh"] + kv[1]["cache_write"] + kv[1]["cache_read"] + kv[1]["output"]))
    ]
    rows.append(
        {
            "source": source,
            "model": model,
            **{
                k: c[k] for k in FIELDS
            },
            "total": total,
            "first": (lo or "")[:10],
            "last": (hi or "")[:10],
            "accounts": acct_rows,
        })
    rows.sort(key=lambda r: -r["total"])
  return rows


def _materialize_rows(rows: list[dict]) -> list[ModelRow]:
  """One ModelRow set per collect from the built row dicts; the accounts list copies, so a
  served tally never shares a mutable container with the whole-tally memo's stored rows."""
  return [ModelRow(**{**row, "accounts": list(row["accounts"])}) for row in rows]


def collect_token_usage(
    claude_homes: dict[str, Path] | None = None,
    codex_homes: dict[str, Path] | None = None,
    opencode_db: Path | None = None,
    cache_path: Path | None = None,
) -> TokenTally:
  """A failing source records a note instead of raising; see the module docstring for the cache.

  Roots default to this host's on-disk layout: data is discovered from config.yaml plus the
  defaults ``~/.claude``, ``~/.codex`` and the opencode database. Tests pass explicit roots.
  ``cache_path`` is the only state the collection persists: the per-file contribution document
  described at module level. None collects cacheless.
  """
  start = time.perf_counter()
  if claude_homes is None or codex_homes is None:
    discovered_claude, discovered_codex = discover_homes(DEFAULT_CLAUDE_DIR, DEFAULT_CODEX_HOME)
    claude_homes = claude_homes if claude_homes is not None else discovered_claude
    codex_homes = codex_homes if codex_homes is not None else discovered_codex
  if opencode_db is None:
    opencode_db = DEFAULT_OPENCODE_DB

  global _aggregate_memo, _tally_memo
  signature = _corpus_signature(claude_homes, codex_homes)
  lookup_sig = _opencode_db_signature(opencode_db)
  tally_memo = _tally_memo
  if lookup_sig is not None and tally_memo is not None and tally_memo[0][:2] == (signature, lookup_sig):
    _, rows, notes = tally_memo
    return TokenTally(
        rows=_materialize_rows(rows),
        notes=list(notes),
        elapsed_s=time.perf_counter() - start,
        scanned_bytes=0,
    )
  fresh_sources = _aggregate_memo is None or _aggregate_memo[0] != signature
  scan: _OpencodeScan | None = None
  if not fresh_sources and lookup_sig is not None and tally_memo is not None \
          and tally_memo[0][0] == signature and tally_memo[0][3]:
    # The db signature moved, so the fast hit missed; the row memo's key diff is the cheap
    # proof of whether the WAL wrote rows the tally reads. An unchanged epoch re-serves the
    # memo and re-signs it at the scan's own signature.
    scan = _advance_opencode_rows(opencode_db)
    if scan.ok and scan.epoch == tally_memo[0][2]:
      _, rows, notes = tally_memo
      _tally_memo = ((signature, scan.sig, scan.epoch, True), rows, list(notes))
      return TokenTally(
          rows=_materialize_rows(rows),
          notes=list(notes),
          elapsed_s=time.perf_counter() - start,
          scanned_bytes=0,
      )
  t = _Tally()
  cache = None
  if fresh_sources and cache_path is not None:
    # The parsed document memoizes per cache path: a changed round re-parses zero document
    # bytes and serves unchanged files from the adopted entry maps.
    key = str(cache_path)
    sources = _tally_cache_docs.get(key)
    if sources is None:
      sources = TallyCache.load(cache_path, t.notes)._sources
      _tally_cache_docs[key] = sources
    cache = TallyCache(sources)
  if fresh_sources:
    notes_from = len(t.notes)
    collect_claude(t, claude_homes, cache)
    collect_codex(t, codex_homes, cache)
    _aggregate_memo = (signature, _SourceAggregate.snapshot(t, notes_from))
  else:
    _aggregate_memo[1].apply(t)
  read_sig, epoch, from_scan = _merge_opencode(t, opencode_db, cache, scan)
  # Save only on the source-walk path: its lookups refreshed the next document. A memo hit's
  # only fresh entry is the opencode db's, whose WAL sig the next load recomputes anyway.
  if cache is not None:
    try:
      cache.save(cache_path)
      # Adopt the round's next document as the memo only where the save succeeded — the
      # on-disk state now describes it, and entries this round stopped seeing (deleted
      # logs) drop out with it. A failed save leaves the previous memo: its per-file
      # signatures gate every lookup, so moved files re-scan and correctness never rides
      # the document.
      _tally_cache_docs[str(cache_path)] = {source: dict(files) for source, files in cache._next.items()}
    except OSError as exc:
      t.notes.append(f"Tally cache: save failed: {exc}")
  rows = _build(t)
  if read_sig is not None:
    _tally_memo = ((signature, read_sig, epoch, from_scan), rows, list(t.notes))
  elapsed = time.perf_counter() - start
  return TokenTally(
      rows=_materialize_rows(rows),
      notes=t.notes,
      elapsed_s=elapsed,
      scanned_bytes=t.scanned_bytes,
  )
