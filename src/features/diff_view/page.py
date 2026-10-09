"""The GitHub-style diff viewer page."""

import socket

import fastapi
from fastapi import responses

from src.runtime import templating

router = fastapi.APIRouter()


@router.get("/diff", response_class=responses.HTMLResponse)
async def diff_viewer(request: fastapi.Request) -> responses.HTMLResponse:
  """Render the GitHub-style diff viewer page."""
  return templating.templates().TemplateResponse(
      request,
      "diff.html",
      context={
          "hostname": socket.gethostname(),
          "static_asset_version": templating.static_asset_version(),
      })
