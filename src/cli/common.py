"""Shared helpers for CharlieBot CLI scripts.

Provides a single place for the POST-to-internal-API pattern used by every CLI
entry point, including consistent error-detail extraction on 4xx/5xx responses
and the restart-crossing call contract: a call whose connection never got
established is retried with bounded exponential backoff (the effect provably
did not happen, so re-sending is safe); a call sent with a lost response is
never retried — instead the CLI reads back that call's own on-disk artifact.

Every failure output stays a JSON object on stderr with exit code 1 plus a
``code`` (server_unavailable / outcome_unknown / server_error) and an ``effect``
(none / unknown) field so the caller can tell "retry safely" from "verify".
"""

from __future__ import annotations

import argparse
import contextlib
import json
import os
import re
import sys
import tempfile
import time
from collections.abc import Callable
from pathlib import Path
from typing import TYPE_CHECKING, Any, NoReturn

if TYPE_CHECKING:
  from src.core.config import CharlieBotConfig
  from src.core.credentials import Credentials

from src.core.constants import SESSION_ID_ENV_VAR
from src.core.home import charliebot_home_dir
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


class _ConnectPhaseError(Exception):
  """The TCP connection never established: the request provably was not sent, so a retry is safe."""


class _SentButLostError(Exception):
  """The connection broke after the request was sent: the outcome is unknown."""


class _CliResponse:
  """One internal-API response: the status the contract's rejection check reads, and the parsed-JSON accessor."""

  def __init__(self, status_code: int, reason: str, body: bytes) -> None:
    self.status_code = status_code
    self._reason = reason
    self._body = body

  def __str__(self) -> str:
    return f"HTTP {self.status_code} {self._reason}".strip()

  def json(self) -> Any:
    return json.loads(self._body)


def _send_request(
    method: str,
    url: str,
    *,
    payload: dict[str, Any] | None,
    params: dict[str, Any] | None,
    headers: dict[str, str],
    timeout: float,
) -> _CliResponse:
  """One request over http.client with the restart-crossing contract's phase separation.

  The connect phase raises _ConnectPhaseError (nothing was sent — a retry is safe); every
  failure after it raises _SentButLostError (the request may have landed — never retried).
  http.client raises separately per phase; requests folds connect and read failures into one
  ConnectionError class, which cannot drive this contract.
  """
  import http.client
  import ssl
  import urllib.parse

  parts = urllib.parse.urlsplit(url)
  path = parts.path or "/"
  query = urllib.parse.urlencode(params) if params is not None else ""
  if parts.query:
    query = f"{parts.query}&{query}" if query else parts.query
  if query:
    path = f"{path}?{query}"
  body = json.dumps(payload).encode("utf-8") if payload is not None else None
  send_headers = dict(headers)
  if body is not None:
    send_headers["Content-Type"] = "application/json"
  if parts.scheme == "https":
    # verify=False parity: the base URL is the config-owned internal server.
    context = ssl.create_default_context()
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE
    conn: http.client.HTTPConnection = http.client.HTTPSConnection(
        parts.hostname, parts.port, timeout=timeout, context=context)
  else:
    conn = http.client.HTTPConnection(parts.hostname, parts.port, timeout=timeout)
  try:
    try:
      conn.connect()
    except (OSError, TimeoutError) as e:
      raise _ConnectPhaseError(str(e)) from e
    try:
      conn.request(method, path, body=body, headers=send_headers)
      raw = conn.getresponse()
      resp_body = raw.read()
    except (OSError, TimeoutError, http.client.HTTPException) as e:
      raise _SentButLostError(str(e)) from e
  finally:
    conn.close()
  return _CliResponse(raw.status, raw.reason, resp_body)


def _request_post(
    url: str,
    *,
    json: dict[str, Any] | None,
    params: dict[str, Any] | None,
    headers: dict[str, str],
    timeout: float,
) -> _CliResponse:
  """The POST transport seam; requests' call shape kept so the tests' patch target and call-args assertions hold."""
  return _send_request("POST", url, payload=json, params=params, headers=headers, timeout=timeout)


def _request_get(
    url: str,
    *,
    params: dict[str, Any] | None,
    headers: dict[str, str],
    timeout: float,
) -> _CliResponse:
  """The GET transport seam; same contract as _request_post."""
  return _send_request("GET", url, payload=None, params=params, headers=headers, timeout=timeout)


def get_config() -> CharlieBotConfig:
  """Resolve the process config, importing its module on first call.

  config's import chain (pydantic models + yaml, ~180 ms of the M92 CLI import
  floor) serves only paths that read config; --help never does, and the
  request path reads only the server port through the fingerprint-keyed
  document below. The module attribute stays the tests' patch target (conftest
  CLI_COMMON_GET_CONFIG_PATCH_TARGET setattrs this name).
  """
  from src.core.config import get_config
  return get_config()


# The request contract's server port, cached under the profile home as a
# fingerprint-keyed document. The key pairs config.yaml's (mtime, size) — the
# reload key the server's own config cache uses — with config.py's, so a deploy
# that moved a default re-prices the cache with one full read. The document is
# written only by a full get_config() resolution, so a hit answers with a value
# the real loader produced; a config edit moves the fingerprint and the next
# call pays the full read and rewrites. An unreadable or foreign-shaped
# document is a miss: the full read below is the recovery and the rewrite
# replaces the document.
_BASE_URL_CACHE_RELPATH = os.path.join("cache", "cli_base_url.json")


def _config_module_fingerprint() -> tuple[float, int]:
  """The (mtime, size) of the checkout's own config.py — the defaults' source."""
  path = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "core", "config.py")
  try:
    st = os.stat(path)
  except OSError:
    return (0.0, 0)
  return (st.st_mtime, st.st_size)


def _cached_server_port() -> int | None:
  """Return the cached server port, or None when the document is absent, stale, or unreadable."""
  from src.core.credentials import _file_fingerprint

  fingerprint = [list(_file_fingerprint("config.yaml")), list(_config_module_fingerprint())]
  try:
    doc = json.loads((Path(charliebot_home_dir()) / _BASE_URL_CACHE_RELPATH).read_text(encoding="utf-8"))
  except (OSError, ValueError):
    return None
  if not isinstance(doc, dict) or doc.get("fingerprint") != fingerprint:
    return None
  port = doc.get("port")
  return port if isinstance(port, int) else None


def _store_base_url_cache(port: int) -> None:
  """Write the fingerprint-keyed port document atomically (a torn write never publishes)."""
  from src.core.credentials import _file_fingerprint

  doc = {"fingerprint": [_file_fingerprint("config.yaml"), _config_module_fingerprint()], "port": port}
  cache_path = Path(charliebot_home_dir()) / _BASE_URL_CACHE_RELPATH
  cache_path.parent.mkdir(parents=True, exist_ok=True)
  descriptor, temp_name = tempfile.mkstemp(dir=cache_path.parent, suffix=".tmp")
  try:
    with os.fdopen(descriptor, "w", encoding="utf-8") as f:
      json.dump(doc, f)
    os.replace(temp_name, cache_path)
  except BaseException:
    with contextlib.suppress(OSError):
      os.unlink(temp_name)
    raise


def _internal_base_url() -> str:
  """The internal API base URL: the cached port when fresh, one full config read otherwise.

  The heavy config import (pydantic + yaml models, ~150 ms of the M97 wall)
  rides only the miss path; the module attribute stays the tests' patch target
  (conftest CLI_COMMON_BASE_URL_PATCH_TARGET setattrs this name).
  """
  port = _cached_server_port()
  if port is None:
    port = get_config().server.port
    _store_base_url_cache(port)
  return f"http://localhost:{port}"


def _sessions_dir() -> Path:
  """The sessions root, derived from the env-resolved home (the same value the config model
  carries; the M102 wrap-verb precedent). The module attribute stays the tests' patch target
  (conftest CLI_COMMON_SESSIONS_DIR_PATCH_TARGET setattrs this name)."""
  from src.core.home import charliebot_home_dir

  return (charliebot_home_dir() / "sessions").resolve()


def get_credentials() -> Credentials:
  """Resolve the process credentials (the light secrets module; config's model stack stays out)."""
  from src.core.credentials import get_credentials
  return get_credentials()


def internal_api_auth_headers() -> dict[str, str]:
  """Authorization header for internal-API calls.

  Returns a Bearer header when the access key is configured — it lives in
  credentials.yaml under ``charliebot.access_key`` — so the internal CLIs
  authenticate against the auth middleware; returns no header when the key is
  empty (the middleware is a no-op in that case).
  """
  from src.core.credentials import configured_access_key
  access_key = configured_access_key()
  if access_key:
    return {"Authorization": f"Bearer {access_key}"}
  return {}


def exit_usage_error(message: str) -> None:
  """Emit a CLI usage error as JSON and exit with argparse-compatible code 2."""
  print(json.dumps({"error": message}), file=sys.stderr)
  sys.exit(2)


def exit_error(message: str) -> NoReturn:
  """Emit a failure as a JSON error object on stderr and exit 1 — the module docstring's failure contract."""
  _exit_with_error({"error": message})


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


def _best_effort_server_version(base_url: str) -> tuple[str | None, str | None]:
  """Best-effort fetch of /api/internal/version. Returns (sha, started_at) or (None, None).

  Swallows every failure (network, non-200, non-JSON) so the CLI error path never raises
  from the hint computation. Bounded by HTTP_VERSION_SKEW_TIMEOUT.
  """
  try:
    resp = _request_get(
        f"{base_url}/api/internal/version",
        params=None,
        headers=internal_api_auth_headers(),
        timeout=HTTP_VERSION_SKEW_TIMEOUT)
    if resp.status_code >= 400:
      return None, None
    info = resp.json()
  except (_ConnectPhaseError, _SentButLostError, ValueError):
    return None, None
  return info.get("sha"), info.get("started_at")


def _maybe_version_skew_hint(base_url: str) -> str | None:
  """Gather server + local SHAs and compose the hint. Pure-failure-safe (never raises)."""
  # buildinfo pulls subprocess (measured ~4 ms of the M92 floor) and serves
  # only the version-skew failure path; the parser-build path never reads a SHA.
  from src.core.buildinfo import read_repo_head_sha

  server_sha, started_at = _best_effort_server_version(base_url)
  local_sha = read_repo_head_sha(SUBPROCESS_GIT_SHA_TIMEOUT)
  return compose_version_skew_hint(server_sha, started_at, local_sha)


_CONNECT_RETRY_BASE_DELAY = 0.25  # seconds; doubles per attempt
_CONNECT_RETRY_MAX_DELAY = 5.0  # seconds


def _exit_with_error(error_obj: dict[str, Any], exit_code: int = 1) -> NoReturn:
  print(json.dumps(error_obj), file=sys.stderr)
  sys.exit(exit_code)


def _exit_server_rejection(
    base_url: str,
    resp: Any,
    rejection_exit_codes: dict[int, int] | None,
) -> NoReturn:
  """Handle a server that explicitly answered with an error status."""
  msg = str(resp)
  with contextlib.suppress(ValueError, KeyError):
    msg = resp.json()["detail"]
  error_obj: dict[str, Any] = {"error": msg, "code": "server_error", "effect": "none"}
  hint = _maybe_version_skew_hint(base_url)
  if hint is not None:
    error_obj["hint"] = hint
  exit_code = 1
  if rejection_exit_codes is not None:
    exit_code = rejection_exit_codes.get(resp.status_code, 1)
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
  base_url = _internal_base_url()
  url = f"{base_url}{endpoint}"
  deadline = time.monotonic() + CLI_CONNECT_TOTAL_TIMEOUT
  attempt = 0
  while True:
    try:
      if method == "POST":
        resp = _request_post(
            url, json=payload, params=params, headers=internal_api_auth_headers(), timeout=HTTP_INTERNAL_API_TIMEOUT)
      elif method == "GET":
        resp = _request_get(url, params=params, headers=internal_api_auth_headers(), timeout=HTTP_INTERNAL_API_TIMEOUT)
      else:
        raise RuntimeError(f"internal-API method is POST or GET, got {method!r}")
    except _ConnectPhaseError as e:
      remaining = deadline - time.monotonic()
      if remaining <= 0:
        _exit_with_error({"error": str(e), "code": "server_unavailable", "effect": "none"})
      delay = min(_CONNECT_RETRY_BASE_DELAY * (2**attempt), _CONNECT_RETRY_MAX_DELAY, remaining)
      attempt += 1
      time.sleep(delay)
      continue
    except _SentButLostError as e:
      # Sent but the response was lost — never re-issue the call (its effect
      # may have landed). Read back this call's own on-disk artifact instead.
      if readback is not None:
        artifact = readback()
        if artifact is not None:
          return artifact
      _exit_with_error({"error": str(e), "code": "outcome_unknown", "effect": unknown_effect})
    if resp.status_code >= 400:
      _exit_server_rejection(base_url, resp, rejection_exit_codes)
    return resp.json()


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
  # Lazy: the readback path is the rare sent-but-lost class, and the threads
  # module's own import chain (config, models, sidebar_state) is ~17 ms of
  # every CLI invocation that imports this module — the numpy weight rides the
  # backends.base import this module never makes.
  from src.core.threads import METADATA_NAME, THREADS_DIR_NAME

  threads_dir = get_config().sessions_dir / session_id / THREADS_DIR_NAME
  if not threads_dir.is_dir():
    return None
  best: dict[str, Any] | None = None
  for thread_dir in threads_dir.iterdir():
    meta_path = thread_dir / METADATA_NAME
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
  parser.add_argument(
      "--session",
      default=None,
      help=f"Session ID (optional; taken from the {SESSION_ID_ENV_VAR} the server writes into the master environment)",
  )


def _exit_ambiguous_session(source_text: str) -> None:
  """Reject an invocation whose identity sources disagree, naming each source and its value."""
  exit_usage_error(f"session id mismatch: {source_text}; refusing to use an ambiguous session")


def resolve_session_id(arg_session: str | None) -> str:
  """Resolve the session id to use for a CLI invocation.

  The server writes ``CHARLIEBOT_SESSION_ID`` into every master process
  environment, so each shell command a master runs carries its own session
  identity wherever it cd's to; that variable is the authoritative source. An
  explicit ``--session`` must agree with it and a mismatch exits 2 naming both,
  because either value can carry a caller's intent. cwd serves as the fallback
  for an invocation the server did not start (a hand-run shell, a tmux backend):
  with the variable absent, ~/.charliebot/sessions/{session_id} supplies the id
  exactly as it does today. With the variable present, a cwd sitting in another
  session's directory routes by the variable and prints a non-fatal warning
  naming both ids, so a stale copied path stays visible while a legitimate cd
  (reading a sibling session's artifacts, entering a worktree) keeps working.
  """
  cwd = Path.cwd().resolve()
  sessions_dir = _sessions_dir()
  cwd_session = cwd.name if cwd.parent == sessions_dir else None

  # An empty value carries no identity, so it reads as absent and the cwd
  # fallback answers, which is what an unstarted-by-server invocation gets.
  env_session = os.environ.get(SESSION_ID_ENV_VAR) or None
  if env_session is not None:
    if arg_session is not None and arg_session != env_session:
      _exit_ambiguous_session(f"--session={arg_session} {SESSION_ID_ENV_VAR}={env_session}")
    if cwd_session is not None and cwd_session != env_session:
      print(
          json.dumps({"note": f"cwd is session dir of {cwd_session}; using {SESSION_ID_ENV_VAR}={env_session}"}),
          file=sys.stderr,
      )
    return env_session

  sources: dict[str, str] = {}
  if arg_session is not None:
    sources["--session"] = arg_session
  if cwd_session is not None:
    sources["cwd"] = cwd_session

  if not sources:
    exit_usage_error("--session required when not running from a CharlieBot session dir")

  unique_session_ids = set(sources.values())
  if len(unique_session_ids) > 1:
    _exit_ambiguous_session(" ".join(f"{name}={value}" for name, value in sources.items()))

  return next(iter(unique_session_ids))
