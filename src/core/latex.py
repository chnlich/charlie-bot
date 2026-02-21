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
