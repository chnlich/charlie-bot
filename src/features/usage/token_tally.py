"""Copy every agent log on this host into the usage ledger's records.

This module is the ledger's writing side (src/features/usage/usage_ledger.py is the reading and
aggregation side): it turns every usage-bearing log line into a UsageRecord and hands the records to
the ledger, whose rows outlive the logs they were parsed from. The entry points are ``capture_local``
(this host's own sources, the call the cron handler, the cold-storage sweep and the CLI share) and
``capture_usage`` (the same with the host and the session tree named by the caller). A capture runs
in the collector, so every read or parse failure raises instead of noting and continuing. The
usage page reads the ledger alone and shows when the last capture finished.

Sources, all local logs (no vendor usage API is called):
  every registered usage source with a module (src/runtime/hooks/usage_source_registration.py): the module's
      ``logs()`` lists the files and its ``read()`` turns one file into records, so this module
      names no backend;
  charlie-bot  the sessions tree: thread result events (``threads/*/data/events.jsonl``),
               master-run captures (``data/master_runs/*/agent.raw.ndjson``) and Run captures
               (``data/runs/*/agent.raw.ndjson``). CharlieBot's own writers define these formats,
               so they stay parsed here.

Signature first: the ledger's ``captured_files`` stores one signature per file. A file whose
signature is its stat pair is skipped, without a read, while the stored signature equals the
current pair. Every other file calls its source's ``read()``, which takes the new signature before
it reads, so an append during the read leaves the stored signature outdated and the next capture
reads the file again. A thread or master log signs ``p2:<mtime_ns>:<size>`` and a Run log
``<mtime_ns>:<size>``.

Record ids of the charlie-bot source: the ledger upserts on them, so a re-parsed file re-writes what
it stored before.
  thread:<session>/<thread>/<i>  per thread result event
  master:<session>/<run>         per master-run capture's trailing result
  run:<run id>                   per Run capture, at most one
The records of the other sources spell their ids in their own modules.

The fallback rule: the charlie-bot source and the CLI sources describe overlapping runs, and a
thread's or run's CLI log can disappear (history pruned) after the capture. The backend id of a
thread or run names its usage source: a registered id through its backend type's attribution, an
id that left config through the source's id prefix. A backend with no source contributes nothing.
  NATIVE     a source with ``run_logs_only``: usage only the charlie-bot log holds (CLC thread
             results, master-run captures, a CLC Run's trailing result line). No sessions;
             nothing can restate it.
  FALLBACK   every other source: the CLI's own log may still exist. The record carries the session
             ids its log names, and the ledger's any-match exclusion retires it the moment any of
             those ids has a NATIVE record of its own. A candidate with no ids cannot key that
             exclusion and contributes nothing.

The Codex rule applies to a source whose implementation module defines ``live_cli_sessions``:
  - its backends log cached reads inside ``input_tokens`` and again in ``cache_read_input_tokens``
    (a Run's ``turn.completed`` lines carry them as ``cached_input_tokens``), so the fresh input is
    the difference and the reads count once;
  - a thread or run is admitted only when none of its session ids names a session whose own log
    still exists (``live_cli_sessions()``). Any match skips the whole candidate, which keeps a
    partially pruned multi-session thread from being counted twice.
"""

from __future__ import annotations

import enum
import os
import pathlib
import socket
from collections.abc import Iterator

import orjson

from src.features import usage
from src.features.usage import usage_ledger
from src.infra import config, ndjson
from src.infra import event_types as ET
from src.runtime import runs
from src.runtime.hooks import usage_source_registration, usage_sources

# Account label for a master-run capture whose context model matches no run_logs_only backend
# in config.yaml (a retired backend's master runs, or an ad-hoc model).
_CLC_MASTER_ACCOUNT = "clc-master"


class _Verdict(enum.Enum):
  """How one charlie-bot thread or run is read, from its backend's usage source (the module docstring)."""

  NATIVE = "native"
  CODEX_RULE = "codex_rule"
  FALLBACK = "fallback"


def backend_registry() -> dict[str, object]:
  """config.yaml's backend options by id — the map the capture (a backend added or retired
  reclassifies a moved file on its next parse) and the usage page's account attribution
  both read. Re-read per call."""
  return {opt.id: opt for opt in config.get_config().backends.options}


def backend_source(backend: str, registry: dict) -> usage_source_registration.UsageSource | None:
  """The usage source one backend id attributes to, or None when it has none.

  A registered id reads its config type; an id that left config reads the id prefix the id rule
  (BackendsConfig in src/infra/config.py) keeps on every id.
  """
  opt = registry.get(backend)
  if opt is not None:
    return usage_source_registration.source_for(str(opt.type))
  return next((s for s in usage_source_registration.sources() if backend.startswith(s.id_prefixes)), None)


def backend_page_source(backend: str, registry: dict) -> str:
  """One charlie-bot record's account id attributed to the usage source of the CLI that ran the
  call, the card the usage page counts it under. ``clc-master`` is the master-run account for a
  context model matching no registered run_logs_only backend. Every other type or id with no
  source raises: a backend with no collection rule must not land silently under a wrong CLI.
  """
  if backend == _CLC_MASTER_ACCOUNT:
    source = next((s for s in usage_source_registration.sources() if s.run_logs_only), None)
  else:
    source = backend_source(backend, registry)
  if source is None:
    raise ValueError(f"{backend}: neither a registered backend type nor a known type prefix has a usage source")
  return source.name


def _run_logs_only(option: object) -> bool:
  """Whether a backend option's usage lives only in CharlieBot's own logs."""
  source = usage_source_registration.source_for(str(option.type))
  return source is not None and source.run_logs_only


def _verdict(source: usage_source_registration.UsageSource) -> _Verdict:
  if source.run_logs_only:
    return _Verdict.NATIVE
  if source.module is not None and hasattr(usage_sources.implementation(source), "live_cli_sessions"):
    return _Verdict.CODEX_RULE
  return _Verdict.FALLBACK


def _backend_verdict(backend: str, registry: dict) -> tuple[_Verdict, usage_source_registration.UsageSource] | None:
  """The verdict and source of one backend id, or None when the id has no usage source."""
  source = backend_source(backend, registry)
  return None if source is None else (_verdict(source), source)


class _LiveSessions:
  """The live CLI session ids per source, read once on first use: a capture asks only when a
  thread or run of a Codex-rule source needs the answer."""

  def __init__(self) -> None:
    self._by_source: dict[str, set[str]] = {}

  def of(self, source: usage_source_registration.UsageSource) -> set[str]:
    if source.name not in self._by_source:
      self._by_source[source.name] = usage_sources.implementation(source).live_cli_sessions()
    return self._by_source[source.name]


def _admitted(
    verdict: _Verdict, source: usage_source_registration.UsageSource, ids: set[str], live: _LiveSessions) -> bool:
  """Whether a thread or run with these session ids contributes records.

  A NATIVE one always does. A FALLBACK one needs at least one id to key the any-match exclusion,
  and under the Codex rule no id may name a live CLI session.
  """
  if verdict is _Verdict.NATIVE:
    return True
  if not ids:
    return False
  return verdict is _Verdict.FALLBACK or not ids & live.of(source)


def _bare_model(model: str) -> str:
  """The row name for a backend config model: the last path segment ("openai/zai-org/
  GLM-5.3-Flash" -> "GLM-5.3-Flash"). Page rows carry the bare model name."""
  return model.rsplit("/", 1)[-1]


def _thread_row_model(meta: dict, registry: dict) -> str:
  """The row name for one thread: the model the thread's metadata recorded (bare), else the
  backend's config model (bare) while the id is registered, else the id minus its type
  prefix ("codex-gpt-5.6-sol-personal" -> "gpt-5.6-sol-personal"), which only a retired id
  whose thread recorded no model reaches. The recorded model leads because an id names a
  family and keeps its name across version bumps (the id rule on BackendsConfig in
  src/infra/config.py): the config model is today's version, and reading it first would
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
  prefix = next((p for s in usage_source_registration.sources() for p in s.id_prefixes if backend.startswith(p)), "")
  return backend.removeprefix(prefix)


# ---------------------------------------------------------------------------
# charlie-bot source: this host's own thread event logs, master and Run raw captures
# ---------------------------------------------------------------------------

# Every record the parser reads (thread result events, the bare session-id event, the
# claude-style init envelope, master-run context/result lines) serializes its type as a
# quoted literal with a space after the colon — charlie-bot writes json.dumps defaults —
# so the substring filter cannot skip a record the full parse would see; it only skips
# parsing irrelevant lines. The claude CLI's own stream (no space) never matches.
_THREAD_MARKERS = (b'"type": "result"', b'"session_id"')
_MASTER_MARKERS = (b'"type": "context"', b'"type": "result"')

# The Run raw stream's prefilter markers: the CLC worker's json.dumps spelling
# ("type": "result", with the space its writer defaults to), the claude CLI's compact
# spelling ("type":"result" — its result line carries no type key until deep into the
# line), the Codex stream's turn.completed and thread.started, and the session_id the
# claude stream's init and result lines carry at top level. A line is parsed only when it
# can contribute usage or an exclusion key.
_RUN_MARKERS = (
    b'"type": "result"',
    b'"type":"result"',
    b'"turn.completed"',
    b'"session_id"',
    b'"thread.started"',
)

# The walk's per-kind (kind, container under the session dir, candidate file name). Candidate
# and container paths are entry.path (os.scandir's absolute form, never ending in the separator)
# plus one separator plus a relative constant: the walk builds ~19k of them per capture, and
# concatenation replaces os.path.join's case analysis.
_SEP = os.sep
_LOG_KINDS = (
    ("thread", runs.THREADS_DIR_NAME, os.path.join(runs.DATA_DIR_NAME, runs.EVENTS_LOG_NAME)),
    ("master", os.path.join(runs.DATA_DIR_NAME, runs.MASTER_RUNS_DIR_NAME), runs.RAW_LOG_NAME),
)


def _subdirs(dirpath: str) -> list[str]:
  """The directory's subdirectory paths, symlinks not entered; a missing directory raises FileNotFoundError."""
  with os.scandir(dirpath) as entries:
    return [entry.path for entry in entries if entry.is_dir() and not entry.is_symlink()]


def _iter_charliebot_logs(sessions: str) -> Iterator[tuple[str, str, os.stat_result]]:
  """Yield ``(kind, path, stat)`` over every session directory's thread event logs (kind
  ``"thread"``) and master raw captures (kind ``"master"``).

  Both file names are pinned by their writers — a thread's event log is always ``events.jsonl``
  under its ``data/`` (threads.thread_events_log_path) and a run's capture always
  ``agent.raw.ndjson`` (runs.RAW_LOG_NAME) — so the walk lists only the three levels whose
  entries it must discover (the sessions root, each ``threads/``, each ``data/master_runs/``)
  and stats each candidate file directly. A missing sessions root, session subtree or candidate
  is an empty corpus, not an error; any other failure raises.
  """
  try:
    session_dirs = _subdirs(sessions)
  except FileNotFoundError:
    return
  for session_dir in session_dirs:
    for kind, container, name in _LOG_KINDS:
      try:
        entries = _subdirs(session_dir + _SEP + container)
      except FileNotFoundError:
        continue
      for entry in entries:
        path = entry + _SEP + name
        try:
          st = os.stat(path)
        except FileNotFoundError:
          continue
        yield kind, path, st


def _iter_run_logs(sessions: str) -> Iterator[tuple[str, os.stat_result]]:
  """Yield every Run raw capture's ``(path, stat)``: ``<session>/data/runs/<run id>/agent.raw.ndjson``.

  A session without a ``data/runs`` directory — most of them, the sessions tree predating Run
  records — is empty, and a run directory that launched no raw log skips. Every other read
  failure raises.
  """
  try:
    session_dirs = _subdirs(sessions)
  except FileNotFoundError:
    return
  for session_dir in session_dirs:
    try:
      run_dirs = _subdirs(_SEP.join((session_dir, runs.DATA_DIR_NAME, runs.RUNS_DIR_NAME)))
    except FileNotFoundError:
      continue
    for run_dir in run_dirs:
      run_log = run_dir + _SEP + runs.RAW_LOG_NAME
      try:
        st = os.stat(run_log)
      except FileNotFoundError:
        continue
      yield run_log, st


def _verdict_counts(usage_payload: dict, verdict: _Verdict) -> tuple[int, int, int, int, int]:
  """A thread result row's (in_fresh, cache_write, cache_read, output, in_unsplit), split by the
  thread backend's verdict: the Codex rule subtracts the cached reads from the input; a NATIVE
  backend's result usage logs no cache fields at all, so the split is unknowable and the whole
  input lands in ``in_unsplit``; a plain FALLBACK keeps the Claude envelope's own split."""
  in_fresh, cache_write, cache_read, output = ET.usage_counts(usage_payload)
  if verdict is _Verdict.CODEX_RULE:
    return in_fresh - cache_read, 0, cache_read, output, 0
  if verdict is _Verdict.NATIVE:
    return 0, 0, 0, output, in_fresh
  return in_fresh, cache_write, cache_read, output, 0


def _thread_metadata(path: str) -> dict | None:
  """The thread's ``{backend, model}`` pair from its metadata.json, or None when the file is
  absent. Any other read/parse failure propagates: the capture raises, same contract as an
  unreadable log file."""
  meta_path = pathlib.Path(path).parent.parent / runs.METADATA_NAME
  try:
    with open(meta_path, "rb") as fh:
      meta = orjson.loads(fh.read())
  except FileNotFoundError:
    return None
  if not isinstance(meta, dict):
    raise ValueError(f"{meta_path}: metadata is not an object")
  return {"backend": meta.get("backend"), "model": meta.get("model")}


def _thread_records(path: str, registry: dict, live: _LiveSessions) -> list[usage_sources.UsageRecord]:
  """One thread event log's records; the capture records the file even when this is empty, so an
  empty thread is never re-parsed.

  Each result event folds into one record, its usage counts split by the backend's verdict. The
  session ids come off every other line carrying one at top level: the codex translation emits
  one session-adopt event per thread.started (the typed ``session_attached`` signal, or its bare
  pre-typed spelling in older logs), and the claude-style init envelope embeds its own. A thread
  with no metadata.json, or whose backend id has no usage source, contributes no records.
  """
  meta = _thread_metadata(path)
  if meta is None or not meta["backend"]:
    return []
  backend = meta["backend"]
  attributed = _backend_verdict(backend, registry)
  if attributed is None:
    return []
  verdict, source = attributed
  results: list[dict] = []
  ids: set[str] = set()
  for line in ndjson.parse_marker_lines(path, _THREAD_MARKERS):
    if line.get("type") == ET.RESULT:
      results.append(line)
    elif isinstance(line.get("session_id"), str) and line["session_id"]:
      ids.add(line["session_id"])
  if not _admitted(verdict, source, ids, live):
    return []
  native = verdict is _Verdict.NATIVE
  model = _thread_row_model(meta, registry)
  parts = pathlib.Path(path).parts
  records = []
  for i, result in enumerate(results):
    in_fresh, cache_write, cache_read, output, in_unsplit = _verdict_counts(result.get("usage") or {}, verdict)
    records.append(
        usage_sources.UsageRecord(
            record_id=f"thread:{parts[-5]}/{parts[-3]}/{i}",
            kind=usage_sources.RecordKind.NATIVE if native else usage_sources.RecordKind.FALLBACK,
            source=usage.CHARLIE_BOT_SOURCE,
            model=model,
            account=backend,
            ts=result.get("timestamp") or "",
            in_fresh=in_fresh,
            cache_write=cache_write,
            cache_read=cache_read,
            output=output,
            in_unsplit=in_unsplit,
            sessions=() if native else tuple(sorted(ids))))
  return records


def _master_records(path: str, registry: dict) -> list[usage_sources.UsageRecord]:
  """The master raw capture's records: one per capture at most, from its trailing result event.

  A CLC-shaped capture self-identifies with ``type: context`` lines that carry the model; its
  trailing ``type: result`` line carries the run's usage with the cached reads inside the one
  input field (``cached_tokens``), which split out like a Run record's do. The timestamp is the
  master_runs directory name, the run's recorded start time: stable across re-parses, unlike the
  file mtime a growing log keeps moving. A capture without any context model (the claude CLI's
  own stream, already covered by its source) or without a trailing result (a run killed
  mid-turn) contributes nothing.
  """
  model = None
  last = None
  for line in ndjson.parse_marker_lines(path, _MASTER_MARKERS):
    if line.get("type") == "context" and line.get("model"):
      model = line["model"]
    elif line.get("type") == ET.RESULT:
      last = line
  if model is None or last is None:
    return []
  usage_payload = last.get("usage") or {}
  input_tokens = usage_payload.get(ET.USAGE_INPUT_TOKENS, 0) or 0
  cached = usage_payload.get("cached_tokens", 0) or 0
  opt = next((o for o in registry.values() if _run_logs_only(o) and o.model == model), None)
  parts = pathlib.Path(path).parts
  return [
      usage_sources.UsageRecord(
          record_id=f"master:{parts[-5]}/{parts[-2]}",
          kind=usage_sources.RecordKind.NATIVE,
          source=usage.CHARLIE_BOT_SOURCE,
          model=_bare_model(model),
          account=opt.id if opt is not None else _CLC_MASTER_ACCOUNT,
          ts=parts[-2],  # the master_runs/<started_at> directory name
          in_fresh=input_tokens - cached,
          cache_write=0,
          cache_read=cached,
          output=usage_payload.get(ET.USAGE_OUTPUT_TOKENS, 0) or 0)
  ]


def capture_charliebot(
    ledger: usage_ledger.UsageLedger, host: str, sessions_dir: pathlib.Path, captured: dict[str, str]) -> int:
  """Copy the charlie-bot thread event logs and master-run captures into the ledger, so a
  thread's result totals survive deletion of its own event log (see the ledger's module docstring).

  A file whose stat signature the ledger already recorded for this host (``captured``, the
  caller's one captured-sigs read for the whole capture) is skipped; every other file is recorded
  with its signature, also one whose content yields no records. The master capture is one NATIVE
  record with no sessions: a master run has no CLI log behind it. A thread's kind and sessions
  follow its backend's verdict (the module docstring).

  Returns the records written.
  """
  registry = backend_registry()
  live = _LiveSessions()
  written = 0
  for kind, path, st in _iter_charliebot_logs(str(sessions_dir)):
    # The ``p2:`` prefix is the spelling every stored thread and master signature already carries.
    sig = f"p2:{st.st_mtime_ns}:{st.st_size}"
    if captured.get(path) == sig:
      continue
    records = _master_records(path, registry) if kind == "master" else _thread_records(path, registry, live)
    written += ledger.record_file(host, path, sig, records)
  return written


def _run_metadata(log_path: str) -> dict:
  """The metadata.json document beside a Run's raw capture.

  A missing file or a non-object document raises: the Run record is written at
  registration, before any raw line lands, so a capture that cannot read it is a corpus
  bug, not a skippable shape.
  """
  meta_path = pathlib.Path(log_path).parent / runs.RUN_METADATA_NAME
  with open(meta_path, "rb") as fh:
    meta = orjson.loads(fh.read())
  if not isinstance(meta, dict):
    raise ValueError(f"{meta_path}: metadata is not an object")
  return meta


def _last_run_result(objects: list[dict]) -> dict | None:
  """The raw stream's last ``type == "result"`` object, or None when it carries none."""
  return next((obj for obj in reversed(objects) if obj.get("type") == ET.RESULT), None)


def _turn_sums(objects: list[dict]) -> tuple[int, int, int] | None:
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
    usage_payload = obj.get("usage") or {}
    cached = usage_payload.get("cached_input_tokens", 0) or 0
    in_fresh += (usage_payload.get("input_tokens", 0) or 0) - cached
    cache_read += cached
    output += usage_payload.get("output_tokens", 0) or 0
  return (in_fresh, cache_read, output) if seen else None


def _run_session_ids(objects: list[dict], meta: dict) -> set[str]:
  """A fallback run's session ids: metadata's native_session_id (absent on many runs), every
  string top-level ``session_id`` in the raw stream (the claude stream's init and result lines),
  and every ``thread.started`` object's ``thread_id`` (the Codex stream). These key the ledger's
  any-match exclusion, so the fallback retires the moment any of the run's CLI logs is captured."""
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
  return ids


def _run_record(
    objects: list[dict], meta: dict, registry: dict, backend: str, run_id: str,
    live: _LiveSessions) -> usage_sources.UsageRecord | None:
  """The Run directory's one ledger record, or None when nothing is storable.

  The backend verdict picks the usage arm:
    NATIVE      the raw stream's trailing result line, native — the run log is the only home the
      usage has. Its input arrives with the cached reads included in one field
      (``cached_tokens``), so in_fresh subtracts them back out.
    CODEX_RULE  the turn.completed lines' summed usage, fallback.
    FALLBACK    the trailing result line through the Claude envelope keys (``ET.usage_counts``),
      fallback — the CLI's own transcript restates it while it exists.
  A backend with no usage source, a run with no usage line of its verdict's shape, and a
  fallback run that is not admitted (see ``_admitted``) contribute nothing — the file still
  records as captured, so it is never re-parsed.
  """
  attributed = _backend_verdict(backend, registry)
  if attributed is None:
    return None
  verdict, source = attributed
  sessions: tuple[str, ...] = ()
  if verdict is _Verdict.NATIVE:
    last = _last_run_result(objects)
    if last is None:
      return None
    usage_payload = last.get("usage") or {}
    input_tokens = usage_payload.get("input_tokens", 0) or 0
    cached = usage_payload.get("cached_tokens", 0) or 0
    in_fresh, cache_write, cache_read = input_tokens - cached, 0, cached
    output = usage_payload.get("output_tokens", 0) or 0
    kind = usage_sources.RecordKind.NATIVE
  else:
    if verdict is _Verdict.CODEX_RULE:
      sums = _turn_sums(objects)
      if sums is None:
        return None
      in_fresh, cache_read, output = sums
      cache_write = 0
    else:
      last = _last_run_result(objects)
      if last is None:
        return None
      in_fresh, cache_write, cache_read, output = ET.usage_counts(last.get("usage") or {})
    ids = _run_session_ids(objects, meta)
    if not _admitted(verdict, source, ids, live):
      return None
    sessions = tuple(sorted(ids))
    kind = usage_sources.RecordKind.FALLBACK
  return usage_sources.UsageRecord(
      record_id=f"run:{run_id}",
      kind=kind,
      source=usage.CHARLIE_BOT_SOURCE,
      model=_thread_row_model({
          "backend": backend,
          "model": meta.get("model")
      }, registry),
      account=backend,
      ts=meta.get("started_at") or "",
      in_fresh=in_fresh,
      cache_write=cache_write,
      cache_read=cache_read,
      output=output,
      sessions=sessions)


def capture_runs(
    ledger: usage_ledger.UsageLedger, host: str, sessions_dir: pathlib.Path, captured: dict[str, str]) -> int:
  """Copy the Run directories' raw captures into the ledger, so a run's totals survive deletion
  of its own raw log (see the ledger's module docstring).

  The corpus is ``<session>/data/runs/<run id>/agent.raw.ndjson`` — the execution record the
  Run workers write. One record per run at most, deduped on the run id; the backend verdict in
  the run's metadata picks the usage arm (see ``_run_record``). A file whose
  ``<mtime_ns>:<size>`` pair ``captured`` already holds for this host is skipped outright — no
  log read, no metadata.json — so an unchanged corpus pays one stat per run. Any other file
  parses and records its signature, also one whose content yields no record, so a result-less
  run is never re-parsed. A missing or unreadable metadata.json raises.

  Returns the records written.
  """
  registry = backend_registry()
  live = _LiveSessions()
  written = 0
  for path, st in _iter_run_logs(str(sessions_dir)):
    sig = f"{st.st_mtime_ns}:{st.st_size}"
    if captured.get(path) == sig:
      continue
    objects = ndjson.parse_marker_lines(path, _RUN_MARKERS)
    meta = _run_metadata(path)
    backend = meta.get("backend")
    record = _run_record(objects, meta, registry, backend, pathlib.Path(path).parent.name, live) if backend else None
    written += ledger.record_file(host, path, sig, [] if record is None else [record])
  return written


# ---------------------------------------------------------------------------
# the capture entry points
# ---------------------------------------------------------------------------


def _capture_source(
    ledger: usage_ledger.UsageLedger, host: str, source: usage_source_registration.UsageSource,
    captured: dict[str, str]) -> int:
  """Copy one registered source's logs into the ledger; returns the records written.

  A file whose signature is its stat pair is skipped while the stored signature equals the
  current pair. Every other file reads through the source's ``read()``, which also decides alone
  when a signature that is not a stat pair has moved; a read that returns the stored signature
  with no records changes nothing and writes nothing.
  """
  implementation = usage_sources.implementation(source)
  written = 0
  for path, account in implementation.logs():
    key = str(path)
    previous = captured.get(key)
    st = os.stat(path)
    if previous == f"{st.st_mtime_ns}:{st.st_size}":
      continue
    sig, records = implementation.read(path, account, previous)
    if sig == previous and not records:
      continue
    written += ledger.record_file(host, key, sig, records)
  return written


def capture_usage(ledger: usage_ledger.UsageLedger, *, host: str, sessions_dir: pathlib.Path) -> dict[str, int]:
  """Capture every registered source and the charlie-bot logs under *sessions_dir* into the
  ledger — the one entry point the cron handler, the cold-storage sweep and the CLI share (see
  the ledger's module docstring), and the one a harness points at a scratch ledger.

  The capture is one commit (``UsageLedger.batch``), each file atomic inside it, so a capture that
  raises leaves the files it finished recorded. A capture that returns stamps ``last_capture_at``
  for *host* in that commit; every error raises.

  Returns the records written per source name; the charlie-bot label sums the thread, master
  and Run captures, whose records carry that source.
  """
  if not usage_source_registration.sources():
    raise RuntimeError("no usage source is registered: the entry point must call registrations.register_all()")
  captured = ledger.captured_sigs(host)
  written: dict[str, int] = {}
  with ledger.batch():
    for source in usage_source_registration.sources():
      if source.module is not None:
        written[source.name] = _capture_source(ledger, host, source, captured)
    written[usage.CHARLIE_BOT_SOURCE] = (
        capture_charliebot(ledger, host, sessions_dir, captured) + capture_runs(ledger, host, sessions_dir, captured))
    ledger.mark_capture_finished(host)
  return written


def capture_local(ledger: usage_ledger.UsageLedger) -> dict[str, int]:
  """Capture this host's own sources with this host's defaults: each registered source's own
  logs and the config's session tree."""
  return capture_usage(ledger, host=socket.gethostname(), sessions_dir=config.get_config().sessions_dir)
