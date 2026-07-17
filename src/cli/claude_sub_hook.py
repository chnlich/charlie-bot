"""Opaque command-hook transport helper used by ``claude-sub``.

The helper intentionally does not interpret Claude Code hook fields.  It authenticates
and forwards the complete JSON object to the per-turn bridge, then applies the bridge's
allow/fail result to Claude's command-hook contract.
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import socket
import sys
from typing import Any


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


def _send_request(socket_path: str, token: str, gate: bool, payload: dict[str, Any]) -> dict[str, Any]:
  envelope = {
      "token": token,
      "gate": gate,
      "payload": payload,
  }
  with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
    client.settimeout(30.0)
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
  parser = argparse.ArgumentParser()
  parser.add_argument("--socket", required=True)
  parser.add_argument("--token", required=True)
  parser.add_argument("--gate", action="store_true")
  args = parser.parse_args(argv)

  try:
    payload = json.load(sys.stdin)
  except (json.JSONDecodeError, UnicodeDecodeError) as error:
    return _fail(f"malformed hook JSON: {error}", gate=args.gate, terminate_parent=True)
  if not isinstance(payload, dict):
    return _fail("hook JSON must be an object", gate=args.gate, terminate_parent=True)

  try:
    response = _send_request(args.socket, args.token, args.gate, payload)
  except (OSError, RuntimeError, UnicodeDecodeError, json.JSONDecodeError) as error:
    return _fail(
        f"transport failure: {error}",
        gate=args.gate,
        terminate_parent=not args.gate,
    )

  if response.get("ok") is True:
    return 0
  error = response.get("error")
  message = error if isinstance(error, str) and error else "bridge rejected the hook"
  return _fail(message, gate=args.gate, terminate_parent=not args.gate)


if __name__ == "__main__":
  raise SystemExit(main())
