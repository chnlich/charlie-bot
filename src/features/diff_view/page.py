"""The GitHub-style diff viewer page."""

import socket

from fastapi import APIRouter, Request
from fastapi.responses import HTMLResponse

from src.runtime import templating

router = APIRouter()


@router.get("/diff", response_class=HTMLResponse)
async def diff_viewer(request: Request) -> HTMLResponse:
  """Render the GitHub-style diff viewer page."""
  return templating.templates().TemplateResponse(
      request,
      "diff.html",
      context={
          "hostname": socket.gethostname(),
          "static_asset_version": templating.static_asset_version(),
      })
