"""Tally token usage per model across every agent log on this host.

One model is one row: the same model served by several subscriptions (Claude config dirs, Codex
homes) is merged, with the per-account split kept as a secondary breakdown on each row.

Sources, all local logs (no vendor usage API is called):
  Claude Code  <config_dir>/projects/**/*.jsonl   assistant message.usage + message.model
  Codex        <codex_home>/sessions/**/*.jsonl   token_count events, model from turn_context
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
corpus memo: its WAL sidecar moves under plain serve traffic, so its entry always round-trips
the persisted document, and the miss path is incremental per message row (see row memo below).
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
              kept in lockstep with the row memo: every merge ends with the partial matching
              the memo, and a merge that moves it adjusts the partial by the scan's deltas.
Vocabulary:
  signature   ``[mtime_ns, size]`` for a log file; a file re-scans whole whenever either value
              moves. The opencode db signs as ``[mtime_ns, size, wal_sig]`` with ``wal_sig`` the
              ``-wal`` sidecar's ``[mtime_ns, size]`` or None — a WAL-mode write grows the
              sidecar without touching the main file, so the main file pair alone cannot see it
  entry       a source's parsed contribution: ``{"sig", "records", "dupes"}`` for a Claude file
              (dupes is the within-file replay count), ``{"sig", "records", "check"}`` for a
              Codex file (check is the root-session self-check pair ``[walked, final_total]`` or
              None), ``{"sig", "records"}`` for the opencode db
  records     Claude: ``[key, model, ts, in_fresh, cache_write, cache_read, output]`` per
              response, replay-deduped within the file; Codex: ``[model, ts, in_fresh,
              cache_read, output]`` per token_count event, model resolved by file position;
              opencode: ``[model, account, ts, in_fresh, cache_write, cache_read, output]`` per
              assistant message with token counts
Cross-file replay dedupe happens at merge (first record wins in walk order), which composed
with within-file first-wins gives exactly the global first-wins a cacheless scan computes.
"""

from __future__ import annotations

import datetime as dt
import json
import os
import re
import sqlite3
import time
from collections import defaultdict
from collections.abc import Callable, Iterator
from dataclasses import dataclass, field
from pathlib import Path
from typing import NamedTuple

from src.core.config import get_config
from src.core.json_utils import write_json_atomically
from src.core.models import BackendType

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

  Reading the backend options keeps a newly added subscription in the tally without an edit here;
  the defaults are always included.
  """
  cfg = get_config()
  claude: set[Path] = {claude_default}
  codex: set[Path] = {codex_default}
  for opt in cfg.backend_options:
    if opt.type == BackendType.CC_CLAUDE and opt.claude_config_dir:
      claude.add(Path(opt.claude_config_dir).expanduser())
    elif opt.type == BackendType.CODEX and opt.codex_home:
      codex.add(Path(opt.codex_home).expanduser())

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

  ``lookup`` serves an entry only while the file's signature matches and copies the hit into
  the next document; ``store`` adds fresh scans there. The saved document therefore holds only
  files seen this run — deleted logs drop out without a separate sweep.
  """

  SCHEMA_VERSION = 1

  def __init__(self, sources: dict[str, dict[str, dict]]):
    self._sources = sources
    self._next: dict[str, dict[str, dict]] = defaultdict(dict)

  @classmethod
  def load(cls, path: Path, notes: list[str]) -> TallyCache:
    """Read the persisted document; an unreadable or stale-schema file starts a cold cache."""
    try:
      doc = json.loads(path.read_text())
    except FileNotFoundError:
      doc = None
    except (OSError, ValueError) as exc:
      notes.append(f"Tally cache: unreadable {path} ({exc}); rebuilt from the logs")
      doc = None
    if not isinstance(doc, dict) or doc.get("version") != cls.SCHEMA_VERSION:
      return cls({})
    return cls(doc.get("sources", {}))

  def save(self, path: Path) -> None:
    """Persist the next document atomically when it moved, creating the cache directory."""
    if self._next == self._sources:
      return
    path.parent.mkdir(parents=True, exist_ok=True)
    write_json_atomically(path, {"version": self.SCHEMA_VERSION, "sources": self._next})

  def lookup(self, source: str, path: Path) -> dict | None:
    """The cached entry for *path* when its signature still matches, else None."""
    entry = self._sources.get(source, {}).get(str(path))
    if entry is None:
      return None
    try:
      st = path.stat()
    except OSError:
      return None
    if entry.get("sig") != [st.st_mtime_ns, st.st_size]:
      return None
    self._next[source][str(path)] = entry
    return entry

  def lookup_sig(self, source: str, key: str, sig: list) -> dict | None:
    """The cached entry for *key* when its stored signature equals *sig*, else None.

    Sibling of ``lookup`` for sources whose signature is not one file's stat: the
    caller computes *sig*.
    """
    entry = self._sources.get(source, {}).get(key)
    if entry is None:
      return None
    if entry.get("sig") != sig:
      return None
    self._next[source][key] = entry
    return entry

  def store(self, source: str, path: Path, entry: dict) -> None:
    """Record one freshly scanned contribution for the next document."""
    self._next[source][str(path)] = entry


def _walk_error_hook(t: _Tally, source: str, label: str, root_name: str) -> Callable[[OSError], None]:
  """The os.walk onerror hook turning an unreadable directory into a per-account note."""

  def _onerror(exc: OSError) -> None:
    if not isinstance(exc, FileNotFoundError):
      t.notes.append(f"{source}: unreadable {label}/{root_name}: {exc}")

  return _onerror


def _iter_jsonl(root: Path, t: _Tally, source: str, label: str) -> Iterator[Path]:
  """Yield ``*.jsonl`` under *root*, recording a note when a directory is unreadable.

  ``Path.rglob`` swallows ``PermissionError`` while walking (shell-glob semantics), so an
  unreadable directory would vanish silently instead of surfacing. ``os.walk``'s ``onerror`` hook
  gets the error instead, which becomes a per-account note; a missing directory is not an error
  here (``discover_homes`` already filters those out for the real on-disk layout).
  """
  for dirpath, _, filenames in os.walk(root, onerror=_walk_error_hook(t, source, label, root.name)):
    for name in filenames:
      if name.endswith(".jsonl"):
        yield Path(dirpath) / name


def _iter_jsonl_stats(root: Path, t: _Tally, source: str,
                      label: str) -> Iterator[tuple[str, os.stat_result | None, str | None]]:
  """Yield ``(path, stat, error)`` for every ``*.jsonl`` under *root* — the ``_iter_jsonl``
  walk contract with each file's stat attached, so the signature walk pays one syscall per
  file instead of one per file plus a re-stat. Like ``os.walk``: symlinked directories are
  neither descended nor listed; a stat failure yields ``(path, None, repr(exc))``."""
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


def _reset_aggregate_memo() -> None:
  """Drop the collection's process-wide memos (test isolation)."""
  global _aggregate_memo, _tally_memo
  _aggregate_memo = None
  _tally_memo = None
  _opencode_row_memos.clear()
  _opencode_row_epochs.clear()
  _opencode_partials.clear()


def _claude_file_contribution(path: Path) -> tuple[dict, int]:
  """Parse one Claude Code jsonl into its cache entry; return (entry, bytes read)."""
  seen: set[str] = set()
  dupes = 0
  records: list[list] = []
  nbytes = 0
  # The signature is taken before the read: a concurrent append mid-read then necessarily
  # outdates the stored sig and the next lookup re-scans, so a partial or extended read can
  # never be served later as if complete.
  st = path.stat()
  with path.open(errors="replace") as fh:
    for line in fh:
      nbytes += len(line)
      if '"usage"' not in line:
        continue
      try:
        rec = json.loads(line)
      except ValueError:
        continue
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
              usage.get("input_tokens", 0) or 0,
              usage.get("cache_creation_input_tokens", 0) or 0,
              usage.get("cache_read_input_tokens", 0) or 0,
              usage.get("output_tokens", 0) or 0
          ])
  return {"sig": [st.st_mtime_ns, st.st_size], "records": records, "dupes": dupes}, nbytes


def collect_claude(t: _Tally, homes: dict[str, Path], cache: TallyCache | None) -> None:
  seen: set[str] = set()
  dupes = 0
  for account, home in homes.items():
    for path in _iter_jsonl(home / "projects", t, "Claude Code", account):
      entry = cache.lookup("claude", path) if cache is not None else None
      if entry is None:
        try:
          entry, nbytes = _claude_file_contribution(path)
        except OSError as exc:
          t.notes.append(f"Claude Code: unreadable {account}/{path.name}: {exc}")
          continue
        t.scanned_bytes += nbytes
        if cache is not None:
          cache.store("claude", path, entry)
      dupes += entry["dupes"]
      for key, model, ts, in_fresh, cache_write, cache_read, output in entry["records"]:
        if key in seen:
          dupes += 1
          continue
        seen.add(key)
        t.add(
            "Claude Code",
            model,
            account,
            ts,
            in_fresh=in_fresh,
            cache_write=cache_write,
            cache_read=cache_read,
            output=output)
  t.notes.append(
      f"Claude Code: {len(seen):,} unique API responses over {len(homes)} config dirs, "
      f"{dupes:,} replayed lines skipped")


def _codex_file_contribution(path: Path) -> tuple[dict, int]:
  """Parse one Codex rollout jsonl into its cache entry; return (entry, bytes read)."""
  # Every record the tally reads (session_meta, turn_context, token_count) serializes
  # its type as a quoted literal in the raw line, so the substring filter cannot skip a
  # record the full parse would see; it only skips parsing irrelevant lines.
  recs: list[dict] = []
  nbytes = 0
  # The signature is taken before the read: a concurrent append mid-read then necessarily
  # outdates the stored sig and the next lookup re-scans, so a partial or extended read can
  # never be served later as if complete.
  st = path.stat()
  with path.open(errors="replace") as fh:
    for line in fh:
      nbytes += len(line)
      if not any(m in line for m in ('"session_meta"', '"turn_context"', '"token_count"')):
        continue
      try:
        recs.append(json.loads(line))
      except ValueError:
        continue
  meta = next((rec for rec in recs if rec.get("type") == "session_meta"), None)
  mp = (meta or {}).get("payload") or {}
  source = json.dumps(mp.get("source") or {})
  is_root = not (mp.get("forked_from_id") or mp.get("parent_thread_id") or "subagent" in source)
  model = next(
      (
          (rec.get("payload") or {}).get("model")
          for rec in recs
          if rec.get("type") in ("session_meta", "turn_context") and (rec.get("payload") or {}).get("model")),
      None,
  )
  walked, final_total = 0, 0
  records: list[list] = []
  for rec in recs:
    payload = rec.get("payload") or {}
    if rec.get("type") in ("session_meta", "turn_context"):
      model = payload.get("model") or model
      continue
    if rec.get("type") != "event_msg" or payload.get("type") != "token_count":
      continue
    info = payload.get("info") or {}
    last, total = info.get("last_token_usage") or {}, info.get("total_token_usage") or {}
    final_total = max(final_total, total.get("total_tokens", 0) or 0)
    cached = last.get("cached_input_tokens", 0) or 0
    fresh = (last.get("input_tokens", 0) or 0) - cached
    out = last.get("output_tokens", 0) or 0
    walked += cached + fresh + out
    records.append([model or "unknown", rec.get("timestamp"), fresh, cached, out])
  check = [walked, final_total] if final_total and is_root else None
  return {"sig": [st.st_mtime_ns, st.st_size], "records": records, "check": check}, nbytes


def collect_codex(t: _Tally, homes: dict[str, Path], cache: TallyCache | None) -> None:
  check: list[tuple[int, int]] = []
  for account, home in homes.items():
    for path in _iter_jsonl(home / "sessions", t, "Codex", account):
      entry = cache.lookup("codex", path) if cache is not None else None
      if entry is None:
        try:
          entry, nbytes = _codex_file_contribution(path)
        except OSError as exc:
          t.notes.append(f"Codex: unreadable {account}/{path.name}: {exc}")
          continue
        t.scanned_bytes += nbytes
        if cache is not None:
          cache.store("codex", path, entry)
      for model, ts, fresh, cached, out in entry["records"]:
        t.add("Codex", model, account, ts, in_fresh=fresh, cache_read=cached, output=out)
      if entry["check"] is not None:
        check.append(tuple(entry["check"]))
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


def _advance_opencode_rows(db: Path) -> _OpencodeScan:
  """Advance the db's row memo to its message table's current rows, bumping the epoch when any
  row moved. Read-only: the scan never writes. Absent or unreadable dbs advance nothing and
  return ``ok=False``."""
  key = str(db)
  sig = _opencode_db_signature(db)
  try:
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
      memo = _opencode_row_memos.setdefault(key, {})
      nbytes, deltas = _scan_opencode_rows(con, memo)
    finally:
      con.close()
  except sqlite3.Error as exc:
    # A failed scan leaves its memo partially advanced at worst; dropping the partial forces
    # the next merge down the full replay, which rebuilds both from whatever the memo holds.
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
  the source's buckets instead of replaying every record; the partial always ends the merge
  matching the row memo it sums."""
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
    # be served while the file still matches it. The row memo and its partial track the db
    # through scans, not through the document, so neither is touched here.
    records = entry["records"]
    epoch = _opencode_row_epochs.get(key, 0)
    _replay_opencode_records(t, records)
    t.notes.append(f"opencode: {len(records):,} assistant messages with token counts")
    return sig, epoch, from_scan
  if scan is None:
    scan = _advance_opencode_rows(db)
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
  if cache is not None and sig is not None:
    records = [rec for _, rec in memo.values() if rec is not None]
    cache.store("opencode", db, {"sig": sig, "records": records})
  # The stored partial must never alias a served tally's containers, so it copies out.
  _opencode_partials[key] = _OpencodePartial(
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
  away, and feed the result into *t*. Returns the contributing-record count."""
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
  cache = TallyCache.load(cache_path, t.notes) if fresh_sources and cache_path is not None else None
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
