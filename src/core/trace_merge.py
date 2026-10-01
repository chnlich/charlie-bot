"""Streaming merge support for Chrome-format JSON traces."""

import collections
import concurrent.futures
import contextlib
import fcntl
import mmap
import re
import shutil
import subprocess
import sys
import tempfile
from collections.abc import Iterator
from pathlib import Path
from typing import BinaryIO

import orjson

from src.core.direct_pass_child import _MIN_CHUNK_BYTES, _chunk_parse_input, _split_chunks
from src.core.gc_control import gc_off
from src.core.log_once import LazyStructlogLogger

log = LazyStructlogLogger()

# Compression level for the merged gzip output. Measured on a 191.2 MB /
# 496,099-event input: level 1 builds in 0.57 s / 15.5 MB against level 6's
# 1.73 s / 11.6 MB. The viewer fetches the whole artifact, so the build wall is
# the user-visible cost; big-payload transport gzip is level 1 for the same reason.
_MERGE_COMPRESSLEVEL = 1

# Events per orjson.dumps call on the merge output. The encoder's per-element text
# is context-free, so a batch's bracket-stripped rendering is byte-identical to the
# per-event form; batching cut the serializer pass from 3.0 s to 1.7 s on the input
# above. Every append site (walk body, thread_name insert, trailing process_
# metadata) checks this bound, so no batch exceeds it.
_MERGE_BATCH_EVENTS = 512

# Sentinel for the walk's single-probe tid read: `event.get("tid", sentinel)` costs
# one dict probe where `in` + indexing costs two. JSON can never produce this object.
_NO_TID = object()

# The flow phases whose id the walk remaps; the scan records the same events' ids.
_FLOW_PHASES = frozenset({"s", "t", "f"})

# Id space one member of a multi-trace merge may allocate: the member's
# sequencers start at 1 + file_index * this stride, so parallel members never
# collide, and the walk fails loudly at the bound (the largest observed trace
# carries 1.07M events — 15x headroom). The sequential form shares one counter
# across traces and never checks a bound.
_MERGE_MEMBER_ID_STRIDE = 1 << 24

# One member build's peak RSS, priced in file bytes: the parse holds the raw bytes beside the
# object tree it finishes (a 1.42 GB / 3.5M-event member peaked at 10.1 GB RSS, 7.1x its bytes),
# and the walk mutates in place. The member wave budgets every concurrent build at this factor
# against the cgroup's free bytes, so the pool cannot OOM its own worker on a big member.
_MERGE_MEMBER_RSS_FACTOR = 8

# The merge walk's stdin pipe capacity. The default 64 KB pipe blocks every batch
# flush until the compressor drains it — measured +0.3-0.6 s per worst-corpus build
# — while 1 MB (this kernel's pipe-max-size) holds several batches, so a flush
# completes without waiting and the compress overlaps the GIL-bound walk.
_MERGE_PIPE_BYTES = 1 << 20

# Chunk helpers parsing one trace's member concurrently; each holds its chunk's
# bytes beside a share of the tree, so the wave stays near half the whole-file peak.
_MERGE_CHUNK_WAVE = 4


class NotATraceError(ValueError):
  """JSON that parses cleanly but carries no Chrome-JSON ``traceEvents`` array.

  The direct-pass validator and the sequential merge fail the build on it; the
  multi-trace merge classifies discovered members with it and skips the file.
  """


def _trace_events_or_raise(trace: object, path: Path) -> list[dict]:
  """Return a Chrome trace's event list; raise on JSON that parses but is not a trace.

  A Chrome-JSON trace is a bare event array or an object carrying a ``traceEvents``
  array (profiler exports keep metadata siblings such as ``deviceProperties`` beside
  it). Any other JSON object — an analysis manifest, a config dump — parses cleanly
  yet carries zero events, so accepting it would merge silence into the output and
  ship an empty artifact.
  """
  if isinstance(trace, list):
    return trace
  if isinstance(trace, dict) and isinstance(trace.get("traceEvents"), list):
    return trace["traceEvents"]
  raise NotATraceError(f"Not a Chrome-JSON trace (no traceEvents array): {path}")


def _gzip_exit_or_raise(gzip_proc: subprocess.Popen, context: str) -> None:
  """Reap the compressor run's exit; a nonzero exit raises with the stderr the run wrote."""
  if gzip_proc.wait() != 0:
    detail = gzip_proc.stderr.read().decode(errors="replace").strip()
    raise RuntimeError(f"isal.igzip -{_MERGE_COMPRESSLEVEL} failed ({context}): {detail}")


def _kill_gzip_run(gzip_proc: subprocess.Popen) -> None:
  """Kill an abandoned compressor run and reap it; kill alone leaves a zombie."""
  gzip_proc.kill()
  gzip_proc.wait()


def igzip_command(*extra_args: str) -> list[str]:
  """The merge family's one compressor invocation: the isal igzip CLI at the merge level.

  Level-1 ISA-L reads ~800 MB/s against gzip's ~200 MB/s on this host's trace
  JSON, and a caller's producer must keep pace with it or the compressor
  becomes the wall. ``sys.executable`` is the process's own interpreter, whose
  site has the declared isal dependency. ``-n`` zeroes the gzip header's name
  and mtime fields — the CLI stamps the wall clock into a stdin-fed stream
  otherwise, so without the flag the artifact bytes are not deterministic run
  to run.
  """
  return [sys.executable, "-m", "isal.igzip", f"-{_MERGE_COMPRESSLEVEL}", "-n", *extra_args]


def _rank_label(path: Path) -> str:
  match = re.search(r"rank(\d+)", path.name, flags=re.IGNORECASE)
  if match:
    return f"rank{match.group(1)}"
  return re.sub(r"\.json$", "", path.name, flags=re.IGNORECASE)


class _EventBatcher:
  """Serializes merged events into the output stream in batches.

  The walk appends to one pending list and hands it to :meth:`flush` at the
  batch bound and once at the end. The stream carries `e1,e2,...` with no
  brackets of its own; a batch's list rendering minus its outer brackets is
  exactly that fragment, so batch boundaries are byte-invisible.
  """

  def __init__(self, output: BinaryIO) -> None:
    self._output = output
    self._emitted_any = False
    self.emitted = 0

  def flush(self, pending: list[dict]) -> None:
    if not pending:
      return
    self.emitted += len(pending)
    if self._emitted_any:
      self._output.write(b",")
    self._emitted_any = True
    # orjson's compact rendering parses to the same trace the stdlib encoder
    # produced; non-ASCII rides raw UTF-8 where the stdlib form emitted \uXXXX.
    # The parse pass rejects the NaN/Infinity literals stdlib json.load accepts,
    # so a trace carrying them fails the build; an in-memory non-finite float
    # (unreachable from a trace file) would render as null here.
    self._output.write(orjson.dumps(pending)[1:-1])
    pending.clear()


class _IdSequencer:
  """Maps each original trace id to a dense sequential int, starting at ``start``.

  One instance spans the whole sequential merge; the key map resets per trace,
  because the same original tid in two ranks is two different threads, while
  the ints keep counting so no two threads or flows ever collide. The member
  form gives each member its own instance inside its id stride.
  """

  def __init__(self, start: int) -> None:
    self._next_id = start
    self._seen: dict[str, int] = {}

  def start_trace(self) -> None:
    """Drop the key map before a new trace's events; the int counter continues."""
    self._seen.clear()

  @property
  def seen(self) -> dict[str, int]:
    """The live key→id map (`start_trace` clears it); the walk probes it inline."""
    return self._seen

  def __call__(self, original: object) -> int:
    key = str(original)
    mapped = self._seen.get(key)
    if mapped is None:
      mapped = self._next_id
      self._seen[key] = mapped
      self._next_id += 1
    return mapped


def _parse_trace_document(path: Path) -> object:
  """The trace's parsed document, read through a mapped view instead of a bytes copy.

  The parse peak holds the raw bytes beside the object tree they become, and
  on a 1.42 GB member that bytes copy alone pushed one pool worker past this
  host's 12 GiB session cgroup. Mapped pages are file-backed and reclaimable,
  so the kernel drops them under pressure instead of OOM-killing the build.
  An empty file keeps the plain-bytes error shape (mmap cannot map one).
  """
  with path.open("rb") as trace_file:
    trace_file.seek(0, 2)
    if trace_file.tell() == 0:
      return orjson.loads(b"")
    trace_file.seek(0)
    with mmap.mmap(trace_file.fileno(), 0, access=mmap.ACCESS_READ) as mapped:
      return orjson.loads(memoryview(mapped))


def _thread_name_event(pid: object, tid: int, raw: object, rank_label: str) -> dict:
  return {"ph": "M", "pid": pid, "tid": tid, "name": "thread_name", "args": {"name": f"{rank_label}/{raw}"}}


def _admit(raw: object, table: dict[str, int], ordered: list[object]) -> None:
  key = str(raw)
  if key not in table:
    table[key] = len(table) + 1
    ordered.append(raw)


def _collect_pid_labels(events: list[dict]) -> tuple[dict[str, str], dict[str, object]]:
  """pid_labels keys stay str(pid) (int 7 and "7" are one pid, last-wins); raw_pids keeps one form per key."""
  pid_labels: dict[str, str] = {}
  raw_pids: dict[str, object] = {}
  for event in events:
    if event.get("ph") == "M" and event.get("name") == "process_labels" and event.get("args"):
      pid = event.get("pid")
      key = str(pid)
      pid_labels[key] = event["args"].get("labels") or ""
      raw_pids.setdefault(key, pid)
  return pid_labels, raw_pids


def _walk_drops(event: dict, ph: object, slim: bool) -> bool:
  """The walk's drop rule; the scan shares it — a sight the scan records but the
  walk drops shifts every later synthetic id off the sequential values."""
  if ph == "M":
    name = event.get("name")
    if name and name.startswith("process_"):
      return True
  return slim and event.get("cat") == "cpu_instant_event"


def _merge_one_trace(
    trace: object,
    path: Path,
    file_index: int,
    batcher: _EventBatcher,
    tid_seq: _IdSequencer,
    flow_seq: _IdSequencer,
    slim: bool,
    labels_in: tuple[dict[str, str], dict[str, object]] | None = None,
    marked_tids: set[object] | None = None,
    emit_tail: bool = True,
) -> None:
  """Walk one parsed trace's events into *batcher*, remapped to synthetic ids.

  The caller parses and owns the per-trace sequencer resets. The keywords carry
  the chunked build's pre-computed state: *labels_in* replaces the process_labels
  collection (a chunk rarely holds every label event), *marked_tids* names the raw
  tid forms whose ``thread_name`` this call emits, *emit_tail* gates the trailing
  process_ metadata (only the last chunk emits it)."""
  events = _trace_events_or_raise(trace, path)
  rank_label = _rank_label(path)

  pid_labels, raw_pids = labels_in if labels_in is not None else _collect_pid_labels(events)

  pid_map: dict[object, str] = {}
  synthetic_meta: dict[str, tuple[int, str]] = {}
  base_sort_index = file_index * 10000
  for key, label in pid_labels.items():
    synthetic_pid = rank_label if label == "CPU" else f"{rank_label}/{label}"
    pid_map[key] = synthetic_pid
    pid_map[raw_pids[key]] = synthetic_pid
    if label == "CPU":
      synthetic_meta[synthetic_pid] = (base_sort_index, f"{rank_label} CPU")
    else:
      gpu_match = re.search(r"GPU\s*(\d+)", label, flags=re.IGNORECASE)
      gpu_index = int(gpu_match.group(1)) if gpu_match else 0
      synthetic_meta[synthetic_pid] = (base_sort_index + 1000 + gpu_index, f"{rank_label} {label}")

  batcher_flush = batcher.flush
  pid_map_get = pid_map.get
  tid_map_get = tid_seq.seen.get
  # tid_raw_map carries one raw value per key so the walk's per-event probe
  # skips the str() the sequencer's str-canonical map needs; on a raw miss the
  # str-keyed lookup still answers, so int 7 and "7" stay one thread (whichever
  # form arrived first allocated, the other rides its str key).
  tid_raw_map: dict[object, int] = {}
  tid_raw_get = tid_raw_map.get
  _str = str
  # The batch append rides the walk loop as plain list ops: one bound method call
  # per event over a 1M-event corpus measured ~0.2 s of the build wall.
  pending: list[dict] = []
  pending_append = pending.append
  batch_bound = _MERGE_BATCH_EVENTS

  for event in events:
    ph = event.get("ph")
    if _walk_drops(event, ph, slim):
      continue
    if slim and isinstance(event.get("args"), dict):
      if "stream" in event["args"]:
        event["args"] = {"stream": event["args"]["stream"]}
      else:
        event.pop("args")

    pid = event.get("pid")
    synthetic_pid = pid_map_get(pid)
    if synthetic_pid is None:
      synthetic_pid = pid_map_get(_str(pid), rank_label)
    event["pid"] = synthetic_pid
    original_tid = event.get("tid", _NO_TID)
    if original_tid is not _NO_TID:
      synthetic_tid = tid_raw_get(original_tid)
      if synthetic_tid is None:
        # First sight of this raw form: the str-canonical map decides whether
        # the thread itself is new (allocate + thread_name) or already seen
        # under another raw form; the raw map records the resolved id either
        # way so later events of this form probe raw only.
        thread_key = _str(original_tid)
        synthetic_tid = tid_map_get(thread_key)
        if synthetic_tid is None:
          # First sight: the sequencer allocates and inserts; the thread_name
          # rides the same first sight instead of a second per-event set probe.
          synthetic_tid = tid_seq(original_tid)
          pending_append(_thread_name_event(event["pid"], synthetic_tid, original_tid, rank_label))
          if len(pending) >= batch_bound:
            batcher_flush(pending)
        tid_raw_map[original_tid] = synthetic_tid
      event["tid"] = synthetic_tid
      if marked_tids is not None and original_tid in marked_tids:
        # The pre-loaded map never misses: this is the chunked walk's only
        # thread_name emitter, once per form, at the sequential first-sight spot.
        marked_tids.discard(original_tid)
        pending_append(_thread_name_event(event["pid"], synthetic_tid, original_tid, rank_label))
        if len(pending) >= batch_bound:
          batcher_flush(pending)
    if ph in _FLOW_PHASES and "id" in event:
      event["id"] = flow_seq(event["id"])
    pending_append(event)
    if len(pending) >= batch_bound:
      batcher_flush(pending)

  if not emit_tail:
    batcher_flush(pending)
    return
  meta_tid = tid_seq("meta")
  for synthetic_pid, (sort_index, label) in synthetic_meta.items():
    for name, args in (
        ("process_name", {"name": label}),
        ("process_labels", {"labels": label}),
        ("process_sort_index", {"sort_index": sort_index}),
    ):
      pending_append({"ph": "M", "pid": synthetic_pid, "tid": meta_tid, "name": name, "args": args})
      if len(pending) >= batch_bound:
        batcher_flush(pending)
  batcher_flush(pending)


@contextlib.contextmanager
def _gzip_output_stream(out_path: Path) -> Iterator[BinaryIO]:
  """Yield the stdin of an igzip run (igzip_command) compressing into ``out_path``.

  The compress must leave the process to overlap the GIL-bound walk; the pipe
  capacity is _MERGE_PIPE_BYTES so a flush completes without blocking. A writer
  failure kills the run and a nonzero wait raises with the compressor stderr.
  The multi-trace merge's ordered fragment stream must keep pace with each
  4-member wave or the stream becomes the wall.
  """
  with out_path.open("wb") as compressed, subprocess.Popen(igzip_command(), stdin=subprocess.PIPE, stdout=compressed,
                                                           stderr=subprocess.PIPE) as gzip_proc:
    output = gzip_proc.stdin
    fcntl.fcntl(output.fileno(), fcntl.F_SETPIPE_SZ, _MERGE_PIPE_BYTES)
    try:
      yield output
      output.close()
      _gzip_exit_or_raise(gzip_proc, "trace merge")
    except BaseException:
      # __exit__ closes stdin again; a killed child makes that flush raise EPIPE,
      # so close the write end here first (idempotent once closed) or the writer's
      # own error would be replaced by it.
      with contextlib.suppress(BrokenPipeError):
        output.close()
      _kill_gzip_run(gzip_proc)
      raise


def merge_traces(paths: list[Path], out_path: Path, slim: bool) -> None:
  """Merge Chrome JSON traces into one gzip-compressed Chrome trace."""
  # A build allocates ~1M dicts per 500k input events and mutates every one;
  # the generational passes over that churn measured 0.3-0.6 s per 1.07M-event
  # build. The build runs in its own fresh process (the route's lean child,
  # trace_merge_child) or a pool worker that runs nothing else, so the disable
  # is scoped to this build; collect reclaims the build's cyclic leftovers so
  # they never accumulate across builds in a long-lived worker.
  with gc_off(collect=True):
    _merge_all(paths, out_path, slim)


def _merge_all(paths: list[Path], out_path: Path, slim: bool) -> None:
  if len(paths) == 1 and _merge_single_trace_chunked(paths[0], out_path, slim):
    return
  tid_seq, flow_seq = _IdSequencer(1), _IdSequencer(1)
  with _gzip_output_stream(out_path) as output:
    output.write(b'{"traceEvents":[')
    batcher = _EventBatcher(output)
    for file_index, path in enumerate(paths):
      # Each walk flushes its own pending list before returning.
      tid_seq.start_trace()
      flow_seq.start_trace()
      _merge_one_trace(_parse_trace_document(path), path, file_index, batcher, tid_seq, flow_seq, slim)
    output.write(b"]}")


class _ChunkHelperError(RuntimeError):
  """A chunk helper failed; the chunked build answers it with the sequential walk."""


def _merge_single_trace_chunked(path: Path, out_path: Path, slim: bool) -> bool:
  """Build one trace's merged artifact with chunk-parallel helpers; False falls back.

  Each helper parses once, holds its tree, reports its chunk's labels and first-sight
  tid/flow forms, and waits for the parent's id maps on stdin; the wave stagger keeps
  one wave's parse peak beside the held trees. The maps are the sequential walk's exact
  allocation order — byte-identical artifact; a failure falls back."""
  size = path.stat().st_size
  split = _split_chunks(path, size, _MIN_CHUNK_BYTES)
  if split is None:
    return False
  object_form, indent, starts = split
  ends = [*starts, size]
  count = len(ends)
  member_dir = Path(tempfile.mkdtemp(prefix="merge-chunks-"))
  fragments = [member_dir / f"{index}.jsonl" for index in range(count)]
  helpers: list[subprocess.Popen] = []
  try:
    reports = []
    for wave_start in range(0, count, _MERGE_CHUNK_WAVE):
      wave = []
      for index in range(wave_start, min(wave_start + _MERGE_CHUNK_WAVE, count)):
        spec = [
            str(path), 0 if index == 0 else ends[index - 1], ends[index], index, count, object_form,
            indent.decode(), slim, index == count - 1,
            str(fragments[index])
        ]
        argv = [sys.executable, str(Path(__file__).resolve()), "--merge-chunk", orjson.dumps(spec).decode()]
        wave.append(
            subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True))
      helpers.extend(wave)
      for proc in wave:
        line = proc.stdout.readline()
        if not line:
          raise _ChunkHelperError(f"chunk helper rc={proc.wait()} died before its report")
        reports.append(orjson.loads(line))

    # The id maps the sequential walk would have built; allocation order is its first-sight order.
    pid_labels, raw_pids, tid_map, tid_raw, flow_map, flow_raw = {}, {}, {}, [], {}, []
    prefixes = []
    for report in reports:
      # Deduped: a re-reported form must not inflate the prefix the marked rule reads.
      prefixes.append(len(tid_map))
      for key, label, raw in report[0]:
        pid_labels[key] = label
        raw_pids.setdefault(key, raw)
      for raw in report[2]:
        _admit(raw, flow_map, flow_raw)
      for raw in report[1]:
        _admit(raw, tid_map, tid_raw)
    labels = [[key, label, raw_pids[key]] for key, label in pid_labels.items()]
    instruction = orjson.dumps(
        [
            labels, [[raw, tid_map[str(raw)]] for raw in tid_raw], [[raw, flow_map[str(raw)]] for raw in flow_raw],
            prefixes
        ]).decode()
    for proc in helpers:
      proc.stdin.write(instruction + "\n")
      proc.stdin.close()
    for proc in helpers:
      if proc.wait() != 0:
        raise _ChunkHelperError(f"chunk helper rc={proc.returncode}: {proc.stderr.read().strip()[:200]}")

    with _gzip_output_stream(out_path) as output:
      output.write(b'{"traceEvents":[')
      emitted = False
      for fragment in fragments:
        if fragment.stat().st_size:
          output.write(b"," if emitted else b"")
          with fragment.open("rb") as fragment_file:
            shutil.copyfileobj(fragment_file, output)
          emitted = True
      output.write(b"]}")
    return True
  except (_ChunkHelperError, OSError, ValueError) as exc:
    # A dead helper leaves no verdict; kill the rest (a helper blocked writing its
    # report would hang a graceful stdin close) and fall back.
    for proc in helpers:
      proc.kill(), proc.wait()
    log.warning("perfetto_merge_chunk_fallback", path=str(path), reason=str(exc)[:300])
    return False
  finally:
    shutil.rmtree(member_dir, ignore_errors=True)


def _merge_chunk_main(argv: list[str]) -> int:
  """One chunk helper: parse and hold its chunk, report, wait for the maps, build.

  The report must cross exactly the stream the build walk crosses; an empty stdin
  read means the parent died and the helper answers nothing."""
  import gc

  path, start, end, index, count, object_form, indent, slim, last, fragment = orjson.loads(argv[0])
  path, fragment_path, indent = Path(path), Path(fragment), indent.encode()
  try:
    gc.disable()
    doc = orjson.loads(_chunk_parse_input(path, start, end, index, count, object_form, indent))
    events = _trace_events_or_raise(doc, path)
  except ValueError as error:  # orjson decode errors and NotATraceError are ValueError subclasses
    print(f"chunk {index} failed to parse: {error}", file=sys.stderr)
    return 1
  pid_labels, raw_pids = _collect_pid_labels(events)
  tids, flows, tid_keys, flow_keys = [], [], {}, {}
  for event in events:
    ph = event.get("ph")
    if _walk_drops(event, ph, slim):
      continue
    raw_tid = event.get("tid", _NO_TID)
    if raw_tid is not _NO_TID:
      _admit(raw_tid, tid_keys, tids)
    if ph in _FLOW_PHASES and "id" in event:
      _admit(event["id"], flow_keys, flows)
  report = [[key, label, raw_pids[key]] for key, label in pid_labels.items()]
  sys.stdout.write(orjson.dumps([report, tids, flows]).decode() + "\n")
  sys.stdout.flush()
  line = sys.stdin.readline()
  if not line:
    print("chunk helper: parent died before the id maps arrived", file=sys.stderr)
    return 3
  labels_in, tid_pairs, flow_pairs, prefixes = orjson.loads(line)
  # Pre-loaded maps carry every id the walk would allocate; an id above the chunk's prefix marks its form.
  pid_labels = {key: label for key, label, _ in labels_in}
  raw_pids = {key: raw for key, _, raw in labels_in}
  tid_seq, flow_seq = _IdSequencer(max((v for _, v in tid_pairs), default=0) + 1), _IdSequencer(1)
  tid_seq._seen = {str(raw): v for raw, v in tid_pairs}
  flow_seq._seen = {str(raw): v for raw, v in flow_pairs}
  with fragment_path.open("wb") as fragment_file:
    _merge_one_trace(
        doc, path, 0, _EventBatcher(fragment_file), tid_seq, flow_seq, slim, (pid_labels, raw_pids),
        {raw for raw in tids if tid_seq._seen[str(raw)] > prefixes[index]}, last)
  return 0


def build_trace_member(path: Path, out_path: Path, file_index: int, slim: bool) -> int:
  """Write one trace's remapped events as an uncompressed comma-joined fragment; return the count.

  ``file_index`` is the trace's position in the merge order and keeps the
  process_sort_index ordering of the whole merge. The sequencers start inside
  the member's id stride so parallel members never collide; exhausting the
  stride fails the build loudly instead of merging the next member's threads.
  Runs in one merge-pool worker per trace — the walk is GIL-bound Python.
  """
  id_start = 1 + file_index * _MERGE_MEMBER_ID_STRIDE
  id_bound = id_start + _MERGE_MEMBER_ID_STRIDE
  with gc_off(collect=True), out_path.open("wb") as output:
    tid_seq = _IdSequencer(id_start)
    flow_seq = _IdSequencer(id_start)
    batcher = _EventBatcher(output)
    trace = _parse_trace_document(path)
    _merge_one_trace(trace, path, file_index, batcher, tid_seq, flow_seq, slim)
    if tid_seq._next_id >= id_bound or flow_seq._next_id >= id_bound:
      raise ValueError(
          f"trace {path} exhausted its member id stride "
          f"({_MERGE_MEMBER_ID_STRIDE} ids); raise _MERGE_MEMBER_ID_STRIDE")
    return batcher.emitted


def _member_outcome(path: Path, fragment: Path, file_index: int, slim: bool) -> int | None:
  """Build one merge member; return its event count, or None for a skipped member.

  The dir shape's ``*.json`` discovery cannot tell an analysis sidecar from a
  trace before the parse (the route's sniff reads the first byte only), so the
  classification happens where the parse runs. A member that parses but is not
  a Chrome-JSON trace skips with a logged warning instead of taking the merged
  view down. Surviving members keep their own file indexes, so a sidecar never
  reorders or relabels the traces around it.
  """
  try:
    return build_trace_member(path, fragment, file_index, slim)
  except NotATraceError as exc:
    log.warning("perfetto_merge_member_skipped", path=str(path), error=str(exc))
    return None


def _merge_memory_budget() -> int | None:
  """The bytes this process's cgroup still grants above its current usage.

  cgroup v2 only: ``/proc/self/cgroup``'s single ``0::`` line names the group
  and ``memory.max``/``memory.current`` sit under the cgroup root. Any miss —
  v1 hosts, a sandbox without the v2 files, macOS — prices nothing and the
  caller submits every member at once (the unbounded shape).
  """
  try:
    group_path = ""
    with open("/proc/self/cgroup", encoding="ascii") as cgroup_file:
      for line in cgroup_file:
        if line.startswith("0::"):
          group_path = line.strip()[3:]
          break
    if not group_path:
      return None
    with open(f"/sys/fs/cgroup{group_path}/memory.max", encoding="ascii") as limit_file:
      limit_text = limit_file.read().strip()
    if limit_text == "max":
      return None
    with open(f"/sys/fs/cgroup{group_path}/memory.current", encoding="ascii") as current_file:
      return int(limit_text) - int(current_file.read())
  except (OSError, ValueError):
    return None


def _member_counts(paths: list[Path], fragments: list[Path], slim: bool,
                   executor: concurrent.futures.Executor) -> Iterator[int | None]:
  """Run the members' builds on ``executor``; yield their counts in merge order.

  The wave size caps concurrently building members so their priced footprints
  (``_MERGE_MEMBER_RSS_FACTOR`` x the largest member's bytes) fit the cgroup's
  free bytes — members too big to coexist build one at a time instead of the
  pool's OOM kill taking the whole merge down. Builds still ride the shared
  pool's own worker cap; a wave larger than it buys nothing.
  """
  if not paths:
    return
  budget = _merge_memory_budget()
  if budget is None:
    wave = len(paths)
  else:
    largest_bytes = max(path.stat().st_size for path in paths)
    wave = min(len(paths), max(1, budget // (_MERGE_MEMBER_RSS_FACTOR * max(largest_bytes, 1))))
  pending: collections.deque[concurrent.futures.Future] = collections.deque()
  submitted = 0
  while submitted < wave:
    pending.append(executor.submit(_member_outcome, paths[submitted], fragments[submitted], submitted, slim))
    submitted += 1
  while pending:
    yield pending.popleft().result()
    if submitted < len(paths):
      pending.append(executor.submit(_member_outcome, paths[submitted], fragments[submitted], submitted, slim))
      submitted += 1


def build_multi_trace_merge(
    paths: list[Path], out_path: Path, slim: bool, executor: concurrent.futures.Executor | None) -> None:
  """Build the multi-trace merged artifact: one pool task per trace, streamed as each completes.

  Each walk runs on ``executor`` (``None`` builds inline, the tests' shape) and
  the single gzip run streams each member's fragment the moment its task
  returns, in merge order — the compress overlaps the members still building.
  The concurrently building members stay inside the memory wave bound
  (:func:`_member_counts`), so a dir of oversized members builds instead of
  OOM-killing a pool worker. A comma precedes a fragment only when an earlier
  one emitted, so empty members stay invisible to the JSON. A member that
  parses but is not a Chrome-JSON trace skips with a logged warning; a merge
  that skips every member raises instead of shipping an empty artifact. Any
  other member failure raises out of the walk order and kills the gzip run;
  the caller owns artifact atomicity.
  """
  member_dir = Path(tempfile.mkdtemp(prefix="merge-members-"))
  try:
    fragments = [member_dir / f"{index}.jsonl" for index in range(len(paths))]
    if executor is None:
      counts: Iterator[int | None] = iter(
          _member_outcome(path, fragment, index, slim)
          for index, (path, fragment) in enumerate(zip(paths, fragments, strict=True)))
    else:
      counts = _member_counts(paths, fragments, slim, executor)
    with _gzip_output_stream(out_path) as output:
      output.write(b'{"traceEvents":[')
      emitted = False
      skipped = 0
      for fragment, count in zip(fragments, counts, strict=True):
        if count is None:
          skipped += 1
          continue
        if not count:
          continue
        if emitted:
          output.write(b",")
        with fragment.open("rb") as fragment_file:
          shutil.copyfileobj(fragment_file, output)
        emitted = True
      output.write(b"]}")
    if paths and skipped == len(paths):
      raise ValueError(
          f"multi-trace merge rejected every member as a non-Chrome-JSON trace: "
          f"{', '.join(str(path) for path in paths)}")
  finally:
    shutil.rmtree(member_dir, ignore_errors=True)


if __name__ == "__main__":
  sys.exit(
      _merge_chunk_main(sys.argv[2:])
      if len(sys.argv) > 1 and sys.argv[1] == "--merge-chunk" else "trace_merge: run the helper with --merge-chunk")
