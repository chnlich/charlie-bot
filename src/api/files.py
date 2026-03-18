"""File server router — serves files and directory listings from the filesystem."""

import html
import mimetypes
from datetime import datetime, timezone
from pathlib import Path
from urllib.parse import quote

import structlog
from fastapi import APIRouter, HTTPException
from fastapi.responses import FileResponse, HTMLResponse

log = structlog.get_logger()
router = APIRouter()


def _human_size(size: int) -> str:
  for unit in ("B", "KB", "MB", "GB", "TB"):
    if size < 1024:
      return f"{size:.1f} {unit}" if unit != "B" else f"{size} {unit}"
    size /= 1024
  return f"{size:.1f} PB"


def _dir_listing_html(dir_path: Path, url_prefix: str) -> str:
  """Return a minimal HTML page listing directory contents."""
  entries: list[dict] = []
  try:
    for child in sorted(dir_path.iterdir(), key=lambda p: (not p.is_dir(), p.name.lower())):
      try:
        stat = child.stat()
      except OSError:
        continue
      entries.append({
        "name": child.name,
        "is_dir": child.is_dir(),
        "size": stat.st_size,
        "mtime": datetime.fromtimestamp(stat.st_mtime, tz=timezone.utc),
      })
  except PermissionError:
    raise HTTPException(status_code=403, detail="Permission denied")

  rows = ""
  # Parent directory link (unless at root)
  if url_prefix.rstrip("/") != "/files":
    parent = "/".join(url_prefix.rstrip("/").split("/")[:-1]) or "/files"
    rows += (
      '<tr>'
      f'<td>📁</td><td><a href="{html.escape(parent)}">..</a></td>'
      '<td></td><td></td>'
      '</tr>\n'
    )

  for e in entries:
    icon = "📁" if e["is_dir"] else "📄"
    name = html.escape(e["name"] + ("/" if e["is_dir"] else ""))
    href = html.escape(f"{url_prefix.rstrip('/')}/{quote(e['name'], safe='')}")
    size = "" if e["is_dir"] else _human_size(e["size"])
    mtime = e["mtime"].strftime("%Y-%m-%d %H:%M")
    rows += (
      f'<tr>'
      f'<td>{icon}</td><td><a href="{href}">{name}</a></td>'
      f'<td style="text-align:right">{size}</td><td>{mtime}</td>'
      f'</tr>\n'
    )

  display_path = html.escape("/" + dir_path.as_posix().lstrip("/"))
  page = f"""<!DOCTYPE html>
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
  return page


@router.get("/{path:path}")
async def serve_file(path: str):
  """Serve a file or directory listing from the filesystem."""
  fs_path = (Path("/") / path).resolve()

  if not fs_path.exists():
    raise HTTPException(status_code=404, detail="Not found")

  if fs_path.is_dir():
    url_prefix = f"/files/{path}" if path else "/files"
    listing = _dir_listing_html(fs_path, url_prefix)
    return HTMLResponse(listing)

  # Serve the file with auto-detected MIME type
  media_type, _ = mimetypes.guess_type(str(fs_path))
  try:
    return FileResponse(str(fs_path), media_type=media_type)
  except PermissionError:
    raise HTTPException(status_code=403, detail="Permission denied")
