"""Shared helpers for CharlieBot CLI scripts.

Provides a single place for the POST-to-internal-API pattern used by every CLI
entry point, including consistent error-detail extraction on 4xx/5xx responses
and the restart-crossing call contract: a call whose connection never got
established is retried with bounded exponential backoff (the effect provably
did not happen, so re-sending is safe); a call sent with a lost response is
never retried — instead the CLI reads back that call's own on-disk artifact.

Every failure output stays a JSON object on stderr with exit code 1, now with
``code`` (server_unavailable / outcome_unknown / server_error) and ``effect``
(none / unknown) fields so the caller can tell "retry safely" from "verify".
"""

import argparse
import contextlib
import json
import re
import sys
import time
from collections.abc import Callable
from pathlib import Path
from typing import Any, NoReturn

import requests

from src.core.buildinfo import read_repo_head_sha
from src.core.config import CharlieBotConfig, get_config
from src.core.timeouts import (
    CLI_CONNECT_TOTAL_TIMEOUT,
    HTTP_INTERNAL_API_TIMEOUT,
    HTTP_VERSION_SKEW_TIMEOUT,
    SUBPROCESS_GIT_SHA_TIMEOUT,
)

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


def validate_repo_path(parser: argparse.ArgumentParser, value: str) -> None:
  """Reject a --repo value that is not an absolute path or does not exist as a directory.

  Calls ``parser.error`` (which raises ``SystemExit``) before any network call so a
  bad repo path never reaches the internal API.
  """
  if not value.startswith("/"):
    parser.error(f"--repo must be an absolute path (starting with '/'), got: {value!r}")
  if not Path(value).is_dir():
    parser.error(f"--repo does not exist: {value!r}")


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
  local_sha = read_repo_head_sha(SUBPROCESS_GIT_SHA_TIMEOUT)
  return compose_version_skew_hint(server_sha, started_at, local_sha)


_CONNECT_RETRY_BASE_DELAY = 0.25  # seconds; doubles per attempt
_CONNECT_RETRY_MAX_DELAY = 5.0  # seconds


def _is_connect_failure(exc: requests.RequestException) -> bool:
  """True only when the request provably never reached the server.

  TCP connect refused/timed out means zero bytes were sent, so the requested
  effect did not happen and a retry is safe. Connection resets are excluded:
  a reset may arrive after the server accepted (and possibly processed) the
  request — that is the outcome_unknown class instead.
  """
  if isinstance(exc, requests.exceptions.ConnectTimeout):
    return True
  if isinstance(exc, requests.exceptions.ConnectionError):
    msg = str(exc)
    return (
        "Failed to establish a new connection" in msg or "Connection refused" in msg or
        "NameResolutionError" in msg  # DNS never resolved — nothing was sent
    )
  return False


def _exit_with_error(error_obj: dict[str, Any], exit_code: int = 1) -> NoReturn:
  print(json.dumps(error_obj), file=sys.stderr)
  sys.exit(exit_code)


def _exit_server_rejection(
    cfg: CharlieBotConfig,
    exc: requests.RequestException,
    rejection_exit_codes: dict[int, int] | None,
) -> NoReturn:
  """Handle a server that explicitly answered with an error status."""
  msg = str(exc)
  with contextlib.suppress(ValueError, KeyError):
    msg = exc.response.json()["detail"]  # type: ignore[union-attr]
  error_obj: dict[str, Any] = {"error": msg, "code": "server_error", "effect": "none"}
  hint = _maybe_version_skew_hint(cfg)
  if hint is not None:
    error_obj["hint"] = hint
  exit_code = 1
  if rejection_exit_codes is not None and exc.response is not None:
    exit_code = rejection_exit_codes.get(exc.response.status_code, 1)
  _exit_with_error(error_obj, exit_code)


def _request_with_contract(
    method: str,
    endpoint: str,
    *,
    payload: dict[str, Any] | None = None,
    params: dict[str, Any] | None = None,
    readback: Callable[[], dict[str, Any] | None] | None = None,
    rejection_exit_codes: dict[int, int] | None = None,
    unknown_effect: str,
) -> dict[str, Any]:
  """Issue one internal-API call under the restart-crossing contract."""
  cfg = get_config()
  url = f"{cfg.server_base_url}{endpoint}"
  request_fn = requests.post if method == "POST" else requests.get
  deadline = time.monotonic() + CLI_CONNECT_TOTAL_TIMEOUT
  attempt = 0
  while True:
    try:
      resp = request_fn(
          url,
          json=payload,
          params=params,
          headers=internal_api_auth_headers(cfg),
          timeout=HTTP_INTERNAL_API_TIMEOUT,
          verify=False)
      resp.raise_for_status()
      return resp.json()
    except requests.RequestException as e:
      if e.response is not None:
        _exit_server_rejection(cfg, e, rejection_exit_codes)
      if _is_connect_failure(e):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
          _exit_with_error({"error": str(e), "code": "server_unavailable", "effect": "none"})
        delay = min(_CONNECT_RETRY_BASE_DELAY * (2**attempt), _CONNECT_RETRY_MAX_DELAY, remaining)
        attempt += 1
        time.sleep(delay)
        continue
      # Sent but the response was lost — never re-issue the call (its effect
      # may have landed). Read back this call's own on-disk artifact instead.
      if readback is not None:
        artifact = readback()
        if artifact is not None:
          return artifact
      _exit_with_error({"error": str(e), "code": "outcome_unknown", "effect": unknown_effect})


def post_internal_api(
    endpoint: str,
    payload: dict[str, Any],
    *,
    readback: Callable[[], dict[str, Any] | None] | None = None,
    rejection_exit_codes: dict[int, int] | None = None,
) -> dict[str, Any]:
  """POST to an internal CharlieBot API endpoint and return the parsed JSON response.

  On failure, writes a JSON error (with ``code``/``effect`` fields, plus the
  version-skew ``hint`` for explicit server rejections) to stderr and exits.
  ``readback`` is invoked exactly once, for the sent-but-lost class: returning
  a non-None artifact makes the call a success, None reports
  ``outcome_unknown``. ``rejection_exit_codes`` maps specific rejection status
  codes to alternate exit codes (schedule-trigger's 422 -> 2 contract).
  """
  return _request_with_contract(
      "POST",
      endpoint,
      payload=payload,
      readback=readback,
      rejection_exit_codes=rejection_exit_codes,
      unknown_effect="unknown",
  )


def get_api(endpoint: str, params: dict[str, Any] | None = None) -> dict[str, Any]:
  """GET a CharlieBot API endpoint and return the parsed JSON response.

  Mirrors ``post_internal_api`` error handling (including the bounded connect
  retry and version-skew hint). A read-only call mutates nothing, so the
  sent-but-lost class reports effect ``none`` and needs no readback.
  """
  return _request_with_contract("GET", endpoint, params=params, unknown_effect="none")


def find_local_thread(
    session_id: str,
    *,
    description: str,
    task_type: str | None,
    description_match: str = "exact",
) -> dict[str, Any] | None:
  """Readback scan: the newest thread metadata matching description + task_type.

  Pure local-disk judgment used when an internal-API POST's response was lost:
  matching this call's own product proves the effect landed, so the call can
  report success without re-sending. Threads in any status count.
  ``description_match`` is ``exact`` or ``contains``.
  """
  threads_dir = get_config().sessions_dir / session_id / "threads"
  if not threads_dir.is_dir():
    return None
  best: dict[str, Any] | None = None
  for thread_dir in threads_dir.iterdir():
    meta_path = thread_dir / "metadata.json"
    try:
      meta = json.loads(meta_path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
      continue
    stored_description = meta.get("description", "")
    if description_match == "exact":
      if stored_description != description:
        continue
    elif description not in stored_description:
      continue
    if (meta.get("task_type") or "implement") != (task_type or "implement"):
      continue
    if best is None or str(meta.get("created_at", "")) > str(best.get("created_at", "")):
      best = meta
  return best


def add_session_arg(parser: argparse.ArgumentParser) -> None:
  """Add the optional ``--session`` flag; ``resolve_session_id`` resolves its value."""
  parser.add_argument("--session", default=None, help="Session ID (optional; auto-derived from cwd)")


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
