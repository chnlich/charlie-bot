"""The direct-pass build's child contract: artifact identity and the loud rejections."""

import gzip
import json
from pathlib import Path

import pytest

from src.api.pages import _build_direct_pass_gzip
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
