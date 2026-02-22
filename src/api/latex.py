"""LaTeX API routes — compile, serve PDF, read/write .tex source."""

import structlog
from fastapi import APIRouter
from fastapi.responses import FileResponse, JSONResponse, PlainTextResponse
from pydantic import BaseModel

from src.core.latex import (
    accept_proposal,
    compile_latex,
    get_git_info,
    get_pdf_path,
    get_pending_proposal,
    get_tex_path,
    reject_proposal,
)

log = structlog.get_logger()

router = APIRouter()


class TexSourceRequest(BaseModel):
  content: str


@router.get('/git-info')
async def get_git_info_endpoint():
  info = await get_git_info()
  if info is None:
    return JSONResponse(content={'error': 'Not a git repo'}, status_code=404)
  return JSONResponse(content=info)


@router.post('/compile')
async def compile_tex():
  """Compile the LaTeX project (runs make pdf)."""
  result = await compile_latex()
  status = 200 if result['ok'] else 500
  return JSONResponse(content=result, status_code=status)


@router.get('/pdf')
async def get_pdf():
  """Serve the compiled PDF file."""
  pdf = get_pdf_path()
  if not pdf.exists():
    return JSONResponse(content={'error': 'PDF not found. Compile first.'}, status_code=404)
  return FileResponse(str(pdf), media_type='application/pdf')


@router.get('/source')
async def get_source():
  """Read the .tex source file."""
  tex = get_tex_path()
  if not tex.exists():
    return JSONResponse(content={'error': 'Source file not found'}, status_code=404)
  return PlainTextResponse(tex.read_text(encoding='utf-8'))


@router.put('/source')
async def put_source(req: TexSourceRequest):
  """Write the .tex source file."""
  tex = get_tex_path()
  tex.write_text(req.content, encoding='utf-8')
  log.info('latex_source_saved', path=str(tex), size=len(req.content))
  return {'ok': True}


@router.get('/diff')
async def get_diff():
  """Return pending AI-proposed diff {old, new}."""
  proposal = get_pending_proposal()
  if proposal is None:
    return JSONResponse(content={'error': 'No pending proposal'}, status_code=404)
  return JSONResponse(content={'old': proposal['old'], 'new': proposal['new']})


@router.post('/accept')
async def accept_edit():
  """Accept the pending AI-proposed TeX edit."""
  if accept_proposal():
    log.info('latex_proposal_accepted')
    return {'ok': True}
  return JSONResponse(content={'error': 'No pending proposal'}, status_code=404)


@router.post('/reject')
async def reject_edit():
  """Reject the pending AI-proposed TeX edit."""
  if reject_proposal():
    log.info('latex_proposal_rejected')
    return {'ok': True}
  return JSONResponse(content={'error': 'No pending proposal'}, status_code=404)
