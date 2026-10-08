"""Codex's usage log, read for the "Codex" usage source (src/runtime/hooks/usage_sources.py).

Codex writes one rollout per session at ``~/.codex/sessions/**/*.jsonl``. A ``session_meta`` line
opens the file, ``turn_context`` lines carry the model in force, and ``token_count`` events carry
the per-request usage (``last_token_usage``) and the running totals (``total_token_usage``). The
wire names are the CODEX_* constants of codex_usage.py. Codex runs from its default home alone.

Record id: ``codex-total:<root session id>:<total input>:<total cached>:<total output>``, one per
``token_count`` event. The root is the rollout's first ``session_meta`` session id, the id a whole
fork tree shares. A forked rollout's copy of its parent's events, and a re-written event (verbatim,
or with the per-request usage zeroed), therefore land on the original event's id and count once,
while the record keeps that event's ``last_token_usage``. The record's session is the rollout's
own id: the last five dash-separated segments of ``rollout-*.jsonl``, or the path relative to the
home for any other file name.
"""

import os
import pathlib
from collections.abc import Iterator

from src.backends.codex import USAGE_SOURCE, codex_usage
from src.infra import ndjson
from src.runtime.hooks import usage_sources

_MARKERS = tuple(
    f'"{name}"'.encode()
    for name in (codex_usage.CODEX_SESSION_META, codex_usage.CODEX_TURN_CONTEXT, codex_usage.CODEX_TOKEN_COUNT))

# Codex has one account: its default home.
_ACCOUNT = "work (default)"


def logs() -> Iterator[tuple[pathlib.Path, str]]:
  """Every rollout under the default Codex home, with its account label."""
  sessions = codex_usage.default_codex_home() / "sessions"
  if sessions.is_dir():
    for path in ndjson.iter_jsonl_files(str(sessions)):
      yield pathlib.Path(path), _ACCOUNT


def _session_id(path: pathlib.Path) -> str:
  """The session a rollout file registers: see the module docstring."""
  name = path.name
  if name.startswith("rollout-") and name.endswith(".jsonl"):
    return "-".join(name[len("rollout-"):-len(".jsonl")].rsplit("-", 5)[-5:])
  return os.path.relpath(path, codex_usage.default_codex_home())


def live_cli_sessions() -> set[str]:
  """The ids of the sessions whose rollout is still on disk."""
  return {_session_id(path) for path, _ in logs()}


def _is_context(line: dict) -> bool:
  return line.get("type") in (codex_usage.CODEX_SESSION_META, codex_usage.CODEX_TURN_CONTEXT)


def read(path: pathlib.Path, account: str, previous: str | None) -> tuple[str, list[usage_sources.UsageRecord]]:
  """One rollout's records and its ``<mtime_ns>:<size>`` signature, taken before the read.

  A ``token_count`` event yields no record while its info is null or its ``last_token_usage`` is
  all zero: those events restate the previous one, and a record for one would overwrite the real
  record it copies. A ``token_count`` event with no ``session_meta`` session id above it, or a
  record-bearing one without ``total_token_usage``, raises. The ``session_meta`` and
  ``turn_context`` lines set the model in force, and the first model the file declares also
  covers a ``token_count`` event that precedes it.
  """
  st = os.stat(path)
  lines = ndjson.parse_marker_lines(path, _MARKERS)
  model = next(
      (
          (line.get("payload") or {}).get("model")
          for line in lines
          if _is_context(line) and (line.get("payload") or {}).get("model")), None)
  root = next(
      (
          (line.get("payload") or {}).get("session_id")
          for line in lines
          if line.get("type") == codex_usage.CODEX_SESSION_META), None)
  session = _session_id(path)
  records: dict[str, usage_sources.UsageRecord] = {}
  for line in lines:
    if _is_context(line):
      model = (line.get("payload") or {}).get("model") or model
      if root is None and line.get("type") == codex_usage.CODEX_SESSION_META:
        root = (line.get("payload") or {}).get("session_id")
      continue
    payload = codex_usage.codex_token_count_payload(line)
    if payload is None:
      continue
    if root is None:
      raise ValueError(f"{path}: token_count event with no session_meta session_id above it")
    info = payload.get("info") or {}
    last = info.get("last_token_usage") or {}
    cached = last.get("cached_input_tokens", 0) or 0
    fresh = (last.get("input_tokens", 0) or 0) - cached
    output = last.get("output_tokens", 0) or 0
    if fresh == 0 and cached == 0 and output == 0:
      continue
    total = info.get("total_token_usage")
    if not isinstance(total, dict):
      raise ValueError(f"{path}: token_count event without total_token_usage")
    total_in = total.get("input_tokens", 0) or 0
    total_cached = total.get("cached_input_tokens", 0) or 0
    total_out = total.get("output_tokens", 0) or 0
    # Within one file only the first record per id is kept.
    record_id = f"codex-total:{root}:{total_in}:{total_cached}:{total_out}"
    if record_id in records:
      continue
    records[record_id] = usage_sources.UsageRecord(
        record_id=record_id,
        kind=usage_sources.RecordKind.NATIVE,
        source=USAGE_SOURCE,
        model=model or "unknown",
        account=account,
        ts=line.get("timestamp") or "",
        in_fresh=fresh,
        cache_write=0,
        cache_read=cached,
        output=output,
        sessions=(session,))
  return f"{st.st_mtime_ns}:{st.st_size}", list(records.values())


def quota_accounts() -> list[usage_sources.QuotaAccount]:
  """Codex's account on the quota panel; the quota module loads on the first call."""
  from src.backends.codex import usage_quota

  return usage_quota.quota_accounts()


def sweep(scope: usage_sources.SweepScope) -> usage_sources.SourceSweep:
  """Codex's part of the cold-storage sweep; the sweep module loads on the first call."""
  from src.backends.codex import usage_sweep

  return usage_sweep.sweep(scope)
