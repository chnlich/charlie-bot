"""The sandbox's TCP connect rule: a process under sandbox.py reaches the proxy and unprivileged ports only."""

import json
import pathlib
import subprocess
import sys

SANDBOX = pathlib.Path(__file__).resolve().with_name("sandbox.py")

# Runs inside the ruleset. Each connect reports its exception class, or "connected".
PROBE = """
import json
import socket

def connect(port):
  with socket.socket() as sock:
    try:
      sock.connect(("127.0.0.1", port))
    except OSError as error:
      return type(error).__name__
    return "connected"

listener = socket.socket()
listener.bind(("127.0.0.1", 0))
listener.listen()
print(json.dumps({
    "443": connect(443),
    "22": connect(22),
    "9": connect(9),
    "listener": connect(listener.getsockname()[1]),
}))
"""


def test_sandboxed_connects_reach_only_the_proxy_and_unprivileged_ports(tmp_path: pathlib.Path) -> None:
  # The probe binds a listener first, so bind() working under the ruleset is part of the check.
  argv = [sys.executable, str(SANDBOX), "--allow-write", str(tmp_path), "--", sys.executable, "-I", "-c", PROBE]
  result = subprocess.run(argv, capture_output=True, text=True, check=True, timeout=30)
  assert json.loads(result.stdout) == {
      "443": "PermissionError",
      "22": "PermissionError",
      "9": "ConnectionRefusedError",
      "listener": "connected",
  }
