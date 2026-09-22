"""The turn process tree's nice raise: spawned children land at TURN_TREE_NICE.

The interactive server paths (voice decode, HTTP handlers) share the box with
every turn's CLI and tool subprocesses; the spawn preexec backgrounds the turn
tree so a contended box arbitrates in the interactive path's favor.
"""

import subprocess
import sys

import pytest
from conftest import build_cli_backend_rig

import src.agents.backends.base as backend_base
from src.agents.backends.opencode import OpenCodeBackend
from src.core.process import make_nice_preexec

_CHILD_PRINTS_NICE = "import os; print(os.nice(0))"


def _child_nice(preexec: object) -> int:
  result = subprocess.run(
      [sys.executable, "-c", _CHILD_PRINTS_NICE],
      preexec_fn=preexec,
      capture_output=True,
      text=True,
      check=True,
      timeout=30,
  )
  return int(result.stdout.strip())


def test_make_nice_preexec_raises_child_nice() -> None:
  assert _child_nice(None) == 0
  assert _child_nice(make_nice_preexec(backend_base.TURN_TREE_NICE)) == backend_base.TURN_TREE_NICE


def test_spawn_preexec_lands_turn_tree_nice(monkeypatch: pytest.MonkeyPatch) -> None:
  backend = build_cli_backend_rig(monkeypatch, OpenCodeBackend, cgroup_session_id=None)
  preexec = backend._spawn_preexec(pdeathsig=False)
  assert callable(preexec)
  assert _child_nice(preexec) == backend_base.TURN_TREE_NICE
