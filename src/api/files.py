"""File server router — serves files and directory listings from the filesystem."""

import asyncio
import html
import json
import math
import mimetypes
import os
import re
from pathlib import Path
from urllib.parse import quote

from fastapi import APIRouter, HTTPException, Request
from fastapi.responses import FileResponse, HTMLResponse, Response

from src.api.auth import request_has_access_key
from src.api.pages import _static_asset_version
from src.core import plan_diff
from src.core.config import get_config, get_credentials
from src.core.memo import BoundedMemo

router = APIRouter()

# Bound on _annotate_memo in annotated diff pages: one compare view reads one
# page against one base at a time, so the cap covers every compare view open
# across tabs, and one slot holds the ~1.5 MB worst annotated page.
_DIFF_ANNOTATE_MEMO_LIMIT = 8

# Bound on _clean_view_memo: one open artifact tab serves one page, so the cap
# covers every clean view open across tabs, and one slot holds the ~1.5 MB
# worst injected page.
_CLEAN_VIEW_MEMO_LIMIT = 8

# Memo key for one annotated diff page: both resolved paths plus each file's
# (mtime_ns, size) taken before its read, and whether the artifact-comments
# injection rode along. The marks are a pure function of the two files' bytes
# and an artifact page is only ever written whole, so an unchanged signature
# pair proves the stored page current; an entry keyed from bytes read before a
# concurrent rewrite is unreachable for the newer bytes. Served strings are
# shared across responses, the no-defensive-copy idiom of the sibling memos.
_AnnotateKey = tuple[str, int, int, str, int, int, bool]

_annotate_memo: BoundedMemo[_AnnotateKey, str] = BoundedMemo(_DIFF_ANNOTATE_MEMO_LIMIT)

# Memo key for one clean artifact view: the resolved path plus the page's
# (mtime_ns, size) taken before its read. The injection is a pure function of
# the page bytes — the session id derives from the path and the static asset
# version is a per-process constant — and an artifact page is only ever written
# whole, so an unchanged signature proves the stored body current; an entry
# keyed from bytes read before a concurrent rewrite is unreachable for the
# newer bytes. Served bodies are shared across responses, the no-defensive-copy
# idiom of the sibling memos.
_CleanViewKey = tuple[str, int, int]

_clean_view_memo: BoundedMemo[_CleanViewKey, bytes] = BoundedMemo(_CLEAN_VIEW_MEMO_LIMIT)

# Client-visible error details of the files route's diff and read arms. The
# "not a session artifact page" sentence is a wire contract the tests pin, so
# each spelling has one home here; the base-side sibling in _resolve_diff_base
# and deps.SESSION_NOT_FOUND_DETAIL are distinct deliberate wordings.
_DIFF_TARGET_DETAIL = "diff target is not a session artifact page: {}"
_DIFF_BASE_NOT_FOUND_DETAIL = "diff base not found: {}"
_PERMISSION_DENIED_DETAIL = "Permission denied"


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
    raise HTTPException(status_code=404, detail=_DIFF_BASE_NOT_FOUND_DETAIL.format(base_path)) from e
  page_sig = (str(page_path), *_file_signature(page_path))
  key: _AnnotateKey = (*base_sig, *page_sig, inject_ui)
  hit = _annotate_memo.get(key)
  if hit is not None:
    return hit
  try:
    base_text = base_path.read_text(encoding="utf-8")
  except OSError as e:
    raise HTTPException(status_code=404, detail=_DIFF_BASE_NOT_FOUND_DETAIL.format(base_path)) from e
  page_text = page_path.read_text(encoding="utf-8")
  page = plan_diff.annotate(base_text, page_text)
  if inject_ui:
    page = _inject_artifact_ui(page, session_id)
  _annotate_memo.store(key, page)
  return page


def _injected_artifact_page(fs_path: Path, session_id: str) -> bytes:
  """The credentialed artifact view's body: the page wrapped in the artifact UI, memoized on the file signature.

  A repeat view of an unchanged page pays one stat and zero file bytes — the
  read re-ran on every view before the memo (~4.8 ms on the 1 MB worst
  artifact, measured, of an ~11.5 ms repeat view). The signature is taken
  before the read, the same ground as _annotate_memo.
  """
  key: _CleanViewKey = (str(fs_path), *_file_signature(fs_path))
  hit = _clean_view_memo.get(key)
  if hit is not None:
    return hit
  page = _inject_artifact_ui(fs_path.read_text(encoding="utf-8"), session_id)
  body = page.encode("utf-8")
  _clean_view_memo.store(key, body)
  return body


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


def _format_mtime(epoch: float) -> str:
  """The listing's UTC minute text, "YYYY-MM-DD HH:MM", from an epoch-seconds float.

  The seconds floor toward minus infinity — the rounding ``time.gmtime`` applies
  to a fractional epoch — and the calendar date comes from integer civil-from-days
  arithmetic, so no per-entry ``gmtime``/``strftime`` pair is needed. The suite
  pins the output byte-identical to ``strftime("%Y-%m-%d %H:%M", gmtime(epoch))``
  over boundary and randomized epochs; the year renders unpadded wherever
  ``gmtime``'s struct year and the reference agree, which the fuzz sweeps from
  negative years through the five-digit zone.
  """
  days, secs_of_day = divmod(math.floor(epoch), 86400)
  hh, rem = divmod(secs_of_day, 3600)
  # days since 1970-01-01 -> (y, m, d), Howard Hinnant's civil_from_days; the
  # era takes plain floor division — Python's // already floors, and carrying
  # the C form's negative-z adjustment here shifts dates before 0000-03-01.
  z = days + 719468
  era = z // 146097
  doe = z - era * 146097
  yoe = (doe - doe // 1460 + doe // 36524 - doe // 146096) // 365
  y = yoe + era * 400
  doy = doe - (365 * yoe + yoe // 4 - yoe // 100)
  mp = (5 * doy + 2) // 153
  d = doy - (153 * mp + 2) // 5 + 1
  m = mp + 3 if mp < 10 else mp - 9
  y += m <= 2
  # The year renders unpadded: the C reference's %Y carries no width, so year
  # 999 is "999", not "0999".
  return "%s-%02d-%02d %02d:%02d" % (y, m, d, hh, rem // 60)


# Bound on _listing_memo: one browser tab lists one directory at a time, so the
# cap covers every listing open across tabs, and one slot holds the ~240 KB
# worst listing on this host's corpus.
_LISTING_MEMO_LIMIT = 8

# Memo key for one directory listing: the resolved directory, the URL prefix the
# links embed, and the walk's own entry snapshot. The served HTML is a pure
# function of that walked state, so equal walked state proves the stored page
# equals what this walk would build — no invalidation rule is needed, and the
# key leans on no rename-atomicity assumption the sibling (mtime_ns, size) memos
# require. The walk itself re-runs on every request (the stat per entry is the
# only way to read mtimes); the memo removes the sort and the per-entry row
# build from a repeat view.
_ListingKey = tuple[str, str, tuple[tuple[bool, str, int, float], ...]]

_listing_memo: BoundedMemo[_ListingKey, str] = BoundedMemo(_LISTING_MEMO_LIMIT)

# A name over [A-Za-z0-9_.~-] is its own html.escape output and its own
# urllib.parse.quote(safe="") output — both functions' always-safe sets — so a
# matching entry renders by interpolation and only the rest pay the per-entry
# escape/quote calls. Session ids (UUIDs) and artifact names match; the
# charliebot corpus is almost entirely safe names.
_SAFE_ENTRY_RE = re.compile(r"[A-Za-z0-9_.~-]+")

# One home for the listing page's chrome (the rows are built per walk): the
# dir-listing byte-pin test in tests/test_files_dir_listing.py formats this
# same template for its reference builder, so a chrome edit cannot desync the
# two builders.
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


def _dir_listing_html(dir_path: Path, url_prefix: str, diff_param: str | None) -> str | None:
  """Return the HTML listing of *dir_path*, or None when it is not a directory.

  Carries the route's dir contract: the ``?diff=`` 400 (a diff target must be a
  session artifact page, never a directory) and the unreadable-directory 403.
  One scandir pass answers is_dir from the directory record and stats each
  entry once; a repeat view of unchanged state serves the memo and pays only
  that walk.
  """
  try:
    scandir_iter = os.scandir(os.fspath(dir_path))
  except NotADirectoryError:
    return None
  except PermissionError as e:
    # The diff 400 outranks the unreadable 403: the route contract checks the
    # diff target before it tries to read the directory.
    if diff_param is not None:
      raise HTTPException(status_code=400, detail=_DIFF_TARGET_DETAIL.format(dir_path)) from e
    raise HTTPException(status_code=403, detail=_PERMISSION_DENIED_DETAIL) from e
  if diff_param is not None:
    scandir_iter.close()
    raise HTTPException(status_code=400, detail=_DIFF_TARGET_DETAIL.format(dir_path))
  entries: list[tuple[bool, str, int, float]] = []
  with scandir_iter:
    for entry in scandir_iter:
      try:
        stat = entry.stat()
        is_dir = entry.is_dir()
      except OSError:
        continue
      entries.append((is_dir, entry.name, stat.st_size, stat.st_mtime))
  key: _ListingKey = (os.fspath(dir_path), url_prefix, tuple(entries))
  hit = _listing_memo.get(key)
  if hit is not None:
    return hit
  entries.sort(key=lambda e: (not e[0], e[1].lower()))

  rows = []
  prefix = url_prefix.rstrip("/")
  # Parent directory link (unless at root)
  if prefix != "/files":
    parent = "/".join(prefix.split("/")[:-1]) or "/files"
    rows.append('<tr>'
                f'<td>📁</td><td><a href="{html.escape(parent)}">..</a></td>'
                '<td></td><td></td>'
                '</tr>\n')

  escaped_prefix = html.escape(prefix)
  for is_dir, name, size, mtime in entries:
    icon = "📁" if is_dir else "📄"
    name_text = name + ("/" if is_dir else "")
    href = f"{escaped_prefix}/{name}"
    if _SAFE_ENTRY_RE.fullmatch(name) is None:
      name_text = html.escape(name_text)
      href = html.escape(f"{prefix}/{quote(name, safe='')}")
    size_text = "" if is_dir else _human_size(size)
    mtime_text = _format_mtime(mtime)
    rows.append(
        f'<tr>'
        f'<td>{icon}</td><td><a href="{href}">{name_text}</a></td>'
        f'<td style="text-align:right">{size_text}</td><td>{mtime_text}</td>'
        f'</tr>\n')

  display_path = html.escape("/" + dir_path.as_posix().lstrip("/"))
  listing = _DIR_LISTING_TEMPLATE.format(display_path=display_path, rows=''.join(rows))
  _listing_memo.store(key, listing)
  return listing


def _resolve_and_list(path: str, url_prefix: str, diff_param: str | None) -> tuple[Path, str | None, bool]:
  """Resolve the request path and attempt its listing in one executor hop.

  Returns ``(resolved_path, listing_html, exists)``. The exists half carries the
  old two-hop ``exists()`` answer: a listing or a ``NotADirectoryError`` proves
  the path present (scandir reached it), and only the ambiguous not-a-directory
  case — a missing path whose parent is a file raises the same error as a plain
  file — pays its explicit ``os.path.exists``. ``FileNotFoundError`` from a
  vanished or absent path answers ``exists=False`` without a second stat.
  """
  fs_path = (Path("/") / path).resolve()
  try:
    listing = _dir_listing_html(fs_path, url_prefix, diff_param)
  except FileNotFoundError:
    return fs_path, None, False
  if listing is not None:
    return fs_path, listing, True
  return fs_path, None, os.path.exists(fs_path)


@router.api_route("/{path:path}", methods=["GET", "HEAD"])
async def serve_file(path: str, request: Request) -> Response:
  """Serve a file or directory listing from the filesystem.

  HEAD answers the same status as GET, which is how the chat asks whether a linked path is
  still there without pulling the file down.
  """
  diff_param = request.query_params.get("diff")
  url_prefix = f"/files/{path}" if path else "/files"
  # One executor hop carries the resolve, the exists answer, and the whole
  # listing build; None means a file, falling through to the artifact and
  # FileResponse arms.
  fs_path, listing, exists = await asyncio.to_thread(_resolve_and_list, path, url_prefix, diff_param)
  if not exists:
    raise HTTPException(status_code=404, detail="Not found")
  if listing is not None:
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
      raise HTTPException(status_code=400, detail=_DIFF_TARGET_DETAIL.format(fs_path))
    base_path = _resolve_diff_base(session_id, diff_param)
    inject_ui = request_has_access_key(request, str(get_credentials().get("charliebot", "access_key") or ""))
    # A cold annotate parses both pages whole (~0.25 s on a 1 MB pair), so the
    # build runs off the event loop; a memo hit answers with zero file bytes.
    html_text = await asyncio.to_thread(_annotated_diff_page, base_path, fs_path, inject_ui, session_id)
    return HTMLResponse(html_text, media_type="text/html")

  if session_id is not None and request_has_access_key(request, str(get_credentials().get("charliebot", "access_key") or
                                                                    "")):
    # One executor hop: signature, memo hit, and on a miss the read+inject+store.
    body = await asyncio.to_thread(_injected_artifact_page, fs_path, session_id)
    return HTMLResponse(body, media_type="text/html")

  # Serve the file with auto-detected MIME type
  media_type, _ = mimetypes.guess_type(str(fs_path))
  try:
    return FileResponse(str(fs_path), media_type=media_type)
  except PermissionError as e:
    raise HTTPException(status_code=403, detail=_PERMISSION_DENIED_DETAIL) from e
