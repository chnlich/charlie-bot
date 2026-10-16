"""Internal API endpoints behind ``charliebot discord reply``, ``read`` and ``check``."""

import fastapi
import pydantic

from src.features.chat_threads import thread_entry
from src.features.discord import discord_commands, discord_listener
from src.infra import config
from src.runtime import session_events, session_store
from src.runtime.api import deps

router = fastapi.APIRouter()


class DiscordReplyRequest(pydantic.BaseModel):
  """Request body for the internal discord/reply endpoint: the calling session posts *text* to its own thread."""
  model_config = pydantic.ConfigDict(extra="forbid")

  session_id: str
  text: str


class DiscordReadRequest(pydantic.BaseModel):
  """Request body for the internal discord/read endpoint: the session reads its own thread, or the channel *url* names."""
  model_config = pydantic.ConfigDict(extra="forbid")

  session_id: str
  url: str | None = None
  limit: int = pydantic.Field(50, ge=1, le=100)


class DiscordCheckRequest(pydantic.BaseModel):
  """Request body for the internal discord/check endpoint: reports the bot token's setup, carrying no fields."""
  model_config = pydantic.ConfigDict(extra="forbid")


@router.post("/discord/reply")
async def discord_reply(
    req: DiscordReplyRequest,
    store: session_store.SessionStore = fastapi.Depends(deps.get_session_store),
    events: session_events.SessionEvents = fastapi.Depends(deps.get_session_events),
    cfg: config.CharlieBotConfig = fastapi.Depends(deps.get_config_on_loop),
) -> dict:
  """Post the calling session's reply to its own Discord thread and return the readback.

  The in-process boundary behind ``charliebot discord reply``: the session's
  platform origin names the thread, and the readback (posted, text, chars,
  chunks, over_budget, answers) is what the CLI prints. Refusals map
  ThreadReplyError's status (404 unknown session, 409 no Discord thread, 422
  blank text or a file-server link, 502 Discord rejected the post after
  retries); nothing is persisted on a refusal. Freshness is gated first: eligible thread messages
  above the session's watermark refuse with a 412 ``stale_thread`` payload
  naming each unseen message, before any chunk posts.
  """
  try:
    await discord_listener.assert_thread_fresh(req.session_id, cfg, store)
    return await discord_listener.post_reply(req.session_id, req.text, cfg, store, events)
  except thread_entry.ThreadReplyError as exc:
    raise fastapi.HTTPException(status_code=exc.status, detail=exc.detail) from exc


@router.post("/discord/read")
async def discord_read(
    req: DiscordReadRequest,
    store: session_store.SessionStore = fastapi.Depends(deps.get_session_store),
    events: session_events.SessionEvents = fastapi.Depends(deps.get_session_events),
    cfg: config.CharlieBotConfig = fastapi.Depends(deps.get_config_on_loop),
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
  try:
    return await discord_commands.read_thread(req.session_id, req.url, req.limit, cfg, store, events)
  except thread_entry.ThreadReplyError as exc:
    raise fastapi.HTTPException(status_code=exc.status, detail=exc.detail) from exc


@router.post("/discord/check")
async def discord_check(
    req: DiscordCheckRequest,
    cfg: config.CharlieBotConfig = fastapi.Depends(deps.get_config_on_loop),
) -> dict:
  """Report the Discord bot token's setup: bot user, message-content intent, per-guild permissions.

  The boundary behind ``charliebot discord check``: ``ok`` is the
  message-content intent on and no guild missing a required permission. Refusals
  map ThreadReplyError's status: 409 when ``credentials.discord.bot_token`` is
  not set, 502 when Discord refuses the token (a 401 means it is invalid). The
  readback carries no token.
  """
  try:
    return await discord_commands.check_setup(cfg)
  except thread_entry.ThreadReplyError as exc:
    raise fastapi.HTTPException(status_code=exc.status, detail=exc.detail) from exc
