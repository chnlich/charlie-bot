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
"""

from __future__ import annotations

import datetime as dt
import json
import sqlite3
import time
from collections import defaultdict
from dataclasses import dataclass, field
from pathlib import Path

from src.core.config import get_config

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


def _iter_jsonl(root: Path, t: _Tally, source: str, label: str):
  """Yield ``*.jsonl`` under *root*, recording a note when the walk itself fails.

  A directory made unreadable mid-traversal surfaces as an ``OSError`` from ``is_dir()`` during
  ``rglob``; wrapping the generator here turns that into a per-account note instead of bubbling out
  of ``collect_token_usage``.
  """
  try:
    yield from root.rglob("*.jsonl")
  except OSError as exc:
    t.notes.append(f"{source}: unreadable {label}/{root.name}: {exc}")


def collect_claude(t: _Tally, homes: dict[str, Path]) -> None:
  seen: set[str] = set()
  dupes = 0
  for account, home in homes.items():
    for path in _iter_jsonl(home / "projects", t, "Claude Code", account):
      try:
        fh = path.open(errors="replace")
      except OSError as exc:
        t.notes.append(f"Claude Code: unreadable {account}/{path.name}: {exc}")
        continue
      with fh:
        for line in fh:
          t.scanned_bytes += len(line)
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
          t.add(
              "Claude Code", model, account, rec.get("timestamp"),
              in_fresh=usage.get("input_tokens", 0) or 0,
              cache_write=usage.get("cache_creation_input_tokens", 0) or 0,
              cache_read=usage.get("cache_read_input_tokens", 0) or 0,
              output=usage.get("output_tokens", 0) or 0,
          )
  t.notes.append(
      f"Claude Code: {len(seen):,} unique API responses over {len(homes)} config dirs, "
      f"{dupes:,} replayed lines skipped")


def collect_codex(t: _Tally, homes: dict[str, Path]) -> None:
  check: list[tuple[int, int]] = []
  for account, home in homes.items():
    for path in _iter_jsonl(home / "sessions", t, "Codex", account):
      try:
        recs: list[dict] = []
        for line in path.open(errors="replace"):
          t.scanned_bytes += len(line)
          try:
            recs.append(json.loads(line))
          except ValueError:
            continue
      except OSError as exc:
        t.notes.append(f"Codex: unreadable {account}/{path.name}: {exc}")
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
        t.add(
            "Codex", model or "unknown", account, rec.get("timestamp"),
            in_fresh=fresh, cache_read=cached, output=out)
      if final_total and is_root:
        check.append((walked, final_total))
  if check:
    w = sum(x for x, _ in check)
    f = sum(y for _, y in check)
    t.notes.append(
        f"Codex: per-request sum {w:,} vs session totals {f:,} ({(w - f) / f * 100:+.2f}% over "
        f"{len(check)} root sessions; /compact resets a session total, so the per-request sum leads)")


def collect_opencode(t: _Tally, db: Path) -> None:
  if not db.exists():
    t.notes.append("opencode: db absent")
    return
  try:
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
  except sqlite3.Error as exc:
    t.notes.append(f"opencode: unreadable db: {exc}")
    return
  n = 0
  try:
    for (raw,) in con.execute("select data from message where data like '%\"tokens\"%'"):
      t.scanned_bytes += len(raw)
      try:
        d = json.loads(raw)
      except ValueError:
        continue
      tok = d.get("tokens")
      if not isinstance(tok, dict) or d.get("role") != "assistant":
        continue
      if not any(tok.get(key) for key in ("input", "output", "total")):
        continue
      created = (d.get("time") or {}).get("created")
      ts = dt.datetime.fromtimestamp(created / 1000, dt.UTC).isoformat() if isinstance(
          created, (int, float)) else None
      model = d.get("modelID") or "unknown"
      if model.startswith("/"):
        model = f"{Path(model).name} ({d.get('providerID')})"
      cache = tok.get("cache") or {}
      t.add(
          "opencode", model, d.get("providerID") or "unknown", ts,
          in_fresh=tok.get("input", 0) or 0,
          cache_write=cache.get("write", 0) or 0,
          cache_read=cache.get("read", 0) or 0,
          output=tok.get("output", 0) or 0)
      n += 1
  finally:
    con.close()
  t.notes.append(f"opencode: {n:,} assistant messages with token counts")


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
) -> TokenTally:
  """Read-only collection; persists nothing. A failing source records a note instead of raising.

  Roots default to this host's on-disk layout: data is discovered from config.yaml plus the
  defaults ``~/.claude``, ``~/.codex`` and the opencode database. Tests pass explicit roots.
  """
  start = time.perf_counter()
  t = _Tally()
  if claude_homes is None or codex_homes is None:
    discovered_claude, discovered_codex = discover_homes(DEFAULT_CLAUDE_DIR, DEFAULT_CODEX_HOME)
    claude_homes = claude_homes if claude_homes is not None else discovered_claude
    codex_homes = codex_homes if codex_homes is not None else discovered_codex
  if opencode_db is None:
    opencode_db = DEFAULT_OPENCODE_DB

  collect_claude(t, claude_homes)
  collect_codex(t, codex_homes)
  collect_opencode(t, opencode_db)
  rows = _build(t)
  elapsed = time.perf_counter() - start
  return TokenTally(
      rows=[ModelRow(**row) for row in rows],
      notes=t.notes,
      elapsed_s=elapsed,
      scanned_bytes=t.scanned_bytes,
  )