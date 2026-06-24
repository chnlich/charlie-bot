"""code-server integration API routes."""

import shutil
import socket
import subprocess
import time
from pathlib import Path

import structlog
from fastapi import APIRouter, Depends, HTTPException, Query

from src.core.config import CharlieBotConfig, get_config

router = APIRouter()
log = structlog.get_logger()

_CODE_SERVER_HOST = "127.0.0.1"
_CONNECT_TIMEOUT_SEC = 0.2
_START_TIMEOUT_SEC = 5.0
_POLL_INTERVAL_SEC = 0.2


def _resolve_code_server_binary(cfg: CharlieBotConfig) -> str | None:
  if cfg.code_server_bin:
    return shutil.which(str(Path(cfg.code_server_bin).expanduser()))
  return shutil.which("code-server")


def is_code_server_available(cfg: CharlieBotConfig) -> bool:
  return _resolve_code_server_binary(cfg) is not None


def _resolve_folder_under_allowed_root(folder: str, cfg: CharlieBotConfig) -> Path:
  folder_path = Path(folder).expanduser().resolve()
  if not folder_path.is_dir():
    raise HTTPException(status_code=400, detail=f"Not a directory: {folder}")
  allowed_roots = [Path(d).expanduser().resolve() for d in cfg.workspace_dirs]
  allowed_roots.append(Path(cfg.worktree_dir).expanduser().resolve())
  if not any(folder_path.is_relative_to(root) for root in allowed_roots):
    raise HTTPException(status_code=400, detail="folder must be under configured workspace_dirs or worktree_dir")
  return folder_path


def _is_listening(port: int) -> bool:
  with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
    sock.settimeout(_CONNECT_TIMEOUT_SEC)
    return sock.connect_ex((_CODE_SERVER_HOST, port)) == 0


def _start_code_server(binary: str, config_path: Path) -> subprocess.Popen:
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
    folder: str = Query(..., description="Folder path to open in code-server"),
    cfg: CharlieBotConfig = Depends(get_config),
):
  binary = _resolve_code_server_binary(cfg)
  if binary is None:
    raise HTTPException(status_code=404, detail="code-server not available on this host")

  folder_path = _resolve_folder_under_allowed_root(folder, cfg)
  try:
    config_path = cfg.code_server_config_path
    port = cfg.code_server_listen_port
  except Exception as exc:
    log.exception("code_server_config_invalid")
    raise HTTPException(status_code=500, detail=str(exc)) from exc

  if not _is_listening(port):
    try:
      process = _start_code_server(binary, config_path)
    except OSError as exc:
      log.exception("code_server_start_failed")
      raise HTTPException(status_code=503, detail="failed to start code-server") from exc

    deadline = time.monotonic() + _START_TIMEOUT_SEC
    while time.monotonic() < deadline:
      if _is_listening(port):
        break
      if process.poll() is not None:
        log.warning("code_server_exited_before_listening", returncode=process.returncode)
        break
      time.sleep(_POLL_INTERVAL_SEC)

    if not _is_listening(port):
      raise HTTPException(status_code=503, detail="failed to start code-server")

  return {"port": port, "folder": str(folder_path)}
