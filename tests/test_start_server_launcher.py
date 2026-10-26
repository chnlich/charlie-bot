"""The launcher (scripts/start-server.sh): tee must outlive Ctrl-C and record the stop.

A terminal Ctrl-C delivers SIGINT to the whole foreground process group, where
the launcher's tee sits. With the plain tee, tee dies on the first signal and
the server's next log write raises BrokenPipeError, so the shutdown lines after
it — up to and including charliebot_shutdown — never reach the log. The fake
`uv` here reproduces a real uvicorn stop: it prints a ready line, then on SIGINT
prints a shutdown marker and exits 0.
"""

from __future__ import annotations

import json
import os
import pathlib
import re
import signal
import subprocess
import sys
import time

import pytest
import systemd_stubs

ROOT = pathlib.Path(__file__).resolve().parents[1]
LAUNCHER = ROOT / "scripts" / "start-server.sh"
RELEASE_SHA = "0123456789abcdef0123456789abcdef01234567"

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


def _read_log(log_dir: pathlib.Path) -> str:
  latest = log_dir / "server-latest.log"
  if not latest.exists():
    return ""
  return latest.read_text(encoding="utf-8", errors="replace")


def _assert_server_scope(proc: subprocess.CompletedProcess[str], bin_dir: pathlib.Path) -> None:
  records = systemd_stubs.read_systemd_run_records(bin_dir)
  assert len(records) == 1
  record = records[0]
  unit_name = str(record["unit_name"])
  assert re.fullmatch(r"charliebot-server-\d{8}_\d{6}-\d+", unit_name)
  assert f"  scope  -> {unit_name}" in proc.stdout.splitlines()
  arguments = record["arguments"]
  assert isinstance(arguments, list)
  assert f"--unit={unit_name}" in arguments
  assert "--slice=charliebot.slice" in arguments
  assert arguments[arguments.index("-p") + 1] == "MemorySwapMax=0"


@pytest.mark.integration
def test_launcher_tee_outlives_sigint_and_records_the_exit_line(tmp_path: pathlib.Path) -> None:
  """After SIGINT to the whole process group, the log file carries the server's
  shutdown marker followed by the launcher's exit line."""
  bin_dir = tmp_path / "bin"
  bin_dir.mkdir()
  (bin_dir / "uv").write_text(FAKE_UV, encoding="utf-8")
  (bin_dir / "uv").chmod(0o755)
  systemd_stubs.install(bin_dir)
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


# A `uv` stand-in for the Python branch: it records its argv and PYTHONUNBUFFERED, prints the ready line and exits
# 0, so the launcher finishes without a signal.
FAKE_UV_RECORDING = f"""\
#!/usr/bin/env python3
import json
import os
import sys

with open(os.environ["FAKE_RECORD"], "w", encoding="utf-8") as record:
  json.dump({{"argv": sys.argv[1:], "PYTHONUNBUFFERED": os.environ.get("PYTHONUNBUFFERED")}}, record)
print({READY_LINE!r}, flush=True)
"""

# A release binary stand-in: --check-config exits 3 with an error when the release holds a check-config-fails marker.
# A real run records the two variables the launcher must set, prints a ready line and exits 0.
FAKE_RELEASE_BINARY = """\
#!/usr/bin/env python3
import json
import os
import pathlib
import sys

here = pathlib.Path(__file__).resolve().parent
if "--check-config" in sys.argv:
  if (here / "check-config-fails").exists():
    print("config error: unknown key server.bogus", file=sys.stderr)
    sys.exit(3)
  sys.exit(0)
record = {key: os.environ.get(key) for key in ("CHARLIEBOT_REPO_ROOT", "CHARLIEBOT_VENV_BIN")}
(here / "run.json").write_text(json.dumps(record), encoding="utf-8")
print("fake_release_ready", flush=True)
"""


def _install_release(releases: pathlib.Path) -> pathlib.Path:
  """A release directory holding the fake binary, with current pointing at it."""
  release = releases / RELEASE_SHA
  release.mkdir(parents=True)
  binary = release / "charliebot-server"
  binary.write_text(FAKE_RELEASE_BINARY, encoding="utf-8")
  binary.chmod(0o755)
  (releases / "current").symlink_to(RELEASE_SHA)
  return release


def _run_launcher(tmp_path: pathlib.Path,
                  releases: pathlib.Path) -> tuple[subprocess.CompletedProcess[str], pathlib.Path]:
  """Run the launcher once with the recording uv on PATH; returns its result and its log directory."""
  bin_dir = tmp_path / "bin"
  bin_dir.mkdir(exist_ok=True)
  uv = bin_dir / "uv"
  uv.write_text(FAKE_UV_RECORDING, encoding="utf-8")
  uv.chmod(0o755)
  systemd_stubs.install(bin_dir)
  log_dir = tmp_path / "logs"
  env = os.environ.copy()
  env["PATH"] = f"{bin_dir}{os.pathsep}{env['PATH']}"
  env["CHARLIEBOT_LOG_DIR"] = str(log_dir)
  env["CHARLIEBOT_RELEASES_DIR"] = str(releases)
  env["FAKE_RECORD"] = str(tmp_path / "uv-record.json")
  proc = subprocess.run(
      ["bash", str(LAUNCHER)],
      env=env,
      stdin=subprocess.DEVNULL,
      capture_output=True,
      text=True,
      timeout=30,
      check=False)
  return proc, log_dir


def test_launcher_without_current_runs_the_python_server_as_before(tmp_path: pathlib.Path) -> None:
  """No current release: the same uv command, the same console and log lines, no release binary involved."""
  releases = tmp_path / "releases"
  releases.mkdir()
  proc, log_dir = _run_launcher(tmp_path, releases)

  assert proc.returncode == 0, proc.stderr
  _assert_server_scope(proc, tmp_path / "bin")
  record = json.loads((tmp_path / "uv-record.json").read_text(encoding="utf-8"))
  assert record == {"argv": ["run", "python3", "server.py"], "PYTHONUNBUFFERED": "1"}
  assert proc.stdout.splitlines()[0] == f"CharlieBot server starting (repo: {ROOT})"
  log_lines = _read_log(log_dir).splitlines()
  assert log_lines[0] == READY_LINE
  assert EXIT_LINE in log_lines[-1]


def test_launcher_with_current_runs_the_release_binary_with_both_variables(tmp_path: pathlib.Path) -> None:
  """A current release starts its binary with the runtime clone and its venv bin, and names it first in the log."""
  releases = tmp_path / "releases"
  release = _install_release(releases)
  proc, log_dir = _run_launcher(tmp_path, releases)

  assert (release / "run.json").exists(), proc.stderr
  assert proc.returncode == 0, proc.stderr
  _assert_server_scope(proc, tmp_path / "bin")
  assert json.loads((release / "run.json").read_text(encoding="utf-8")) == {
      "CHARLIEBOT_REPO_ROOT": str(ROOT),
      "CHARLIEBOT_VENV_BIN": str(ROOT / ".venv" / "bin"),
  }
  assert not (tmp_path / "uv-record.json").exists(), "the Python branch must not run for a current release"
  log_lines = _read_log(log_dir).splitlines()
  assert "rust" in log_lines[0] and RELEASE_SHA in log_lines[0]
  assert "fake_release_ready" in log_lines


def test_launcher_with_current_stops_when_check_config_fails_and_names_the_rollback(tmp_path: pathlib.Path) -> None:
  """A failing --check-config ends the launcher with its status, prints the rollback command, and never serves."""
  releases = tmp_path / "releases"
  release = _install_release(releases)
  (release / "check-config-fails").write_text("", encoding="utf-8")
  proc, _ = _run_launcher(tmp_path, releases)

  assert proc.returncode == 3, proc.stdout + proc.stderr
  assert "config error" in proc.stderr
  assert "scripts/switch-server.sh python" in proc.stdout
  assert not (release / "run.json").exists()
  assert not (tmp_path / "uv-record.json").exists()
