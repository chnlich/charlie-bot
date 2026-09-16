"""The session-tree preview entry point: one isolated trial instance of the real app.

``charliebot session-tree preview --home DIR --port PORT`` prepares, validates and
runs a foreground CharlieBot instance for the user's session-tree UI trial (plan 1
v4: the independent instance carries the trial; real migration waits for an
explicit request). This module owns the whole lifecycle; the CLI verb is a thin
dispatcher onto :func:`run_preview_command`.

Contract:

- **The real app.** The shipped ``server.app`` (pages, static UI, task APIs,
  websockets) serves the trial; only the application lifetime and the reachable
  mechanism surface differ from production. The normal production startup path is
  untouched and remains available to the old runtime.
- **Backend selection.** ``--backend`` names the trial's default charlie-code
  entry (required for a fresh home; a restart must match the home's stored
  default). ``--add-backend`` (repeatable) adds further explicitly selected
  charlie-code entries: on a fresh home they extend the initial seed; on an
  existing validated preview home every requested entry and its referenced
  credential are validated from the source profile before anything is written,
  then appended under the home writer fence. A restart without the flag keeps
  the stored catalog, credentials, default, tasks, native history, paths and
  access key untouched. Non-charlie-code entries, duplicates, entries the
  schema cannot interpret, missing provider credentials and a live fence
  holder all refuse before any persistent change.
- **Home isolation.** ``--home`` resolves to a path outside and nonoverlapping
  with the production home, the production workspace dirs, and the running
  checkout. A fresh path is seeded with minimal private instance config and its
  own random access key; an existing validated preview home keeps its config and
  user-created tasks across restarts. Existing legacy/migrated state, unrelated
  configurations, occupied ports, and unprovable instance ownership refuse before
  any write. Logs and generated files stay inside the preview home.
- **Environment selection.** ``CHARLIEBOT_HOME`` is switched and inherited
  CharlieBot session/Run/credential identities are cleared before any cached
  application configuration or singleton can bind to the old home. ``HOME`` and
  ``CODEX_HOME`` are never repurposed. Worker workspace discovery and worktree
  creation are scoped to preview-owned directories through the preview config,
  and the execution adapter's launch workspace boundary refuses any repo outside
  them. This is supported-client isolation, not a sandbox for a malicious
  process sharing the host account.
- **Native CLC isolation.** The configured backend must be ``charlie-code``; the
  preview installs the adapter's ``extra_flags`` seam at process start so every
  manager/worker/review/retry/continuation build appends
  ``--session-dir <home>/clc-sessions`` — the CLI's own session-directory
  override, never a host installation or config change. Backends whose native
  isolation is not proven refuse; the host-global tmux terminal refuses at the
  request boundary.
- **Lifetime.** The preview holds the normal home writer fence for its whole run
  and releases it on startup or shutdown failure. Its startup runs the v2 task
  recovery over this home's own sessions only; the scheduler, external trigger
  recovery, external messaging, global cgroup/worktree cleanup, and the other
  shared provisioners never start. Scheduled-task mutation routes, external
  messaging routes, and delayed-trigger creation refuse at the request boundary,
  so a clicked legacy route cannot act on global state.
- **Readiness.** When the instance is up it prints the URL, the actual home, the
  source branch and the full source SHA — never a secret or a provider endpoint —
  and records them with the serving identity in
  ``<home>/state/preview_instance.json``.
"""

from __future__ import annotations

import json
import os
import socket
import subprocess
import sys
import threading
from collections.abc import AsyncIterator, Callable
from contextlib import asynccontextmanager
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

import yaml

from src.core.buildinfo import init_build_info
from src.core.config import (
    CHARLIEBOT_HOME_ENV,
    CharlieBotConfig,
    charliebot_home_dir,
    get_config,
    load_config,
    load_credentials,
)
from src.core.constants import REPO_ROOT
from src.core.home_writer_fence import HomeWriterFence, acquire_home_writer_fence
from src.core.init import init_charliebot_home
from src.core.json_utils import atomic_write_text, load_json_meta
from src.core.log_once import LazyStructlogLogger
from src.core.models import BackendType, utc_now
from src.core.runs import read_pid_stat
from src.core.task_recovery import reconcile_task_tree
from src.core.yaml_utils import load_yaml, save_yaml

log = LazyStructlogLogger()

PREVIEW_RECORD_REL = "state/preview_instance.json"
PREVIEW_FORMAT_VERSION = 1
PREVIEW_LOG_DIRNAME = "logs"
PREVIEW_NATIVE_DIRNAME = "clc-sessions"
PREVIEW_WORKSPACES_DIRNAME = "workspaces"
PREVIEW_WORKTREES_DIRNAME = "worktrees"

# The trial config contract: exactly these top-level sections. Anything else is
# an unrelated configuration and refuses instead of being carried into a trial.
_ALLOWED_CONFIG_TOP_KEYS = frozenset({"server", "paths", "backends"})

# Inherited CharlieBot identity/credential environment that must never reach the
# preview process or its children: children get preview-owned identities and this
# instance's credentials (task_execution._child_env), and a charlie-code child
# gets only the key the preview config references.
_INHERITED_IDENTITY_ENV_VARS = (
    "CHARLIEBOT_SESSION_ID",
    "CHARLIEBOT_RUN_TOKEN",
    "CHARLIE_CODE_API_KEY",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_API_KEY",
)

# Reachable mechanisms the preview does not run. Startup never starts them AND
# the request boundary refuses them, so a clicked legacy route cannot act on
# global state from a trial instance.
_PREVIEW_REFUSED_MUTATION_PREFIXES = ("/api/cron",)
_PREVIEW_REFUSED_MUTATION_PATHS = frozenset({
    "/api/internal/schedule-trigger",  # delayed/external trigger creation
    "/api/internal/slack/reply",  # external messaging
    "/api/internal/slack/ack",
})
_REFUSED_MUTATION_METHODS = frozenset({"POST", "PUT", "PATCH", "DELETE"})
_REFUSED_WEBSOCKET_PATHS = frozenset({"/ws/terminal"})
_WS_REFUSED_CLOSE_CODE = 4403

_CRON_DISABLED_REASON = "Scheduled task management is disabled in the session-tree preview instance."
_EXTERNAL_DISABLED_REASON = (
    "External messaging and delayed triggers are disabled in the session-tree preview instance.")


class PreviewRefused(RuntimeError):
  """A preparation or launch precondition failed; the message names the reason."""

  def __init__(self, reason: str, *, details: list[str] | None = None) -> None:
    super().__init__(reason)
    self.details = details


class PreviewWorkspaceError(RuntimeError):
  """A launch tried to select a repository outside the preview workspace boundary."""


# ---------------------------------------------------------------------------
# Pure path/ownership checks (no writes, no config binding)
# ---------------------------------------------------------------------------


def preview_record_path(home: Path) -> Path:
  """The preview instance record inside *home* (evidence, not the exclusion)."""
  return home / PREVIEW_RECORD_REL


def _resolved(path: Path | str) -> Path:
  return Path(path).expanduser().resolve()


def _contains(container: Path, item: Path) -> bool:
  return item == container or item.is_relative_to(container)


def _overlap(a: Path, b: Path) -> bool:
  return _contains(a, b) or _contains(b, a)


def resolve_preview_home(home_raw: str) -> Path:
  """Resolve ``--home`` to the complete real path, symlinks included."""
  raw = (home_raw or "").strip()
  if not raw:
    raise PreviewRefused("--home is required: give the preview instance its own directory")
  if not raw.startswith(("~", "/")):
    raise PreviewRefused(f"--home must be an absolute path or start with '~'; got {raw!r}")
  return _resolved(raw)


def check_home_location(home: Path, *, source_home: Path, source_workspace_dirs: list[str]) -> None:
  """Refuse a home overlapping the production home, production workspaces, or the checkout."""
  if _overlap(home, source_home):
    raise PreviewRefused(
        f"--home {home} overlaps the production home {source_home}; "
        "the preview needs a directory outside it")
  for raw_dir in source_workspace_dirs:
    workspace = _resolved(raw_dir)
    if _overlap(home, workspace):
      raise PreviewRefused(
          f"--home {home} overlaps the production workspace dir {workspace}; "
          "the preview needs a directory outside every configured workspace")
  if _overlap(home, REPO_ROOT):
    raise PreviewRefused(
        f"--home {home} overlaps the running checkout {REPO_ROOT}; "
        "the preview home must not live inside the source tree")


def check_port(port: int, *, source_server_port: int) -> None:
  """Refuse an out-of-range port, the source server's port, and an occupied one."""
  if not 1 <= port <= 65535:
    raise PreviewRefused(f"--port must be between 1 and 65535; got {port}")
  if port == source_server_port:
    raise PreviewRefused(
        f"--port {port} is the source profile's server port; the preview instance needs its own port")
  with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
    # SO_REUSEADDR keeps TIME_WAIT sockets from reading as occupancy; a live
    # listener still refuses the bind.
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
      sock.bind(("127.0.0.1", port))
    except OSError as e:
      raise PreviewRefused(f"--port {port} is not free on 127.0.0.1: {e}") from e


def shutil_which(binary: str) -> str | None:
  """Indirection over ``shutil.which`` so tests can simulate an absent launcher."""
  import shutil

  return shutil.which(binary)


def _launcher_supports_session_dir(binary: str) -> bool:
  """Whether *binary* advertises ``--session-dir`` (its native session-directory override)."""
  try:
    proc = subprocess.run([binary, "--help"], capture_output=True, text=True, check=False, timeout=30)
  except (OSError, subprocess.SubprocessError) as e:
    raise PreviewRefused(f"the charlie-code launcher at {binary} could not be run: {e}") from e
  return "--session-dir" in (proc.stdout + proc.stderr)


def check_launcher() -> None:
  """The charlie-code launcher must exist and support its session-directory override."""
  binary = shutil_which("charlie-code")
  if binary is None:
    raise PreviewRefused(
        "the charlie-code launcher is not installed; the preview cannot run its selected backend")
  if not _launcher_supports_session_dir(binary):
    raise PreviewRefused(
        "the installed charlie-code does not support --session-dir; native session isolation "
        "cannot be guaranteed, so the preview refuses to start")


def check_ui_assets() -> None:
  """The shipped UI must be present in the running tree before the instance starts."""
  missing = [str(p.relative_to(REPO_ROOT)) for p in (
      REPO_ROOT / "web" / "templates" / "index.html",
      REPO_ROOT / "web" / "static" / "css" / "tailwind.css",
  ) if not p.is_file()]
  if missing:
    raise PreviewRefused("the running checkout is missing the shipped UI assets: " + ", ".join(missing))


def _legacy_home_evidence(home: Path) -> list[str]:
  """Named evidence that an existing non-preview directory is not a trial home."""
  evidence: list[str] = []
  if (home / "config.yaml").is_file():
    evidence.append("config.yaml exists (an existing configuration this entry point did not seed)")
  if (home / "credentials.yaml").is_file():
    evidence.append("credentials.yaml exists")
  if (home / "memory").is_dir():
    evidence.append("memory/ exists")
  if (home / "config.d").is_dir():
    evidence.append("config.d/ exists (scheduled task configuration)")
  if (home / "triggers").is_dir():
    evidence.append("triggers/ exists")
  if (home / "session_tree_migration.json").is_file():
    evidence.append("session_tree_migration.json exists (migration products)")
  if (home / "session_aliases.json").is_file():
    evidence.append("session_aliases.json exists (migration products)")
  if (home / "state" / "session_tree_migration").is_dir():
    evidence.append("state/session_tree_migration/ exists (migration products)")
  legacy_sessions = 0
  sessions_dir = home / "sessions"
  if sessions_dir.is_dir():
    for meta_path in sorted(sessions_dir.glob("*/metadata.json"))[:50]:
      raw = load_json_meta(meta_path, "preview_home_meta_unreadable")
      if raw is not None and not raw.get("profile"):
        legacy_sessions += 1
  if legacy_sessions:
    evidence.append(f"sessions/ holds {legacy_sessions}+ legacy v1 session records")
  return evidence


def read_preview_record(home: Path) -> dict | None:
  """The validated preview instance record, or None when the home carries none."""
  raw = load_json_meta(preview_record_path(home), "preview_instance_record_unreadable")
  if not isinstance(raw, dict):
    return None
  if raw.get("format_version") != PREVIEW_FORMAT_VERSION:
    return None
  if raw.get("home") != str(home):
    return None
  return raw


# The writer fence's own state files: acquiring the fence creates them before
# the seed runs, so a directory holding only these is still a fresh seed target.
_FENCE_STATE_FILES = frozenset({"home_writer.lock", "writer_identity.json"})


def _effective_entries(home: Path) -> list[Path]:
  """The home's entries, ignoring a state dir that holds only the writer fence's files."""
  entries = list(home.iterdir())
  state = home / "state"
  if state.is_dir() and {p.name for p in state.iterdir()} <= _FENCE_STATE_FILES:
    entries = [entry for entry in entries if entry != state]
  return entries


def classify_home(home: Path) -> bool:
  """Is *home* a fresh seed target (absent, empty, or only fence state so far)?

  Anything else must be a validated preview home; a non-empty directory without
  a valid preview record refuses with the named evidence.
  """
  if not home.exists():
    return True
  if not home.is_dir():
    raise PreviewRefused(f"--home {home} exists and is not a directory")
  if _effective_entries(home):
    if read_preview_record(home) is None:
      evidence = _legacy_home_evidence(home)
      details = evidence or ["the directory is not empty and carries no session-tree preview record"]
      raise PreviewRefused(
          f"--home {home} exists and is not a session-tree preview home "
          "(no valid preview_instance.json record); refusing to touch it", details=details)
    return False
  return True


# ---------------------------------------------------------------------------
# Backend selection
# ---------------------------------------------------------------------------


def read_source_backend(backend_id: str) -> tuple[dict, tuple[str, str] | None]:
  """Read the one selected backend entry (and its credential section) from the source profile.

  Returns ``(entry, credential)`` where *credential* is
  ``(section_name, {"api_key": ...})`` only when the entry references one. The
  rest of the source profile — operator key, sessions, memory, schedules,
  triggers, native sessions — is never read into the trial.
  """
  if not backend_id:
    raise PreviewRefused(
        "a fresh preview home requires --backend ID: name the backend this trial instance will run")
  return _read_source_backend_option(backend_id)


def read_source_backend_additions(
    add_ids: list[str] | None, *, exclude_ids: set[str],
) -> list[tuple[dict, tuple[str, str] | None]]:
  """Read and validate the explicitly requested additional backend entries, in request order.

  Every requested id must resolve to an isolated charlie-code entry in the
  source profile with its referenced credential present; anything the schema
  cannot interpret already refuses inside ``load_config`` with the entry's own
  id and field named. A repeated request or an id already chosen for the trial
  refuses instead of silently deduplicating.
  """
  resolved: list[tuple[dict, tuple[str, str] | None]] = []
  seen: set[str] = set()
  for backend_id in (add_ids or []):
    if backend_id in seen:
      raise PreviewRefused(f"--add-backend {backend_id!r} is requested more than once")
    if backend_id in exclude_ids:
      raise PreviewRefused(
          f"--add-backend {backend_id!r} is already part of this trial's backend selection")
    resolved.append(_read_source_backend_option(backend_id))
    seen.add(backend_id)
  return resolved


def _read_source_backend_option(backend_id: str) -> tuple[dict, tuple[str, str] | None]:
  """One validated source entry: isolated type, runnable settings, referenced credential."""
  cfg = load_config()
  option = cfg.get_backend_option(backend_id)
  if option is None:
    raise PreviewRefused(f"backend {backend_id!r} is not configured in the source profile {cfg.config_file}")
  if option.type is not BackendType.CHARLIE_CODE:
    raise PreviewRefused(
        f"backend {backend_id!r} has type {option.type.value!r}; the preview isolates native "
        "session state only for charlie-code (its --session-dir override), so other backend "
        "types are refused until their isolation is proven")
  if not option.api_base:
    raise PreviewRefused(
        f"backend {backend_id!r} declares no api_base; the charlie-code adapter cannot run "
        "without one, so the entry is refused instead of seeded broken")
  entry = json.loads(option.model_dump_json())
  credential: tuple[str, str] | None = None
  if getattr(option, "credential", None):
    section = str(option.credential)
    creds = load_credentials()
    api_key = creds.get(section, "api_key")
    if api_key is None:
      raise PreviewRefused(
          f"backend {backend_id!r} references credentials.{section}.api_key, which is not set "
          f"in {creds.path}; the provider credential is required to run the trial")
    credential = (section, {"api_key": str(api_key)})
  return entry, credential


# ---------------------------------------------------------------------------
# Preparation (validation only — no writes)
# ---------------------------------------------------------------------------


@dataclass
class PreviewSetup:
  """Everything the entry point needs to seed, validate and run one preview instance."""
  home: Path
  port: int
  url: str
  backend_id: str
  backend_entry: dict
  # Explicitly requested additional entries (validated source reads): seeded on
  # a fresh home, appended to an existing validated home's catalog under the
  # fence. Empty unless the caller asked for them.
  backend_additions: list[tuple[dict, tuple[str, str] | None]]
  credential_section: tuple[str, str] | None
  fresh_hint: bool
  source_branch: str
  source_sha: str
  started_at: str
  log_path: Path
  native_sessions_dir: Path
  access_key: str


def _read_source_identity() -> tuple[str, str]:
  """The running checkout's branch and full HEAD SHA (the printed provenance)."""

  def git(*args: str) -> str:
    proc = subprocess.run(
        ["git", *args], cwd=str(REPO_ROOT), capture_output=True, text=True, check=True, timeout=30)
    return proc.stdout.strip()

  try:
    return git("rev-parse", "--abbrev-ref", "HEAD"), git("rev-parse", "HEAD")
  except (OSError, subprocess.SubprocessError, subprocess.CalledProcessError) as e:
    raise PreviewRefused(f"the running checkout has no readable git identity: {e}") from e


def _new_access_key() -> str:
  import secrets

  return "preview-key-" + secrets.token_hex(16)


def prepare_preview(home_raw: str, port: int, backend_id: str | None,
                    add_backend_ids: list[str] | None = None) -> PreviewSetup:
  """Validate every launch precondition and resolve the instance's identity; no writes.

  The environment still selects the source profile here: the backend entries and
  their credential material are read from it, the overlap and port checks judge
  the preview path against it, and the environment switch happens later in
  :func:`activate_preview_environment`.
  """
  try:
    return _prepare_preview(home_raw, port, backend_id, add_backend_ids)
  except PreviewRefused:
    raise
  except (ValueError, OSError) as e:
    # A source profile that cannot be read (malformed config/credentials) is an
    # environmental refusal, not a traceback: surface it structured.
    raise PreviewRefused(f"preview preparation could not read the source profile: {e}") from e


def _prepare_preview(home_raw: str, port: int, backend_id: str | None,
                     add_backend_ids: list[str] | None) -> PreviewSetup:
  home = resolve_preview_home(home_raw)
  source_cfg = load_config()
  check_home_location(home, source_home=charliebot_home_dir(),
                      source_workspace_dirs=list(source_cfg.paths.workspace_dirs))
  check_port(port, source_server_port=source_cfg.server.port)
  fresh = classify_home(home)
  if fresh:
    entry, credential = read_source_backend(backend_id or "")
    additions = read_source_backend_additions(add_backend_ids, exclude_ids={backend_id} if backend_id else set())
  else:
    entries, stored_backend_id, credential = _stored_backend_catalog(home)
    if backend_id and backend_id != stored_backend_id:
      raise PreviewRefused(
          f"--backend {backend_id!r} does not match the preview home's configured backend "
          f"{stored_backend_id!r}; restart either without --backend or with the home's own backend")
    backend_id = stored_backend_id
    entry = entries[0]
    additions = read_source_backend_additions(add_backend_ids, exclude_ids={e["id"] for e in entries})
  assert backend_id is not None
  check_launcher()
  check_ui_assets()
  branch, sha = _read_source_identity()
  access_key = _existing_access_key(home) if not fresh else _new_access_key()
  return PreviewSetup(
      home=home,
      port=port,
      url=f"http://127.0.0.1:{port}",
      backend_id=backend_id,
      backend_entry=entry,
      backend_additions=additions,
      credential_section=credential,
      fresh_hint=fresh,
      source_branch=branch,
      source_sha=sha,
      started_at=datetime.now(UTC).isoformat(),
      log_path=home / PREVIEW_LOG_DIRNAME / f"preview-{datetime.now(UTC).strftime('%Y%m%d-%H%M%S')}.log",
      native_sessions_dir=home / PREVIEW_NATIVE_DIRNAME,
      access_key=access_key,
  )


# ---------------------------------------------------------------------------
# Existing preview home readers / validators
# ---------------------------------------------------------------------------


def validate_existing_config(home: Path) -> dict:
  """Load and structurally validate the preview home's config; refuse unrelated shapes."""
  path = home / "config.yaml"
  data = load_yaml(path)
  if not isinstance(data, dict):
    raise PreviewRefused(f"{path} is not a config mapping; this is not a usable preview home")
  unexpected = sorted(set(data) - _ALLOWED_CONFIG_TOP_KEYS)
  if unexpected:
    raise PreviewRefused(
        f"{path} carries sections outside the preview trial contract: {', '.join(unexpected)}; "
        "a preview home holds only server/paths/backends",
        details=[f"unexpected section: {key}" for key in unexpected])
  missing = sorted(_ALLOWED_CONFIG_TOP_KEYS - set(data))
  if missing:
    raise PreviewRefused(f"{path} is missing the preview trial sections: {', '.join(missing)}")
  try:
    CharlieBotConfig(charliebot_home=home, **data)
  except Exception as e:
    raise PreviewRefused(f"{path} does not validate against the config schema: {e}") from e
  options = data["backends"].get("options")
  if not isinstance(options, list) or not options:
    count = len(options) if isinstance(options, list) else "non-list"
    raise PreviewRefused(f"{path} must configure at least one backend for the trial; found {count}")
  seen_ids: set[str] = set()
  for entry in options:
    if not isinstance(entry, dict) or entry.get("type") != BackendType.CHARLIE_CODE.value:
      raise PreviewRefused(
          f"{path} configures a non-charlie-code backend; the preview isolates native state only "
          "for charlie-code")
    entry_id = entry.get("id")
    if not entry_id:
      raise PreviewRefused(f"{path} configures a backend without an id")
    if entry_id in seen_ids:
      raise PreviewRefused(f"{path} configures backend id {entry_id!r} more than once")
    seen_ids.add(str(entry_id))
  for raw_dir in data["paths"].get("workspace_dirs", []):
    workspace = _resolved(raw_dir)
    if not _contains(home, workspace):
      raise PreviewRefused(
          f"{path} points paths.workspace_dirs outside the preview home ({workspace}); the trial "
          "workspace boundary must stay inside the home")
  worktree_dir = _resolved(data["paths"].get("worktree_dir", ""))
  if not _contains(home, worktree_dir):
    raise PreviewRefused(
        f"{path} points paths.worktree_dir outside the preview home ({worktree_dir}); worker "
        "worktrees must stay inside the home")
  creds = load_yaml(home / "credentials.yaml", default={})
  for entry in options:
    referenced = entry.get("credential")
    if referenced:
      if not isinstance(creds, dict) or not (creds.get(str(referenced)) or {}).get("api_key"):
        raise PreviewRefused(
            f"the preview home's credentials.yaml has no {referenced}.api_key for the configured "
            "backend; the provider credential is required to run the trial")
  return data


def _stored_backend_catalog(home: Path) -> tuple[list[dict], str, tuple[str, str] | None]:
  """The existing preview home's backend catalog, its default id and the default's credential."""
  options = validate_existing_config(home)["backends"]["options"]
  entries = [dict(entry) for entry in options]
  backend_id = entries[0].get("id")
  if not backend_id:
    raise PreviewRefused(f"{home / 'config.yaml'} configures a backend without an id")
  credential: tuple[str, str] | None = None
  referenced = entries[0].get("credential")
  if referenced:
    creds = load_yaml(home / "credentials.yaml", default={})
    key = (creds or {}).get(str(referenced), {}).get("api_key") if isinstance(creds, dict) else None
    credential = (str(referenced), {"api_key": str(key)})
  return entries, str(backend_id), credential


def _existing_access_key(home: Path) -> str:
  """The preview home's own access key; restarts keep it so saved logins keep working."""
  creds = load_yaml(home / "credentials.yaml", default={})
  key = creds.get("charliebot", {}).get("access_key") if isinstance(creds, dict) else None
  if not key:
    raise PreviewRefused(
        f"{home / 'credentials.yaml'} has no charliebot.access_key; the preview instance's "
        "browser credential is missing and must be restored before restart")
  return str(key)


# ---------------------------------------------------------------------------
# Seeding / validation under the fence
# ---------------------------------------------------------------------------


def _preview_config_data(setup: PreviewSetup) -> dict:
  """The minimal private trial config: bind address, scoped paths, the selected backends.

  The explicitly requested default stays the first option (the resolution every
  default-backend reader uses) and the only preference entry, so no Run ever
  falls over to another model silently; the additional selected entries sit
  after it as explicit choices only.
  """
  return {
      "server": {"host": "127.0.0.1", "port": setup.port},
      "paths": {
          "workspace_dirs": [str(setup.home / PREVIEW_WORKSPACES_DIRNAME)],
          "worktree_dir": str(setup.home / PREVIEW_WORKTREES_DIRNAME),
      },
      "backends": {
          "options": [setup.backend_entry, *[entry for entry, _ in setup.backend_additions]],
          "preference": [setup.backend_id],
      },
  }


def _credentials_text(setup: PreviewSetup) -> str:
  lines = [f"charliebot:\n  access_key: {setup.access_key}\n"]
  written: set[str] = set()
  for credential in (setup.credential_section, *[cred for _, cred in setup.backend_additions]):
    if credential is None:
      continue
    section, keys = credential
    if section in written:
      continue
    written.add(section)
    lines.append(f"{section}:\n  api_key: {keys['api_key']}\n")
  return "".join(lines)


def write_instance_record(setup: PreviewSetup, *, ready: bool, stopped: bool = False) -> None:
  """Write the instance record: the serving identity, provenance and readiness."""
  pid_start = read_pid_stat(os.getpid())
  record: dict[str, Any] = {
      "format_version": PREVIEW_FORMAT_VERSION,
      "kind": "session-tree-preview",
      "home": str(setup.home),
      "port": setup.port,
      "url": setup.url,
      "backend": setup.backend_id,
      "source_branch": setup.source_branch,
      "source_sha": setup.source_sha,
      "pid": os.getpid(),
      "pid_start": pid_start[0] if pid_start else None,
      "started_at": setup.started_at,
      "ready": ready,
  }
  if ready:
    record["ready_at"] = datetime.now(UTC).isoformat()
  if stopped:
    record["stopped_at"] = datetime.now(UTC).isoformat()
  record_path = preview_record_path(setup.home)
  record_path.parent.mkdir(parents=True, exist_ok=True)
  atomic_write_text(record_path, json.dumps(record, indent=2, sort_keys=True))


def _extend_existing_home_catalog(setup: PreviewSetup) -> None:
  """Append the validated requested entries to an existing home's catalog; the fence is held.

  The authoritative duplicate check runs here, under the fence, against the
  home's current config (a caller between preparation and fence acquisition
  could have changed it). Only the options list and missing referenced
  credential sections are written: the default, its order, the bind address,
  the paths, the stored access key and every existing section stay as they are.
  """
  config_path = setup.home / "config.yaml"
  data = validate_existing_config(setup.home)
  options = data["backends"]["options"]
  existing_ids = {str(entry["id"]) for entry in options}
  new_credentials: dict[str, str] = {}
  for entry, credential in setup.backend_additions:
    if entry["id"] in existing_ids:
      raise PreviewRefused(
          f"--add-backend {entry['id']!r} is already in the preview home's catalog; "
          "nothing to add")
    options.append(entry)
    if credential is not None:
      section, keys = credential
      new_credentials.setdefault(section, keys["api_key"])
  # The credentials store's shape validates before the first write too: a
  # refusal here must leave the home untouched, not half-extended.
  credentials_path = setup.home / "credentials.yaml"
  creds = load_yaml(credentials_path, default={})
  if not isinstance(creds, dict):
    raise PreviewRefused(f"{credentials_path} is not a credentials mapping; refusing to extend it")
  save_yaml(config_path, data)
  added_sections = []
  for section, api_key in new_credentials.items():
    if (creds.get(section) or {}).get("api_key"):
      continue  # the home already holds this provider credential; never overwrite a stored secret
    creds[section] = {"api_key": api_key}
    added_sections.append(section)
  if added_sections:
    atomic_write_text(
        credentials_path,
        yaml.safe_dump(creds, allow_unicode=True, default_flow_style=False),
        private=True)
  log.info(
      "preview_catalog_extended", home=str(setup.home),
      added=[entry["id"] for entry, _ in setup.backend_additions],
      credential_sections_added=sorted(added_sections))


def seed_or_validate_preview_home(setup: PreviewSetup) -> None:
  """Seed a fresh preview home or validate the existing one; the caller holds the writer fence.

  A fresh home gets the minimal private config (bind address, scoped paths, the
  one selected backend), its own random access key, the preview directory
  scaffold, and the instance record. An existing validated home keeps its config
  and tasks; only the instance-owned bind address is updated and the record is
  rewritten for this run.
  """
  if classify_home(setup.home):
    setup.home.mkdir(parents=True, exist_ok=True)
    save_yaml(setup.home / "config.yaml", _preview_config_data(setup))
    atomic_write_text(setup.home / "credentials.yaml", _credentials_text(setup), private=True)
    for dirname in (PREVIEW_NATIVE_DIRNAME, PREVIEW_WORKSPACES_DIRNAME, PREVIEW_WORKTREES_DIRNAME,
                    PREVIEW_LOG_DIRNAME):
      (setup.home / dirname).mkdir(parents=True, exist_ok=True)
    log.info("preview_home_seeded", home=str(setup.home), backend=setup.backend_id)
  else:
    data = validate_existing_config(setup.home)
    server = dict(data.get("server") or {})
    if server.get("port") != setup.port or server.get("host") != "127.0.0.1":
      server.update({"host": "127.0.0.1", "port": setup.port})
      data["server"] = server
      save_yaml(setup.home / "config.yaml", data)
      log.info("preview_bind_address_updated", home=str(setup.home), port=setup.port)
    if setup.backend_additions:
      _extend_existing_home_catalog(setup)
    # A restart never rekeys the instance: the stored key stays authoritative.
    setup.access_key = _existing_access_key(setup.home)
    for dirname in (PREVIEW_NATIVE_DIRNAME, PREVIEW_WORKSPACES_DIRNAME, PREVIEW_WORKTREES_DIRNAME,
                    PREVIEW_LOG_DIRNAME):
      (setup.home / dirname).mkdir(parents=True, exist_ok=True)
    log.info("preview_home_reused", home=str(setup.home))
  write_instance_record(setup, ready=False)


# ---------------------------------------------------------------------------
# Environment selection and native-state isolation
# ---------------------------------------------------------------------------


def activate_preview_environment(setup: PreviewSetup) -> None:
  """Select the preview profile before any cached config or singleton binds to the old home."""
  os.environ[CHARLIEBOT_HOME_ENV] = str(setup.home)
  for var in _INHERITED_IDENTITY_ENV_VARS:
    os.environ.pop(var, None)
  resolved = charliebot_home_dir()
  if resolved != setup.home:
    raise PreviewRefused(
        f"the selected home resolved to {resolved}, not the requested {setup.home}")
  cfg = load_config()
  if cfg.charliebot_home != setup.home:
    raise PreviewRefused(
        f"the loaded config still binds {cfg.charliebot_home}; the preview environment did not switch")
  log.info("preview_environment_selected", home=str(setup.home), backend=setup.backend_id)


def assert_no_bound_singletons() -> None:
  """Fail fast if an application singleton was constructed before the environment switch."""
  from src.api import deps

  bound = [name for name in ("_session_manager", "_thread_manager", "_trigger_manager", "_task_manager")
           if getattr(deps, name, None) is not None]
  if bound:
    raise PreviewRefused(
        "application singletons bound before the preview environment switched: " + ", ".join(bound))


def wrap_build_backend(original: Callable[..., Any], clc_sessions: Path) -> Callable[..., Any]:
  """Wrap one ``build_backend`` so charlie-code builds carry ``--session-dir`` isolation."""

  def wrapped(option: Any, cfg: Any, **kwargs: Any) -> Any:
    if getattr(option, "type", None) is BackendType.CHARLIE_CODE:
      kwargs["extra_flags"] = [
          *(kwargs.get("extra_flags") or []),
          "--session-dir",
          str(clc_sessions),
      ]
    return original(option, cfg, **kwargs)

  return wrapped


def install_native_session_isolation(clc_sessions: Path) -> None:
  """Route every charlie-code build in this process through the home's own session dir.

  The registry is the one backend construction path (manager turns, worker work
  and review runs, retries, continuations all resolve through it), and the
  worker module's bound name is its other documented patch target. The flag is
  the CLI's own session-directory override — the host installation and its
  config are never modified.
  """
  import importlib

  registry = importlib.import_module("src.agents.backends.registry")
  worker_module = importlib.import_module("src.agents.worker")
  wrapped = wrap_build_backend(registry.build_backend, clc_sessions)
  registry.build_backend = wrapped
  worker_module.build_backend = wrapped
  log.info("preview_native_session_isolation_installed", clc_sessions=str(clc_sessions))


def make_workspace_guard(cfg: CharlieBotConfig) -> Callable[[Path], None]:
  """The launcher workspace boundary: only the preview-owned workspace dirs are selectable."""
  roots = [_resolved(d) for d in cfg.paths.workspace_dirs]

  def guard(repo_path: Path) -> None:
    resolved = _resolved(repo_path)
    if not any(_contains(root, resolved) for root in roots):
      raise PreviewWorkspaceError(
          f"preview workspace boundary: {resolved} is outside this instance's workspace dirs "
          f"{[str(r) for r in roots]}")

  return guard


# ---------------------------------------------------------------------------
# Preview lifetime and reachable-mechanism gate
# ---------------------------------------------------------------------------


def refusal_reason(path: str, method: str) -> str | None:
  """The preview gate's refusal for a mutation of a mechanism the trial does not run."""
  if method not in _REFUSED_MUTATION_METHODS:
    return None
  if path.startswith(_PREVIEW_REFUSED_MUTATION_PREFIXES):
    return _CRON_DISABLED_REASON
  if path in _PREVIEW_REFUSED_MUTATION_PATHS:
    return _EXTERNAL_DISABLED_REASON
  return None


class PreviewUnavailableGate:
  """Refuse the mechanisms the preview does not run, at the request boundary.

  Pure ASGI like the server's own middleware: mutation routes of the scheduler
  domain and the external messaging/trigger entries answer 403 with the reason,
  and the host-global terminal websocket is closed before accept. Read-only
  routes keep serving the instance's own home.
  """

  def __init__(self, app: Any) -> None:
    self.app = app

  async def __call__(self, scope: dict, receive: Any, send: Any) -> None:
    scope_type = scope["type"]
    if scope_type == "websocket":
      if scope.get("path") in _REFUSED_WEBSOCKET_PATHS:
        log.info("preview_gate_refused", path=scope.get("path"), kind="websocket")
        await send({"type": "websocket.close", "code": _WS_REFUSED_CLOSE_CODE})
        return
      await self.app(scope, receive, send)
      return
    if scope_type != "http":
      await self.app(scope, receive, send)
      return
    path = scope.get("path", "")
    method = scope.get("method", "GET")
    reason = refusal_reason(path, method)
    if reason is None:
      await self.app(scope, receive, send)
      return
    log.info("preview_gate_refused", path=path, method=method)
    body = json.dumps({"detail": reason}).encode("utf-8")
    await send({
        "type": "http.response.start",
        "status": 403,
        "headers": [
            (b"content-type", b"application/json"),
            (b"content-length", str(len(body)).encode("latin-1")),
        ],
    })
    await send({"type": "http.response.body", "body": body})


_preview_active = False


def is_preview_mode() -> bool:
  """Whether this process serves a session-tree preview instance (drives the UI indicator)."""
  return _preview_active


def _activate_preview_mode() -> None:
  global _preview_active
  _preview_active = True


def make_preview_lifespan(setup: PreviewSetup) -> Callable[[Any], AsyncIterator[None]]:
  """The preview application lifetime: this instance's own recovery, nothing global."""

  @asynccontextmanager
  async def lifespan(app: Any) -> AsyncIterator[None]:
    cfg = get_config()
    if cfg.charliebot_home != setup.home:
      raise PreviewRefused(
          f"the preview lifespan bound {cfg.charliebot_home}, not the prepared home {setup.home}")
    boot_time = utc_now()
    try:
      init_build_info()
      await init_charliebot_home()
      log.info("preview_home_ready", path=str(cfg.charliebot_home))
      # The v2 recovery owner reconciles only this instance's own task nodes
      # and their Runs. The v1 scan, scheduler, trigger recovery, external
      # messaging, global cgroup sweep and the other shared provisioners never
      # start in a preview instance; the request-boundary gate keeps their
      # routes unreachable.
      from src.api.deps import session_manager, task_manager

      tree = task_manager()
      tree.dispatch.executor.launch_workspace_guard = make_workspace_guard(cfg)
      stats = await reconcile_task_tree(cfg, tree, session_manager())
      log.info("preview_task_tree_recovery_done", **stats)
      _activate_preview_mode()
      write_instance_record(setup, ready=True)
      print(
          f"session-tree preview ready: {setup.url} (home: {setup.home}, "
          f"branch: {setup.source_branch}, sha: {setup.source_sha})",
          flush=True)
      yield
    finally:
      from src.api import ext_usage, pages
      from src.core.http import close_http_client
      from src.core.streaming import streaming_manager

      await ext_usage.stop_poller()
      await close_http_client()
      await streaming_manager.close_all()
      pages.shutdown_merge_executor()
      write_instance_record(setup, ready=False, stopped=True)
      log.info("preview_shutdown", home=str(setup.home), uptime_s=round(
          (utc_now() - boot_time).total_seconds(), 1))

  return lifespan


def build_preview_app(setup: PreviewSetup) -> Any:
  """The real shipped application with the preview lifetime and the mechanism gate."""
  if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))
  import importlib

  app = importlib.import_module("server").app
  app.router.lifespan_context = make_preview_lifespan(setup)
  app.add_middleware(PreviewUnavailableGate)
  return app


class _TeeStream:
  """Duplicate one output stream into the preview home's log file, flushed per write."""

  def __init__(self, stream: Any, log_file: Any) -> None:
    self._stream = stream
    self._log_file = log_file
    self._lock = threading.Lock()

  def write(self, data: str) -> int:
    with self._lock:
      self._stream.write(data)
      self._stream.flush()
      self._log_file.write(data)
      self._log_file.flush()
    return len(data)

  def flush(self) -> None:
    with self._lock:
      self._stream.flush()
      self._log_file.flush()

  @property
  def encoding(self) -> str | None:
    return getattr(self._stream, "encoding", None)

  def isatty(self) -> bool:
    return False


def install_log_capture(log_path: Path) -> None:
  """Keep every log and generated line inside the preview home while echoing to the console."""
  log_path.parent.mkdir(parents=True, exist_ok=True)
  log_file = log_path.open("a", encoding="utf-8")
  sys.stdout = _TeeStream(sys.stdout, log_file)  # type: ignore[assignment]
  sys.stderr = _TeeStream(sys.stderr, log_file)  # type: ignore[assignment]


# ---------------------------------------------------------------------------
# The entry point
# ---------------------------------------------------------------------------


def run_preview_command(home_raw: str, port: int, backend_id: str | None,
                        add_backend_ids: list[str] | None = None) -> None:
  """Prepare, validate and run one foreground preview instance; returns after clean shutdown.

  Every refusal exits through :class:`PreviewRefused` before any write; the
  writer fence is held for the whole run and released on every exit, including
  startup and shutdown failures. ``add_backend_ids`` are extra explicitly
  selected charlie-code entries: seeded on a fresh home, appended to an
  existing validated home's catalog once the fence is held.
  """
  setup = prepare_preview(home_raw, port, backend_id, add_backend_ids)
  fence: HomeWriterFence = acquire_home_writer_fence(setup.home, purpose="session-tree preview")
  try:
    seed_or_validate_preview_home(setup)
    assert_no_bound_singletons()
    activate_preview_environment(setup)
    install_native_session_isolation(setup.native_sessions_dir)
    install_log_capture(setup.log_path)
    print(
        f"starting session-tree preview: home={setup.home} port={setup.port} "
        f"backend={setup.backend_id} branch={setup.source_branch} sha={setup.source_sha}",
        flush=True)
    _run_uvicorn(setup)
  finally:
    fence.release()


def _run_uvicorn(setup: PreviewSetup) -> None:
  import uvicorn

  from src.core.timeouts import SERVER_GRACEFUL_SHUTDOWN_TIMEOUT

  uvicorn.run(
      build_preview_app(setup),
      host="127.0.0.1",
      port=setup.port,
      log_level="warning",
      # uvicorn 0.42 applies this to uvicorn.error, uvicorn.access and
      # uvicorn.asgi; the request-log middleware covers the request lines.
      timeout_graceful_shutdown=SERVER_GRACEFUL_SHUTDOWN_TIMEOUT,
  )
