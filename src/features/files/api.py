"""File server router — serves files and directory listings from the filesystem."""

import asyncio
import datetime
import html
import mimetypes
import os
import pathlib
from urllib import parse

import fastapi
from fastapi import responses

from src.infra import human_size
from src.runtime.file_urls import FILE_SERVER_MOUNTS
from src.runtime.hooks import wiring

router = fastapi.APIRouter()

# Client-visible detail of the listing's and the read's 403.
_PERMISSION_DENIED_DETAIL = "Permission denied"

# One home for the listing page's chrome (the rows are built per walk).
_DIR_LISTING_TEMPLATE = """<!DOCTYPE html>
<html>
<head><meta charset="utf-8"><title>Index of {display_path}</title>
<style>
  body {{ font-family: monospace; margin: 2em; }}
  table {{ border-collapse: collapse; }}
  td, th {{ padding: 4px 12px; text-align: left; }}
  a {{ text-decoration: none; color: #0366d6; }}
  a:hover {{ text-decoration: underline; }}
</style>
</head>
<body>
<h2>Index of {display_path}</h2>
<table>
<tr><th></th><th>Name</th><th>Size</th><th>Modified</th></tr>
{rows}
</table>
</body>
</html>"""


def _dir_listing_page(dir_path: pathlib.Path, url_prefix: str) -> str | None:
  """The listing page, or None when *dir_path* is not a directory.

  Carries the route's dir contract: the unreadable-directory 403.
  The walk is Path.iterdir with an is_dir in the sort key and a stat and an
  is_dir per child; each row is escaped, quoted and appended in turn.
  """
  entries: list[dict] = []
  try:
    for child in sorted(dir_path.iterdir(), key=lambda p: (not p.is_dir(), p.name.lower())):
      try:
        stat = child.stat()
      except OSError:
        continue
      entries.append(
          {
              "name": child.name,
              "is_dir": child.is_dir(),
              "size": stat.st_size,
              "mtime": datetime.datetime.fromtimestamp(stat.st_mtime, tz=datetime.UTC),
          })
  except NotADirectoryError:
    return None
  except PermissionError as e:
    raise fastapi.HTTPException(status_code=403, detail=_PERMISSION_DENIED_DETAIL) from e

  rows = ""
  # Parent directory link (unless at a mount root)
  if url_prefix.rstrip("/") != FILE_SERVER_MOUNTS[0]:
    parent = "/".join(url_prefix.rstrip("/").split("/")[:-1]) or FILE_SERVER_MOUNTS[0]
    rows += ('<tr>'
             f'<td>📁</td><td><a href="{html.escape(parent)}">..</a></td>'
             '<td></td><td></td>'
             '</tr>\n')

  for e in entries:
    icon = "📁" if e["is_dir"] else "📄"
    name = html.escape(e["name"] + ("/" if e["is_dir"] else ""))
    href = html.escape(f"{url_prefix.rstrip('/')}/{parse.quote(e['name'], safe='')}")
    size = "" if e["is_dir"] else human_size.format_size(e["size"])
    mtime = e["mtime"].strftime("%Y-%m-%d %H:%M")
    rows += (
        f'<tr>'
        f'<td>{icon}</td><td><a href="{href}">{name}</a></td>'
        f'<td style="text-align:right">{size}</td><td>{mtime}</td>'
        f'</tr>\n')

  display_path = html.escape("/" + dir_path.as_posix().lstrip("/"))
  return _DIR_LISTING_TEMPLATE.format(display_path=display_path, rows=rows)


def _resolve_and_list(path: str, url_prefix: str) -> tuple[pathlib.Path, str | None, bool]:
  """Resolve the request path and attempt its listing in one executor hop.

  Returns ``(resolved_path, listing_html, exists)``. The exists half carries the
  old two-hop ``exists()`` answer: a listing or a ``NotADirectoryError`` proves
  the path present (scandir reached it), and only the ambiguous not-a-directory
  case — a missing path whose parent is a file raises the same error as a plain
  file — pays its explicit ``os.path.exists``. ``FileNotFoundError`` from a
  vanished or absent path answers ``exists=False`` without a second stat.
  """
  fs_path = (pathlib.Path("/") / path).resolve()
  try:
    listing = _dir_listing_page(fs_path, url_prefix)
  except FileNotFoundError:
    return fs_path, None, False
  if listing is not None:
    return fs_path, listing, True
  return fs_path, None, os.path.exists(fs_path)


@router.api_route("/{path:path}", methods=["GET", "HEAD"])
async def serve_file(path: str, request: fastapi.Request) -> responses.Response:
  """Serve a file or directory listing from the filesystem, unless a registered file view answers the path first.

  HEAD answers the same status as GET, which is how the chat asks whether a linked path is
  still there without pulling the file down.
  """
  for view in wiring.file_views():
    response = await view(request, path)
    if response is not None:
      return response
  url_prefix = f"{FILE_SERVER_MOUNTS[0]}/{path}" if path else FILE_SERVER_MOUNTS[0]
  # One executor hop carries the resolve, the exists answer, and the whole
  # listing build; None means a file, falling through to responses.FileResponse.
  fs_path, listing, exists = await asyncio.to_thread(_resolve_and_list, path, url_prefix)
  if not exists:
    raise fastapi.HTTPException(status_code=404, detail="Not found")
  if listing is not None:
    return responses.HTMLResponse(listing)

  # Serve the file with auto-detected MIME type
  media_type, _ = mimetypes.guess_type(str(fs_path))
  try:
    return responses.FileResponse(str(fs_path), media_type=media_type)
  except PermissionError as e:
    raise fastapi.HTTPException(status_code=403, detail=_PERMISSION_DENIED_DETAIL) from e


# The file server answers under the one canonical prefix FILE_SERVER_MOUNTS holds: "/absolute_filepath",
# the form written into chat text. The prefix names what has to follow it, so a link missing its absolute
# prefix reads as wrong where it is written. The "/files" and "/file" spellings are unmounted: nothing
# answers there, both 404. The routes list is complete here, after every route of `router` is declared.
mounted_router = fastapi.APIRouter()
for _mount in FILE_SERVER_MOUNTS:
  mounted_router.include_router(router, prefix=_mount)
