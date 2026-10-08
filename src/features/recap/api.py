"""Session recap API: the extracted recap of a divider and its generated summary."""

import asyncio

from fastapi import APIRouter, Depends

from src.infra.config import CharlieBotConfig
from src.infra.models import SessionMetadata
from src.infra.responses import FastJsonResponse
from src.runtime.api.deps import get_config_on_loop, get_session_events, get_session_store, require_session
from src.runtime.session_events import SessionEvents
from src.runtime.session_store import SessionStore

router = APIRouter()


@router.get('/{session_id}/recap')
async def get_session_recap(
    session_id: str,
    upto: int | None = None,
    _meta: SessionMetadata = Depends(require_session),
    session_events: SessionEvents = Depends(get_session_events),
) -> FastJsonResponse:
  """Pure-extraction recap (no LLM) plus any cached Haiku summary for a divider.

  ``upto`` is a global event_index (default: latest). Returns ordered asks, the
  last exchange, the cached summary (or null), and whether that summary is stale.
  """
  from src.features.recap import recap
  if upto is None:
    count = await asyncio.to_thread(session_events.get_chat_event_count_sync, session_id)
    upto = max(0, count - 1)
  # The chat UI re-requests an open recap panel on every re-materialization, so a
  # repeat read answers from the extract + summary-cache memos on the event loop;
  # the executor round-trips are paid only on a memo miss.
  extract = recap.extract_recap_memo_hit(session_id, upto)
  if extract is None:
    extract = await asyncio.to_thread(recap.extract_recap, session_events, session_id, upto)
  summary = recap.summary_lookup_memo_hit(session_events, session_id, upto)
  if summary is None:
    summary = await asyncio.to_thread(recap.lookup_cached_summary, session_events, session_id, upto)
  summary_text, stale = summary
  return FastJsonResponse({**extract, "summary": summary_text, "summary_stale": stale})


@router.post('/{session_id}/recap/summarize')
async def summarize_session_recap(
    session_id: str,
    upto: int,
    _meta: SessionMetadata = Depends(require_session),
    store: SessionStore = Depends(get_session_store),
    session_events: SessionEvents = Depends(get_session_events),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
) -> dict:
  """Generate (via a light backend), cache, and return the recap summary for a divider."""
  from src.features.recap import recap
  summary = await recap.generate_and_cache_summary(store, session_events, session_id, upto, cfg)
  return {"summary": summary}
