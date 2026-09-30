"""The single-trace merged-trace build's child: byte parity with the in-process
build, and the exit classes the route maps back to its error types."""

import json
import sys
from pathlib import Path

import pytest

from src.api import pages
from src.core import trace_merge_child
from src.core.trace_merge import NotATraceError, merge_traces

REPO_ROOT = str(Path(pages.__file__).resolve().parents[2])


def _write_trace(path: Path, marker: str = "event") -> None:
  path.write_text(
      json.dumps({"traceEvents": [{
          "ph": "X",
          "pid": 1,
          "tid": 1,
          "name": marker
      }]}),
      encoding="utf-8",
  )


def _child_argv(paths: list[Path], out_path: Path) -> list[str]:
  """The child main's argv shape (sys.argv: the script path at [0])."""
  return [str(Path(trace_merge_child.__file__)), REPO_ROOT, *[str(path) for path in paths], str(out_path), "0"]


def test_child_build_serves_the_in_process_bytes(tmp_path: Path) -> None:
  """The child runs the same merge_traces the route ran in-process; the artifact must
  be byte-identical or a cached view built the other way diverges from a fresh one."""
  trace = tmp_path / "rank0.json"
  _write_trace(trace)
  child_out = tmp_path / "child.json.gz"
  inproc_out = tmp_path / "inproc.json.gz"

  assert trace_merge_child.main(_child_argv([trace], child_out)) == trace_merge_child.EXIT_OK
  merge_traces([trace], inproc_out, slim=False)
  assert child_out.read_bytes() == inproc_out.read_bytes()


def test_child_main_runs_from_the_spawn_argv(tmp_path: Path) -> None:
  """The parent's argv is the child program's whole interface: the module must build
  when executed as a script, the way the route's Popen runs it."""
  import subprocess

  trace = tmp_path / "rank0.json"
  _write_trace(trace)
  out = tmp_path / "out.json.gz"
  proc = subprocess.run(
      [sys.executable, str(Path(trace_merge_child.__file__)), REPO_ROOT,
       str(trace), str(out), "0"],
      capture_output=True,
      text=True,
      check=False)
  assert proc.returncode == trace_merge_child.EXIT_OK, proc.stderr
  assert out.is_file()


def test_child_exit_classes_mirror_the_in_process_errors(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
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


def test_route_build_maps_the_child_exits_back(tmp_path: Path) -> None:
  """The route function re-raises the in-process error types: a not-a-trace input
  fails the build with NotATraceError, a successful build lands the artifact."""
  trace = tmp_path / "rank0.json"
  _write_trace(trace)
  out = tmp_path / "out.json.gz"
  pages._build_single_trace_merge([trace], out, slim=False)
  assert out.is_file()

  manifest = tmp_path / "manifest.json"
  manifest.write_text(json.dumps({"A": []}), encoding="utf-8")
  with pytest.raises(NotATraceError):
    pages._build_single_trace_merge([manifest], tmp_path / "no.json.gz", slim=False)
