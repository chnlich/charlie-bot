"""File server router — serves files and directory listings from the filesystem."""

import asyncio
import html
import json
import mimetypes
from datetime import datetime, timezone
from pathlib import Path
from urllib.parse import quote

import structlog
from fastapi import APIRouter, HTTPException, Request
from fastapi.responses import FileResponse, HTMLResponse

from src.api.auth import request_has_access_key
from src.core.config import get_config

log = structlog.get_logger()
router = APIRouter()

_ARTIFACT_SCRIPT_TAG = "<script src=/static/js/artifact-comments.js></script>"


def _artifact_session_id(fs_path: Path) -> str | None:
  """Return the session id owning an artifact page, or None when it belongs to no session.

  Anchored on the configured sessions root, not on the path's shape: a page counts only
  when it sits under ``<sessions_dir>/<session>/...`` with ``artifacts`` as its immediate
  parent directory, so ``<root>/artifacts/x.html`` (no session component) and any
  artifact-shaped path outside the root are excluded. Both sides are resolved — fs_path
  by ``serve_file``, the root here — so a symlink on either side cannot misjudge.
  """
  root = get_config().sessions_dir.resolve()
  try:
    rel = fs_path.relative_to(root)
  except ValueError:
    return None
  if fs_path.parent.name != "artifacts":
    return None
  if len(rel.parts) < 3:
    return None
  return rel.parts[0]


def _inject_artifact_ui(html_text: str, session_id: str) -> str:
  """Insert the session-id assignment and the artifact-comments script before the last
  </body>, or append without one. The inline assignment precedes the external script tag
  so the id is set before artifact-comments.js runs."""
  tags = (f"<script>window.__cbcServerSessionId={json.dumps(session_id)};</script>\n"
          f"{_ARTIFACT_SCRIPT_TAG}")
  idx = html_text.rfind("</body>")
  if idx == -1:
    return html_text + "\n" + tags + "\n"
  return html_text[:idx] + tags + "\n" + html_text[idx:]


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
      entries.append(
          {
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
    rows += ('<tr>'
             f'<td>📁</td><td><a href="{html.escape(parent)}">..</a></td>'
             '<td></td><td></td>'
             '</tr>\n')

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
        f'</tr>\n')

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


@router.api_route("/{path:path}", methods=["GET", "HEAD"])
async def serve_file(path: str, request: Request):
  """Serve a file or directory listing from the filesystem.

  HEAD answers the same status as GET, which is how the chat asks whether a linked path is
  still there without pulling the file down.
  """
  fs_path = await asyncio.to_thread(lambda: (Path("/") / path).resolve())

  if not await asyncio.to_thread(fs_path.exists):
    raise HTTPException(status_code=404, detail="Not found")

  if await asyncio.to_thread(fs_path.is_dir):
    url_prefix = f"/files/{path}" if path else "/files"
    listing = await asyncio.to_thread(_dir_listing_html, fs_path, url_prefix)
    return HTMLResponse(listing)

  # Standalone artifact HTML gets the review UI injected here — the single chokepoint
  # that serves every artifact page — regardless of how the artifact was authored, but
  # only for readers who carry a valid access key: only they can post a comment, so only
  # they see the comment entry. An uncredentialed reader gets the file's original bytes.
  session_id = _artifact_session_id(fs_path) if fs_path.suffix.lower() == ".html" else None
  if session_id is not None and request_has_access_key(request, get_config().charliebot_access_key):
    html_text = await asyncio.to_thread(lambda: fs_path.read_text(encoding="utf-8"))
    return HTMLResponse(_inject_artifact_ui(html_text, session_id), media_type="text/html")

  # Serve the file with auto-detected MIME type
  media_type, _ = mimetypes.guess_type(str(fs_path))
  try:
    return FileResponse(str(fs_path), media_type=media_type)
  except PermissionError:
    raise HTTPException(status_code=403, detail="Permission denied")
