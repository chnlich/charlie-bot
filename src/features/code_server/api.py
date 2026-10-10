"""code-server integration API routes."""

import pathlib
import shutil
import socket
import subprocess
import time

import fastapi

from src.features.code_server import config as code_server_config
from src.infra import config, log_once
from src.runtime.api import deps

router = fastapi.APIRouter()
log = log_once.LazyStructlogLogger()

_CODE_SERVER_HOST = "127.0.0.1"
_POLL_INTERVAL_SEC = 0.2

# Socket connect probe deciding whether an existing code-server already answers.
CODE_SERVER_CONNECT_TIMEOUT = 0.2  # seconds

# Wait for the spawned code-server to accept connections before giving up.
CODE_SERVER_START_TIMEOUT = 5.0  # seconds

# One client-visible spelling for both 503 raisers below (spawn failure and
# the failed wait-for-listen), so tests and greps pin a single home.
_START_FAILURE_DETAIL = "failed to start code-server"


def _resolve_code_server_executable(cfg: config.CharlieBotConfig) -> str | None:
  if cfg.code_server.bin:
    return shutil.which(str(pathlib.Path(cfg.code_server.bin).expanduser()))
  return shutil.which("code-server")


def is_code_server_available(cfg: config.CharlieBotConfig) -> bool:
  return _resolve_code_server_executable(cfg) is not None


def code_server_enabled() -> bool:
  """The template global of that name: whether the current config can open code-server."""
  return is_code_server_available(config.get_config())


def _resolve_folder_under_allowed_root(folder: str, cfg: config.CharlieBotConfig) -> pathlib.Path:
  folder_path = pathlib.Path(folder).expanduser().resolve()
  if not folder_path.is_dir():
    raise fastapi.HTTPException(status_code=400, detail=f"Not a directory: {folder}")
  allowed_roots = [pathlib.Path(d).expanduser().resolve() for d in cfg.paths.workspace_dirs]
  allowed_roots.append(pathlib.Path(cfg.paths.worktree_dir).expanduser().resolve())
  if not any(folder_path.is_relative_to(root) for root in allowed_roots):
    raise fastapi.HTTPException(
        status_code=400, detail="folder must be under configured paths.workspace_dirs or paths.worktree_dir")
  return folder_path


def _is_listening(port: int) -> bool:
  with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
    sock.settimeout(CODE_SERVER_CONNECT_TIMEOUT)
    return sock.connect_ex((_CODE_SERVER_HOST, port)) == 0


def _start_code_server(binary: str, config_path: pathlib.Path) -> subprocess.Popen:
  return subprocess.Popen(
      [binary, "--config", str(config_path)],
      stdin=subprocess.DEVNULL,
      stdout=subprocess.DEVNULL,
      stderr=subprocess.DEVNULL,
      start_new_session=True,
      close_fds=True,
  )


@router.get("/open")
def open_code_server(
    folder: str = fastapi.Query(..., description="Folder path to open in code-server"),
    cfg: config.CharlieBotConfig = fastapi.Depends(deps.get_config_on_loop),
) -> dict:
  binary = _resolve_code_server_executable(cfg)
  if binary is None:
    raise fastapi.HTTPException(status_code=404, detail="code-server not available on this host")

  folder_path = _resolve_folder_under_allowed_root(folder, cfg)
  try:
    config_path = code_server_config.code_server_config_path(cfg)
    port = code_server_config.code_server_listen_port(cfg)
  except Exception as exc:
    log.exception("code_server_config_invalid")
    raise fastapi.HTTPException(status_code=500, detail=str(exc)) from exc

  if not _is_listening(port):
    try:
      process = _start_code_server(binary, config_path)
    except OSError as exc:
      log.exception("code_server_start_failed")
      raise fastapi.HTTPException(status_code=503, detail=_START_FAILURE_DETAIL) from exc

    deadline = time.monotonic() + CODE_SERVER_START_TIMEOUT
    while time.monotonic() < deadline:
      if _is_listening(port):
        break
      if process.poll() is not None:
        log.warning("code_server_exited_before_listening", returncode=process.returncode)
        break
      time.sleep(_POLL_INTERVAL_SEC)

    if not _is_listening(port):
      raise fastapi.HTTPException(status_code=503, detail=_START_FAILURE_DETAIL)

  return {"port": port, "folder": str(folder_path)}
