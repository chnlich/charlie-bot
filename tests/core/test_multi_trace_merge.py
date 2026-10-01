"""The multi-trace merged build's wave contract: bounded concurrency, identical artifact."""

import concurrent.futures
import gzip
import json
import threading
from pathlib import Path

import pytest

from src.core import trace_merge


def _write_trace(path: Path, rank: int, events: int) -> None:
  trace = {"traceEvents": [{"ph": "M", "name": "process_labels", "pid": rank, "args": {"labels": f"GPU {rank}"}}]}
  trace["traceEvents"] += [
      {
          "ph": "B",
          "pid": rank,
          "tid": 1,
          "ts": index,
          "name": "span",
          "id": f"flow{index}"
      } for index in range(events)
  ]
  path.write_text(json.dumps(trace))


def _build(paths: list[Path], out_path: Path, slim: bool = False) -> list[dict]:
  executor = concurrent.futures.ThreadPoolExecutor(max_workers=4)
  try:
    trace_merge.build_multi_trace_merge(paths, out_path, slim=slim, executor=executor)
  finally:
    executor.shutdown(wait=True)
  return json.loads(gzip.decompress(out_path.read_bytes()))["traceEvents"]


def test_wave_bound_keeps_the_artifact_identical(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  paths = []
  for rank in range(2):
    path = tmp_path / f"trace_rank{rank}.json"
    _write_trace(path, rank, 50)
    paths.append(path)
  monkeypatch.setattr(trace_merge, "_merge_memory_budget", lambda: None)
  unbounded = _build(paths, tmp_path / "unbounded.json.gz")
  monkeypatch.setattr(trace_merge, "_merge_memory_budget", lambda: 1)
  bounded = _build(paths, tmp_path / "bounded.json.gz")
  assert bounded == unbounded
  assert len(unbounded) >= 2 * 50


def test_one_byte_budget_builds_members_one_at_a_time(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  paths = []
  for rank in range(3):
    path = tmp_path / f"trace_rank{rank}.json"
    _write_trace(path, rank, 10)
    paths.append(path)
  state = {"active": 0, "peak": 0}
  lock = threading.Lock()
  real_outcome = trace_merge._member_outcome

  def counting_outcome(path: Path, fragment: Path, index: int, slim: bool) -> int | None:
    with lock:
      state["active"] += 1
      state["peak"] = max(state["peak"], state["active"])
    try:
      return real_outcome(path, fragment, index, slim)
    finally:
      with lock:
        state["active"] -= 1

  monkeypatch.setattr(trace_merge, "_member_outcome", counting_outcome)
  monkeypatch.setattr(trace_merge, "_merge_memory_budget", lambda: 1)
  assert len(_build(paths, tmp_path / "out.json.gz")) >= 3 * 10
  assert state["peak"] == 1


def test_no_budget_builds_every_member_at_once(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  paths = []
  for rank in range(4):
    path = tmp_path / f"trace_rank{rank}.json"
    _write_trace(path, rank, 10)
    paths.append(path)
  barrier = threading.Barrier(4, timeout=10)
  real_outcome = trace_merge._member_outcome

  def barrier_outcome(path: Path, fragment: Path, index: int, slim: bool) -> int | None:
    barrier.wait()
    return real_outcome(path, fragment, index, slim)

  monkeypatch.setattr(trace_merge, "_member_outcome", barrier_outcome)
  monkeypatch.setattr(trace_merge, "_merge_memory_budget", lambda: None)
  assert len(_build(paths, tmp_path / "out.json.gz")) >= 4 * 10


def _write_pretty_trace(path: Path, rank: int, events_per_pid: int) -> None:
  """A pretty trace whose identities span any chunk split: labels away from their pid's events."""
  trace: dict = {"traceEvents": [], "deviceProperties": [{"gpu": rank}]}
  events = trace["traceEvents"]
  events.append({"ph": "M", "name": "process_labels", "pid": 7 + rank, "args": {"labels": f"GPU {3 + rank}"}})
  for index in range(events_per_pid):
    events.append(
        {
            "ph": "X",
            "pid": 7 + rank,
            "tid": index % 2,
            "ts": index,
            "name": "span",
            "id": f"f{index % 3}",
            "args": {
                "stream": 1
            }
        })
    events.append({"ph": "s", "pid": 7 + rank, "tid": "1", "ts": index, "name": "flow", "id": index % 3})
  events.append({"ph": "X", "pid": 7 + rank, "tid": 9, "ts": 10_200, "name": "late span"})
  events.append({"ph": "M", "name": "process_labels", "pid": 7 + rank, "args": {"labels": f"GPU {4 + rank}"}})
  events.append({"ph": "X", "pid": str(7 + rank), "tid": 1, "ts": 10_000, "name": "str-pid span"})
  path.write_text(json.dumps(trace, indent=2), encoding="utf-8")


@pytest.mark.integration
@pytest.mark.parametrize("slim", [False, True], ids=["plain", "slim"])
def test_chunked_member_serves_the_sequential_bytes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, slim: bool) -> None:
  """The chunked member build ships the sequential member walk's exact bytes, stride ids
  included, for the plain and the slim walk alike."""
  paths = []
  for rank in range(2):
    path = tmp_path / f"trace_rank{rank}.json"
    _write_pretty_trace(path, rank, 120)
    paths.append(path)
  chunked, sequential = tmp_path / "chunked.json.gz", tmp_path / "sequential.json.gz"

  monkeypatch.setattr(trace_merge, "_MIN_CHUNK_BYTES", 256)
  _build(paths, chunked, slim=slim)
  monkeypatch.setattr(trace_merge, "_split_chunks", lambda *args: None)
  _build(paths, sequential, slim=slim)
  assert chunked.read_bytes() == sequential.read_bytes()
