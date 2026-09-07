"""Streaming merge support for Chrome-format JSON traces."""

import gzip
import json
import re
from pathlib import Path
from typing import TextIO

# Compression level for the merged gzip output. Measured on a 191.2 MB /
# 496,099-event input: level 1 builds in 0.57 s / 15.5 MB against level 6's
# 1.73 s / 11.6 MB. The viewer fetches the whole artifact, so the build wall is
# the user-visible cost; big-payload transport gzip is level 1 for the same reason.
_MERGE_COMPRESSLEVEL = 1

# Events per json.dumps call on the merge output. The C encoder's per-element text
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

  def __init__(self, output: TextIO) -> None:
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
      self._output.write(",")
    self._emitted_any = True
    # ensure_ascii=True: the C encoder is ~3x faster on CJK-bearing payloads and
    # never slower on ASCII-only ones; both renderings parse to the same trace.
    text = json.dumps(self._pending, ensure_ascii=True, separators=(",", ":"))
    self._output.write(text[1:-1])
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

  def __call__(self, original: object) -> int:
    key = str(original)
    if key not in self._seen:
      self._seen[key] = self._next_id
      self._next_id += 1
    return self._seen[key]


def _merge_one_trace(
    path: Path,
    file_index: int,
    batcher: _EventBatcher,
    tid_seq: _IdSequencer,
    flow_seq: _IdSequencer,
    slim: bool,
) -> None:
  with path.open("r", encoding="utf-8") as input_file:
    trace = json.load(input_file)
  if isinstance(trace, dict):
    events = trace.get("traceEvents") or []
  elif isinstance(trace, list):
    events = trace
  else:
    raise ValueError(f"Trace root must be an object or array: {path}")
  rank_label = _rank_label(path)
  tid_seq.start_trace()
  flow_seq.start_trace()

  pid_labels: dict[str, str] = {}
  for event in events:
    if event.get("ph") == "M" and event.get("name") == "process_labels" and event.get("args"):
      pid_labels[str(event.get("pid"))] = event["args"].get("labels") or ""

  pid_map: dict[str, str] = {}
  synthetic_meta: dict[str, tuple[int, str]] = {}
  base_sort_index = file_index * 10000
  for original_pid, label in pid_labels.items():
    synthetic_pid = rank_label if label == "CPU" else f"{rank_label}/{label}"
    pid_map[original_pid] = synthetic_pid
    if label == "CPU":
      synthetic_meta[synthetic_pid] = (base_sort_index, f"{rank_label} CPU")
    else:
      gpu_match = re.search(r"GPU\s*(\d+)", label, flags=re.IGNORECASE)
      gpu_index = int(gpu_match.group(1)) if gpu_match else 0
      synthetic_meta[synthetic_pid] = (base_sort_index + 1000 + gpu_index, f"{rank_label} {label}")

  emitted_thread_names: set[str] = set()
  batcher_add = batcher.add
  pid_map_get = pid_map.get
  flow_seq_call = flow_seq

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

    remapped_pid = pid_map_get(str(event.get("pid")), rank_label)
    event["pid"] = remapped_pid
    if "tid" in event:
      original_tid = event["tid"]
      synthetic_tid = tid_seq(original_tid)
      event["tid"] = synthetic_tid
      thread_key = str(original_tid)
      if thread_key not in emitted_thread_names:
        emitted_thread_names.add(thread_key)
        batcher_add(
            {
                "ph": "M",
                "pid": remapped_pid,
                "tid": synthetic_tid,
                "name": "thread_name",
                "args": {
                    "name": f"{rank_label}/{original_tid}"
                },
            })
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
  with gzip.open(out_path, "wt", encoding="utf-8", compresslevel=_MERGE_COMPRESSLEVEL) as output:
    output.write('{"traceEvents":[')
    batcher = _EventBatcher(output)
    for file_index, path in enumerate(paths):
      _merge_one_trace(path, file_index, batcher, tid_seq, flow_seq, slim)
    batcher.flush()
    output.write("]}")
