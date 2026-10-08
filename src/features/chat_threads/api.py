"""The sidebar Threads view's list route."""

from fastapi import APIRouter, Depends, Request
from starlette.responses import Response

from src.features.chat_threads.sidebar import THREADS_VIEW
from src.infra.config import CharlieBotConfig
from src.runtime.api.deps import get_config_on_loop, get_session_manager, get_thread_manager
from src.runtime.api.sessions import _active_listing_corpus, _sessions_list_response, _SessionsListMemos
from src.runtime.sessions import SessionManager
from src.runtime.threads import ThreadManager

router = APIRouter()

_chat_threads_list_memos = _SessionsListMemos()


@router.get("/chat-threads")
async def list_chat_threads(
    request: Request,
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
    thread_mgr: ThreadManager = Depends(get_thread_manager),
) -> Response:
  """List the active chat-thread subtree newest first: the sidebar Threads view's rows.

  The complement of the Workspace root list over the same active corpus: the
  only rows kept are the Slack/Discord thread sessions — a session carrying a
  ``slack_origin`` or ``discord_origin`` — and every descendant their
  ``task_parent_id`` chains reach, the projected legacy worker-thread leaves
  included. Row shape, projection, schedule join, and render are the shared
  helper's; the render memos are this route's own, so the two lists never
  evict each other.
  """
  rows, derived = await _active_listing_corpus(session_mgr)
  chat_threads = (await session_mgr.view_subtree_roots()).get(THREADS_VIEW, {})
  rows = [row for row in rows if row.id in chat_threads]
  return await _sessions_list_response(request, rows, derived, cfg, thread_mgr, _chat_threads_list_memos)
