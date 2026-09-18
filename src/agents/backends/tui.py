"""TUI backend — runs the `claude` CLI inside an isolated tmux session.

Each CharlieBot session_id maps to one tmux session named ``charliebot-{id}``
under the ``charliebot`` tmux socket so it never collides with the user's
normal tmux sessions. Per WebSocket connection, a ``tmux attach`` PTY is
spawned and bytes are forwarded between the PTY and the browser's xterm.js
terminal. Multiple browsers attaching the same session is tmux-native.
"""

from __future__ import annotations

import contextlib
import json
import os
import time
from pathlib import Path
from typing import TYPE_CHECKING, Any

from src.agents.backends.base import SKIP_PERMISSIONS_SETTINGS, build_claude_argv
from src.agents.backends.pty_common import (
    PTY_EXIT,
    PtyAttachment,
    _run_pty_relay,
    _start_tmux_session,
    _tmux_binary,
    # re-export: imported from this module by src/api/sessions.py + src/core/sessions.py
    # and monkeypatched here by tests
    kill_tmux_session,  # noqa: F401
    tmux_session_exists,
    tmux_session_name,
)
from src.core import claude_accounts
from src.core.config import CharlieBotConfig
from src.core.home import CLAUDE_CONFIG_DIR_ENV_VAR, default_claude_dir
from src.core.log_once import LazyStructlogLogger
from src.core.models import BackendType

log = LazyStructlogLogger()

# fastapi serves only run_tui_attachment's annotation (future-annotations keep it
# unevaluated); the module also rides the claude-sub worker launch via
# mark_project_trusted, so the web framework must stay out of its import.
if TYPE_CHECKING:
  from fastapi import WebSocket

_CLAUDE_TUI_SETTINGS = json.dumps(SKIP_PERMISSIONS_SETTINGS, separators=(",", ":"))
_BUSY_THRESHOLD_SECONDS = 3.0

# Transcript paths are stable per session id (claude treats a session's jsonl
# as an append-only log it never relocates), so a hit memoizes for the process
# life and the exists() recheck covers deletion. A miss re-globs only after
# this TTL, since a fresh session's jsonl appears when claude starts.
_JSONL_MISS_TTL_SECONDS = 30.0
_jsonl_path_memo: dict[str, tuple[Path | None, float]] = {}


def _find_existing_claude_jsonl(session_id: str) -> Path | None:
  """Glob ~/.claude/projects/*/<session_id>.jsonl and return first match (or None), memoized per session id."""
  entry = _jsonl_path_memo.get(session_id)
  if entry is not None:
    path, miss_deadline = entry
    if path is not None:
      if path.exists():
        return path
    elif time.monotonic() < miss_deadline:
      return None
  matches = claude_accounts.transcript_matches(default_claude_dir(), session_id)
  path = matches[0] if matches else None
  _jsonl_path_memo[session_id] = (path, time.monotonic() + _JSONL_MISS_TTL_SECONDS)
  return path


def reset_jsonl_memo_for_tests() -> None:
  _jsonl_path_memo.clear()


def _claude_jsonl_busy(session_id: str, threshold_seconds: float = _BUSY_THRESHOLD_SECONDS) -> bool:
  """Return True if claude's jsonl for this session was written to within threshold_seconds.
  Uses the same glob path as _find_existing_claude_jsonl. Returns False if no jsonl found."""
  jsonl = _find_existing_claude_jsonl(session_id)
  if jsonl is None:
    return False
  mtime = jsonl.stat().st_mtime
  return (time.time() - mtime) < threshold_seconds


def _claude_config_path() -> Path:
  config_dir = os.environ.get(CLAUDE_CONFIG_DIR_ENV_VAR)
  if config_dir:
    return Path(config_dir) / ".claude.json"
  return Path.home() / ".claude.json"


def mark_project_trusted(config: dict[str, Any], project_path: str) -> bool:
  """Mark *project_path* trusted in a ``.claude.json``-shaped *config* dict.

  *config* maps project path strings under ``"projects"`` to per-project entries.
  The entry gains ``hasTrustDialogAccepted: true`` and, when absent,
  ``projectOnboardingSeenCount: 1``; Claude Code skips its interactive workspace-trust
  dialog only when both are present. Returns True when the entry changed, so the caller
  writes the file only then.
  """
  project = config.setdefault("projects", {}).setdefault(project_path, {})
  changed = project.get("hasTrustDialogAccepted") is not True or "projectOnboardingSeenCount" not in project
  project["hasTrustDialogAccepted"] = True
  project.setdefault("projectOnboardingSeenCount", 1)
  return changed


def _ensure_claude_project_trusted(working_dir: Path) -> None:
  """Mark CharlieBot's generated Claude TUI cwd trusted before interactive startup."""
  project_path = str(working_dir.resolve())
  config_path = _claude_config_path()
  config = json.loads(config_path.read_text(encoding="utf-8")) if config_path.exists() else {}
  if not mark_project_trusted(config, project_path):
    return
  config_path.parent.mkdir(parents=True, exist_ok=True)
  config_path.write_text(json.dumps(config, indent=2) + "\n", encoding="utf-8")
  log.info("tui_claude_project_trusted", path=project_path, config_path=str(config_path))


async def ensure_tmux_session(
    session_id: str,
    working_dir: Path,
    *,
    model: str | None = None,
    effort: str | None = None,
    disallowed_tools: list[str] | None = None,
    inject_env: dict[str, str] | None = None,
    native_session_id: str | None = None,
    instructions_text: str | None = None,
) -> None:
  """Idempotently create the tmux session running Claude TUI in *working_dir*.

  ``native_session_id`` is the claude conversation id the TUI launches under —
  the task id on the v1 path, and the task id qualified by the launch's
  instruction hash on v2 (a changed hash starts a fresh native context; an
  unchanged hash resumes). ``instructions_text`` is written to the working
  directory's CLAUDE.md before the session exists, so the launched claude
  reads exactly the snapshot's managed instruction bytes.
  """
  name = tmux_session_name(session_id)
  working_dir.mkdir(parents=True, exist_ok=True)
  if instructions_text is not None:
    (working_dir / "CLAUDE.md").write_text(instructions_text, encoding="utf-8")
  _ensure_claude_project_trusted(working_dir)
  if await tmux_session_exists(session_id):
    return
  native_id = native_session_id or session_id
  resume = _find_existing_claude_jsonl(native_id) is not None
  command_args = build_claude_argv(
      native_id,
      resume,
      settings=_CLAUDE_TUI_SETTINGS,
      model=model,
      effort=effort,
      disallowed_tools=disallowed_tools,
  )
  tmux_env_args: list[str] = []
  if inject_env is not None:
    for key, value in inject_env.items():
      tmux_env_args.extend(["-e", f"{key}={value}"])
  log.info("tui_claude_invocation", mode="resume" if resume else "fresh", session_id=session_id)
  await _start_tmux_session(name, str(working_dir), tmux_env_args, command_args)
  log.info("tui_tmux_session_created", session_id=session_id, name=name, cwd=str(working_dir))


class TuiBackend:
  """Lightweight backend descriptor for tui-cli sessions.

  Does not inherit AgentBackend — the SDK streaming template is the wrong
  shape for a long-lived interactive PTY. The actual lifecycle work
  (tmux + PTY) lives in module-level helpers invoked from the WebSocket
  handler and session manager.
  """

  type = BackendType.TUI_CLI

  def __init__(self, **_kwargs: object) -> None:
    # Validate tmux is available at construction time so config errors fail fast.
    _tmux_binary()


async def run_tui_attachment(
    websocket: WebSocket, session_id: str, cfg: "CharlieBotConfig", task_tree: object = None,
) -> None:
  """Per-WS PTY loop: spawn `tmux attach`, pump bytes, handle pty_input/pty_resize.

  Returns when the WebSocket disconnects or the PTY exits. Called from the
  session WebSocket handler after subscription + catchup.

  A v2 task node launches its terminal through the context boundary: one Run
  per actual terminal launch, carrying the committed instruction snapshot, the
  run's own signed credential, and the (pid, pid_start) identity — the same
  Run/credential owners as a headless launch. Re-attaching a live terminal
  never creates a Run or relaunches anything; a later explicit terminal launch
  uses the current rules.
  """
  sessions_dir = cfg.sessions_dir
  launch: "TuiTaskLaunch | None" = None
  try:
    # Lazy: the launch seam lives with the other Run owners, whose module must
    # not be pulled onto this transport module's import path.
    from src.core.task_execution import TuiTaskLaunch, prepare_tui_task_launch
    launch = await prepare_tui_task_launch(cfg, session_id, task_tree)
  except Exception as e:  # surface to client
    log.exception("tui_task_launch_failed", session_id=session_id)
    with contextlib.suppress(Exception):
      await websocket.send_json({"type": PTY_EXIT, "error": str(e)})
    return
  try:
    if launch is None:
      await ensure_tmux_session(session_id, sessions_dir / session_id)
    else:
      await ensure_tmux_session(
          session_id,
          sessions_dir / session_id,
          model=launch.model,
          native_session_id=launch.native_session_id,
          inject_env=launch.inject_env,
          instructions_text=launch.instructions_text,
      )
      await launch.record_process()
  except Exception as e:  # surface to client
    log.exception("tui_ensure_session_failed", session_id=session_id)
    if launch is not None:
      # The Run is registered and its snapshot committed but nothing launched
      # (or the pane never appeared): land the definite terminal fact instead
      # of leaving a permanently queued ghost Run.
      from src.core.task_execution import fail_unlaunched_tui_run
      await fail_unlaunched_tui_run(launch._tree, session_id, launch.run_id, reason=str(e))
    with contextlib.suppress(Exception):
      await websocket.send_json({"type": PTY_EXIT, "error": str(e)})
    return
  finally:
    if launch is not None:
      from src.core.task_execution import release_tui_launch
      release_tui_launch(session_id)

  attachment = PtyAttachment(session_id)
  try:
    attachment.spawn()
  except Exception as e:
    log.exception("tui_pty_spawn_failed", session_id=session_id)
    with contextlib.suppress(Exception):
      await websocket.send_json({"type": PTY_EXIT, "error": str(e)})
    return

  try:
    await _run_pty_relay(websocket, attachment, pump_name=f"tui-pump-{session_id[:8]}")
  finally:
    try:
      from src.api.deps import session_manager
      from src.core.autonamer import maybe_auto_name_from_claude_ai_title

      session_mgr = session_manager()
      meta = await session_mgr.get_session(session_id)
      if meta is None:
        log.warning("tui_autoname_session_missing", session_id=session_id)
      else:
        await maybe_auto_name_from_claude_ai_title(meta, session_mgr)
    except Exception as e:  # autonaming must not break PTY cleanup
      log.warning("tui_autoname_failed", session_id=session_id, error=str(e), exc_info=True)
