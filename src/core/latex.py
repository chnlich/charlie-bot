"""LaTeX project configuration and compilation."""

import asyncio
from pathlib import Path

import structlog

log = structlog.get_logger()

# Single source of truth — change here when adding more projects.
LATEX_PROJECT = {
    'project_dir': Path('/home/chnlich/workspace/lichao-tree'),
    'tex_file': Path('doc/report.tex'),
    'pdf_file': Path('doc/report.pdf'),
    'build_cmd': 'make pdf',
}


def get_tex_path() -> Path:
  return LATEX_PROJECT['project_dir'] / LATEX_PROJECT['tex_file']


def get_pdf_path() -> Path:
  return LATEX_PROJECT['project_dir'] / LATEX_PROJECT['pdf_file']


# Proposal state — stores snapshot and pending AI edit for user review.
_pending_proposal: dict | None = None
_tex_snapshot: str | None = None


def snapshot_tex() -> None:
  """Read the current .tex file into _tex_snapshot."""
  global _tex_snapshot
  _tex_snapshot = get_tex_path().read_text(encoding='utf-8')


def check_tex_changed() -> dict | None:
  """Compare on-disk .tex with _tex_snapshot.

  If different: revert file to snapshot, store {old, new} in _pending_proposal,
  and return the proposal dict. If unchanged: return None.
  """
  global _pending_proposal, _tex_snapshot
  if _tex_snapshot is None:
    return None
  current = get_tex_path().read_text(encoding='utf-8')
  if current == _tex_snapshot:
    return None
  # Revert file to snapshot so the UI still shows the old content
  get_tex_path().write_text(_tex_snapshot, encoding='utf-8')
  _pending_proposal = {'old': _tex_snapshot, 'new': current}
  return _pending_proposal


def get_pending_proposal() -> dict | None:
  """Return the current pending proposal, or None."""
  return _pending_proposal


def accept_proposal() -> bool:
  """Write the proposed new content to disk and clear the proposal."""
  global _pending_proposal
  if _pending_proposal is None:
    return False
  get_tex_path().write_text(_pending_proposal['new'], encoding='utf-8')
  _pending_proposal = None
  return True


def reject_proposal() -> bool:
  """Clear the pending proposal (keep reverted on-disk content)."""
  global _pending_proposal
  if _pending_proposal is None:
    return False
  _pending_proposal = None
  return True


def clear_snapshot() -> None:
  """Clear _tex_snapshot (called when no change was detected)."""
  global _tex_snapshot
  _tex_snapshot = None


async def get_git_info() -> dict | None:
  """Return git repo info for the project dir, or None if not a git repo."""
  project_dir = str(LATEX_PROJECT['project_dir'])
  try:
    root_proc = await asyncio.create_subprocess_exec(
        'git',
        '-C',
        project_dir,
        'rev-parse',
        '--show-toplevel',
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.DEVNULL,
    )
    root_out, _ = await root_proc.communicate()
    if root_proc.returncode != 0:
      return None
    repo_path = root_out.decode().strip()

    branch_proc = await asyncio.create_subprocess_exec(
        'git',
        '-C',
        project_dir,
        'rev-parse',
        '--abbrev-ref',
        'HEAD',
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.DEVNULL,
    )
    branch_out, _ = await branch_proc.communicate()
    branch = branch_out.decode().strip() if branch_proc.returncode == 0 else 'unknown'

    return {'repo_name': Path(repo_path).name, 'repo_path': repo_path, 'branch': branch}
  except Exception as e:
    log.warning('get_git_info_error', error=str(e))
    return None


async def compile_latex() -> dict:
  """Run make pdf in the project dir. Returns {ok, log}."""
  project_dir = LATEX_PROJECT['project_dir']
  cmd = LATEX_PROJECT['build_cmd']
  log.info('latex_compile_start', project_dir=str(project_dir))
  try:
    proc = await asyncio.create_subprocess_shell(
        cmd,
        cwd=str(project_dir),
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.STDOUT,
    )
    stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=60)
    output = stdout.decode('utf-8', errors='replace')
    ok = proc.returncode == 0
    if not ok:
      log.warning('latex_compile_failed', returncode=proc.returncode)
    else:
      log.info('latex_compile_done')
    return {'ok': ok, 'log': output}
  except asyncio.TimeoutError:
    log.warning('latex_compile_timeout')
    return {'ok': False, 'log': 'Compilation timed out after 60s'}
  except Exception as e:
    log.warning('latex_compile_error', error=str(e))
    return {'ok': False, 'log': str(e)}
