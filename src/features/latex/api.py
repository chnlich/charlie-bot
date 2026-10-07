"""LaTeX API routes — compile, serve PDF, read/write .tex source."""

import asyncio
from collections.abc import Callable

import fastapi
import pydantic
from fastapi import responses

from src.features.latex import latex
from src.infra import log_once

log = log_once.LazyStructlogLogger()

router = fastapi.APIRouter()


class TexSourceRequest(pydantic.BaseModel):
  content: str


@router.get('/git-info')
async def get_git_info_endpoint() -> responses.JSONResponse:
  info = await latex.get_git_info()
  if info is None:
    return responses.JSONResponse(content={'error': 'Not a git repo'}, status_code=404)
  return responses.JSONResponse(content=info)


@router.post('/compile')
async def compile_tex() -> responses.JSONResponse:
  """Compile the LaTeX project (runs make pdf)."""
  result = await latex.compile_latex()
  status = 200 if result['ok'] else 500
  return responses.JSONResponse(content=result, status_code=status)


@router.get('/pdf')
async def get_pdf() -> responses.Response:
  """Serve the compiled PDF file."""
  pdf = latex.get_pdf_path()
  if not pdf.exists():
    return responses.JSONResponse(content={'error': 'PDF not found. Compile first.'}, status_code=404)
  return responses.FileResponse(str(pdf), media_type='application/pdf')


@router.get('/source')
async def get_source() -> responses.Response:
  """Read the .tex source file."""
  tex = latex.get_tex_path()
  if not tex.exists():
    return responses.JSONResponse(content={'error': 'Source file not found'}, status_code=404)
  content = await asyncio.to_thread(tex.read_text, encoding='utf-8')
  return responses.PlainTextResponse(content)


@router.put('/source')
async def put_source(req: TexSourceRequest) -> dict:
  """Write the .tex source file."""
  tex = latex.get_tex_path()
  await asyncio.to_thread(tex.write_text, req.content, encoding='utf-8')
  log.info('latex_source_saved', path=str(tex), size=len(req.content))
  return {'ok': True}


@router.get('/diff')
async def get_diff() -> responses.JSONResponse:
  """Return pending AI-proposed diff {old, new}."""
  proposal = latex.get_pending_proposal()
  if proposal is None:
    return responses.JSONResponse(content={'error': 'No pending proposal'}, status_code=404)
  return responses.JSONResponse(content={'old': proposal['old'], 'new': proposal['new']})


async def _settle_proposal(settle: Callable[[], bool], log_event: str) -> dict | responses.JSONResponse:
  """Run one pending-proposal consumer off the event loop; 404 when none is pending."""
  if await asyncio.to_thread(settle):
    log.info(log_event)
    return {'ok': True}
  return responses.JSONResponse(content={'error': 'No pending proposal'}, status_code=404)


@router.post('/accept', response_model=None)
async def accept_edit() -> dict | responses.JSONResponse:
  """Accept the pending AI-proposed TeX edit."""
  return await _settle_proposal(latex.accept_proposal, 'latex_proposal_accepted')


@router.post('/reject', response_model=None)
async def reject_edit() -> dict | responses.JSONResponse:
  """Reject the pending AI-proposed TeX edit."""
  return await _settle_proposal(latex.reject_proposal, 'latex_proposal_rejected')
