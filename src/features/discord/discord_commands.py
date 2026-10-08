"""Discord server-side commands — the thread read and the setup check behind the CLI verbs.

A Discord-summoned session reads its own thread through ``read_thread`` (the
internal ``/discord/read`` endpoint): the read pages the whole thread
oldest-first through the bot client, prepends the thread's starter message
when it lives in the parent channel, and marks exactly the unread messages it
returns as read — Discord has no separate ack verb, so the read is the ack.
With a channel link it reads that channel instead and marks nothing. Every
Discord call runs inside the server process through the client
``discord_listener._bot_client`` builds — the token never reaches the CLI.
``check_setup`` (the internal ``/discord/check``
endpoint) reports the bot identity, the message-content intent, and the
per-guild missing permissions, so the operator can see what the bot token can
and cannot do. Refusals raise ``ThreadReplyError`` with the HTTP status the
endpoint maps onto the response.
"""

from __future__ import annotations

from typing import cast

from src.features.chat_threads import thread_entry
from src.features.discord import discord_client, discord_listener
from src.features.discord.metadata import DiscordSessionFields
from src.infra import config, metadata_slots
from src.runtime import sessions


def _message_view(message: dict, *, unread: bool, allowed_users: dict[str, str]) -> dict:
  """One thread message as the readback states it: ids, author name, person, text, attachments, unread flag.

  The author name is the author's ``global_name`` when set, else its
  ``username``; ``person`` is the name the ``discord.allowed_users`` map gives
  the author's id, None when the author's id is not in the map. ``content``
  and ``attachments`` arrive possibly empty, never absent, from the REST
  readback.
  """
  author = message["author"]
  return {
      "id": message["id"],
      "author_id": author["id"],
      "author": author.get("global_name") or author["username"],
      "person": allowed_users.get(author["id"]),
      "timestamp": message["timestamp"],
      "content": message.get("content") or "",
      "attachments": [a["url"] for a in message.get("attachments") or []],
      "unread": unread,
  }


async def _read_thread_messages(client: discord_client.DiscordClient, thread_id: str) -> list[dict]:
  """Every message of the thread *thread_id*, oldest first, paged 100 at a time from id "0".

  Discord caps one read at 100 messages, so each page continues after the
  previous page's last id until one comes back short. A failed page raises
  ``ThreadReplyError`` 502 — the API error's text names the call, never the
  token.
  """
  messages: list[dict] = []
  after = "0"
  while True:
    try:
      page = await client.get_messages(thread_id, after=after, limit=100)
    except discord_client.DiscordAPIError as e:
      raise thread_entry.ThreadReplyError(502, str(e)) from e
    messages.extend(page)
    if len(page) < 100:
      return messages
    after = page[-1]["id"]


async def _read_own_thread(
    session_id: str, limit: int, cfg: config.CharlieBotConfig, session_mgr: sessions.SessionManager) -> dict:
  """Read the session's own Discord thread; the unread messages returned are marked read.

  The thread's starter rides first, outside the window count, when it lives in
  the parent channel — a 404 there means the thread has none (a forum post, or
  a thread not started from a message). The window is *limit* messages
  starting at the oldest unread one, or the newest *limit* when nothing is
  unread. The unread messages inside the window are acked through
  ``thread_entry.ack_messages``, which advances the watermark; ``more_unread``
  counts the unread left outside the window.
  """
  meta = await thread_entry.require_thread_session(discord_listener.DISCORD, session_id, session_mgr)
  fields = cast(DiscordSessionFields, metadata_slots.fields_of(meta, "discord"))
  origin = fields.discord_origin
  assert origin is not None
  client = discord_listener._bot_client()
  adapter = discord_listener.DiscordThreadAdapter(client)
  watermark = fields.discord_watermark_id
  thread_messages = await _read_thread_messages(client, origin.thread_id)
  try:
    starter = await client.get_message(origin.parent_channel_id, origin.thread_id)
  except discord_client.DiscordAPIError as e:
    if e.status != 404:
      raise thread_entry.ThreadReplyError(502, str(e)) from e
    starter = None
  # One read feeds both the window and the unread flags: a message landing after
  # this fetch is simply not in the readback, never an id the window pick misses.
  unread_ids = {
      m["id"] for m in thread_entry.unread_after(
          thread_messages,
          eligible=lambda m: discord_listener.eligible_message(m, cfg.discord.allowed_users),
          message_id=lambda m: m["id"],
          watermark=watermark,
          id_key=discord_client.snowflake_key)
  }
  if unread_ids:
    start = next(i for i, m in enumerate(thread_messages) if m["id"] in unread_ids)
    window = thread_messages[start:start + limit]
  else:
    window = thread_messages[-limit:]
  window_unread_ids = [m["id"] for m in window if m["id"] in unread_ids]
  watermark_id: str | None = watermark
  if window_unread_ids:
    ack = await thread_entry.ack_messages(adapter, session_id, window_unread_ids, cfg, session_mgr)
    watermark_id = ack["watermark_id"]
  allowed_users = cfg.discord.allowed_users
  messages = [] if starter is None else [_message_view(starter, unread=False, allowed_users=allowed_users)]
  messages.extend(_message_view(m, unread=m["id"] in unread_ids, allowed_users=allowed_users) for m in window)
  return {"messages": messages, "watermark_id": watermark_id, "more_unread": len(unread_ids) - len(window_unread_ids)}


async def _read_linked_channel(url: str, limit: int, cfg: config.CharlieBotConfig) -> dict:
  """Read the newest *limit* messages of the channel *url* names; nothing is marked read.

  A url that is not a discord.com channel link refuses with 422. Discord
  refusing the read with 403 or 404 means the bot cannot see the channel and
  refuses with 404; any other Discord refusal is 502.
  """
  try:
    _guild_id, channel_id, _message_id = discord_client.parse_message_link(url)
  except ValueError as e:
    raise thread_entry.ThreadReplyError(422, str(e)) from e
  client = discord_listener._bot_client()
  try:
    newest = await client.get_messages(channel_id, limit=limit)
  except discord_client.DiscordAPIError as e:
    if e.status in (403, 404):
      raise thread_entry.ThreadReplyError(404, "the bot cannot see this channel") from e
    raise thread_entry.ThreadReplyError(502, str(e)) from e
  allowed_users = cfg.discord.allowed_users
  return {
      "messages": [_message_view(m, unread=False, allowed_users=allowed_users) for m in newest],
      "watermark_id": None,
      "more_unread": 0,
  }


async def read_thread(
    session_id: str,
    url: str | None,
    limit: int,
    cfg: config.CharlieBotConfig,
    session_mgr: sessions.SessionManager,
) -> dict:
  """Read one Discord thread server-side and return its messages, oldest first.

  Without *url*, the session's own thread (404 unknown session, 409 no
  Discord thread): the starter message rides first when the thread has one in
  the parent channel, the window is *limit* messages starting at the oldest
  unread one — or the newest *limit* when nothing is unread — and the unread
  messages in the window are marked read, so reading is the ack. The readback
  carries ``watermark_id`` (after the ack) and ``more_unread`` (unread
  messages left outside the window); every message view names its author's
  configured ``person``. With *url*, a discord.com channel link:
  the newest *limit* messages of that channel, all with ``unread: false``,
  nothing marked. A Discord refusal beyond the mapped statuses above is 502;
  its text names the failing call, never the token.
  """
  if url is not None:
    return await _read_linked_channel(url, limit, cfg)
  return await _read_own_thread(session_id, limit, cfg, session_mgr)


async def check_setup(cfg: config.CharlieBotConfig) -> dict:
  """Report the Discord bot token's setup: identity, intent, and per-guild permissions.

  Refuses with 409 when ``credentials.discord.bot_token`` is unset — there is
  nothing to check. Otherwise the three ``@me`` reads name the bot user, the
  application flags (the message-content intent), and every guild's permission
  bitfield; ``ok`` is the intent on and no guild missing anything. A failed
  Discord call raises ``ThreadReplyError`` 502 (a 401 means the token is
  invalid). The readback carries no token.
  """
  if config.get_credentials().get("discord", "bot_token") is None:
    raise thread_entry.ThreadReplyError(409, "credentials.discord.bot_token is not set")
  client = discord_listener._bot_client()
  try:
    user = await client.get_current_user()
    application = await client.get_current_application()
    guilds = await client.list_current_user_guilds()
  except discord_client.DiscordAPIError as e:
    raise thread_entry.ThreadReplyError(502, str(e)) from e
  intent = discord_client.message_content_intent_enabled(application["flags"])
  guild_views = [
      {
          "id": g["id"],
          "name": g["name"],
          "missing_permissions": discord_client.missing_permissions(int(g["permissions"])),
      } for g in guilds
  ]
  return {
      "ok": intent and not any(view["missing_permissions"] for view in guild_views),
      "bot_user": {
          "id": user["id"],
          "username": user["username"]
      },
      "application_id": application["id"],
      "message_content_intent": intent,
      "guilds": guild_views,
  }
