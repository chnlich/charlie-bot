"""Shared helpers for CharlieBot CLI scripts.

Provides a single place for the POST-to-internal-API pattern used by every CLI
entry point, including consistent error-detail extraction on 4xx/5xx responses.
"""

import json
import sys
from pathlib import Path
from typing import Any

import requests

from src.core.config import get_config
from src.core.timeouts import HTTP_INTERNAL_API_TIMEOUT


def post_internal_api(endpoint: str, payload: dict[str, Any]) -> dict[str, Any]:
  """POST to an internal CharlieBot API endpoint and return the parsed JSON response.

  On requests.RequestException, extracts the server's ``detail`` field when
  available, writes a JSON error to stderr, and calls ``sys.exit(1)``.
  """
  cfg = get_config()
  try:
    resp = requests.post(
        f"{cfg.server_base_url}{endpoint}", json=payload, timeout=HTTP_INTERNAL_API_TIMEOUT, verify=False)
    resp.raise_for_status()
    return resp.json()
  except requests.RequestException as e:
    msg = str(e)
    if e.response is not None:
      try:
        msg = e.response.json()["detail"]
      except (ValueError, KeyError):
        pass
    print(json.dumps({"error": msg}), file=sys.stderr)
    sys.exit(1)


def resolve_session_id(arg_session: str | None) -> str:
  """Resolve the session id to use for a CLI invocation.

  Master CC always cd's into ~/.charliebot/sessions/{session_id} before
  running these CLIs. We use that fact to (a) auto-derive the session id
  when --session is omitted, and (b) reject mismatches when an explicit
  --session disagrees with cwd — which catches the fork-imitation bug
  where master copies an old session id from inherited transcript history.
  """
  cwd = Path.cwd().resolve()
  sessions_dir = get_config().sessions_dir.resolve()
  in_session_dir = cwd.parent == sessions_dir
  if in_session_dir:
    sid = cwd.name
    if arg_session is None or arg_session == sid:
      return sid
    print(
        json.dumps({
            "error": (
                f"session id mismatch: cwd={sid} --session={arg_session}; "
                "refusing to delegate to a different session"
            )
        }),
        file=sys.stderr,
    )
    sys.exit(2)
  if arg_session is None:
    print(
        json.dumps({"error": "--session required when not running from a CharlieBot session dir"}),
        file=sys.stderr,
    )
    sys.exit(2)
  return arg_session
