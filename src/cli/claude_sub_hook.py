"""Opaque command-hook transport helper used by ``claude-sub``.

The helper intentionally does not interpret Claude Code hook fields.  It authenticates
and forwards the complete JSON object to the per-turn bridge, then applies the bridge's
allow/fail result to Claude's command-hook contract.

The registered command runs this file with ``-S`` (site skipped), and the helper is
machine-invoked once per hook event, so its import floor stays stdlib (no argparse,
no typing) plus the one ``src`` import below, which resolves through the repo root
``__file__`` names.  The argv contract is exactly what ``claude_sub`` registers:
``--socket PATH --token TOKEN [--gate]``.
"""

from __future__ import annotations

import json
import os
import signal
import socket
import sys

_USAGE = "usage: claude_sub_hook.py --socket PATH --token TOKEN [--gate]"


def _terminate_parent_group() -> None:
  parent_pid = os.getppid()
  try:
    process_group = os.getpgid(parent_pid)
    os.killpg(process_group, signal.SIGTERM)
  except OSError as error:
    print(f"claude-sub hook helper could not terminate Claude process group: {error}", file=sys.stderr)


def _fail(message: str, *, gate: bool, terminate_parent: bool) -> int:
  print(f"claude-sub hook bridge: {message}", file=sys.stderr)
  if terminate_parent:
    _terminate_parent_group()
  return 2 if gate else 1


def _parse_hook_argv(argv: list[str]) -> tuple[str, str, bool]:
  """The registered argv contract, parsed strictly; any deviation is a misconfiguration."""
  socket_path: str | None = None
  token: str | None = None
  gate = False
  i = 0
  while i < len(argv):
    flag = argv[i]
    if flag == "--socket" and i + 1 < len(argv):
      socket_path = argv[i + 1]
      i += 2
    elif flag == "--token" and i + 1 < len(argv):
      token = argv[i + 1]
      i += 2
    elif flag == "--gate":
      gate = True
      i += 1
    else:
      raise ValueError(f"unexpected hook argument {flag!r}; {_USAGE}")
  if socket_path is None or token is None:
    raise ValueError(f"--socket and --token are required; {_USAGE}")
  return socket_path, token, gate


def _send_request(socket_path: str, token: str, gate: bool, payload: dict[str, object],
                  socket_timeout: float) -> dict[str, object]:
  envelope = {
      "token": token,
      "gate": gate,
      "payload": payload,
  }
  with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
    client.settimeout(socket_timeout)
    client.connect(socket_path)
    client.sendall((json.dumps(envelope, ensure_ascii=False, separators=(",", ":")) + "\n").encode("utf-8"))
    response = b""
    while not response.endswith(b"\n"):
      chunk = client.recv(4096)
      if not chunk:
        raise RuntimeError("hook bridge closed the connection without a response")
      response += chunk
  decoded = json.loads(response.decode("utf-8"))
  if not isinstance(decoded, dict):
    raise RuntimeError("hook bridge response was not a JSON object")
  return decoded


def main(argv: list[str] | None = None) -> int:
  try:
    socket_path, token, gate = _parse_hook_argv(sys.argv[1:] if argv is None else argv)
  except ValueError as error:
    # An unparseable argv fails closed: rc 2 blocks the gate events and surfaces
    # the misconfiguration on the non-gate ones.
    print(f"claude-sub hook bridge: {error}", file=sys.stderr)
    return 2
  # -S skips site, so nothing pins ``src`` for this process; the repo root rides
  # __file__ (src/cli/claude_sub_hook.py) and serves the import below.
  helper_dir = os.path.dirname(os.path.abspath(__file__))
  repo_root = os.path.dirname(os.path.dirname(helper_dir))
  if repo_root not in sys.path:
    sys.path.insert(0, repo_root)
  from src.core.timeouts import CLAUDE_SUB_HOOK_SOCKET_TIMEOUT

  try:
    payload = json.load(sys.stdin)
  except (json.JSONDecodeError, UnicodeDecodeError) as error:
    return _fail(f"malformed hook JSON: {error}", gate=gate, terminate_parent=True)
  if not isinstance(payload, dict):
    return _fail("hook JSON must be an object", gate=gate, terminate_parent=True)
  try:
    response = _send_request(socket_path, token, gate, payload, CLAUDE_SUB_HOOK_SOCKET_TIMEOUT)
  except (OSError, RuntimeError, UnicodeDecodeError, json.JSONDecodeError) as error:
    return _fail(
        f"transport failure: {error}",
        gate=gate,
        terminate_parent=not gate,
    )

  if response.get("ok") is True:
    return 0
  error = response.get("error")
  message = error if isinstance(error, str) and error else "bridge rejected the hook"
  return _fail(message, gate=gate, terminate_parent=not gate)


if __name__ == "__main__":
  raise SystemExit(main())
