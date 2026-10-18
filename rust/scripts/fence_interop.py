"""Check that the Rust and Python home writer fences exclude each other, in both directions.

Run from the repository root after `cd rust && cargo build --release`:

    uv run python rust/scripts/fence_interop.py

Rust first: with charliebot-server running on a fresh home under /tmp, Python's
`acquire_home_writer_fence(home, purpose="test")` must raise `HomeWriterActiveError` naming the Rust
server's pid; once the server stops on SIGTERM, the identity record is gone and Python acquires.
Python first: with a Python process holding the fence, charliebot-server must exit 3 and log
`server_startup_refused_home_writer_fence` naming the Python holder's pid and purpose. Exits 1 on
any failed expectation.
"""

import json
import os
import pathlib
import signal
import socket
import subprocess
import sys
import tempfile
import time

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
RUST_BINARY = REPO_ROOT / "rust" / "target" / "release" / "charliebot-server"
PORT_RANGE = range(18600, 18700)
TIMEOUT_S = 60.0

CONFIG_YAML = """\
server:
  host: 127.0.0.1
  port: {port}
backends:
  options:
    - id: claude-fable
      label: CC Fable
      type: cc-claude
      model: claude-fable-5-1
"""

# Try the fence once; print the refusal or the acquisition as one JSON line.
PYTHON_TRY = """
import json, pathlib, sys
from src.runtime import home_writer_fence
try:
  fence = home_writer_fence.acquire_home_writer_fence(pathlib.Path(sys.argv[1]), purpose="test")
except home_writer_fence.HomeWriterActiveError as error:
  print(json.dumps({"refused": type(error).__name__, "message": str(error)}))
else:
  fence.release()
  print(json.dumps({"acquired": True}))
"""

# Hold the fence until stdin closes.
PYTHON_HOLD = """
import os, pathlib, sys
from src.runtime import home_writer_fence
fence = home_writer_fence.acquire_home_writer_fence(pathlib.Path(sys.argv[1]), purpose="python holder")
print(f"held {os.getpid()}", flush=True)
sys.stdin.read()
fence.release()
"""


def free_port() -> int:
  for port in PORT_RANGE:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
      try:
        probe.bind(("127.0.0.1", port))
      except OSError:
        continue
      return port
  raise SystemExit(f"no free port in {PORT_RANGE.start}-{PORT_RANGE.stop - 1}")


def environment(scratch: pathlib.Path, home: pathlib.Path) -> dict[str, str]:
  real_home = pathlib.Path.home()
  return {
      "PATH": os.environ["PATH"],
      "HOME": str(scratch / "fakehome"),
      "CHARLIEBOT_HOME": str(home),
      "CHARLIEBOT_REPO_ROOT": str(REPO_ROOT),
      "CHARLIEBOT_VENV_BIN": str(REPO_ROOT / ".venv" / "bin"),
      "HTTP_PROXY": "http://127.0.0.1:9",
      "HTTPS_PROXY": "http://127.0.0.1:9",
      "NO_PROXY": "127.0.0.1,localhost",
      "UV_CACHE_DIR": os.environ.get("UV_CACHE_DIR", str(real_home / ".cache" / "uv")),
      "UV_PYTHON_INSTALL_DIR": os.environ.get("UV_PYTHON_INSTALL_DIR", str(real_home / ".local" / "share" / "uv" / "python")),
  }


def python(code: str, home: pathlib.Path, env: dict[str, str], **kwargs: object) -> subprocess.Popen:
  return subprocess.Popen(["uv", "run", "--no-sync", "python", "-c", code, str(home)], cwd=REPO_ROOT, env=env,
                          stdout=subprocess.PIPE, text=True, **kwargs)


def try_fence(home: pathlib.Path, env: dict[str, str]) -> dict:
  process = python(PYTHON_TRY, home, env)
  out, _ = process.communicate(timeout=TIMEOUT_S)
  if process.returncode != 0:
    raise SystemExit(f"the Python fence attempt exited {process.returncode}")
  return json.loads(out.strip().splitlines()[-1])


def wait_listening(port: int, process: subprocess.Popen) -> None:
  deadline = time.monotonic() + TIMEOUT_S
  while time.monotonic() < deadline:
    if process.poll() is not None:
      raise SystemExit(f"charliebot-server exited {process.returncode} during startup")
    try:
      socket.create_connection(("127.0.0.1", port), timeout=1).close()
      return
    except OSError:
      time.sleep(0.1)
  raise SystemExit(f"charliebot-server did not listen on {port} within {TIMEOUT_S} s")


def expect(condition: bool, description: str, failures: list[str]) -> None:
  print(f"{'PASS' if condition else 'FAIL'} {description}")
  if not condition:
    failures.append(description)


def rust_first(scratch: pathlib.Path, failures: list[str]) -> None:
  home = scratch / "rust-first" / "home"
  home.mkdir(parents=True)
  port = free_port()
  (home / "config.yaml").write_text(CONFIG_YAML.format(port=port), encoding="utf-8")
  env = environment(scratch, home)
  sandbox = REPO_ROOT / "rust" / "parity" / "sandbox.py"
  log_path = scratch / "rust-first.log"
  with log_path.open("wb") as log:
    server = subprocess.Popen(
        [sys.executable, str(sandbox), "--allow-write", str(scratch), "--", str(RUST_BINARY)],
        cwd=scratch, env=env, stdout=log, stderr=subprocess.STDOUT)
  try:
    wait_listening(port, server)
    outcome = try_fence(home, env)
    expect(outcome.get("refused") == "HomeWriterActiveError",
           f"Python's acquire refuses while Rust holds the fence: {outcome}", failures)
    expect(f"held by pid {server.pid} " in outcome.get("message", ""), "the refusal names the Rust server's pid",
           failures)
  finally:
    server.send_signal(signal.SIGTERM)
    server.wait(timeout=TIMEOUT_S)
  expect(not (home / "state" / "writer_identity.json").exists(), "the Rust server removed its identity record on exit",
         failures)
  events = [json.loads(line)["event"] for line in log_path.read_text().splitlines() if line.startswith("{")]
  expect(events[-1] == "charliebot_shutdown", "the Rust server logged charliebot_shutdown last", failures)
  outcome = try_fence(home, env)
  expect(outcome == {"acquired": True}, f"Python acquires after the Rust server stops: {outcome}", failures)


def python_first(scratch: pathlib.Path, failures: list[str]) -> None:
  home = scratch / "python-first" / "home"
  home.mkdir(parents=True)
  (home / "config.yaml").write_text(CONFIG_YAML.format(port=free_port()), encoding="utf-8")
  env = environment(scratch, home)
  holder = python(PYTHON_HOLD, home, env, stdin=subprocess.PIPE)
  try:
    # The holder's own structlog lines share its stdout; the "held <pid>" line follows them.
    line = []
    while line[:1] != ["held"]:
      text = holder.stdout.readline()
      if not text:
        raise SystemExit("the Python holder exited before taking the fence")
      line = text.split()
    holder_pid = int(line[1])
    sandbox = REPO_ROOT / "rust" / "parity" / "sandbox.py"
    server = subprocess.run(
        [sys.executable, str(sandbox), "--allow-write", str(scratch), "--", str(RUST_BINARY)],
        cwd=scratch, env=env, capture_output=True, text=True, timeout=TIMEOUT_S)
    expect(server.returncode == 3, f"charliebot-server exits 3 while Python holds the fence (got {server.returncode})",
           failures)
    refusals = [json.loads(line) for line in server.stdout.splitlines() if line.startswith("{")]
    refusals = [event for event in refusals if event["event"] == "server_startup_refused_home_writer_fence"]
    message = refusals[0]["error"] if refusals else ""
    print(f"     refusal: {message}")
    expect(f"held by pid {holder_pid} " in message and "purpose 'python holder'" in message,
           "the refusal names the Python holder's pid and purpose", failures)
  finally:
    holder.stdin.close()
    holder.wait(timeout=TIMEOUT_S)


def main() -> int:
  if not RUST_BINARY.is_file():
    raise SystemExit(f"{RUST_BINARY} is missing; run `cargo build --release` in rust/ first")
  failures: list[str] = []
  with tempfile.TemporaryDirectory(prefix="charliebot-fence-", dir="/tmp") as scratch_name:
    scratch = pathlib.Path(scratch_name)
    (scratch / "fakehome").mkdir()
    rust_first(scratch, failures)
    python_first(scratch, failures)
  print(f"{len(failures)} failure(s)")
  return 1 if failures else 0


if __name__ == "__main__":
  sys.exit(main())
