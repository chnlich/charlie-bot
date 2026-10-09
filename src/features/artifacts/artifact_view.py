"""The artifact view of the file server: the comment tray on session artifact pages and the ?diff= compare page.

The artifacts package registers serve_artifact_path as a file view, so the file server reaches
it only through the wiring registry. Without this package the file server serves an artifact
page as the plain HTML file it is.
"""

import asyncio
import json
import pathlib
import stat

import fastapi
from fastapi import responses

from src.features.artifacts import plan_diff
from src.infra import config
from src.runtime import templating

# Client-visible error details of the artifact view's diff arm. The
# "not a session artifact page" sentence is a wire contract the tests pin, so
# each spelling has one home here; the base-side sibling in _resolve_diff_base
# and deps.SESSION_NOT_FOUND_DETAIL are distinct deliberate wordings.
_DIFF_TARGET_DETAIL = "diff target is not a session artifact page: {}"
_DIFF_BASE_NOT_FOUND_DETAIL = "diff base not found: {}"

# Session artifact pages carry injected per-session state (the comment tray and
# its inline session id), so no stored copy may outlive the request that read
# it: the artifact responses set Cache-Control: no-store.
_NO_STORE_HEADERS = {"Cache-Control": "no-store"}


def _artifact_session_id(fs_path: pathlib.Path) -> str | None:
  """Return the session id owning an artifact page, or None when it belongs to no session.

  Anchored on the configured sessions root, not on the path's shape: a page counts only
  when it sits under ``<sessions_dir>/<session>/...`` with ``artifacts`` as its immediate
  parent directory, so ``<root>/artifacts/x.html`` (no session component) and any
  artifact-shaped path outside the root are excluded. Both sides are resolved — fs_path
  by ``serve_file``, the root here — so a symlink on either side cannot misjudge.
  """
  root = config.get_config().sessions_dir.resolve()
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
      f"<script src=/static/js/comment_post.js?v={templating.static_asset_version()}></script>\n"
      f"<script src=/static/js/artifact-comments.js?v={templating.static_asset_version()}></script>")
  idx = html_text.rfind("</body>")
  if idx == -1:
    return html_text + "\n" + tags + "\n"
  return html_text[:idx] + tags + "\n" + html_text[idx:]


def _resolve_diff_base(session_id: str, diff_param: str) -> pathlib.Path:
  """Resolve the ``?diff=`` query parameter of a diff request to the base page's path.

  The parameter is a session-relative artifact path — the plan registry's ``versions[].file``
  form, e.g. ``artifacts/plan_01_v1.html``. It must resolve to a ``.html`` page whose
  immediate parent is a session's ``artifacts`` directory (the same predicate the target
  passes); anything else is a malformed request → 400. A base that is missing or unreadable
  is 404 naming it — a reader who sees no marks has to be able to trust there are none, so
  a broken diff never falls back to the clean page.
  """
  candidate = (config.get_config().sessions_dir / session_id / diff_param).resolve()
  if candidate.suffix.lower() != ".html" or _artifact_session_id(candidate) is None:
    raise fastapi.HTTPException(status_code=400, detail=f"diff base is not a session artifact page: {diff_param}")
  return candidate


def _read_diff_base(session_id: str, diff_param: str) -> str:
  """Read the base page named by the ``?diff=`` query parameter of a diff request."""
  candidate = _resolve_diff_base(session_id, diff_param)
  try:
    return candidate.read_text(encoding="utf-8")
  except OSError as e:
    raise fastapi.HTTPException(status_code=404, detail=_DIFF_BASE_NOT_FOUND_DETAIL.format(candidate)) from e


def _resolve_target(path: str, diff_param: str | None) -> tuple[pathlib.Path, str] | None:
  """The resolved path and the owning session of the artifact page the request addresses, or None to pass.

  One executor hop carries the resolve and every stat. None leaves the answer to the file
  server: a path that is no artifact page, a directory (its listing) and a missing path (its
  404). A ``?diff=`` request that addresses anything but an existing artifact page is the 400
  below; a missing path still passes, since the file server's 404 precedes every diff check.
  """
  fs_path = (pathlib.Path("/") / path).resolve()
  session_id = _artifact_session_id(fs_path) if fs_path.suffix.lower() == ".html" else None
  if diff_param is None:
    return (fs_path, session_id) if session_id is not None and fs_path.is_file() else None
  try:
    st_mode = fs_path.stat().st_mode
  except (FileNotFoundError, NotADirectoryError):
    return None
  except PermissionError:
    # The diff 400 outranks the unreadable 403: the diff target is checked before
    # the path is read.
    st_mode = None
  # A diff request addresses two artifact pages. Both must pass the artifact
  # predicate before anything is served, so a malformed address is rejected
  # rather than silently answered with the clean page.
  if session_id is None or st_mode is None or stat.S_ISDIR(st_mode):
    raise fastapi.HTTPException(status_code=400, detail=_DIFF_TARGET_DETAIL.format(fs_path))
  return fs_path, session_id


async def serve_artifact_path(request: fastapi.Request, path: str) -> responses.Response | None:
  """Answer a session artifact page or a ``?diff=`` compare request; None passes every other path to the file server.

  A request with no ``diff`` parameter whose path does not end in ``.html`` is no artifact
  page: it passes with no executor hop, so a plain file or a listing pays nothing here.
  """
  diff_param = request.query_params.get("diff")
  if diff_param is None and not path.lower().endswith(".html"):
    return None
  # Standalone artifact HTML gets the review UI injected here — the single chokepoint
  # that serves every artifact page — regardless of how the artifact was authored and
  # unconditionally: the auth middleware owns the credential gate (an uncredentialed
  # reader is answered 401 before this route runs), so a per-request credential branch
  # here could only ever make the comment tray silently vanish behind a stale cookie.
  target = await asyncio.to_thread(_resolve_target, path, diff_param)
  if target is None:
    return None
  fs_path, session_id = target
  if diff_param is not None:
    # The marks themselves are spliced into the response before the comment layer wraps them.
    base_text = await asyncio.to_thread(_read_diff_base, session_id, diff_param)
    page_text = await asyncio.to_thread(lambda: fs_path.read_text(encoding="utf-8"))
    html_text = plan_diff.annotate(base_text, page_text)
    html_text = _inject_artifact_ui(html_text, session_id)
    return responses.HTMLResponse(html_text, media_type="text/html", headers=_NO_STORE_HEADERS)

  html_text = await asyncio.to_thread(lambda: fs_path.read_text(encoding="utf-8"))
  html_text = await asyncio.to_thread(_inject_artifact_ui, html_text, session_id)
  return responses.HTMLResponse(html_text, media_type="text/html", headers=_NO_STORE_HEADERS)
