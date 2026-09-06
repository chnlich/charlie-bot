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
    text = json.dumps(self._pending, ensure_ascii=False, separators=(",", ":"))
    self._output.write(text[1:-1])
    self._pending.clear()


def _merge_one_trace(
    path: Path,
    file_index: int,
    batcher: _EventBatcher,
    next_tid: int,
    next_flow_id: int,
    slim: bool,
) -> tuple[int, int]:
  with path.open("r", encoding="utf-8") as input_file:
    trace = json.load(input_file)
  if isinstance(trace, dict):
    events = trace.get("traceEvents") or []
  elif isinstance(trace, list):
    events = trace
  else:
    raise ValueError(f"Trace root must be an object or array: {path}")
  rank_label = _rank_label(path)

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

  tid_map: dict[str, int] = {}
  flow_id_map: dict[str, int] = {}
  emitted_thread_names: set[str] = set()

  def get_numeric_tid(original_tid: object) -> int:
    nonlocal next_tid
    key = str(original_tid)
    if key not in tid_map:
      tid_map[key] = next_tid
      next_tid += 1
    return tid_map[key]

  def get_numeric_flow_id(original_id: object) -> int:
    nonlocal next_flow_id
    key = str(original_id)
    if key not in flow_id_map:
      flow_id_map[key] = next_flow_id
      next_flow_id += 1
    return flow_id_map[key]

  for event in events:
    event_name = event.get("name")
    if event.get("ph") == "M" and event_name and event_name.startswith("process_"):
      continue
    if slim and event.get("cat") == "cpu_instant_event":
      continue
    if slim and isinstance(event.get("args"), dict):
      if "stream" in event["args"]:
        event["args"] = {"stream": event["args"]["stream"]}
      else:
        event.pop("args")

    remapped_pid = pid_map.get(str(event.get("pid")), rank_label)
    event["pid"] = remapped_pid
    if "tid" in event:
      original_tid = event["tid"]
      synthetic_tid = get_numeric_tid(original_tid)
      event["tid"] = synthetic_tid
      thread_key = str(original_tid)
      if thread_key not in emitted_thread_names:
        emitted_thread_names.add(thread_key)
        batcher.add(
            {
                "ph": "M",
                "pid": remapped_pid,
                "tid": synthetic_tid,
                "name": "thread_name",
                "args": {
                    "name": f"{rank_label}/{original_tid}"
                },
            })
    if event.get("ph") in {"s", "t", "f"} and "id" in event:
      event["id"] = get_numeric_flow_id(event["id"])
    batcher.add(event)

  meta_tid = get_numeric_tid("meta")
  for synthetic_pid, (sort_index, label) in synthetic_meta.items():
    for name, args in (
        ("process_name", {"name": label}),
        ("process_labels", {"labels": label}),
        ("process_sort_index", {"sort_index": sort_index}),
    ):
      batcher.add({"ph": "M", "pid": synthetic_pid, "tid": meta_tid, "name": name, "args": args})

  return next_tid, next_flow_id


def merge_traces(paths: list[Path], out_path: Path, slim: bool) -> None:
  """Merge Chrome JSON traces into one gzip-compressed Chrome trace."""
  next_tid = 1
  next_flow_id = 1
  with gzip.open(out_path, "wt", encoding="utf-8", compresslevel=_MERGE_COMPRESSLEVEL) as output:
    output.write('{"traceEvents":[')
    batcher = _EventBatcher(output)
    for file_index, path in enumerate(paths):
      next_tid, next_flow_id = _merge_one_trace(
          path,
          file_index,
          batcher,
          next_tid,
          next_flow_id,
          slim,
      )
    batcher.flush()
    output.write("]}")
