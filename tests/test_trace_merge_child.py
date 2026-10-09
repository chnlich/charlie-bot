"""The single-trace merged-trace build's child: byte parity with the in-process
build, and the exit classes the route maps back to its error types."""

import json
import pathlib
import sys

import pytest

from src.features.trace import api as trace_api
from src.features.trace import trace_merge, trace_merge_child
from tests.core import test_multi_trace_merge

REPO_ROOT = str(pathlib.Path(trace_api.__file__).resolve().parents[3])


def _write_trace(path: pathlib.Path, marker: str = "event") -> None:
  path.write_text(
      json.dumps({"traceEvents": [{
          "ph": "X",
          "pid": 1,
          "tid": 1,
          "name": marker
      }]}),
      encoding="utf-8",
  )


def _child_argv(paths: list[pathlib.Path], out_path: pathlib.Path) -> list[str]:
  """The child main's argv shape (sys.argv: the script path at [0])."""
  return [str(pathlib.Path(trace_merge_child.__file__)), REPO_ROOT, *[str(path) for path in paths], str(out_path), "0"]


def test_child_build_serves_the_in_process_bytes(tmp_path: pathlib.Path) -> None:
  """The child runs the same merge_traces the route ran in-process; the artifact must
  be byte-identical or a cached view built the other way diverges from a fresh one."""
  trace = tmp_path / "rank0.json"
  _write_trace(trace)
  child_out = tmp_path / "child.json.gz"
  inproc_out = tmp_path / "inproc.json.gz"

  assert trace_merge_child.main(_child_argv([trace], child_out)) == trace_merge_child.EXIT_OK
  trace_merge.merge_traces([trace], inproc_out, slim=False)
  assert child_out.read_bytes() == inproc_out.read_bytes()


def test_child_main_runs_from_the_spawn_argv(tmp_path: pathlib.Path) -> None:
  """The parent's argv is the child program's whole interface: the module must build
  when executed as a script, the way the route's Popen runs it."""
  import subprocess

  trace = tmp_path / "rank0.json"
  _write_trace(trace)
  out = tmp_path / "out.json.gz"
  proc = subprocess.run(
      [sys.executable,
       str(pathlib.Path(trace_merge_child.__file__)), REPO_ROOT,
       str(trace), str(out), "0"],
      capture_output=True,
      text=True,
      check=False)
  assert proc.returncode == trace_merge_child.EXIT_OK, proc.stderr
  assert out.is_file()


def test_child_exit_classes_mirror_the_in_process_errors(
    tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
  """A JSON body with no traceEvents is the route's NotATraceError; a decode failure
  is the generic build failure. The route maps the exit classes back to these types."""
  manifest = tmp_path / "manifest.json"
  manifest.write_text(json.dumps({"A": [{"rank": 0}]}), encoding="utf-8")
  assert trace_merge_child.main(_child_argv([manifest], tmp_path / "out.json.gz")) == trace_merge_child.EXIT_NOT_A_TRACE
  assert "no traceEvents array" in capsys.readouterr().err

  corrupt = tmp_path / "corrupt.json"
  corrupt.write_text("{broken", encoding="utf-8")
  assert trace_merge_child.main(_child_argv([corrupt], tmp_path / "out.json.gz")) == trace_merge_child.EXIT_FAILED
  capsys.readouterr()


def test_route_build_maps_the_child_exits_back(tmp_path: pathlib.Path) -> None:
  """The route function re-raises the in-process error types: a not-a-trace input
  fails the build with NotATraceError, a successful build lands the artifact."""
  trace = tmp_path / "rank0.json"
  _write_trace(trace)
  out = tmp_path / "out.json.gz"
  trace_api._build_single_trace_merge([trace], out, slim=False)
  assert out.is_file()

  manifest = tmp_path / "manifest.json"
  manifest.write_text(json.dumps({"A": []}), encoding="utf-8")
  with pytest.raises(trace_merge.NotATraceError):
    trace_api._build_single_trace_merge([manifest], tmp_path / "no.json.gz", slim=False)


# The real sequential build runs the full merge path per output; measured
# 1.01-1.13s across runs, past the 2s unit budget under host load.
@pytest.mark.integration
def test_chunked_build_serves_the_sequential_bytes(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The chunked path must ship the sequential build's exact bytes; a drifted synthetic id diverges."""
  trace = tmp_path / "trace_rank0.json"
  test_multi_trace_merge._write_pretty_trace(trace, 0, 120)
  plain, sequential, slim, slim_sequential = (
      tmp_path / n
      for n in ("chunked.json.gz", "sequential.json.gz", "chunked-slim.json.gz", "sequential-slim.json.gz"))

  monkeypatch.setattr(trace_merge, "_MIN_CHUNK_BYTES", 256)
  trace_merge.merge_traces([trace], plain, slim=False)
  trace_merge.merge_traces([trace], slim, slim=True)
  monkeypatch.setattr(trace_merge, "_split_chunks", lambda *args: None)
  trace_merge.merge_traces([trace], sequential, slim=False)
  trace_merge.merge_traces([trace], slim_sequential, slim=True)
  assert plain.read_bytes() == sequential.read_bytes()
  assert slim.read_bytes() == slim_sequential.read_bytes()
