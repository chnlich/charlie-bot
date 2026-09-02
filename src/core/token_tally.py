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
identical token values, so first-wins cannot move a sum. The opencode db stays outside the
memo: its WAL sidecar moves under plain serve traffic, so its entry always round-trips the
persisted document.
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
import sqlite3
import time
from collections import defaultdict
from dataclasses import dataclass, field
from pathlib import Path
from typing import NamedTuple

from src.core.config import get_config
from src.core.json_utils import write_json_atomically

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
  by_account: defaultdict = field(
      default_factory=lambda: defaultdict(lambda: dict.fromkeys(FIELDS, 0)))
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
        by_model={k: dict(v) for k, v in t.by_model.items()},
        by_account={k: dict(v) for k, v in t.by_account.items()},
        span={k: tuple(v) for k, v in t.span.items()},
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
    if opt.type == "cc-claude" and opt.claude_config_dir:
      claude.add(Path(opt.claude_config_dir).expanduser())
    elif opt.type == "codex" and opt.codex_home:
      codex.add(Path(opt.codex_home).expanduser())

  claude_map = {_account_label(p, ".claude"): p for p in sorted(claude) if (p / "projects").is_dir()}
  codex_map = {_account_label(p, ".codex"): p for p in sorted(codex) if (p / "sessions").is_dir()}
  return claude_map, codex_map


def _account_label(path: Path, stem: str) -> str:
  """Derive an account label from a dir name: strip a leading '.' and the provider prefix.

  The provider default dir is labelled ``work (default)``; a custom dir ``.claude-ext-1`` reads
  as ``ext-1``. Mirrors the derivation in ``src/api/ext_usage.py`` without importing it (core must
  not depend on the api layer).
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


def _iter_jsonl(root: Path, t: _Tally, source: str, label: str):
  """Yield ``*.jsonl`` under *root*, recording a note when a directory is unreadable.

  ``Path.rglob`` swallows ``PermissionError`` while walking (shell-glob semantics), so an
  unreadable directory would vanish silently instead of surfacing. ``os.walk``'s ``onerror`` hook
  gets the error instead, which becomes a per-account note; a missing directory is not an error
  here (``discover_homes`` already filters those out for the real on-disk layout).
  """

  def _onerror(exc: OSError) -> None:
    if not isinstance(exc, FileNotFoundError):
      t.notes.append(f"{source}: unreadable {label}/{root.name}: {exc}")

  for dirpath, _, filenames in os.walk(root, onerror=_onerror):
    for name in filenames:
      if name.endswith(".jsonl"):
        yield Path(dirpath) / name


def _corpus_signature(claude_homes: dict[str, Path], codex_homes: dict[str, Path]) -> tuple:
  """Walk signature of the Claude+Codex corpus: home pairs, every jsonl's stat pair, and the
  walk's own error strings. Any corpus or permission move changes the tuple."""
  sig = []
  for source, homes, sub in (("Claude Code", claude_homes, "projects"),
                             ("Codex", codex_homes, "sessions")):
    probe = _Tally()
    entries = []
    for label, home in homes.items():
      per_home = []
      for path in _iter_jsonl(home / sub, probe, source, label):
        try:
          st = path.stat()
          per_home.append((str(path), st.st_mtime_ns, st.st_size))
        except OSError as exc:
          per_home.append((str(path), None, repr(exc)))
      entries.append((label, tuple(sorted(per_home))))
    home_pairs = tuple(sorted((label, str(path)) for label, path in homes.items()))
    sig.append((source, home_pairs, tuple(entries), tuple(probe.notes)))
  return tuple(sig)


# The aggregate memo pair (walk signature, partial); see the module docstring.
_aggregate_memo: tuple[tuple, _SourceAggregate] | None = None


def _reset_aggregate_memo() -> None:
  """Drop the aggregate memo (test isolation)."""
  global _aggregate_memo
  _aggregate_memo = None


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
      records.append([
          key, model, rec.get("timestamp"),
          usage.get("input_tokens", 0) or 0,
          usage.get("cache_creation_input_tokens", 0) or 0,
          usage.get("cache_read_input_tokens", 0) or 0,
          usage.get("output_tokens", 0) or 0])
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
            "Claude Code", model, account, ts,
            in_fresh=in_fresh, cache_write=cache_write, cache_read=cache_read, output=output)
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
          if rec.get("type") in ("session_meta", "turn_context")
          and (rec.get("payload") or {}).get("model")
      ),
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


# The scan projects each matching row's tally fields inside SQLite: one json_extract parse
# per row there beats fetching every matching row's whole data blob and parsing in Python
# (~0.5 s vs ~0.8 s over this host's 27.9k-row message table). json_valid keeps the old
# json.loads failure mode: the LIKE prefilter can match a malformed row, and skipping it
# must not error the query. Non-object tokens project NULLs, dropped by the filter below.
_OPENCODE_SCAN_SQL = """
select json_extract(data, '$.modelID'), json_extract(data, '$.providerID'),
       json_extract(data, '$.time.created'),
       json_extract(data, '$.tokens.input'), json_extract(data, '$.tokens.output'),
       json_extract(data, '$.tokens.total'),
       json_extract(data, '$.tokens.cache.write'), json_extract(data, '$.tokens.cache.read'),
       length(data)
from message
where data like '%"tokens"%'
  and json_valid(data)
  and json_extract(data, '$.role') = 'assistant'
"""


def _opencode_row(row: tuple) -> list | None:
  """Tally record for one projected message row, or None when it contributes nothing."""
  model, provider, created, in_fresh, output, total, cache_write, cache_read = row
  if not (in_fresh or output or total):
    return None
  model = model or "unknown"
  if model.startswith("/"):
    model = f"{Path(model).name} ({provider})"
  ts = dt.datetime.fromtimestamp(created / 1000, dt.UTC).isoformat() if isinstance(
      created, (int, float)) else None
  return [model, provider or "unknown", ts,
          in_fresh or 0, cache_write or 0, cache_read or 0, output or 0]


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


def collect_opencode(t: _Tally, db: Path, cache: TallyCache | None) -> None:
  if not db.exists():
    t.notes.append("opencode: db absent")
    return
  sig = _opencode_db_signature(db)
  entry = cache.lookup_sig("opencode", str(db), sig) if cache is not None and sig is not None else None
  if entry is None:
    # The signature is taken before the read: a concurrent write mid-scan then necessarily
    # outdates the stored sig and the next lookup re-scans, so a partial or extended read can
    # never be served later as if complete.
    try:
      con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    except sqlite3.Error as exc:
      t.notes.append(f"opencode: unreadable db: {exc}")
      return
    records: list[list] = []
    nbytes = 0
    try:
      for row in con.execute(_OPENCODE_SCAN_SQL):
        nbytes += row[8]
        rec = _opencode_row(row[:8])
        if rec is not None:
          records.append(rec)
    finally:
      con.close()
    t.scanned_bytes += nbytes
    if cache is not None and sig is not None:
      cache.store("opencode", db, {"sig": sig, "records": records})
  else:
    records = entry["records"]
  for model, account, ts, in_fresh, cache_write, cache_read, output in records:
    t.add(
        "opencode", model, account, ts,
        in_fresh=in_fresh, cache_write=cache_write, cache_read=cache_read, output=output)
  t.notes.append(f"opencode: {len(records):,} assistant messages with token counts")


def _build(t: _Tally) -> list[dict]:
  rows: list[dict] = []
  for (source, model), c in t.by_model.items():
    lo, hi = t.span[(source, model)]
    accts: dict[str, dict] = {
        a: dict(v) for (s, m, a), v in t.by_account.items() if s == source and m == model}
    total = c["in_fresh"] + c["cache_write"] + c["cache_read"] + c["output"]
    acct_rows = [
        AccountRow(
            name=a,
            calls=v["calls"],
            output=v["output"],
            total=v["in_fresh"] + v["cache_write"] + v["cache_read"] + v["output"],
        )
        for a, v in sorted(accts.items(), key=lambda kv: -(
            kv[1]["in_fresh"] + kv[1]["cache_write"] + kv[1]["cache_read"] + kv[1]["output"]))
    ]
    rows.append({
        "source": source, "model": model, **{k: c[k] for k in FIELDS},
        "total": total, "first": (lo or "")[:10], "last": (hi or "")[:10], "accounts": acct_rows,
    })
  rows.sort(key=lambda r: -r["total"])
  return rows


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
  t = _Tally()
  if claude_homes is None or codex_homes is None:
    discovered_claude, discovered_codex = discover_homes(DEFAULT_CLAUDE_DIR, DEFAULT_CODEX_HOME)
    claude_homes = claude_homes if claude_homes is not None else discovered_claude
    codex_homes = codex_homes if codex_homes is not None else discovered_codex
  if opencode_db is None:
    opencode_db = DEFAULT_OPENCODE_DB

  global _aggregate_memo
  signature = _corpus_signature(claude_homes, codex_homes)
  cache = TallyCache.load(cache_path, t.notes) if cache_path is not None else None
  memo = _aggregate_memo
  fresh_sources = memo is None or memo[0] != signature
  if fresh_sources:
    notes_from = len(t.notes)
    collect_claude(t, claude_homes, cache)
    collect_codex(t, codex_homes, cache)
    _aggregate_memo = (signature, _SourceAggregate.snapshot(t, notes_from))
  else:
    memo[1].apply(t)
  collect_opencode(t, opencode_db, cache)
  # Save only on the source-walk path: its lookups refreshed the next document. A memo hit's
  # only fresh entry is the opencode db's, whose WAL sig the next load recomputes anyway.
  if cache is not None and fresh_sources:
    try:
      cache.save(cache_path)
    except OSError as exc:
      t.notes.append(f"Tally cache: save failed: {exc}")
  rows = _build(t)
  elapsed = time.perf_counter() - start
  return TokenTally(
      rows=[ModelRow(**row) for row in rows],
      notes=t.notes,
      elapsed_s=elapsed,
      scanned_bytes=t.scanned_bytes,
  )
