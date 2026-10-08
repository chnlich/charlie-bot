"""Internal API endpoints behind ``charliebot discord reply``, ``read`` and ``check``."""

from fastapi import APIRouter, Depends, HTTPException

from src.infra.config import CharlieBotConfig
from src.infra.models import DiscordCheckRequest, DiscordReadRequest, DiscordReplyRequest
from src.runtime.api.deps import get_config_on_loop, get_session_manager
from src.runtime.sessions import SessionManager

router = APIRouter()


@router.post("/discord/reply")
async def discord_reply(
    req: DiscordReplyRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
) -> dict:
  """Post the calling session's reply to its own Discord thread and return the readback.

  The in-process boundary behind ``charliebot discord reply``: the session's
  ``discord_origin`` names the thread, and the readback (posted, text, chars,
  chunks, over_budget, answers) is what the CLI prints. Refusals map
  ThreadReplyError's status (404 unknown session, 409 no Discord thread, 422
  blank text or a file-server link, 502 Discord rejected the post after
  retries); nothing is persisted on a refusal. Freshness is gated first: eligible thread messages
  above the session's watermark refuse with a 412 ``stale_thread`` payload
  naming each unseen message, before any chunk posts.
  """
  # The M99 server import floor carries no Discord-gateway stack for endpoints a
  # server may never call; the imports ride the handlers that reach the gateway.
  from src.features.chat_threads.thread_entry import ThreadReplyError
  from src.features.discord import discord_listener
  try:
    await discord_listener.assert_thread_fresh(req.session_id, cfg, session_mgr)
    return await discord_listener.post_reply(req.session_id, req.text, cfg, session_mgr)
  except ThreadReplyError as exc:
    raise HTTPException(status_code=exc.status, detail=exc.detail) from exc


@router.post("/discord/read")
async def discord_read(
    req: DiscordReadRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
) -> dict:
  """Read the calling session's Discord thread (or the channel *url* names) and return its messages.

  The boundary behind ``charliebot discord read``: without *url* the session's
  own thread is read oldest first (the thread's starter rides first), the
  window is *limit* messages starting at the oldest unread one — or the newest
  *limit* when nothing is unread — and the unread messages returned are marked
  read (Discord has no separate ack verb), so the readback carries the
  watermark after the ack plus ``more_unread``. With *url*, the newest *limit*
  messages of that channel come back all unread-false and nothing is marked.
  Refusals map ThreadReplyError's status: 404 unknown session, 409 no Discord
  thread, 422 a *url* that is not a discord.com link, 404 a channel the bot
  cannot see, 502 Discord refused the read.
  """
  from src.features.chat_threads.thread_entry import ThreadReplyError
  from src.features.discord.discord_commands import read_thread
  try:
    return await read_thread(req.session_id, req.url, req.limit, cfg, session_mgr)
  except ThreadReplyError as exc:
    raise HTTPException(status_code=exc.status, detail=exc.detail) from exc


@router.post("/discord/check")
async def discord_check(
    req: DiscordCheckRequest,
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
) -> dict:
  """Report the Discord bot token's setup: bot user, message-content intent, per-guild permissions.

  The boundary behind ``charliebot discord check``: ``ok`` is the
  message-content intent on and no guild missing a required permission. Refusals
  map ThreadReplyError's status: 409 when ``credentials.discord.bot_token`` is
  not set, 502 when Discord refuses the token (a 401 means it is invalid). The
  readback carries no token.
  """
  from src.features.chat_threads.thread_entry import ThreadReplyError
  from src.features.discord.discord_commands import check_setup
  try:
    return await check_setup(cfg)
  except ThreadReplyError as exc:
    raise HTTPException(status_code=exc.status, detail=exc.detail) from exc
