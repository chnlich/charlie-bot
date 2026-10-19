"""The scratch tree lifecycle: the run end decision and the start sweep."""

import json
import os
import pathlib
import subprocess
import sys
import time

from rust.parity import scratch

PARITY_DIR = pathlib.Path(__file__).resolve().parent
AGE = 7 * 60 * 60


def _make_tree(root: pathlib.Path, name: str = "charliebot-parity-test") -> pathlib.Path:
  tree = root / name
  tree.mkdir()
  return tree


def _record_from_dead_process(tree: pathlib.Path) -> None:
  """Record an owner pid through a child that exits before the sweep runs."""
  subprocess.run(
      [
          sys.executable, "-c", "import pathlib, sys; sys.path.insert(0, sys.argv[2]); import scratch; "
          "scratch.record_owner(pathlib.Path(sys.argv[1]))",
          str(tree),
          str(PARITY_DIR)
      ],
      check=True)


def test_finalize_removes_a_passing_run_tree(tmp_path: pathlib.Path, capsys) -> None:
  tree = _make_tree(tmp_path)
  scratch.finalize(tree, keep=False, clean=True, runs=[])
  assert not tree.exists()
  assert capsys.readouterr().out == ""


def test_finalize_removes_a_failing_run_tree_and_names_keep(tmp_path: pathlib.Path, capsys) -> None:
  tree = _make_tree(tmp_path)
  scratch.finalize(tree, keep=False, clean=False, runs=[])
  assert not tree.exists()
  assert "--keep" in capsys.readouterr().out


def test_finalize_keeps_a_requested_tree_with_its_marker(tmp_path: pathlib.Path, capsys) -> None:
  tree = _make_tree(tmp_path)
  runs = [{"run": "00-01", "scenario": "s", "direction": None, "trees": {}}]
  scratch.finalize(tree, keep=True, clean=False, runs=runs)
  assert json.loads((tree / scratch.RUNS_MANIFEST).read_text()) == runs
  assert (tree / scratch.KEEP_MARKER).is_file()
  assert "kept" in capsys.readouterr().out


def test_sweep_removes_a_tree_whose_owning_process_is_gone(tmp_path: pathlib.Path) -> None:
  tree = _make_tree(tmp_path)
  _record_from_dead_process(tree)
  scratch.sweep(root=tmp_path)
  assert not tree.exists()


def test_sweep_keeps_a_tree_of_a_live_process(tmp_path: pathlib.Path) -> None:
  tree = _make_tree(tmp_path)
  scratch.record_owner(tree)
  scratch.sweep(root=tmp_path)
  assert tree.exists()


def test_sweep_keeps_a_marked_tree_of_a_dead_process(tmp_path: pathlib.Path) -> None:
  tree = _make_tree(tmp_path)
  _record_from_dead_process(tree)
  (tree / scratch.KEEP_MARKER).touch()
  scratch.sweep(root=tmp_path)
  assert tree.exists()


def test_sweep_removes_an_unrecorded_tree_older_than_six_hours(tmp_path: pathlib.Path) -> None:
  tree = _make_tree(tmp_path)
  old = time.time() - AGE
  os.utime(tree, (old, old))
  scratch.sweep(root=tmp_path)
  assert not tree.exists()


def test_sweep_keeps_a_fresh_unrecorded_tree(tmp_path: pathlib.Path) -> None:
  # A run between mkdtemp and its owner record has no record yet and must survive a concurrent sweep.
  tree = _make_tree(tmp_path)
  scratch.sweep(root=tmp_path)
  assert tree.exists()


def test_sweep_leaves_the_port_lock_directory(tmp_path: pathlib.Path) -> None:
  tree = _make_tree(tmp_path, name=scratch.PORT_LOCK_DIR.name)
  old = time.time() - AGE
  os.utime(tree, (old, old))
  scratch.sweep(root=tmp_path)
  assert tree.exists()
