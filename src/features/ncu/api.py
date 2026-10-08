"""The Nsight Compute (.ncu-rep) report viewer page."""

import asyncio
from pathlib import Path

from fastapi import APIRouter, Query, Request
from fastapi.responses import HTMLResponse
from starlette.responses import Response

from src.infra.constants import FILE_SERVER_MOUNTS, NCU_VIEWER_PATH
from src.runtime import templating

router = APIRouter()


def _ncu_error_page(request: Request, message: str, status_code: int) -> HTMLResponse:
  """Render ncu.html with a clean error message and a 4xx status."""
  return templating.templates().TemplateResponse(
      request,
      "ncu.html",
      context={
          "error": message,
          "report": None
      },
      status_code=status_code,
  )


@router.get(NCU_VIEWER_PATH, response_class=HTMLResponse)
async def ncu_viewer(
    request: Request,
    file: list[str] = Query(default=[]),
) -> Response:
  """Render the Nsight Compute (.ncu-rep) report viewer page.

  `file` is a repeatable list of absolute paths. v1 renders the first report;
  additional paths are accepted but only noted, not diffed.
  """
  if not file:
    return _ncu_error_page(
        request,
        "No report specified. Provide a 'file' query param with an absolute path to a .ncu-rep file.",
        400,
    )

  target = file[0]
  path = Path(target)
  if not path.is_absolute():
    return _ncu_error_page(request, f"Report path must be absolute: {target}", 400)

  if not await asyncio.to_thread(path.is_file):
    return _ncu_error_page(request, f"Report not found: {target}", 404)

  # The NCU report parser rides the viewer like croniter rides its next-run
  # resolutions: the M99 server import floor carries no report-parsing stack.
  from src.features.ncu.ncu_parsing import NcuParseError, parse_ncu_report

  try:
    report = await asyncio.to_thread(parse_ncu_report, str(path))
  except NcuParseError as exc:
    return _ncu_error_page(request, str(exc), 422)

  download_url = FILE_SERVER_MOUNTS[0] + str(path)
  return templating.templates().TemplateResponse(
      request,
      "ncu.html",
      context={
          "error": None,
          "report": report,
          "report_path": str(path),
          "filename": path.name,
          "download_url": download_url,
          "extra_count": len(file) - 1,
          "ncu_ui_cmd": f"ncu-ui {path}",
          "ncu_details_cmd": f"ncu --import {path} --page details",
          "ncu_source_cmd": f"ncu --import {path} --page source --print-source sass",
          "ncu_session_cmd": f"ncu --import {path} --page session",
      })
