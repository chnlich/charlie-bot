"""PR_SET_PDEATHSIG=SIGKILL binding: piped-backend children die with the parent.

Mechanism-level acceptance for the orphan-serve fault class: an intermediate
process spawning a long-lived child through the helper's preexec loses that
child to the kernel no matter how the intermediate dies (SIGKILL, unhandled
SIGTERM), while a control pair spawned without the helper keeps its child
alive — proving the mechanism, not some ambient behavior, authored the reap.
"""

import os
import signal
import subprocess
import sys
import time
from pathlib import Path

import pytest

from src.core import process as core_process

REPO_ROOT = Path(__file__).resolve().parents[1]

# Intermediate: spawn a long-lived child through the real asyncio spawn path
# (the preexec helper only when told), print the child's pid, then stay alive
# until the test signals it. Python installs no SIGTERM handler, so both death
# modes the test exercises are genuinely unhandled.
_INTERMEDIATE = """\
import asyncio
import sys

sys.path.insert(0, sys.argv[1])

from src.core.process import make_pdeathsig_kill_preexec


async def main() -> None:
    proc = await asyncio.create_subprocess_exec(
        sys.executable,
        "-c",
        "import time; time.sleep(60)",
        stdin=asyncio.subprocess.DEVNULL,
        stdout=asyncio.subprocess.DEVNULL,
        stderr=asyncio.subprocess.DEVNULL,
        start_new_session=True,
        preexec_fn=make_pdeathsig_kill_preexec() if sys.argv[2] == "with-helper" else None,
    )
    print(proc.pid, flush=True)
    await asyncio.Event().wait()


asyncio.run(main())
"""

# How long a child may outlive its parent before the test considers it leaked.
_DISAPPEAR_TIMEOUT = 2.0

linux_only = pytest.mark.skipif(sys.platform != "linux", reason="PR_SET_PDEATHSIG is Linux-only")


def _pid_alive(pid: int) -> bool:
  try:
    os.kill(pid, 0)
  except ProcessLookupError:
    return False
  return True


def _spawn_intermediate(tmp_path: Path, *, use_helper: bool) -> tuple[subprocess.Popen, int]:
  """Spawn the intermediate and return it with its long-lived child's pid."""
  script = tmp_path / f"intermediate_{'helper' if use_helper else 'plain'}.py"
  script.write_text(_INTERMEDIATE, encoding="utf-8")
  intermediate = subprocess.Popen(
      [sys.executable, str(script),
       str(REPO_ROOT), "with-helper" if use_helper else "plain"],
      stdout=subprocess.PIPE,
      stderr=subprocess.PIPE,
      text=True,
  )
  assert intermediate.stdout is not None
  line = intermediate.stdout.readline().strip()
  if not line:
    raise AssertionError(
        f"intermediate exited at setup; stderr: {intermediate.stderr.read() if intermediate.stderr else ''}")
  child_pid = int(line)
  assert _pid_alive(intermediate.pid), "intermediate not alive at setup"
  assert _pid_alive(child_pid), "child not alive at setup"
  return intermediate, child_pid


def _cleanup(intermediate: subprocess.Popen, child_pid: int) -> None:
  """Kill whatever survives; each pid probe makes re-killing an already-dead pid a no-op."""
  for pid in (intermediate.pid, child_pid):
    if _pid_alive(pid):
      os.kill(pid, signal.SIGKILL)
  intermediate.wait(timeout=10)


def _wait_child_gone(child_pid: int) -> bool:
  deadline = time.monotonic() + _DISAPPEAR_TIMEOUT
  while time.monotonic() < deadline and _pid_alive(child_pid):
    time.sleep(0.05)
  return not _pid_alive(child_pid)


def test_recheck_decision_self_kills_on_parent_mismatch() -> None:
  assert core_process._pdeathsig_should_self_kill(1234, 1) is True


def test_recheck_decision_continues_on_parent_match() -> None:
  assert core_process._pdeathsig_should_self_kill(1234, 1234) is False


def test_preexec_factory_returns_none_off_linux(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr(sys, "platform", "darwin")
  assert core_process.make_pdeathsig_kill_preexec() is None


@linux_only
def test_preexec_factory_builds_callable_on_linux() -> None:
  assert callable(core_process.make_pdeathsig_kill_preexec())


@linux_only
@pytest.mark.parametrize("sig", [signal.SIGKILL, signal.SIGTERM], ids=lambda sig: sig.name)
def test_signal_to_intermediate_kernel_reaps_child(tmp_path: Path, sig: signal.Signals) -> None:
  intermediate, child_pid = _spawn_intermediate(tmp_path, use_helper=True)
  try:
    os.kill(intermediate.pid, sig)
    intermediate.wait(timeout=10)
    assert intermediate.returncode == -sig
    reaped = _wait_child_gone(child_pid)
  finally:
    _cleanup(intermediate, child_pid)
  assert reaped, f"child survived its parent's {sig.name} by more than 2 s — PDEATHSIG did not fire"


@linux_only
def test_control_without_helper_child_survives_parent_death(tmp_path: Path) -> None:
  """Without the helper the child must survive the same wait — otherwise the two
  helper legs prove nothing about which mechanism authored the reap."""
  intermediate, child_pid = _spawn_intermediate(tmp_path, use_helper=False)
  try:
    os.kill(intermediate.pid, signal.SIGKILL)
    intermediate.wait(timeout=10)
    assert intermediate.returncode == -signal.SIGKILL
    time.sleep(_DISAPPEAR_TIMEOUT)
    alive_after_wait = _pid_alive(child_pid)
  finally:
    _cleanup(intermediate, child_pid)
  assert alive_after_wait, ("control child was reaped without the helper — the reap is not authored by PDEATHSIG")
