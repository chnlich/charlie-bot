"""Git-based backup for CharlieBot state (~/.charliebot/ as a git repo)."""

import asyncio

import structlog

from src.core.config import CharliBotConfig

log = structlog.get_logger()


async def _run(cmd: list[str], cwd: str) -> tuple[int, str]:
  proc = await asyncio.create_subprocess_exec(
    *cmd,
    cwd=cwd,
    stdout=asyncio.subprocess.PIPE,
    stderr=asyncio.subprocess.PIPE,
  )
  stdout, stderr = await proc.communicate()
  return proc.returncode or 0, (stdout or stderr).decode().strip()


async def init_backup_repo(cfg: CharliBotConfig) -> None:
  """Ensure ~/.charliebot/ is a git repo for backup purposes."""
  home = str(cfg.charliebot_home)
  git_dir = cfg.charliebot_home / ".git"
  if git_dir.exists():
    return

  code, out = await _run(["git", "init"], cwd=home)
  if code != 0:
    log.error("backup_git_init_failed", output=out)
    return

  # Initial commit so the repo has a valid HEAD
  await _run(["git", "add", "-A"], cwd=home)
  await _run(["git", "commit", "-m", "chore: initial backup snapshot", "--allow-empty"], cwd=home)
  log.info("backup_repo_initialized", path=home)


async def run_backup(cfg: CharliBotConfig) -> None:
  """Stage all changes in ~/.charliebot/ and commit if there are diffs."""
  home = str(cfg.charliebot_home)

  await _run(["git", "add", "-A"], cwd=home)

  # Check if there is anything to commit
  code, _ = await _run(["git", "diff", "--cached", "--quiet"], cwd=home)
  if code == 0:
    return  # Nothing changed

  code, out = await _run(
    ["git", "commit", "-m", "chore: automatic backup snapshot"],
    cwd=home,
  )
  if code != 0:
    log.error("backup_commit_failed", output=out)
  else:
    log.info("backup_committed")
