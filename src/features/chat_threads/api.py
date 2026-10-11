"""The sidebar Threads view's list route."""

import fastapi
from starlette import responses

from src.features.chat_threads import sidebar
from src.runtime import session_listing
from src.runtime.api import deps, sessions

router = fastapi.APIRouter()

_chat_threads_list_memos = sessions._SessionsListMemos()


@router.get("/chat-threads")
async def list_chat_threads(
    request: fastapi.Request,
    listing: session_listing.SessionListing = fastapi.Depends(deps.get_session_listing),
) -> responses.Response:
  """List the active chat-thread subtree newest first: the sidebar Threads view's rows.

  The complement of the Workspace root list over the same active corpus: the
  only rows kept are the Slack/Discord thread sessions — a session carrying a
  ``slack_origin`` or ``discord_origin`` — and every descendant their
  ``task_parent_id`` chains reach. Row shape, schedule join, and render are
  the shared helper's; the render memos are this route's own, so the two lists
  never evict each other.
  """
  rows, derived = await sessions._active_listing_corpus(listing)
  chat_threads = (await listing.view_subtree_roots()).get(sidebar.THREADS_VIEW, {})
  rows = [row for row in rows if row.id in chat_threads]
  return await sessions._sessions_list_response(request, rows, derived, _chat_threads_list_memos)
