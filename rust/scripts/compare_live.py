"""Compare the Rust charliebot-server with the Python server.py, live, on two copies of one temp home.

Run from the repository root after `cd rust && cargo build --release`:

    uv run python rust/scripts/compare_live.py

The script writes a synthetic config.yaml (two backends, no integrations) and credentials.yaml (a
synthetic access key) into a fresh home under /tmp, copies the home, and starts server.py on one copy
and rust/target/release/charliebot-server on the other, each on a free port in 18600-18699, with HOME
pointed into the temp directory and outbound HTTP sent to a closed port. The Python server's startup
sweep of session cgroups is replaced with a no-op: the sweep walks the host's shared cgroup tree.

Each check sends one request to both servers with the same Host header and compares the answers:
status, headers (case-insensitive; `date` and `server` dropped), and body. Bodies compare after
gzip decoding (the Python server compresses with ISA-L, the Rust one with zlib, so only the decoded
bytes and the headers around them are comparable), with each home's path replaced by `<home>` and
the version's `started_at` by `<started_at>`. After both servers stop on SIGTERM, the script
compares their `http_request` log events. It prints one PASS or FAIL line per check and exits 1 on
any difference.
"""

import argparse
import dataclasses
import gzip
import http.client
import json
import os
import pathlib
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
RUST_BINARY = REPO_ROOT / "rust" / "target" / "release" / "charliebot-server"
PORT_RANGE = range(18600, 18700)
STARTUP_TIMEOUT_S = 90.0
STOP_TIMEOUT_S = 15.0
HOST_HEADER = "charliebot.test"

ACCESS_KEY = "synthetic-access-key-0001"
CONFIG_YAML = """\
server:
  host: 127.0.0.1
  port: {port}
backends:
  options:
    - id: claude-fable
      label: "CC · Fable"
      type: cc-claude
      model: claude-fable-5-1
    - id: codex-luna
      label: "Codex · Luna"
      type: codex
      model: gpt-6-luna
"""
CREDENTIALS_YAML = f"charliebot:\n  access_key: {ACCESS_KEY}\n"

# server.main() after replacing the startup cgroup sweep, which removes empty cgroups host-wide.
PYTHON_LAUNCHER = """
from src.infra import process
process.sweep_stale_session_cgroups = lambda: None
import server
server.main()
"""

BEARER = {"Authorization": f"Bearer {ACCESS_KEY}"}
COOKIE = {"Cookie": f"charliebot_access_key={ACCESS_KEY}"}
BROWSER = {"Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"}
GZIP = {"Accept-Encoding": "gzip, deflate, br"}

# One file per mount: web/static (a body past one 64 KiB chunk), then four page packages.
STATIC_FILES = (
    "/static/js/chat/turn-engine.js",
    "/static/features/voice/voice-input.js",
    "/static/features/terminal/terminal_panel.js",
    "/static/features/artifacts/artifacts.css",
    "/static/backends/codex/codex.css",
)


@dataclasses.dataclass(frozen=True)
class Check:
  name: str
  method: str
  path: str
  headers: dict[str, str] = dataclasses.field(default_factory=dict)
  websocket: bool = False


def checks() -> list[Check]:
  listed = [
      Check("auth status", "GET", "/api/auth/status"),
      Check("version without the key", "GET", "/api/internal/version"),
      Check("version with the key", "GET", "/api/internal/version", BEARER),
      Check("index without the key", "GET", "/"),
      Check("index with the key cookie", "GET", "/", COOKIE),
      Check("index with the key cookie, gzip", "GET", "/", COOKIE | GZIP),
      Check("browser navigation without the key", "GET", "/api/internal/version", BROWSER),
  ]
  for path in STATIC_FILES:
    listed.append(Check(f"static {path}", "GET", path))
    listed.append(Check(f"static {path}, gzip", "GET", path, GZIP))
  listed += [
      Check("WebSocket upgrade without a token", "GET", "/ws/terminal", websocket=True),
      Check("WebSocket upgrade on an unknown path", "GET", f"/ws/nope?token={ACCESS_KEY}", websocket=True),
      Check("HEAD on a GET route", "HEAD", "/"),
      Check("POST on a GET route", "POST", "/api/auth/status", BEARER),
      Check("unknown path with the key", "GET", "/no/such/route", BEARER),
      Check("unknown path without the key", "GET", "/no/such/route"),
      Check("trailing-slash redirect", "GET", "/api/internal/version/?a=1", BEARER),
      Check("percent-encoded route path", "GET", "/api/auth/%73tatus"),
      Check("bad run token", "GET", "/api/internal/version", {"Authorization": "Bearer eyJ4Ijox.c2ln"}),
      Check("static range", "GET", "/static/css/styles.css", {"Range": "bytes=0-9"}),
      Check("static not modified", "GET", "/static/css/styles.css", {"If-Modified-Since": "Fri, 01 Jan 2100 00:00:00 GMT"}),
      Check("static HEAD", "HEAD", "/static/css/styles.css", GZIP),
      Check("static POST", "POST", "/static/css/styles.css"),
      Check("static directory", "GET", "/static/js/"),
      Check("static traversal", "GET", "/static/../server.py"),
      Check("static missing file", "GET", "/static/js/no-such-file.js"),
      Check("static package without a static directory", "GET", "/static/features/backup/x.js"),
  ]
  return listed


@dataclasses.dataclass
class Reply:
  status: int
  headers: dict[str, list[str]]
  body: bytes


@dataclasses.dataclass
class Server:
  label: str
  home: pathlib.Path
  port: int
  process: subprocess.Popen
  log_path: pathlib.Path


def free_ports(count: int) -> list[int]:
  found = []
  for port in PORT_RANGE:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
      try:
        probe.bind(("127.0.0.1", port))
      except OSError:
        continue
    found.append(port)
    if len(found) == count:
      return found
  raise SystemExit(f"fewer than {count} free ports in {PORT_RANGE.start}-{PORT_RANGE.stop - 1}")


def write_home(home: pathlib.Path, port: int) -> None:
  home.mkdir(parents=True)
  (home / "config.yaml").write_text(CONFIG_YAML.format(port=port), encoding="utf-8")
  (home / "credentials.yaml").write_text(CREDENTIALS_YAML, encoding="utf-8")
  (home / "credentials.yaml").chmod(0o600)


def isolated_env(scratch: pathlib.Path, home: pathlib.Path) -> dict[str, str]:
  real_home = pathlib.Path.home()
  return {
      "PATH": os.environ["PATH"],
      "HOME": str(scratch / "fakehome"),
      "CHARLIEBOT_HOME": str(home),
      "HTTP_PROXY": "http://127.0.0.1:9",
      "HTTPS_PROXY": "http://127.0.0.1:9",
      "NO_PROXY": "127.0.0.1,localhost",
      "UV_CACHE_DIR": os.environ.get("UV_CACHE_DIR", str(real_home / ".cache" / "uv")),
      "UV_PYTHON_INSTALL_DIR": os.environ.get("UV_PYTHON_INSTALL_DIR", str(real_home / ".local" / "share" / "uv" / "python")),
  }


def start(label: str, argv: list[str], env: dict[str, str], home: pathlib.Path, port: int,
          scratch: pathlib.Path) -> Server:
  log_path = scratch / f"{label}.log"
  with log_path.open("wb") as log:
    process = subprocess.Popen(argv, cwd=REPO_ROOT, env=env, stdout=log, stderr=subprocess.STDOUT,
                               start_new_session=True)
  return Server(label, home, port, process, log_path)


def wait_ready(server: Server) -> None:
  deadline = time.monotonic() + STARTUP_TIMEOUT_S
  while time.monotonic() < deadline:
    if server.process.poll() is not None:
      raise SystemExit(f"{server.label} exited with {server.process.returncode} during startup; log:\n"
                       f"{server.log_path.read_text(errors='replace')[-4000:]}")
    try:
      if fetch(server.port, Check("ready", "GET", "/api/auth/status")).status == 200:
        return
    except OSError:
      time.sleep(0.2)
  raise SystemExit(f"{server.label} was not ready within {STARTUP_TIMEOUT_S} s")


def stop(server: Server) -> int:
  """SIGTERM the server's launcher, wait for its process group to empty, and return its shell-style status.

  `uv run` forwards the signal to the Python server and reports a signal death as 128 + the signal.
  Both servers re-raise SIGTERM after their graceful shutdown, so each ends killed by it.
  """
  server.process.send_signal(signal.SIGTERM)
  code = server.process.wait(timeout=STOP_TIMEOUT_S)
  code = 128 - code if code < 0 else code
  deadline = time.monotonic() + STOP_TIMEOUT_S
  while time.monotonic() < deadline:
    try:
      os.killpg(server.process.pid, 0)
    except ProcessLookupError:
      return code
    time.sleep(0.1)
  raise SystemExit(f"{server.label}'s process group outlived {STOP_TIMEOUT_S} s after SIGTERM")


def fetch(port: int, check: Check) -> Reply:
  connection = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
  try:
    connection.putrequest(check.method, check.path, skip_host=True, skip_accept_encoding=True)
    connection.putheader("Host", HOST_HEADER)
    for name, value in check.headers.items():
      connection.putheader(name, value)
    connection.endheaders()
    response = connection.getresponse()
    body = response.read()
    headers: dict[str, list[str]] = {}
    for name, value in response.getheaders():
      headers.setdefault(name.lower(), []).append(value)
    return Reply(response.status, headers, body)
  finally:
    connection.close()


def fetch_websocket(port: int, check: Check) -> Reply:
  request = (f"GET {check.path} HTTP/1.1\r\nHost: {HOST_HEADER}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
             "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n")
  with socket.create_connection(("127.0.0.1", port), timeout=10) as connection:
    connection.sendall(request.encode("ascii"))
    raw = b""
    while chunk := connection.recv(65536):
      raw += chunk
  head, _, body = raw.partition(b"\r\n\r\n")
  status_line, *header_lines = head.decode("latin-1").split("\r\n")
  headers: dict[str, list[str]] = {}
  for line in header_lines:
    name, _, value = line.partition(":")
    headers.setdefault(name.strip().lower(), []).append(value.strip())
  return Reply(int(status_line.split()[1]), headers, body)


def normalized(reply: Reply, server: Server) -> Reply:
  headers = {name: sorted(values) for name, values in reply.headers.items() if name not in ("date", "server")}
  body = reply.body
  if headers.get("content-encoding") == ["gzip"]:
    body = gzip.decompress(body)
    headers.pop("content-length", None)
  body = body.replace(str(server.home).encode(), b"<home>")
  if reply.headers.get("content-type") == ["application/json"] and b"started_at" in body:
    document = json.loads(body)
    document["started_at"] = "<started_at>"
    body = json.dumps(document).encode()
  return Reply(reply.status, headers, body)


def differences(python: Reply, rust: Reply) -> list[str]:
  found = []
  if python.status != rust.status:
    found.append(f"status {python.status} != {rust.status}")
  for name in sorted(set(python.headers) | set(rust.headers)):
    if python.headers.get(name) != rust.headers.get(name):
      found.append(f"header {name}: {python.headers.get(name)} != {rust.headers.get(name)}")
  if python.body != rust.body:
    offset = next((i for i, (a, b) in enumerate(zip(python.body, rust.body)) if a != b),
                  min(len(python.body), len(rust.body)))
    found.append(f"body differs at byte {offset} (lengths {len(python.body)} and {len(rust.body)}): "
                 f"{python.body[offset:offset + 60]!r} != {rust.body[offset:offset + 60]!r}")
  return found


def python_request_events(log_text: str) -> list[tuple[str, ...]]:
  """(method, path, status, error) of each http_request line in structlog's console format."""
  events = []
  for line in log_text.splitlines():
    if "] http_request " not in line:
      continue
    fields = dict(re.findall(r"(\w+)=(\S+)", line.split("] http_request ", 1)[1]))
    events.append((fields["method"], fields["path"], fields["status"], fields.get("error", "")))
  return events


def rust_request_events(log_text: str) -> list[tuple[str, ...]]:
  events = []
  for line in log_text.splitlines():
    if not line.startswith("{"):
      continue
    event = json.loads(line)
    if event["event"] == "http_request":
      events.append((event["method"], event["path"], str(event["status"]), event.get("error", "")))
  return events


def main() -> int:
  parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
  parser.add_argument("--keep", action="store_true", help="keep the temp directory after a passing run")
  args = parser.parse_args()
  if not RUST_BINARY.is_file():
    raise SystemExit(f"{RUST_BINARY} is missing; run `cargo build --release` in rust/ first")

  scratch = pathlib.Path(tempfile.mkdtemp(prefix="charliebot-compare-", dir="/tmp"))
  (scratch / "fakehome").mkdir()
  python_port, rust_port = free_ports(2)
  write_home(scratch / "py" / "home", python_port)
  shutil.copytree(scratch / "py" / "home", scratch / "rs" / "home")
  (scratch / "rs" / "home" / "config.yaml").write_text(CONFIG_YAML.format(port=rust_port), encoding="utf-8")

  python_env = isolated_env(scratch, scratch / "py" / "home")
  rust_env = isolated_env(scratch, scratch / "rs" / "home") | {"CHARLIEBOT_REPO_ROOT": str(REPO_ROOT)}
  servers = [
      start("python", ["uv", "run", "--no-sync", "python", "-c", PYTHON_LAUNCHER], python_env,
            scratch / "py" / "home", python_port, scratch),
      start("rust", [str(RUST_BINARY)], rust_env, scratch / "rs" / "home", rust_port, scratch),
  ]
  failures = 0
  try:
    for server in servers:
      wait_ready(server)
    print(f"python on :{python_port}, rust on :{rust_port}, scratch {scratch}")
    listed = checks()
    for check in listed:
      fetcher = fetch_websocket if check.websocket else fetch
      python, rust = (normalized(fetcher(server.port, check), server) for server in servers)
      found = differences(python, rust)
      summary = f"{python.status}" + (f" {python.headers['content-encoding'][0]}" if "content-encoding" in python.headers else "")
      if found:
        failures += 1
        print(f"FAIL {check.name} [{check.method} {check.path}]")
        for line in found:
          print(f"     {line}")
      else:
        print(f"PASS {check.name} [{check.method} {check.path}] -> {summary}, {len(python.body)} bytes")
  finally:
    codes = {server.label: stop(server) for server in servers}
  print(f"exit statuses after SIGTERM: {codes}")

  logs = {server.label: server.log_path.read_text(errors="replace") for server in servers}
  http_checks = sum(1 for check in checks() if not check.websocket)
  python_events = python_request_events(logs["python"])[-http_checks:]
  rust_events = rust_request_events(logs["rust"])[-http_checks:]
  if python_events == rust_events and len(rust_events) == http_checks:
    print(f"PASS http_request events: the last {http_checks} match in method, path, status and error")
  else:
    failures += 1
    print("FAIL http_request events")
    for python, rust in zip(python_events, rust_events):
      if python != rust:
        print(f"     {python} != {rust}")
  for label in ("python", "rust"):
    if codes[label] != 128 + signal.SIGTERM:
      failures += 1
      print(f"FAIL {label} ended with status {codes[label]} after SIGTERM, not {128 + signal.SIGTERM}")
  shutdown_logged = any(json.loads(line)["event"] == "charliebot_shutdown"
                        for line in logs["rust"].splitlines() if line.startswith("{"))
  if not shutdown_logged:
    failures += 1
    print("FAIL rust logged no charliebot_shutdown")

  print(f"{failures} failure(s)")
  if failures or args.keep:
    print(f"logs and homes kept in {scratch}")
  else:
    shutil.rmtree(scratch)
  return 1 if failures else 0


if __name__ == "__main__":
  sys.exit(main())
