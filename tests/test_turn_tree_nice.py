"""The turn process tree's nice raise: the turn tree runs TURN_TREE_NICE backgrounded.

The interactive server paths (voice decode, HTTP handlers) share the box with
every turn's CLI and tool subprocesses; the spawn preexec backgrounds the turn
tree so a contended box arbitrates in the interactive path's favor. The
preexec raise is an increment over the spawner's own nice and saturates at the
kernel's nice ceiling (19); the preexec-free path renices the child to
TURN_TREE_NICE absolute.
"""

import os
import subprocess
import sys
from pathlib import Path

import pytest
from conftest import build_cli_backend_rig

import src.agents.backends.base as backend_base
from src.agents.backends.opencode import OpenCodeBackend
from src.core.process import make_nice_preexec

_CHILD_PRINTS_NICE = "import os; print(os.nice(0))"

# The kernel's nice(2) ceiling: os.nice clamps silently, so a spawner already
# at nice 10 cannot reach baseline + TURN_TREE_NICE (10 + 10 clamps to 19).
_KERNEL_NICE_MAX = 19


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
  # os.nice in the preexec is an increment: the expectation rides a preexec-free
  # sibling's reading because the spawner's own nice is not assumed 0 (the host
  # cron runs this suite niced).
  baseline = _child_nice(None)
  expected = min(baseline + backend_base.TURN_TREE_NICE, _KERNEL_NICE_MAX)
  assert _child_nice(make_nice_preexec(backend_base.TURN_TREE_NICE)) == expected


def test_spawn_preexec_lands_turn_tree_nice(monkeypatch: pytest.MonkeyPatch) -> None:
  backend = build_cli_backend_rig(monkeypatch, OpenCodeBackend, cgroup_session_id=None)
  preexec = backend._spawn_preexec()
  assert callable(preexec)
  expected = min(_child_nice(None) + backend_base.TURN_TREE_NICE, _KERNEL_NICE_MAX)
  assert _child_nice(preexec) == expected


def test_apply_turn_tree_limits_writes_pid_and_renices(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The parent-side application: the child's pid lands in cgroup.procs and the nice raise lands."""
  from src.core.process import SessionCgroup

  procs = tmp_path / "cgroup.procs"
  procs.write_text("")

  class _LimitsBackend(backend_base.AgentBackend):

    def _build_command(self, prompt: str) -> list[str]:
      return ["bash", "-c", "exit 0"]

  backend = _LimitsBackend()
  backend._active_session_cgroup = SessionCgroup(path=tmp_path, memory_max_mb=64, events_before=None)
  seen: dict[str, tuple[int, int]] = {}
  monkeypatch.setattr(os, "setpriority", lambda which, who, prio: seen.setdefault("nice", (who, prio)))
  backend._apply_turn_tree_limits(4242)
  assert procs.read_text() == "4242"
  assert seen["nice"] == (4242, backend_base.TURN_TREE_NICE)


@pytest.mark.asyncio
async def test_raw_log_spawn_lands_turn_tree_nice_parent_side(tmp_path: Path) -> None:
  """The preexec-free raw-log spawn still lands the child at TURN_TREE_NICE.

  run()'s transport spawns without preexec_fn (the vfork fast path) and applies
  the limits parent-side; the child's own nice reading is the contract.
  """

  class _NiceReportingBackend(backend_base.AgentBackend):

    def _build_command(self, prompt: str) -> list[str]:
      return ["bash", "-c", "printf '%s' \"$(python3 -c 'import os; print(os.nice(0))')\""]

  backend = _NiceReportingBackend(log_dir=tmp_path / "logs")
  raw_log = tmp_path / "logs" / "agent.raw.ndjson"
  [event async for event in backend.run("ignored", str(tmp_path), {"PATH": "/usr/bin:/bin"})]
  assert raw_log.read_text().strip() == str(backend_base.TURN_TREE_NICE)
