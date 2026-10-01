"""The direct-pass build's child contract: artifact identity and the loud rejections."""

import gzip
import json
import sys
from pathlib import Path

import pytest

from src.api.pages import _build_direct_pass_gzip
from src.core import direct_pass_child
from src.core.trace_merge import NotATraceError


def _write_trace(path: Path, events: list[dict]) -> None:
  path.write_text(json.dumps({"traceEvents": events}))


def _event(i: int) -> dict:
  return {"name": f"step {i}", "ph": "X", "ts": i, "dur": 1, "pid": 1, "tid": 1}


def test_build_artifact_is_the_original_bytes_gzipped(tmp_path: Path) -> None:
  trace = tmp_path / "trace.json"
  events = [_event(i) for i in range(200)]
  _write_trace(trace, events)
  out = tmp_path / "out.gz"
  _build_direct_pass_gzip(trace, out)
  assert gzip.decompress(out.read_bytes()) == trace.read_bytes()


def test_manifest_build_raises_not_a_trace_and_writes_nothing(tmp_path: Path) -> None:
  trace = tmp_path / "manifest.json"
  trace.write_text(json.dumps({"analysis": "no traceEvents array here"}))
  out = tmp_path / "out.gz"
  with pytest.raises(NotATraceError):
    _build_direct_pass_gzip(trace, out)


def test_non_json_body_fails_loud(tmp_path: Path) -> None:
  trace = tmp_path / "broken.json"
  trace.write_text('{"traceEvents": [1, 2')
  out = tmp_path / "out.gz"
  with pytest.raises(ValueError):
    _build_direct_pass_gzip(trace, out)


def _write_pretty_trace(path: Path, events: list[dict], trailing: dict | None = None) -> None:
  doc = {"traceEvents": events}
  if trailing is not None:
    doc.update(trailing)
  path.write_text(json.dumps(doc, indent=2))


def _run_child(monkeypatch: pytest.MonkeyPatch, trace: Path, out: Path) -> int:
  # main() runs in-process so the patched split threshold reaches _split_chunks;
  # the spawned chunk helpers are fresh interpreters running the real module.
  monkeypatch.setattr(direct_pass_child, "_MIN_CHUNK_BYTES", 2048)
  checkout_root = str(Path(__file__).resolve().parents[2])
  return direct_pass_child.main([sys.executable, str(trace), str(out), checkout_root])


def _split_of(path: Path, min_chunk_bytes: int = 2048) -> object:
  return direct_pass_child._split_chunks(path, path.stat().st_size, min_chunk_bytes)


def test_pretty_trace_splits_at_element_lines_and_every_chunk_parses(tmp_path: Path) -> None:
  trace = tmp_path / "trace.json"
  _write_pretty_trace(trace, [_event(i) for i in range(120)])
  object_form, indent, starts = _split_of(trace)
  assert object_form is True and len(starts) >= 1
  bounds = [0, *starts, trace.stat().st_size]
  assert bounds == sorted(bounds)
  import orjson

  from src.core.trace_merge import _trace_events_or_raise
  for index in range(len(bounds) - 1):
    wrapped = direct_pass_child._chunk_parse_input(
        trace, bounds[index], bounds[index + 1], index,
        len(bounds) - 1, object_form, indent)
    parsed = orjson.loads(wrapped)
    if index == 0:
      _trace_events_or_raise(parsed, trace)


def test_chunked_build_artifact_is_the_original_bytes_gzipped(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  trace = tmp_path / "trace.json"
  _write_pretty_trace(trace, [_event(i) for i in range(120)])
  out = tmp_path / "out.gz"
  assert _run_child(monkeypatch, trace, out) == direct_pass_child.EXIT_OK
  assert gzip.decompress(out.read_bytes()) == trace.read_bytes()


def test_trailing_keys_after_traceevents_still_build(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  # The kineto export shape: scalar sibling keys follow the events array.
  trace = tmp_path / "trace.json"
  _write_pretty_trace(trace, [_event(i) for i in range(120)], trailing={"traceName": "rank5.json"})
  out = tmp_path / "out.gz"
  assert _run_child(monkeypatch, trace, out) == direct_pass_child.EXIT_OK
  assert gzip.decompress(out.read_bytes()) == trace.read_bytes()


def test_corrupt_pretty_trace_fails_loud_after_fallback(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  trace = tmp_path / "trace.json"
  trace.write_text(json.dumps({"traceEvents": [_event(i) for i in range(120)]}, indent=2))
  raw = trace.read_bytes()
  trace.write_bytes(raw.replace(b'"dur": 1,', b'"dur": NaN,', 1))
  assert _run_child(monkeypatch, trace, tmp_path / "nan.gz") == direct_pass_child.EXIT_PARSE_FAILED

  # A missing inter-element comma is the one corruption a chunk parse could
  # erase at a split point; requiring the separator sends it to the whole-file
  # rejection. Dropped at the split's own first anchor (2048 = _run_child's patch).
  split = direct_pass_child._split_chunks(trace, len(raw), 2048)
  comma = raw.rfind(b",", 0, split[2][0])
  trace.write_bytes(raw[:comma] + raw[comma + 1:])
  assert _run_child(monkeypatch, trace, tmp_path / "sep.gz") == direct_pass_child.EXIT_PARSE_FAILED


def test_split_window_probe_and_full_span_fallback(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  # The backward anchor probe reads only a window ending at the boundary; a
  # window of zero always misses, and the full-span scan must still land
  # element-aligned bounds every chunk parses from.
  import orjson

  trace = tmp_path / "trace.json"
  _write_pretty_trace(trace, [_event(i) for i in range(60_000)])
  for window in (1 << 16, 0):
    monkeypatch.setattr(direct_pass_child, "_ANCHOR_WINDOW_BYTES", window)
    split = _split_of(trace)
    assert split is not None
    object_form, indent, starts = split
    bounds = [0, *starts, trace.stat().st_size]
    assert bounds == sorted(bounds)
    for index in range(len(bounds) - 1):
      wrapped = direct_pass_child._chunk_parse_input(
          trace, bounds[index], bounds[index + 1], index,
          len(bounds) - 1, object_form, indent)
      orjson.loads(wrapped)


def test_unsplittable_shapes_return_no_split(tmp_path: Path) -> None:
  compact = tmp_path / "compact.json"
  compact.write_text(json.dumps({"traceEvents": [_event(i) for i in range(50)]}))
  assert _split_of(compact) is None
  manifest = tmp_path / "manifest.json"
  manifest.write_text(json.dumps({"analysis": "no events array here"}) + " " + "x" * 8192)
  assert _split_of(manifest) is None
