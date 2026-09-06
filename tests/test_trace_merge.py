import gzip
import json
from pathlib import Path

from src.core.trace_merge import merge_traces


def _write_trace(path: Path, events: list[dict], *, bare: bool = False) -> None:
  payload: list[dict] | dict[str, list[dict]] = events if bare else {"traceEvents": events}
  path.write_text(json.dumps(payload), encoding="utf-8")


def _read_merged(path: Path) -> list[dict]:
  with gzip.open(path, "rt", encoding="utf-8") as merged_file:
    return json.load(merged_file)["traceEvents"]


def _rank_events(rank: int) -> list[dict]:
  return [
      {
          "ph": "M",
          "pid": 10,
          "tid": 0,
          "name": "process_labels",
          "args": {
              "labels": "CPU"
          }
      },
      {
          "ph": "M",
          "pid": 20,
          "tid": 0,
          "name": "process_labels",
          "args": {
              "labels": "GPU 0"
          }
      },
      {
          "ph": "M",
          "pid": 10,
          "tid": 0,
          "name": "process_name",
          "args": {
              "name": "original"
          }
      },
      {
          "ph": "X",
          "pid": 10,
          "tid": 7,
          "name": f"cpu-{rank}",
          "ts": rank + 1,
          "args": {
              "value": rank
          }
      },
      {
          "ph": "X",
          "pid": 20,
          "tid": 8,
          "name": f"gpu-{rank}",
          "ts": rank + 2
      },
      {
          "ph": "s",
          "pid": 10,
          "tid": 7,
          "name": f"flow-start-{rank}",
          "id": 99
      },
      {
          "ph": "f",
          "pid": 20,
          "tid": 8,
          "name": f"flow-end-{rank}",
          "id": 99
      },
  ]


def test_merge_remaps_processes_threads_flows_and_metadata(tmp_path: Path) -> None:
  rank0 = tmp_path / "trace_rank0.json"
  rank1 = tmp_path / "trace_RANK1.json"
  output = tmp_path / "merged.json.gz"
  _write_trace(rank0, _rank_events(0))
  _write_trace(rank1, _rank_events(1), bare=True)

  merge_traces([rank0, rank1], output, slim=False)
  events = _read_merged(output)
  by_name = {event.get("name"): event for event in events if event.get("name")}

  assert by_name["cpu-0"]["pid"] == "rank0"
  assert by_name["gpu-0"]["pid"] == "rank0/GPU 0"
  assert by_name["cpu-1"]["pid"] == "rank1"
  assert by_name["gpu-1"]["pid"] == "rank1/GPU 0"

  thread_names = {event["args"]["name"]: event["tid"] for event in events if event.get("name") == "thread_name"}
  assert set(thread_names) == {"rank0/7", "rank0/8", "rank1/7", "rank1/8"}
  assert len(set(thread_names.values())) == 4
  assert by_name["cpu-0"]["tid"] == thread_names["rank0/7"]
  assert by_name["gpu-1"]["tid"] == thread_names["rank1/8"]

  assert by_name["flow-start-0"]["id"] == by_name["flow-end-0"]["id"]
  assert by_name["flow-start-1"]["id"] == by_name["flow-end-1"]["id"]
  assert by_name["flow-start-0"]["id"] != by_name["flow-start-1"]["id"]

  process_meta = [event for event in events if event.get("name", "").startswith("process_")]
  assert len(process_meta) == 12
  assert all(event["args"].get("labels") not in {"CPU", "GPU 0"} for event in process_meta)

  sort_indices = {
      event["pid"]: event["args"]["sort_index"] for event in process_meta if event["name"] == "process_sort_index"
  }
  assert sort_indices == {
      "rank0": 0,
      "rank0/GPU 0": 1000,
      "rank1": 10000,
      "rank1/GPU 0": 11000,
  }
  labels = {event["pid"]: event["args"]["labels"] for event in process_meta if event["name"] == "process_labels"}
  assert labels == {
      "rank0": "rank0 CPU",
      "rank0/GPU 0": "rank0 GPU 0",
      "rank1": "rank1 CPU",
      "rank1/GPU 0": "rank1 GPU 0",
  }


def test_slim_reduces_args_and_drops_cpu_instants(tmp_path: Path) -> None:
  trace = tmp_path / "rank3.json"
  slim_output = tmp_path / "slim.json.gz"
  full_output = tmp_path / "full.json.gz"
  events = [
      {
          "ph": "M",
          "pid": 1,
          "name": "process_labels",
          "args": {
              "labels": "CPU"
          }
      },
      {
          "ph": "X",
          "pid": 1,
          "tid": 1,
          "name": "keep",
          "args": {
              "stream": 4,
              "detail": "drop"
          }
      },
      {
          "ph": "X",
          "pid": 1,
          "tid": 2,
          "name": "empty",
          "args": {
              "detail": "drop"
          }
      },
      {
          "ph": "i",
          "pid": 1,
          "tid": 3,
          "name": "memory",
          "cat": "cpu_instant_event",
          "args": {
              "stream": 9
          },
      },
  ]
  _write_trace(trace, events)

  merge_traces([trace], slim_output, slim=True)
  slim_by_name = {event.get("name"): event for event in _read_merged(slim_output)}
  assert slim_by_name["keep"]["args"] == {"stream": 4}
  assert "args" not in slim_by_name["empty"]
  assert "memory" not in slim_by_name

  merge_traces([trace], full_output, slim=False)
  full_by_name = {event.get("name"): event for event in _read_merged(full_output)}
  assert full_by_name["keep"]["args"] == {"stream": 4, "detail": "drop"}
  assert full_by_name["empty"]["args"] == {"detail": "drop"}
  assert full_by_name["memory"]["args"] == {"stream": 9}


def test_serializer_emits_unchanged_event_sequence(tmp_path: Path) -> None:
  rank0 = tmp_path / "trace_rank0.json"
  rank1 = tmp_path / "trace_rank1.json"
  output = tmp_path / "merged.json.gz"
  _write_trace(rank0, _rank_events(0))
  _write_trace(rank1, _rank_events(1))

  merge_traces([rank0, rank1], output, slim=False)
  events = _read_merged(output)

  # The C-encoder serializer must reproduce the exact event sequence the pure-Python
  # json.dump path produced before: every event the fixtures contribute, in order,
  # with thread_name/process_ metadata interleaved exactly as before.
  names = [event.get("name") for event in events if event.get("name")]
  assert "cpu-0" in names and "cpu-1" in names
  assert "gpu-0" in names and "gpu-1" in names
  assert "flow-start-0" in names and "flow-end-0" in names
  assert "flow-start-1" in names and "flow-end-1" in names
  assert names.index("cpu-0") < names.index("cpu-1")

  cpu0 = next(e for e in events if e.get("name") == "cpu-0")
  assert cpu0 == {
      "ph": "X",
      "pid": "rank0",
      "ts": 1,
      "args": {
          "value": 0
      },
      "name": "cpu-0",
      "tid": next(e["tid"] for e in events if e.get("name") == "thread_name" and e["args"]["name"] == "rank0/7"),
  }
  # The gzip header contains a timestamp, so compare product semantics after decompression.
  second = tmp_path / "merged2.json.gz"
  merge_traces([rank0, rank1], second, slim=False)
  assert _read_merged(second) == events


def _batch_events(count: int) -> list[dict]:
  return [{
      "ph": "X",
      "pid": 7,
      "tid": index % 3,
      "name": f"evt-{index}",
      "ts": index,
      "args": {
          "value": index,
          "text": f"payload-{index}",
      },
  } for index in range(count)]


def _merged_payload(path: Path) -> str:
  with gzip.open(path, "rt", encoding="utf-8") as merged_file:
    return merged_file.read()


def test_batched_serializer_matches_per_event_rendering(tmp_path: Path) -> None:
  # Batch boundaries must be invisible in the payload: the bracket-stripped batch
  # renderings concatenate into exactly the per-event form the pre-batch writer
  # produced, comma placement and thread_name interleaving included.
  trace = tmp_path / "trace.json"
  _write_trace(trace, _batch_events(2 * 512 + 3))
  output = tmp_path / "merged.json.gz"

  merge_traces([trace], output, slim=False)

  payload = _merged_payload(output)
  events = json.loads(payload)["traceEvents"]
  expected = ",".join(json.dumps(event, ensure_ascii=False, separators=(",", ":")) for event in events)
  assert payload == '{"traceEvents":[' + expected + "]}"


def test_single_event_and_batch_flush_produce_valid_payload(tmp_path: Path) -> None:
  # One event total exercises the first-batch flush (no leading comma) and the
  # trailing flush in one pass.
  trace = tmp_path / "trace.json"
  _write_trace(trace, _batch_events(1))
  output = tmp_path / "merged.json.gz"

  merge_traces([trace], output, slim=False)

  events = _read_merged(output)
  assert [event["name"] for event in events if event.get("name", "").startswith("evt-")] == ["evt-0"]
