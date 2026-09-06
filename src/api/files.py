"""File server router — serves files and directory listings from the filesystem."""

import asyncio
import html
import json
import mimetypes
from datetime import UTC, datetime
from pathlib import Path
from urllib.parse import quote

from fastapi import APIRouter, HTTPException, Request
from fastapi.responses import FileResponse, HTMLResponse

from src.api.auth import request_has_access_key
from src.api.pages import _static_asset_version
from src.core import plan_diff
from src.core.config import get_config
from src.core.memo import BoundedMemo

router = APIRouter()

# Bound on _annotate_memo in annotated diff pages: one compare view reads one
# page against one base at a time, so the cap covers every compare view open
# across tabs, and one slot holds the ~1.5 MB worst annotated page.
_DIFF_ANNOTATE_MEMO_LIMIT = 8

# Memo key for one annotated diff page: both resolved paths plus each file's
# (mtime_ns, size) taken before its read, and whether the artifact-comments
# injection rode along. The marks are a pure function of the two files' bytes
# and an artifact page is only ever written whole, so an unchanged signature
# pair proves the stored page current; an entry keyed from bytes read before a
# concurrent rewrite is unreachable for the newer bytes. Served strings are
# shared across responses, the no-defensive-copy idiom of the sibling memos.
_AnnotateKey = tuple[str, int, int, str, int, int, bool]

_annotate_memo: BoundedMemo[_AnnotateKey, str] = BoundedMemo(_DIFF_ANNOTATE_MEMO_LIMIT)


def _file_signature(path: Path) -> tuple[int, int]:
  """(mtime_ns, size) of *path*; artifact writers publish whole files, so a rewrite always moves it."""
  st = path.stat()
  return (st.st_mtime_ns, st.st_size)


def _annotated_diff_page(base_path: Path, page_path: Path, inject_ui: bool, session_id: str) -> str:
  """The diff page's target annotated against its base, repeats served from the memo.

  A cold annotate parses both pages end to end (~0.25 s on a 1 MB pair,
  measured) — work per request no repeat view must re-run, since neither bytes
  nor marks can change between views.
  """
  try:
    base_sig = (str(base_path), *_file_signature(base_path))
  except OSError as e:
    raise HTTPException(status_code=404, detail=f"diff base not found: {base_path}") from e
  page_sig = (str(page_path), *_file_signature(page_path))
  key: _AnnotateKey = (*base_sig, *page_sig, inject_ui)
  hit = _annotate_memo.get(key)
  if hit is not None:
    return hit
  try:
    base_text = base_path.read_text(encoding="utf-8")
  except OSError as e:
    raise HTTPException(status_code=404, detail=f"diff base not found: {base_path}") from e
  page_text = page_path.read_text(encoding="utf-8")
  page = plan_diff.annotate(base_text, page_text)
  if inject_ui:
    page = _inject_artifact_ui(page, session_id)
  _annotate_memo.store(key, page)
  return page


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
  """Insert the session-id assignment and the comment scripts before the last
  </body>, or append without one. The inline assignment precedes the external
  script tags so the id is set before the comment scripts run."""
  tags = (
      f"<script>window.__cbcServerSessionId={json.dumps(session_id)};</script>\n"
      f"<script src=/static/js/comment_post.js?v={_static_asset_version()}></script>\n"
      f"<script src=/static/js/artifact-comments.js?v={_static_asset_version()}></script>")
  idx = html_text.rfind("</body>")
  if idx == -1:
    return html_text + "\n" + tags + "\n"
  return html_text[:idx] + tags + "\n" + html_text[idx:]


def _resolve_diff_base(session_id: str, diff_param: str) -> Path:
  """Resolve the ``?diff=`` query parameter of a diff request to the base page's path.

  The parameter is a session-relative artifact path — the plan registry's ``versions[].file``
  form, e.g. ``artifacts/plan_01_v1.html``. It must resolve to a ``.html`` page whose
  immediate parent is a session's ``artifacts`` directory (the same predicate the target
  passes); anything else is a malformed request → 400. A base that is missing or unreadable
  is 404 naming it — a reader who sees no marks has to be able to trust there are none, so
  a broken diff never falls back to the clean page.
  """
  candidate = (get_config().sessions_dir / session_id / diff_param).resolve()
  if candidate.suffix.lower() != ".html" or _artifact_session_id(candidate) is None:
    raise HTTPException(status_code=400, detail=f"diff base is not a session artifact page: {diff_param}")
  return candidate


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
              "mtime": datetime.fromtimestamp(stat.st_mtime, tz=UTC),
          })
  except PermissionError as e:
    raise HTTPException(status_code=403, detail="Permission denied") from e

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
  return f"""<!DOCTYPE html>
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


@router.api_route("/{path:path}", methods=["GET", "HEAD"])
async def serve_file(path: str, request: Request):
  """Serve a file or directory listing from the filesystem.

  HEAD answers the same status as GET, which is how the chat asks whether a linked path is
  still there without pulling the file down.
  """
  fs_path = await asyncio.to_thread((Path("/") / path).resolve)

  if not await asyncio.to_thread(fs_path.exists):
    raise HTTPException(status_code=404, detail="Not found")

  diff_param = request.query_params.get("diff")
  if await asyncio.to_thread(fs_path.is_dir):
    if diff_param is not None:
      raise HTTPException(status_code=400, detail=f"diff target is not a session artifact page: {fs_path}")
    url_prefix = f"/files/{path}" if path else "/files"
    listing = await asyncio.to_thread(_dir_listing_html, fs_path, url_prefix)
    return HTMLResponse(listing)

  # Standalone artifact HTML gets the review UI injected here — the single chokepoint
  # that serves every artifact page — regardless of how the artifact was authored, but
  # only for readers who carry a valid access key: only they can post a comment, so only
  # they see the comment entry. An uncredentialed reader gets the file's original bytes.
  session_id = _artifact_session_id(fs_path) if fs_path.suffix.lower() == ".html" else None
  if diff_param is not None:
    # A diff request addresses two artifact pages. Both must pass the artifact
    # predicate before anything is served, so a malformed address is rejected
    # rather than silently answered with the clean page. The marks themselves
    # are spliced into the response before the optional comment layer below.
    if session_id is None:
      raise HTTPException(status_code=400, detail=f"diff target is not a session artifact page: {fs_path}")
    base_path = _resolve_diff_base(session_id, diff_param)
    inject_ui = request_has_access_key(request, get_config().charliebot_access_key)
    # A cold annotate parses both pages whole (~0.25 s on a 1 MB pair), so the
    # build runs off the event loop; a memo hit answers with zero file bytes.
    html_text = await asyncio.to_thread(_annotated_diff_page, base_path, fs_path, inject_ui, session_id)
    return HTMLResponse(html_text, media_type="text/html")

  if session_id is not None and request_has_access_key(request, get_config().charliebot_access_key):
    html_text = await asyncio.to_thread(lambda: fs_path.read_text(encoding="utf-8"))
    html_text = await asyncio.to_thread(_inject_artifact_ui, html_text, session_id)
    return HTMLResponse(html_text, media_type="text/html")

  # Serve the file with auto-detected MIME type
  media_type, _ = mimetypes.guess_type(str(fs_path))
  try:
    return FileResponse(str(fs_path), media_type=media_type)
  except PermissionError as e:
    raise HTTPException(status_code=403, detail="Permission denied") from e
