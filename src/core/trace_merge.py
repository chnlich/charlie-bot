"""Streaming merge support for Chrome-format JSON traces."""

import gzip
import re
from pathlib import Path
from typing import BinaryIO

import orjson

# Compression level for the merged gzip output. Measured on a 191.2 MB /
# 496,099-event input: level 1 builds in 0.57 s / 15.5 MB against level 6's
# 1.73 s / 11.6 MB. The viewer fetches the whole artifact, so the build wall is
# the user-visible cost; big-payload transport gzip is level 1 for the same reason.
_MERGE_COMPRESSLEVEL = 1

# Events per orjson.dumps call on the merge output. The encoder's per-element text
# is context-free, so a batch's bracket-stripped rendering is byte-identical to the
# per-event form; batching cut the serializer pass from 3.0 s to 1.7 s on the input
# above. The batch is the only buffering beyond the gzip stream.
_MERGE_BATCH_EVENTS = 512


def _rank_label(path: Path) -> str:
  match = re.search(r"rank(\d+)", path.name, flags=re.IGNORECASE)
  if match:
    return f"rank{match.group(1)}"
  return re.sub(r"\.json$", "", path.name, flags=re.IGNORECASE)


class _EventBatcher:
  """Serializes merged events into the output stream in batches.

  The stream carries `e1,e2,...` with no brackets of its own; a batch's list
  rendering minus its outer brackets is exactly that fragment.
  """

  def __init__(self, output: BinaryIO) -> None:
    self._output = output
    self._pending: list[dict] = []
    self._emitted_any = False

  def add(self, event: dict) -> None:
    self._pending.append(event)
    if len(self._pending) >= _MERGE_BATCH_EVENTS:
      self.flush()

  def flush(self) -> None:
    if not self._pending:
      return
    if self._emitted_any:
      self._output.write(b",")
    self._emitted_any = True
    # orjson's compact rendering parses to the same trace the stdlib encoder
    # produced; non-ASCII rides raw UTF-8 where the stdlib form emitted \uXXXX.
    # The parse pass rejects the NaN/Infinity literals stdlib json.load accepts,
    # so a trace carrying them fails the build; an in-memory non-finite float
    # (unreachable from a trace file) would render as null here.
    self._output.write(orjson.dumps(self._pending)[1:-1])
    self._pending.clear()


class _IdSequencer:
  """Maps each original trace id to a dense sequential int, starting at 1.

  One instance spans the whole merge; the key map resets per trace, because the
  same original tid in two ranks is two different threads, while the ints keep
  counting so no two threads or flows ever collide.
  """

  def __init__(self) -> None:
    self._next_id = 1
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
  if isinstance(trace, dict):
    events = trace.get("traceEvents") or []
  elif isinstance(trace, list):
    events = trace
  else:
    raise ValueError(f"Trace root must be an object or array: {path}")
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

  batcher_add = batcher.add
  pid_map_get = pid_map.get
  tid_map_get = tid_seq.seen.get
  tid_seq_call = tid_seq
  flow_seq_call = flow_seq
  _str = str

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
    if "tid" in event:
      original_tid = event["tid"]
      thread_key = _str(original_tid)
      synthetic_tid = tid_map_get(thread_key)
      if synthetic_tid is None:
        # First sight: the sequencer allocates and inserts; the thread_name
        # rides the same first sight instead of a second per-event set probe.
        synthetic_tid = tid_seq_call(original_tid)
        batcher_add(
            {
                "ph": "M",
                "pid": event["pid"],
                "tid": synthetic_tid,
                "name": "thread_name",
                "args": {
                    "name": f"{rank_label}/{original_tid}"
                },
            })
      event["tid"] = synthetic_tid
    if ph in {"s", "t", "f"} and "id" in event:
      event["id"] = flow_seq_call(event["id"])
    batcher_add(event)

  meta_tid = tid_seq("meta")
  for synthetic_pid, (sort_index, label) in synthetic_meta.items():
    for name, args in (
        ("process_name", {"name": label}),
        ("process_labels", {"labels": label}),
        ("process_sort_index", {"sort_index": sort_index}),
    ):
      batcher.add({"ph": "M", "pid": synthetic_pid, "tid": meta_tid, "name": name, "args": args})


def merge_traces(paths: list[Path], out_path: Path, slim: bool) -> None:
  """Merge Chrome JSON traces into one gzip-compressed Chrome trace."""
  tid_seq = _IdSequencer()
  flow_seq = _IdSequencer()
  with gzip.open(out_path, "wb", compresslevel=_MERGE_COMPRESSLEVEL) as output:
    output.write(b'{"traceEvents":[')
    batcher = _EventBatcher(output)
    for file_index, path in enumerate(paths):
      _merge_one_trace(path, file_index, batcher, tid_seq, flow_seq, slim)
    batcher.flush()
    output.write(b"]}")
