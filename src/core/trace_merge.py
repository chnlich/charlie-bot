"""Streaming merge support for Chrome-format JSON traces."""

import concurrent.futures
import contextlib
import fcntl
import re
import shutil
import subprocess
import sys
import tempfile
from collections.abc import Iterator
from pathlib import Path
from typing import BinaryIO

import orjson

from src.core.gc_control import gc_off

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

# Id space one member of a multi-trace merge may allocate: the member's
# sequencers start at 1 + file_index * this stride, so parallel members never
# collide, and the walk fails loudly at the bound (the largest observed trace
# carries 1.07M events — 15x headroom). The sequential form shares one counter
# across traces and never checks a bound.
_MERGE_MEMBER_ID_STRIDE = 1 << 24

# The merge walk's stdin pipe capacity. The default 64 KB pipe blocks every batch
# flush until the compressor drains it — measured +0.3-0.6 s per worst-corpus build
# — while 1 MB (this kernel's pipe-max-size) holds several batches, so a flush
# completes without waiting and the compress overlaps the GIL-bound walk.
_MERGE_PIPE_BYTES = 1 << 20


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
  raise ValueError(f"Not a Chrome-JSON trace (no traceEvents array): {path}")


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


def _merge_one_trace(
    path: Path,
    file_index: int,
    batcher: _EventBatcher,
    tid_seq: _IdSequencer,
    flow_seq: _IdSequencer,
    slim: bool,
) -> None:
  trace = orjson.loads(path.read_bytes())
  events = _trace_events_or_raise(trace, path)
  rank_label = _rank_label(path)
  tid_seq.start_trace()
  flow_seq.start_trace()

  # pid_labels keys stay str(pid) — int 7 and "7" are one pid merged last-wins,
  # the rule the readers always applied — while pid_map also carries one raw
  # value per key, so the walk remaps with one dict probe per event; a
  # str-keyed map paid a str() on every event of a ~500k-event corpus.
  pid_labels: dict[str, str] = {}
  raw_pids: dict[str, object] = {}
  for event in events:
    if event.get("ph") == "M" and event.get("name") == "process_labels" and event.get("args"):
      pid = event.get("pid")
      key = str(pid)
      pid_labels[key] = event["args"].get("labels") or ""
      raw_pids.setdefault(key, pid)

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
    if ph == "M":
      event_name = event.get("name")
      if event_name and event_name.startswith("process_"):
        continue
    if slim and event.get("cat") == "cpu_instant_event":
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
          pending_append(
              {
                  "ph": "M",
                  "pid": event["pid"],
                  "tid": synthetic_tid,
                  "name": "thread_name",
                  "args": {
                      "name": f"{rank_label}/{original_tid}"
                  },
              })
          if len(pending) >= batch_bound:
            batcher_flush(pending)
        tid_raw_map[original_tid] = synthetic_tid
      event["tid"] = synthetic_tid
    if ph in {"s", "t", "f"} and "id" in event:
      event["id"] = flow_seq(event["id"])
    pending_append(event)
    if len(pending) >= batch_bound:
      batcher_flush(pending)

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
  # build. The build runs inside the merge process pool (spawn context, whose
  # workers run nothing else), so the disable is scoped to this build; collect
  # reclaims the build's cyclic leftovers so they never accumulate across
  # builds in a long-lived worker.
  with gc_off(collect=True):
    _merge_all(paths, out_path, slim)


def _merge_all(paths: list[Path], out_path: Path, slim: bool) -> None:
  tid_seq = _IdSequencer(1)
  flow_seq = _IdSequencer(1)
  with _gzip_output_stream(out_path) as output:
    output.write(b'{"traceEvents":[')
    batcher = _EventBatcher(output)
    for file_index, path in enumerate(paths):
      # Each walk flushes its own pending list before returning.
      _merge_one_trace(path, file_index, batcher, tid_seq, flow_seq, slim)
    output.write(b"]}")


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
    _merge_one_trace(path, file_index, batcher, tid_seq, flow_seq, slim)
    if tid_seq._next_id >= id_bound or flow_seq._next_id >= id_bound:
      raise ValueError(
          f"trace {path} exhausted its member id stride "
          f"({_MERGE_MEMBER_ID_STRIDE} ids); raise _MERGE_MEMBER_ID_STRIDE")
    return batcher.emitted


def build_multi_trace_merge(
    paths: list[Path], out_path: Path, slim: bool, executor: concurrent.futures.Executor | None) -> None:
  """Build the multi-trace merged artifact: one pool task per trace, streamed as each completes.

  Each walk runs on ``executor`` (``None`` builds inline, the tests' shape) and
  the single gzip run streams each member's fragment the moment its task
  returns, in merge order — the compress overlaps the members still building.
  A comma precedes a fragment only when an earlier one emitted, so empty
  members stay invisible to the JSON. A failed member raises out of the walk
  order and kills the gzip run; the caller owns artifact atomicity.
  """
  member_dir = Path(tempfile.mkdtemp(prefix="merge-members-"))
  try:
    fragments = [member_dir / f"{index}.jsonl" for index in range(len(paths))]
    if executor is None:
      counts: Iterator[int] = iter(
          build_trace_member(path, fragment, index, slim)
          for index, (path, fragment) in enumerate(zip(paths, fragments, strict=True)))
    else:
      futures = [
          executor.submit(build_trace_member, path, fragment, index, slim)
          for index, (path, fragment) in enumerate(zip(paths, fragments, strict=True))
      ]
      counts = (future.result() for future in futures)
    with _gzip_output_stream(out_path) as output:
      output.write(b'{"traceEvents":[')
      emitted = False
      for fragment, count in zip(fragments, counts, strict=True):
        if not count:
          continue
        if emitted:
          output.write(b",")
        with fragment.open("rb") as fragment_file:
          shutil.copyfileobj(fragment_file, output)
        emitted = True
      output.write(b"]}")
  finally:
    shutil.rmtree(member_dir, ignore_errors=True)
