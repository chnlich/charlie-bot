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


async def get_git_info() -> dict | None:
  """Return git repo info for the project dir, or None if not a git repo."""
  project_dir = str(LATEX_PROJECT['project_dir'])
  try:
    root_proc = await asyncio.create_subprocess_exec(
        'git', '-C', project_dir, 'rev-parse', '--show-toplevel',
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.DEVNULL,
    )
    root_out, _ = await root_proc.communicate()
    if root_proc.returncode != 0:
      return None
    repo_path = root_out.decode().strip()

    branch_proc = await asyncio.create_subprocess_exec(
        'git', '-C', project_dir, 'rev-parse', '--abbrev-ref', 'HEAD',
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
