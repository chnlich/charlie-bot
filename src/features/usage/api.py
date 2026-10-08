"""The /token-usage page: per-model token usage, read from the usage ledger alone.

A capture (src/features/usage/token_tally.py: the cron handler, the cold-storage sweep, ``charliebot
usage-ledger capture``) writes the ledger; loading this page never captures. The page shows the ledger
as of the last capture. Its cards are the registered usage sources (src/runtime/hooks/usage_source_registration.py)
in registration order, followed by the sources whose usage lives only in CharlieBot's own run logs.
"""

import asyncio
import datetime as dt
import json
import re
import socket
import time
from typing import TYPE_CHECKING

from fastapi import APIRouter, Request
from fastapi.responses import HTMLResponse

from src.features.usage import CHARLIE_BOT_SOURCE
from src.infra import log_once, tasks
from src.runtime import templating
from src.runtime.hooks import usage_source_registration, usage_sources, wiring

if TYPE_CHECKING:
  from src.features.usage import usage_ledger

log = log_once.LazyStructlogLogger()

router = APIRouter()


def preload_usage_tally_stack() -> None:
  """Import the tally stack the usage page and ledger handlers first-import.

  The modules mirror the lazy sets in ``_read_ledger``, ``_account_source`` and the scheduler's
  ledger handler, plus each registered source's implementation module; keep them in step. A
  long-lived server that starts before a deploy keeps its in-memory modules while a request-time
  first-import reads the newer files, and the mixed-version import raises inside the request (the
  page 500s until restart). Loading here pins the stack to the code the server started with.
  A failure only logs: the request path keeps its own import as the loud fallback.
  """
  started = time.monotonic()
  import sqlite3  # noqa: F401  -- the pin is the import itself

  from src.features.usage import token_tally, usage_ledger  # noqa: F401
  for source in usage_source_registration.sources():
    if source.module is not None:
      usage_sources.implementation(source)
  log.info("usage_tally_stack_preloaded", duration_ms=round((time.monotonic() - started) * 1000))


_warmup_task: asyncio.Task | None = None


async def start_service(ctx: wiring.ServiceContext) -> None:
  """Preload the tally stack on a worker thread; the server does not wait for it.

  The stack loads at startup, not on the request path: a request-time first-import reads
  whatever files a mid-flight deploy left under a server whose in-memory modules are the
  started code, and the mixed-version import 500s the usage page and the ledger cron handler
  until restart. Same thread pattern as the speech service: the server import floor stays.
  """
  global _warmup_task
  _warmup_task = tasks.create_logged_task(asyncio.to_thread(preload_usage_tally_stack), name="usage-tally-warmup")


async def stop_service() -> None:
  """Cancel the preload and wait for it; returns at once when start_service never ran."""
  global _warmup_task
  task, _warmup_task = _warmup_task, None
  await tasks.cancel_and_wait(task)


def _read_ledger() -> tuple[list[usage_ledger.LedgerRow], dict[str, str], dt.datetime | None, float, dict[str, object]]:
  """Read the page rows and the last capture time from the ledger alone — so the numbers survive
  deletion of the logs they were parsed from — plus the backend registry the rows' charlie-bot
  accounts attribute against.

  Runs in a thread. A read error propagates to the request: the page fails loudly instead of
  rendering rows it could not read.
  """
  # The ledger + tally stack (sqlite3, the token_tally parsers) rides the page like croniter
  # rides its next-run resolutions: the M99 server import floor carries no tally stack for a
  # page that may never load.
  from src.features.usage import token_tally, usage_ledger

  started = time.monotonic()
  with usage_ledger.UsageLedger(usage_ledger.default_ledger_path()) as ledger:
    rows, native_starts = ledger.model_rows_with_native_starts()
    last_capture = ledger.last_capture_at(socket.gethostname())
  return rows, native_starts, last_capture, time.monotonic() - started, token_tally.backend_registry()


def _compact(n: float) -> str:
  """Render a large count compactly: 1.23M, 456K, else a plain comma-formatted number."""
  for cut, suffix in ((1e9, "B"), (1e6, "M"), (1e3, "K")):
    if abs(n) >= cut:
      return f"{n / cut:.2f}".rstrip("0").rstrip(".") + suffix
  return f"{int(n):,}"


_MODEL_LEAF_SUFFIX = re.compile(r"\s*\([^()]*\)$")


def _model_leaf(model: str) -> str:
  """The model name as a reader knows it: the last / segment minus a trailing ' (provider)'
  suffix, case kept — `zai-org/GLM-5.3-Flash` and `Kimi-K3 (amd-kimi-k3)` read as
  GLM-5.3-Flash and Kimi-K3."""
  return _MODEL_LEAF_SUFFIX.sub("", model.rsplit("/", 1)[-1])


def _cards() -> tuple[usage_source_registration.UsageSource, ...]:
  """The usage sources in card order: registration order, then the sources whose usage lives
  only in CharlieBot's own logs. The per-source tiles iterate it, and each row's slot number sent
  to the charts is its position here (from 1). A charlie-bot row's accounts attribute to the CLI
  that ran the call (see ``_account_source``), so the ledger's own charlie-bot spelling never
  reaches the page."""
  registered = usage_source_registration.sources()
  return (*(s for s in registered if not s.run_logs_only), *(s for s in registered if s.run_logs_only))


def _account_source(
    row: usage_ledger.LedgerRow, account: str, registry: dict, cards: tuple[usage_source_registration.UsageSource,
                                                                            ...]) -> tuple[str, bool]:
  """(card name, fallback mark) for one ledger account.

  A row's own source names the CLI whose log its records were read from, so its accounts
  keep it. A charlie-bot row's accounts are backend ids instead, so each attributes to the
  CLI that ran the call (``backend_page_source`` on *registry*): usage native to CharlieBot's
  own logs counts as is, while every other backend's counted records are fallbacks behind
  their CLI's own log — the mark the account's sub-row carries.
  """
  if row.source != CHARLIE_BOT_SOURCE:
    return row.source, False
  # The tally rides the page like the ledger read does (see _read_ledger): imported here so
  # the module stays off the server's import floor.
  from src.features.usage import token_tally

  name = token_tally.backend_page_source(account, registry)
  return name, not next(card for card in cards if card.name == name).run_logs_only


def _merge_ledger_rows(
    rows: list[usage_ledger.LedgerRow], registry: dict, cards: tuple[usage_source_registration.UsageSource,
                                                                     ...]) -> list[dict]:
  """Fold the ledger's per-(source, model) rows into one page row per model.

  Sources spell one model differently — opencode `zai-org/GLM-5.3-Flash`, charlie-bot
  `GLM-5.3-Flash`, opencode path models `Kimi-K3 (amd-kimi-k3)` — so rows group on the
  casefolded leaf name, and versions (`claude-fable-5` vs `claude-fable-5-1`) stay apart.
  The merged row displays the largest part's (by total) spelling, carries one segment per
  attributed source for the stacked charts, and lists one (source · account) sub-row per
  account across the parts; the segments and sub-rows sum to the row. The attributed
  source is the CLI that ran the call (see ``_account_source``), so a charlie-bot row
  splits between CLC and the fallback CLIs its accounts ran on.
  """
  slots = {card.name: slot for slot, card in enumerate(cards, 1)}
  groups: dict[str, list[usage_ledger.LedgerRow]] = {}
  for row in rows:
    groups.setdefault(_model_leaf(row.model).casefold(), []).append(row)
  merged = []
  for parts in groups.values():
    accounts: dict[tuple[str, str, bool], dict[str, int]] = {}
    seg_totals: dict[str, int] = {}
    seg_outputs: dict[str, int] = {}
    for part in parts:
      for account in part.accounts:
        source, fallback = _account_source(part, account.name, registry, cards)
        acc = accounts.setdefault((source, account.name, fallback), {"calls": 0, "output": 0, "total": 0})
        acc["calls"] += account.calls
        acc["output"] += account.output
        acc["total"] += account.total
        seg_totals[source] = seg_totals.get(source, 0) + account.total
        seg_outputs[source] = seg_outputs.get(source, 0) + account.output
    first = min((p.first for p in parts if p.first), default="")
    last = max((p.last for p in parts if p.last), default="")
    ranked = sorted(accounts.items(), key=lambda kv: (-kv[1]["total"], kv[0]))
    merged.append(
        {
            "model": _model_leaf(max(parts, key=lambda p: p.total).model),
            "calls": sum(p.calls for p in parts),
            "in_fresh": sum(p.in_fresh for p in parts),
            "cache_write": sum(p.cache_write for p in parts),
            "cache_read": sum(p.cache_read for p in parts),
            "in_unsplit": sum(p.in_unsplit for p in parts),
            "output": sum(p.output for p in parts),
            "total": sum(p.total for p in parts),
            "fallback_output": sum(p.fallback_output for p in parts),
            "accounts":
                [
                    {
                        "name": f"{source} · {name}{' (fallback)' if fallback else ''}",
                        "calls": acc["calls"],
                        "output": acc["output"],
                        "total": acc["total"]
                    } for (source, name, fallback), acc in ranked
                ],
            "segments":
                [
                    {
                        "slot": slots[card.name],
                        "total": seg_totals[card.name],
                        "output": seg_outputs[card.name],
                    } for card in cards if card.name in seg_totals
                ],
            "window": f"{first} → {last}",
        })
  merged.sort(key=lambda m: (-m["total"], m["model"]))
  return merged


def _as_of(last_capture: dt.datetime | None) -> str:
  """The ledger's age line: the last capture's finish in local time, or that none has run."""
  if last_capture is None:
    return "no capture yet"
  return f"as of {last_capture.astimezone().strftime('%Y-%m-%d %H:%M %Z')}"


def _token_usage_context(
    rows: list[usage_ledger.LedgerRow],
    native_starts: dict[str, str],
    last_capture: dt.datetime | None,
    elapsed_s: float,
    registry: dict,
) -> dict:
  """Prepare the display context for the token_usage template from one ledger read.

  Merges the ledger's rows into one page row per model for the charts, the table and the
  top ranks, while the per-source tiles count attributed accounts (they answer how much
  each CLI ran). Computes the aggregate stats the page renders server-side (hero, tiles,
  conclusions) and the serialized JS payload for the charts and table.
  """
  cards = _cards()
  tot = {
      "in_fresh": sum(r.in_fresh for r in rows),
      "cache_write": sum(r.cache_write for r in rows),
      "cache_read": sum(r.cache_read for r in rows),
      "in_unsplit": sum(r.in_unsplit for r in rows),
      "output": sum(r.output for r in rows),
      "total": sum(r.total for r in rows),
      "calls": sum(r.calls for r in rows),
  }
  merged = _merge_ledger_rows(rows, registry, cards)
  window = (
      (min(r.first for r in rows if r.first),
       max(r.last for r in rows if r.last)) if rows and any(r.first for r in rows) else ("", ""))
  cache_share = tot["cache_read"] / tot["total"] * 100 if tot["total"] else 0.0
  out_share = tot["output"] / tot["total"] if tot["total"] else 0.0
  top = max(merged, key=lambda m: m["total"]) if merged else None
  top_out = max(merged, key=lambda m: m["output"]) if merged else None
  # The tiles count attributed accounts: a charlie-bot row's accounts attribute to the CLI
  # that ran the call, so a model's native usage and its counted fallbacks land under their
  # own sources, and the model count dedupes on the canonical name the merged rows key on.
  sums: dict[str, dict] = {card.name: {"total": 0, "output": 0, "models": set()} for card in cards}
  for row in rows:
    canonical = _model_leaf(row.model).casefold()
    for account in row.accounts:
      source, _fallback = _account_source(row, account.name, registry, cards)
      bucket = sums[source]
      bucket["total"] += account.total
      bucket["output"] += account.output
      bucket["models"].add(canonical)
  per_src: dict[str, dict] = {}
  for card in cards:
    bucket = sums[card.name]
    # The ledger's charlie-bot rows are all usage native to CharlieBot's logs, so a
    # run_logs_only source's native start is the charlie-bot span the ledger keeps.
    ledger_src = CHARLIE_BOT_SOURCE if card.run_logs_only else card.name
    per_src[card.name] = {
        "total": bucket["total"],
        "t_comp": _compact(bucket["total"]),
        "output": bucket["output"],
        "models": len(bucket["models"]),
        "share": bucket["total"] / tot["total"] * 100 if tot["total"] else 0.0,
        "native_start": native_starts.get(ledger_src, ""),
    }
  ctx = {
      "rows": merged,
      "tot_compact": _compact(tot["total"]),
      "in_compact": _compact(tot["in_fresh"] + tot["cache_write"] + tot["cache_read"] + tot["in_unsplit"]),
      "out_compact": _compact(tot["output"]),
      "cr_compact": _compact(tot["cache_read"]),
      "cw_compact": _compact(tot["cache_write"]),
      "fresh_compact": _compact(tot["in_fresh"]),
      "fresh_percent": tot["in_fresh"] / tot["total"] * 100 if tot["total"] else 0.0,
      "out_share": out_share,
      "per_src": per_src,
      "usage_sources": [card.name for card in cards],
      "tot_calls": f"{tot['calls']:,}",
      "top_escaped": top["model"] if top else "",
      "top_compact": _compact(top["total"]) if top else "0",
      "top_out_escaped": top_out["model"] if top_out else "",
      "top_out_compact": _compact(top_out["output"]) if top_out else "0",
      "elapsed_s": elapsed_s,
  }
  return {
      "ctx": ctx,
      "payload": json.dumps({"rows": merged}, ensure_ascii=False),
      "window": window,
      "window_str": f"{window[0]} → {window[1]}" if rows else "",
      "cache_share": cache_share,
      "generated": dt.datetime.now().astimezone().strftime("%Y-%m-%d %H:%M %Z"),
      "as_of": _as_of(last_capture),
  }


@router.get("/token-usage", response_class=HTMLResponse)
async def token_usage_viewer(request: Request) -> HTMLResponse:
  """Render the per-model token usage tally page from the ledger as of the last capture.

  The read runs in a thread pool, never on the event loop, and never captures.
  """
  rows, native_starts, last_capture, elapsed_s, registry = await asyncio.to_thread(_read_ledger)
  return templating.templates().TemplateResponse(
      request,
      "token_usage.html",
      context=_token_usage_context(rows, native_starts, last_capture, elapsed_s, registry),
  )
