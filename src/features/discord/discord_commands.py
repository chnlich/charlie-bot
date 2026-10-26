"""Discord server-side commands — the thread read and the setup check behind the CLI verbs.

A Discord-summoned session reads its own thread through ``read_thread`` (the
internal ``/discord/read`` endpoint): the read pages the whole thread
oldest-first through the bot client, prepends the thread's starter message
when it lives in the parent channel, marks exactly the unread messages it
returns as read (Discord has no separate ack verb, so the read is the ack),
and flags the pending summon's mention with ``summon: true`` — the one fact
the one-line summon input cannot carry.
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

import asyncio
from typing import cast

from src.features.chat_threads import thread_entry
from src.features.discord import discord_client, discord_listener, metadata
from src.infra import config, metadata_slots
from src.infra import event_types as ET
from src.runtime import session_events, session_store
from src.runtime.api import message_utils


def _message_view(message: dict, *, unread: bool, summon: bool, allowed_users: dict[str, str]) -> dict:
  """One thread message as the readback states it: ids, author name, person, text, attachments, unread and summon flags.

  The author name is the author's ``global_name`` when set, else its
  ``username``; ``person`` is the name the ``discord.allowed_users`` map gives
  the author's id, None when the author's id is not in the map. ``content``
  and ``attachments`` arrive possibly empty, never absent, from the REST
  readback. ``summon`` marks the mention message of a pending summon — the
  summon this round answers (the readback of a linked channel carries no
  session, so it names none).
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
      "summon": summon,
  }


def _pending_summon_mention_ids(events: list[dict]) -> set[str]:
  """The mention message ids of the session's pending summons, as the readback flags them.

  A summon is pending while its event carries the discord block, no
  ``nudge_of``, and no master_done names its id (a nudge carries the block
  with ``nudge_of``, so it never counts); a lost summon the boot backfill
  marked is not pending either — the marker names its event id, and a lost
  summon never gets a master_done. A block without a mention id names no
  message to flag.
  """
  answered: set[str] = set()
  marked: set[str] = set()
  backfill_key = discord_listener.DISCORD.backfill_key
  for ev in events:
    if ev.get("type") == ET.MASTER_DONE:
      answered.update(message_utils.master_done_input_event_ids(ev))
    elif backfill_key in ev:
      marked.add(ev[backfill_key].get(ET.INPUT_EVENT_ID))
  pending: set[str] = set()
  for ev in events:
    if ev.get("type") != ET.AGENT_MESSAGE or not isinstance(ev.get("discord"), dict):
      continue
    block = ev["discord"]
    if "nudge_of" in block or ev["id"] in answered or ev["id"] in marked or block.get("mention_id") is None:
      continue
    pending.add(str(block["mention_id"]))
  return pending


async def _read_thread_messages(client: discord_client.DiscordClient, thread_id: str) -> list[dict]:
  """Every message of the thread *thread_id*, oldest first.

  A failed page raises ``ThreadReplyError`` 502 — the API error's text names the call, never the
  token.
  """
  try:
    return await discord_client.read_all_messages(client, thread_id)
  except discord_client.DiscordAPIError as e:
    raise thread_entry.ThreadReplyError(502, str(e)) from e


async def _read_own_thread(
    session_id: str, limit: int, cfg: config.CharlieBotConfig, store: session_store.SessionStore,
    events: session_events.SessionEvents) -> dict:
  """Read the session's own Discord thread; the unread messages returned are marked read.

  The thread's starter rides first, outside the window count, when it lives in
  the parent channel — a 404 there means the thread has none (a forum post, or
  a thread not started from a message). The window is *limit* messages
  starting at the earlier of the oldest unread message and the oldest pending
  summon's mention — or the newest *limit* when neither exists — so the read
  always shows the summon this round answers even though the summon step
  marked it read. A pending summon is a summon event in the session log that
  no master_done names yet and no lost-summon backfill marker marks
  (:func:`_pending_summon_mention_ids`); its mention
  message carries ``summon: true``, every other message ``summon: false``
  (the starter included when the mention is the starter). The unread messages
  inside the window are acked through ``thread_entry.ack_messages``, which
  advances the watermark; ``more_unread`` counts the unread left outside the
  window.
  """
  meta = await thread_entry.require_thread_session(discord_listener.DISCORD, session_id, store)
  fields = cast(metadata.DiscordSessionFields, metadata_slots.fields_of(meta, "discord"))
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
  pending_ids = _pending_summon_mention_ids(await asyncio.to_thread(events.load_chat_events_sync, session_id))
  starts = []
  if unread_ids:
    starts.append(next(i for i, m in enumerate(thread_messages) if m["id"] in unread_ids))
  if pending_ids:
    # A summon whose mention started the thread has its mention in the parent
    # channel, so it names no window start here: the starter view carries it.
    first_summon = next((i for i, m in enumerate(thread_messages) if m["id"] in pending_ids), None)
    if first_summon is not None:
      starts.append(first_summon)
  start = min(starts) if starts else None
  window = thread_messages[-limit:] if start is None else thread_messages[start:start + limit]
  window_unread_ids = [m["id"] for m in window if m["id"] in unread_ids]
  watermark_id: str | None = watermark
  if window_unread_ids:
    ack = await thread_entry.ack_messages(adapter, session_id, window_unread_ids, cfg, store, events)
    watermark_id = ack["watermark_id"]
  allowed_users = cfg.discord.allowed_users
  messages = [] if starter is None else [
      _message_view(starter, unread=False, summon=starter["id"] in pending_ids, allowed_users=allowed_users)
  ]
  messages.extend(
      _message_view(m, unread=m["id"] in unread_ids, summon=m["id"] in pending_ids, allowed_users=allowed_users)
      for m in window)
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
      "messages": [_message_view(m, unread=False, summon=False, allowed_users=allowed_users) for m in newest],
      "watermark_id": None,
      "more_unread": 0,
  }


async def read_thread(
    session_id: str,
    url: str | None,
    limit: int,
    cfg: config.CharlieBotConfig,
    store: session_store.SessionStore,
    events: session_events.SessionEvents,
) -> dict:
  """Read one Discord thread server-side and return its messages, oldest first.

  Without *url*, the session's own thread (404 unknown session, 409 no
  Discord thread): the starter message rides first when the thread has one in
  the parent channel, the window is *limit* messages starting at the earlier
  of the oldest unread message and the oldest pending summon's mention — or
  the newest *limit* when neither exists — and the unread messages in the
  window are marked read, so reading is the ack. The readback carries
  ``watermark_id`` (after the ack) and ``more_unread`` (unread messages left
  outside the window); every message view names its author's configured
  ``person`` and flags a pending summon's mention with ``summon: true``.
  With *url*, a discord.com channel link:
  the newest *limit* messages of that channel, all with ``unread: false``,
  nothing marked. A Discord refusal beyond the mapped statuses above is 502;
  its text names the failing call, never the token.
  """
  if url is not None:
    return await _read_linked_channel(url, limit, cfg)
  return await _read_own_thread(session_id, limit, cfg, store, events)


async def check_setup() -> dict:
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
