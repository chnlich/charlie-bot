"""The launcher (scripts/start-server.sh): tee must outlive Ctrl-C and record the stop.

A terminal Ctrl-C delivers SIGINT to the whole foreground process group, where
the launcher's tee sits. With the plain tee, tee dies on the first signal and
the server's next log write raises BrokenPipeError, so the shutdown lines after
it — up to and including charliebot_shutdown — never reach the log. The fake
`uv` here reproduces a real uvicorn stop: it prints a ready line, then on SIGINT
prints a shutdown marker and exits 0.
"""

from __future__ import annotations

import os
import signal
import subprocess
import sys
import time
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
LAUNCHER = ROOT / "scripts" / "start-server.sh"

READY_LINE = "fake_server_ready"
MARKER_LINE = "fake_server_shutdown_marker"
EXIT_LINE = "launcher: server exited rc=0"

# A `uv` stand-in: ready line, then a uvicorn-shaped stop on SIGINT (print the
# shutdown marker, exit 0). Like uvicorn, it handles the signal itself and exits
# normally — the launcher's trailing lines only run when the pipeline's members
# exit normally.
FAKE_UV = f"""\
#!/usr/bin/env python3
import signal
import sys
import time

print({READY_LINE!r}, flush=True)


def _stop(signum, frame):
    print({MARKER_LINE!r}, flush=True)
    sys.exit(0)


signal.signal(signal.SIGINT, _stop)
while True:
    time.sleep(3600)
"""


def _read_log(log_dir: Path) -> str:
  latest = log_dir / "server-latest.log"
  if not latest.exists():
    return ""
  return latest.read_text(encoding="utf-8", errors="replace")


@pytest.mark.integration
def test_launcher_tee_outlives_sigint_and_records_the_exit_line(tmp_path: Path) -> None:
  """After SIGINT to the whole process group, the log file carries the server's
  shutdown marker followed by the launcher's exit line."""
  bin_dir = tmp_path / "bin"
  bin_dir.mkdir()
  (bin_dir / "uv").write_text(FAKE_UV, encoding="utf-8")
  (bin_dir / "uv").chmod(0o755)
  log_dir = tmp_path / "logs"

  env = os.environ.copy()
  env["PATH"] = f"{bin_dir}{os.pathsep}{env['PATH']}"
  env["CHARLIEBOT_LOG_DIR"] = str(log_dir)

  # A background child of a non-interactive shell inherits SIGINT ignored, while
  # the terminal's launcher has it at default; restore SIG_DFL before exec so
  # the SIGINT below lands the way a terminal Ctrl-C does, and run the launcher
  # as its own process group (the replication scripts/start-server.sh stops
  # under live in).
  sigint_restore = (
      "import os, signal, sys; signal.signal(signal.SIGINT, signal.SIG_DFL); "
      "os.execvp('bash', ['bash', sys.argv[1]])")
  proc = subprocess.Popen(
      [sys.executable, "-c", sigint_restore, str(LAUNCHER)],
      env=env,
      stdin=subprocess.DEVNULL,
      stdout=subprocess.DEVNULL,
      stderr=subprocess.DEVNULL,
      start_new_session=True,
  )
  try:
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
      if READY_LINE in _read_log(log_dir):
        break
      assert proc.poll() is None, f"launcher exited before its server was ready: {_read_log(log_dir)}"
      time.sleep(0.05)
    else:
      pytest.fail(f"the ready line never reached the log: {_read_log(log_dir)!r}")

    os.killpg(os.getpgid(proc.pid), signal.SIGINT)

    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
      log_text = _read_log(log_dir)
      if EXIT_LINE in log_text:
        break
      if proc.poll() is not None:
        break  # the launcher is gone; what the log carries now is what it wrote
      time.sleep(0.05)
    log_text = _read_log(log_dir)
    assert proc.poll() == 0, f"launcher rc={proc.poll()}, log tail: {log_text[-400:]!r}"
    assert MARKER_LINE in log_text, f"the shutdown marker never reached the log: {log_text!r}"
    assert EXIT_LINE in log_text, f"the launcher's exit line never reached the log: {log_text!r}"
    assert log_text.index(MARKER_LINE) < log_text.index(EXIT_LINE), (
        f"the exit line must follow the shutdown marker, got: {log_text!r}")
  finally:
    if proc.poll() is None:
      os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
    proc.wait()
