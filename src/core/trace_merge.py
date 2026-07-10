"""Streaming merge support for Chrome-format JSON traces."""

import gzip
import json
import re
from pathlib import Path
from typing import TextIO


def _rank_label(path: Path) -> str:
  match = re.search(r"rank(\d+)", path.name, flags=re.IGNORECASE)
  if match:
    return f"rank{match.group(1)}"
  return re.sub(r"\.json$", "", path.name, flags=re.IGNORECASE)


def _write_event(output: TextIO, event: dict, first_event: bool) -> bool:
  if not first_event:
    output.write(",")
  json.dump(event, output, ensure_ascii=False, separators=(",", ":"))
  return False


def _merge_one_trace(
    path: Path,
    file_index: int,
    output: TextIO,
    first_event: bool,
    next_tid: int,
    next_flow_id: int,
    slim: bool,
) -> tuple[bool, int, int]:
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
        first_event = _write_event(
            output,
            {
                "ph": "M",
                "pid": remapped_pid,
                "tid": synthetic_tid,
                "name": "thread_name",
                "args": {"name": f"{rank_label}/{original_tid}"},
            },
            first_event,
        )
    if event.get("ph") in {"s", "t", "f"} and "id" in event:
      event["id"] = get_numeric_flow_id(event["id"])
    first_event = _write_event(output, event, first_event)

  meta_tid = get_numeric_tid("meta")
  for synthetic_pid, (sort_index, label) in synthetic_meta.items():
    for name, args in (
        ("process_name", {"name": label}),
        ("process_labels", {"labels": label}),
        ("process_sort_index", {"sort_index": sort_index}),
    ):
      first_event = _write_event(
          output,
          {"ph": "M", "pid": synthetic_pid, "tid": meta_tid, "name": name, "args": args},
          first_event,
      )

  return first_event, next_tid, next_flow_id


def merge_traces(paths: list[Path], out_path: Path, slim: bool) -> None:
  """Merge Chrome JSON traces into one gzip-compressed Chrome trace."""
  first_event = True
  next_tid = 1
  next_flow_id = 1
  with gzip.open(out_path, "wt", encoding="utf-8") as output:
    output.write('{"traceEvents":[')
    for file_index, path in enumerate(paths):
      first_event, next_tid, next_flow_id = _merge_one_trace(
          path,
          file_index,
          output,
          first_event,
          next_tid,
          next_flow_id,
          slim,
      )
    output.write("]}")
