"""File server router — serves files and directory listings from the filesystem."""

import asyncio
import html
import math
import mimetypes
import os
import pathlib
import re
from urllib import parse

import fastapi
from fastapi import responses

from src.infra import constants, human_size, memo
from src.infra import responses as responses_api
from src.runtime.hooks import wiring

router = fastapi.APIRouter()


class _ServedFileResponse(responses.FileResponse):
  # Starlette 1.0.0 exposes the read chunk size as this class attribute (no
  # __init__ parameter). The 64 KiB default prices a page-cache serve at
  # ~250 MB/s: one executor hop plus one ASGI send per chunk. A 1 MiB chunk
  # cuts both ~16x per MB and is the transport's only knob; the wire bytes are
  # identical, so no served body changes.
  chunk_size = 1 << 20


# Bound on the bare-file arm's gzip memo: the arm serves the file server's
# repeat views of gzip-able files (html pages above all — the M105 html
# witness's 15.4 ms per repeat serve was the middleware's per-request per-chunk
# inline deflate). Four slots cover the pages a user re-opens across tabs; one
# slot holds the compressed form of a file up to the raw-size cap.
_SERVED_FILE_GZIP_MEMO_LIMIT = 4

# Raw-size cap of the same arm. Above it the serve stays on the streaming
# _ServedFileResponse arm: a whole-body read plus its gzip form would hold
# multi-hundred-MB resident per slot for files the middleware already serves
# chunk-wise without buffering.
_SERVED_FILE_GZIP_MAX_BYTES = 16 << 20

# Media gate of the same arm: the text formats the transport compresses and
# repeat-serves. The gzip middleware's skip list (server.py) is this gate's
# complement in spirit — every prefix here stays outside that list — while
# unknown and binary media types (application/octet-stream above all) keep the
# streaming arm: their gzip form is a ratio gamble and their chunked-identity
# contract is pinned by the suite.
_SERVED_FILE_GZIP_MEDIA_PREFIXES = ("text/",)
_SERVED_FILE_GZIP_MEDIA_TYPES = frozenset(
    {
        "application/json",
        "application/javascript",
        "text/javascript",
        "application/xml",
        "image/svg+xml",
    })

_served_file_gzip_memo: memo.StatSignatureMemo[pathlib.Path,
                                               bytes] = memo.StatSignatureMemo(_SERVED_FILE_GZIP_MEMO_LIMIT)

# Client-visible detail of the listing's and the read's 403.
_PERMISSION_DENIED_DETAIL = "Permission denied"


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
  return f"{y}-{m:02d}-{d:02d} {hh:02d}:{rem // 60:02d}"


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

_listing_memo: memo.BoundedMemo[_ListingKey, str] = memo.BoundedMemo(_LISTING_MEMO_LIMIT)

# The gzip form of the same page, keyed and bounded alike. It lives in its own
# memo so a client that sends no Accept-Encoding: gzip never pays the deflate.
_listing_gzip_memo: memo.BoundedMemo[_ListingKey, bytes] = memo.BoundedMemo(_LISTING_MEMO_LIMIT)

# A row's bytes are a pure function of its key: the entry's walked tuple plus
# the URL prefix the href embeds — the same walked-state ground the page memo's
# key stands on. A rebuild after a corpus move re-renders only the entries whose
# tuple moved (a metadata rename into a session dir moves exactly that dir's
# mtime); the cap holds several listings' working sets and evicts the stale
# mtimes such renames leave behind LRU-first.
_ROW_MEMO_LIMIT = 8192
_RowKey = tuple[str, bool, str, int, float]
_row_memo: memo.BoundedMemo[_RowKey, str] = memo.BoundedMemo(_ROW_MEMO_LIMIT)

# A name over [A-Za-z0-9_.~-] is its own html.escape output and its own
# urllib.parse.quote(safe="") output — both functions' always-safe sets — so a
# matching entry renders by interpolation and only the rest pay the per-entry
# escape/quote calls. Session ids (UUIDs) and artifact names match; the
# charliebot corpus is almost entirely safe names.
_SAFE_ENTRY_RE = re.compile(r"[A-Za-z0-9_.~-]+")

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


def _dir_listing_page(dir_path: pathlib.Path, url_prefix: str) -> tuple[str | None, _ListingKey | None]:
  """The listing page and its memo key, or (None, None) when *dir_path* is not a directory.

  The key is the walked state the page is a pure function of; the route's gzip
  arm memoizes the page's compressed form under it (``_listing_page_gzip``).
  Carries the route's dir contract: the unreadable-directory 403.
  One scandir pass answers is_dir from the directory record and stats each
  entry once; a repeat view of unchanged state serves the memo and pays only
  that walk.
  """
  try:
    scandir_iter = os.scandir(os.fspath(dir_path))
  except NotADirectoryError:
    return None, None
  except PermissionError as e:
    raise fastapi.HTTPException(status_code=403, detail=_PERMISSION_DENIED_DETAIL) from e
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
    return hit, key
  entries.sort(key=lambda e: (not e[0], e[1].lower()))

  rows = []
  prefix = url_prefix.rstrip("/")
  # Parent directory link (unless at a mount root)
  if prefix != constants.FILE_SERVER_MOUNTS[0]:
    parent = "/".join(prefix.split("/")[:-1]) or constants.FILE_SERVER_MOUNTS[0]
    rows.append('<tr>'
                f'<td>📁</td><td><a href="{html.escape(parent)}">..</a></td>'
                '<td></td><td></td>'
                '</tr>\n')

  escaped_prefix = html.escape(prefix)
  for is_dir, name, size, mtime in entries:
    row_key = (url_prefix, is_dir, name, size, mtime)
    row = _row_memo.get(row_key)
    if row is None:
      icon = "📁" if is_dir else "📄"
      name_text = name + ("/" if is_dir else "")
      href = f"{escaped_prefix}/{name}"
      if _SAFE_ENTRY_RE.fullmatch(name) is None:
        name_text = html.escape(name_text)
        href = html.escape(f"{prefix}/{parse.quote(name, safe='')}")
      size_text = "" if is_dir else human_size.format_size(size)
      mtime_text = _format_mtime(mtime)
      row = (
          f'<tr>'
          f'<td>{icon}</td><td><a href="{href}">{name_text}</a></td>'
          f'<td style="text-align:right">{size_text}</td><td>{mtime_text}</td>'
          f'</tr>\n')
      _row_memo.store(row_key, row)
    rows.append(row)

  display_path = html.escape("/" + dir_path.as_posix().lstrip("/"))
  listing = _DIR_LISTING_TEMPLATE.format(display_path=display_path, rows=''.join(rows))
  _listing_memo.store(key, listing)
  return listing, key


def _listing_page_gzip(key: _ListingKey, listing: str) -> bytes:
  """The listing page's gzip form, memoized beside the plain page."""
  return responses_api.gzip_form(_listing_gzip_memo, key, lambda: listing.encode("utf-8"))


def _resolve_and_list(path: str, url_prefix: str) -> tuple[pathlib.Path, tuple[str, _ListingKey] | None, bool]:
  """Resolve the request path and attempt its listing in one executor hop.

  Returns ``(resolved_path, page, exists)`` where *page* is the listing page
  and its memo key. The exists half carries the
  old two-hop ``exists()`` answer: a listing or a ``NotADirectoryError`` proves
  the path present (scandir reached it), and only the ambiguous not-a-directory
  case — a missing path whose parent is a file raises the same error as a plain
  file — pays its explicit ``os.path.exists``. ``FileNotFoundError`` from a
  vanished or absent path answers ``exists=False`` without a second stat.
  """
  fs_path = (pathlib.Path("/") / path).resolve()
  try:
    page = _dir_listing_page(fs_path, url_prefix)
  except FileNotFoundError:
    return fs_path, None, False
  if page[0] is not None:
    return fs_path, (page[0], page[1]), True
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
  url_prefix = f"{constants.FILE_SERVER_MOUNTS[0]}/{path}" if path else constants.FILE_SERVER_MOUNTS[0]
  # One executor hop carries the resolve, the exists answer, and the whole
  # listing build; None means a file, falling through to responses.FileResponse.
  fs_path, page, exists = await asyncio.to_thread(_resolve_and_list, path, url_prefix)
  if not exists:
    raise fastapi.HTTPException(status_code=404, detail="Not found")
  if page is not None:
    listing, listing_key = page
    if responses_api.request_wants_gzip(request):
      body = await asyncio.to_thread(_listing_page_gzip, listing_key, listing)
      return responses.Response(content=body, media_type="text/html", headers=responses_api.GZIP_RESPONSE_HEADERS)
    return responses.HTMLResponse(listing)

  # Serve the file with auto-detected MIME type. A gzip-accepting GET of a
  # gated media type under the memo cap rides the memo arm: Content-Encoding
  # set upstream is what makes the middleware skip its per-request per-chunk
  # inline deflate, and the stat signature proves a repeat hit's stored bytes.
  # Every other shape — no-gzip clients, Range requests, unlisted media types,
  # over-cap files — stays on the streaming arm unchanged.
  media_type, _ = mimetypes.guess_type(str(fs_path))
  if (responses_api.request_wants_gzip(request) and "range" not in request.headers and media_type is not None and
      (media_type.startswith(_SERVED_FILE_GZIP_MEDIA_PREFIXES) or media_type in _SERVED_FILE_GZIP_MEDIA_TYPES)):
    compressed = await asyncio.to_thread(
        responses_api.gzip_file_fresh, _served_file_gzip_memo, fs_path, _SERVED_FILE_GZIP_MAX_BYTES)
    if compressed is not None:
      return responses.Response(content=compressed, media_type=media_type, headers=responses_api.GZIP_RESPONSE_HEADERS)
  try:
    return _ServedFileResponse(str(fs_path), media_type=media_type)
  except PermissionError as e:
    raise fastapi.HTTPException(status_code=403, detail=_PERMISSION_DENIED_DETAIL) from e


# The file server answers under the one canonical prefix FILE_SERVER_MOUNTS holds: "/absolute_filepath",
# the form written into chat text. The prefix names what has to follow it, so a link missing its absolute
# prefix reads as wrong where it is written. The "/files" and "/file" spellings are unmounted: nothing
# answers there, both 404. The routes list is complete here, after every route of `router` is declared.
mounted_router = fastapi.APIRouter()
for _mount in constants.FILE_SERVER_MOUNTS:
  mounted_router.include_router(router, prefix=_mount)
