"""Shared helpers for CharlieBot CLI scripts.

Provides a single place for the POST-to-internal-API pattern used by every CLI
entry point, including consistent error-detail extraction on 4xx/5xx responses.
"""

import json
import sys
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
