"""Shared helpers for CharlieBot CLI scripts.

Provides a single place for the POST-to-internal-API pattern used by every CLI
entry point, including consistent error-detail extraction on 4xx/5xx responses.
"""

import json
import re
import sys
from pathlib import Path
from typing import Any

import requests

from src.core.config import CharlieBotConfig, get_config
from src.core.timeouts import HTTP_INTERNAL_API_TIMEOUT

TASK_SPEC_REQUIRED_HEADINGS = (
    "Goal",
    "Source Files",
    "Required Behavior",
    "Acceptance Tests",
    "Reviewer Checklist",
    "Out of Scope",
)


def internal_api_auth_headers(cfg: CharlieBotConfig) -> dict[str, str]:
  """Authorization header for internal-API calls.

  Returns a Bearer header when ``charliebot_access_key`` is configured so the
  internal CLIs authenticate against the auth middleware; returns no header when
  the key is empty (the middleware is a no-op in that case).
  """
  if cfg.charliebot_access_key:
    return {"Authorization": f"Bearer {cfg.charliebot_access_key}"}
  return {}


def exit_usage_error(message: str) -> None:
  """Emit a CLI usage error as JSON and exit with argparse-compatible code 2."""
  print(json.dumps({"error": message}), file=sys.stderr)
  sys.exit(2)


def read_required_text_file(flag_name: str, file_path: str) -> str:
  """Read a required text file, exiting non-zero on a missing or empty file."""
  path = Path(file_path)
  if not path.is_file():
    exit_usage_error(f"{flag_name} not found: {file_path}")
  content = path.read_text()
  if not content.strip():
    exit_usage_error(f"{flag_name} is empty: {file_path}")
  return content


def validate_task_spec_markdown(content: str) -> None:
  """Validate the structured Markdown task spec contract used by delegation."""
  missing_headings = [
      heading for heading in TASK_SPEC_REQUIRED_HEADINGS
      if re.search(rf"^## {re.escape(heading)}[ \t]*$", content, flags=re.MULTILINE) is None
  ]
  if missing_headings:
    exit_usage_error(
        "task spec missing required headings: " + ", ".join(f"## {heading}" for heading in missing_headings))

  source_match = re.search(
      r"^## Source Files[ \t]*\n(?P<section>.*?)(?=^## [^\n]+|\Z)",
      content,
      flags=re.MULTILINE | re.DOTALL,
  )
  if source_match is None:
    raise RuntimeError("Source Files heading presence was validated but section extraction failed")

  saw_source_entry = False
  for line in source_match.group("section").splitlines():
    stripped = line.strip()
    if not stripped.startswith("- "):
      continue
    saw_source_entry = True
    if stripped == "- (none)":
      continue
    if stripped.startswith("- /"):
      source_path = stripped[2:].strip()
      if not Path(source_path).exists():
        exit_usage_error(f"task spec source file not found: {source_path}")
      continue
    exit_usage_error(f"task spec source file entries must be absolute paths or - (none): {stripped}")

  if not saw_source_entry:
    exit_usage_error("task spec Source Files section must list source files or - (none)")


def post_internal_api(endpoint: str, payload: dict[str, Any]) -> dict[str, Any]:
  """POST to an internal CharlieBot API endpoint and return the parsed JSON response.

  On requests.RequestException, extracts the server's ``detail`` field when
  available, writes a JSON error to stderr, and calls ``sys.exit(1)``.
  """
  cfg = get_config()
  try:
    resp = requests.post(
        f"{cfg.server_base_url}{endpoint}",
        json=payload,
        headers=internal_api_auth_headers(cfg),
        timeout=HTTP_INTERNAL_API_TIMEOUT,
        verify=False)
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
  when --session is omitted, and (b) reject mismatches across explicit and
  cwd-derived sources — which catches stale copied session ids before they
  can mutate the wrong session.
  """
  cwd = Path.cwd().resolve()
  sessions_dir = get_config().sessions_dir.resolve()
  sources: dict[str, str] = {}
  if arg_session is not None:
    sources["--session"] = arg_session
  if cwd.parent == sessions_dir:
    sources["cwd"] = cwd.name

  if not sources:
    print(
        json.dumps({"error": "--session required when not running from a CharlieBot session dir"}),
        file=sys.stderr,
    )
    sys.exit(2)

  unique_session_ids = set(sources.values())
  if len(unique_session_ids) > 1:
    source_text = " ".join(f"{name}={value}" for name, value in sources.items())
    print(
        json.dumps({"error": f"session id mismatch: {source_text}; refusing to use an ambiguous session"}),
        file=sys.stderr,
    )
    sys.exit(2)

  return next(iter(unique_session_ids))
