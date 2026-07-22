"""Shared helpers for CharlieBot CLI scripts.

Provides a single place for the POST-to-internal-API pattern used by every CLI
entry point, including consistent error-detail extraction on 4xx/5xx responses.
"""

import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any

import requests

from src.core.config import CharlieBotConfig, get_config
from src.core.timeouts import HTTP_INTERNAL_API_TIMEOUT, HTTP_VERSION_SKEW_TIMEOUT, SUBPROCESS_VERSION_SKEW_TIMEOUT

TASK_SPEC_REQUIRED_HEADINGS = (
    "Goal",
    "Source Files",
    "Required Behavior",
    "Acceptance Tests",
    "Reviewer Checklist",
    "Out of Scope",
)

# Repo root derived from this file: src/cli/common.py -> parents[2] == repo root.
_REPO_ROOT = Path(__file__).resolve().parents[2]


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


def compose_version_skew_hint(
    server_sha: str | None,
    started_at: str | None,
    local_sha: str | None,
) -> str | None:
  """Pure function: compose a version-skew hint string, or return None when there is no skew.

  Returns None when either SHA is missing (best-effort fetch failed) or when the SHAs match.
  When they differ, returns a single-line hint naming both SHAs (and the server start time
  when known) so the user knows a server restart may be required.

  Unit-testable without a server: callers pass the SHAs they gathered.
  """
  if not server_sha or not local_sha:
    return None
  if server_sha == local_sha:
    return None
  started_clause = f" (started {started_at})" if started_at else ""
  return (f"server running {server_sha}{started_clause}, repo at {local_sha} "
          f"— server restart may be required")


def _read_local_repo_sha() -> str | None:
  """Return `git rev-parse --short HEAD` in the repo root, or None on any failure.

  Every failure mode (missing git, non-zero exit, timeout) is swallowed so the CLI
  error path never stalls on a best-effort hint computation.
  """
  try:
    proc = subprocess.run(
        ["git", "rev-parse", "--short", "HEAD"],
        cwd=str(_REPO_ROOT),
        capture_output=True,
        timeout=SUBPROCESS_VERSION_SKEW_TIMEOUT,
    )
  except (OSError, subprocess.SubprocessError):
    return None
  if proc.returncode != 0:
    return None
  return proc.stdout.decode().strip() or None


def _best_effort_server_version(cfg: CharlieBotConfig) -> tuple[str | None, str | None]:
  """Best-effort fetch of /api/internal/version. Returns (sha, started_at) or (None, None).

  Swallows every failure (network, non-200, non-JSON) so the CLI error path never raises
  from the hint computation. Bounded by HTTP_VERSION_SKEW_TIMEOUT.
  """
  try:
    resp = requests.get(
        f"{cfg.server_base_url}/api/internal/version",
        headers=internal_api_auth_headers(cfg),
        timeout=HTTP_VERSION_SKEW_TIMEOUT,
        verify=False,
    )
    resp.raise_for_status()
    info = resp.json()
  except (requests.RequestException, ValueError):
    return None, None
  return info.get("sha"), info.get("started_at")


def _maybe_version_skew_hint(cfg: CharlieBotConfig) -> str | None:
  """Gather server + local SHAs and compose the hint. Pure-failure-safe (never raises)."""
  server_sha, started_at = _best_effort_server_version(cfg)
  local_sha = _read_local_repo_sha()
  return compose_version_skew_hint(server_sha, started_at, local_sha)


def post_internal_api(endpoint: str, payload: dict[str, Any]) -> dict[str, Any]:
  """POST to an internal CharlieBot API endpoint and return the parsed JSON response.

  On requests.RequestException, extracts the server's ``detail`` field when
  available, writes a JSON error to stderr (with an optional version-skew ``hint``
  when the server and repo SHAs differ), and calls ``sys.exit(1)``.
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
    error_obj: dict[str, Any] = {"error": msg}
    hint = _maybe_version_skew_hint(cfg)
    if hint is not None:
      error_obj["hint"] = hint
    print(json.dumps(error_obj), file=sys.stderr)
    sys.exit(1)


def get_api(endpoint: str, params: dict[str, Any] | None = None) -> dict[str, Any]:
  """GET a CharlieBot API endpoint and return the parsed JSON response.

  Mirrors ``post_internal_api`` error handling (including the version-skew hint).
  Used by read-only CLI verbs (e.g. ``plan list``) that hit the public sessions router.
  """
  cfg = get_config()
  try:
    resp = requests.get(
        f"{cfg.server_base_url}{endpoint}",
        params=params,
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
    error_obj: dict[str, Any] = {"error": msg}
    hint = _maybe_version_skew_hint(cfg)
    if hint is not None:
      error_obj["hint"] = hint
    print(json.dumps(error_obj), file=sys.stderr)
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
