"""Tests for the subprocess TERM→KILL escalation in src/core/process.py.

The seven call sites lean on the return for the reap: whatever runs after the
call (temp-directory removal, port rebinding) must never race the dead child,
so the escalation path has to end with a completed wait, not just a signal.
"""

import subprocess
import sys

from src.core.process import terminate_and_wait


def test_terminate_ends_a_cooperative_child_with_sigterm():
  proc = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
  terminate_and_wait(proc, term_timeout_s=10, kill_timeout_s=10)
  assert proc.returncode == -15


def test_ignored_sigterm_escalates_to_sigkill_and_reaps():
  # The child must have the handler installed before the TERM arrives, so it
  # announces readiness and the test reads that line before escalating.
  proc = subprocess.Popen(
      [sys.executable, "-c",
       "import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); print('ready', flush=True); time.sleep(30)"],
      stdout=subprocess.PIPE, text=True)
  assert proc.stdout.readline().strip() == "ready"
  terminate_and_wait(proc, term_timeout_s=0.2, kill_timeout_s=10)
  assert proc.returncode == -9


def test_already_exited_child_is_a_noop():
  done = subprocess.Popen([sys.executable, "-c", "print('done')"])
  done.wait()
  terminate_and_wait(done, term_timeout_s=10, kill_timeout_s=10)
  assert done.returncode == 0
