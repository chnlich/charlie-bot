"""Unit tests for the Discord server-side commands (src.features.discord.discord_commands).

Every test drives read_thread / check_setup (and, for the HTTP refusals, the
internal endpoints) against a recording fake client handed to
``discord_listener._bot_client`` — no network, real session blocks on a tmp
home, synthetic ids throughout.
"""

from __future__ import annotations

import json
import pathlib
from unittest import mock

import conftest
import pytest

from src.features.discord import discord_client, discord_commands, discord_listener
from src.features.discord.metadata import DiscordOrigin
from src.infra import config, metadata_slots, models

_GUILD = "800000000000000001"
_PARENT = "800000000000000002"
_THREAD = "800000000000000003"
_OTHER_CHANNEL = "800000000000000004"
_OTHER_GUILD = "800000000000000005"
_USER = "700000000000000001"
_BOT = "600000000000000001"
# Placeholder account and person names for the discord.allowed_users map: no real identity.
_PERSON = "tester"
_OTHER_USER = "700000000000000002"
_UNLISTED = "700000000000000003"


def _mid(i: int) -> str:
  """Synthetic snowflake i: ids of different digit counts must never string-sort."""
  return str(900000000000000000 + i)


class FakeDiscordClient:
  """Recording Discord REST double for discord_commands tests; never touches the network.

  ``channels`` maps channel id to its messages oldest-first. ``get_messages``
  mirrors the wire contract: without ``after`` Discord serves the newest
  ``limit``, with it the first ones above the floor — both oldest-first, the
  shape the real client's sort produces. ``get_message`` answers the
  single-message read, 404 when the id is absent (the thread-has-no-starter
  case); channels in ``hidden`` refuse reads with 403. The check paths answer
  from ``user`` / ``application`` / ``guilds``. Implements only what the
  commands paths may call; a regression to calling anything else fails here
  with an AttributeError by construction.
  """

  def __init__(
      self,
      *,
      channels: dict[str, list[dict]] | None = None,
      hidden: tuple[str, ...] = (),
      user: dict | None = None,
      application: dict | None = None,
      guilds: list[dict] | None = None,
  ) -> None:
    self.calls: list[tuple[str, dict]] = []
    self.channels = channels or {}
    self.hidden = set(hidden)
    self.user = user
    self.application = application
    self.guilds = guilds or []
    self.posts: list[dict] = []

  async def get_message(self, channel_id: str, message_id: str) -> dict:
    self.calls.append(("get_message", {"channel_id": channel_id, "message_id": message_id}))
    for message in self.channels.get(channel_id, []):
      if message["id"] == message_id:
        return message
    raise discord_client.DiscordAPIError(
        "GET", f"/channels/{channel_id}/messages/{message_id}", 404, 10008, "Unknown Message")

  async def get_messages(self, channel_id: str, *, after: str | None = None, limit: int = 100) -> list[dict]:
    self.calls.append(("get_messages", {"channel_id": channel_id, "after": after, "limit": limit}))
    if channel_id in self.hidden:
      raise discord_client.DiscordAPIError("GET", f"/channels/{channel_id}/messages", 403, 50001, "Missing Access")
    floor = discord_client.snowflake_key(after) if after is not None else 0
    matches = [m for m in self.channels.get(channel_id, []) if discord_client.snowflake_key(m["id"]) > floor]
    return matches[-limit:] if after is None else matches[:limit]

  async def create_message(self, channel_id: str, content: str) -> dict:
    self.calls.append(("create_message", {"channel_id": channel_id}))
    self.posts.append({"channel_id": channel_id, "content": content})
    return {"id": _mid(99)}

  async def get_current_user(self) -> dict:
    self.calls.append(("get_current_user", {}))
    return self.user

  async def get_current_application(self) -> dict:
    self.calls.append(("get_current_application", {}))
    return self.application

  async def list_current_user_guilds(self) -> list[dict]:
    self.calls.append(("list_current_user_guilds", {}))
    return self.guilds


def _build_cfg(tmp_path: pathlib.Path) -> config.CharlieBotConfig:
  """Config with the home under tmp_path, the stubbed test token, and one mapped account."""
  conftest.stub_credentials({"discord": {"bot_token": "test-bot-token"}})
  return config.CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      discord={"allowed_users": {
          _USER: _PERSON,
          _OTHER_USER: "tester-two"
      }},
      backends=conftest.fake_backends(),
  )


def _rig(tmp_path: pathlib.Path,
         **client_kwargs: object) -> tuple[config.CharlieBotConfig, conftest.SessionBlocks, FakeDiscordClient]:
  """Rig: cfg and session home rooted at tmp_path, plus the recording fake client."""
  cfg = _build_cfg(tmp_path)
  return cfg, conftest.build_session_blocks(cfg), FakeDiscordClient(**client_kwargs)


def _message(
    i: int,
    content: str,
    *,
    message_id: str | None = None,
    author: str = _USER,
    username: str = "plain-name",
    global_name: str | None = None,
    bot: bool = False,
    attachments: tuple[str, ...] = (),
) -> dict:
  """One REST message object: number i fixes both the id and the timestamp."""
  author_obj = {"id": author, "username": username, "global_name": global_name}
  if bot:
    author_obj["bot"] = True
  return {
      "id": message_id or _mid(i),
      "author": author_obj,
      "timestamp": f"2026-08-26T00:00:{i:02d}Z",
      "content": content,
      "attachments": [{
          "url": url
      } for url in attachments],
      "type": 0,
  }


async def _make_session(session_blocks: conftest.SessionBlocks, *, watermark: str | None = None) -> str:
  """A Discord-backed session like a summon leaves it, with an optional read watermark."""
  meta = await conftest.create_root_session(
      session_blocks,
      models.CreateSessionRequest(
          session_id=discord_listener.summon_session_id(_GUILD, _THREAD),
          name="discord session",
          discord_origin=DiscordOrigin(guild_id=_GUILD, parent_channel_id=_PARENT, thread_id=_THREAD)))
  if watermark is not None:
    metadata_slots.set_fields(meta, "discord", discord_watermark_id=watermark)
    meta.updated_at = models.utc_now()
    await session_blocks.store.save_metadata(meta)
  return meta.id


def _ack_events(session_blocks: conftest.SessionBlocks, session_id: str) -> list[dict]:
  return [ev for ev in session_blocks.events.load_chat_events_sync(session_id) if ev["type"] == "discord_ack"]


# ---------------------------------------------------------------------------
# Read: the session's own thread
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_read_marks_only_returned_unread_and_reports_more_unread(tmp_path: pathlib.Path) -> None:
  """The window starts at the oldest unread; the ack covers exactly its unread ids."""
  cfg, session_blocks, client = _rig(
      tmp_path,
      channels={
          _THREAD:
              [
                  _message(1, "hello"),
                  _message(2, "second"),
                  _message(3, "bot noise", author=_BOT, bot=True),
                  _message(4, "first follow up"),
                  _message(5, "attached", attachments=("https://cdn.discordapp.com/attachments/shot.png",)),
                  _message(6, "bot again", author=_BOT, bot=True),
                  _message(7, "third follow up"),
                  _message(8, "fourth follow up"),
                  _message(9, "fifth follow up"),
                  _message(10, "sixth follow up"),
              ],
      })
  session_id = await _make_session(session_blocks, watermark=_mid(2))
  # Unread runs m4..m10 minus the ineligible bot messages (m3, m6); the window
  # is the four messages from the oldest unread (m4) on, bot message included,
  # read. The bot message is not unread and rides outside the ack.
  with mock.patch(conftest.DISCORD_LISTENER_BOT_CLIENT_PATCH_TARGET, return_value=client):
    result = await discord_commands.read_thread(session_id, None, 4, cfg, session_blocks.store, session_blocks.events)

  assert [m["id"] for m in result["messages"]] == [_mid(i) for i in (4, 5, 6, 7)]
  assert [m["unread"] for m in result["messages"]] == [True, True, False, True]
  assert result["messages"][1]["attachments"] == ["https://cdn.discordapp.com/attachments/shot.png"]
  assert result["watermark_id"] == _mid(7)
  assert result["more_unread"] == 3

  meta = await session_blocks.store.get_session(session_id)
  assert meta is not None and metadata_slots.fields_of(meta, "discord").discord_watermark_id == _mid(7)
  acks = _ack_events(session_blocks, session_id)
  assert len(acks) == 1
  assert acks[0]["discord_ack"] == {"message_ids": [_mid(4), _mid(5), _mid(7)], "watermark_id": _mid(7)}

  # A second read picks up the run the first window cut: the rest of the unread.
  with mock.patch(conftest.DISCORD_LISTENER_BOT_CLIENT_PATCH_TARGET, return_value=client):
    again = await discord_commands.read_thread(session_id, None, 4, cfg, session_blocks.store, session_blocks.events)
  assert [m["id"] for m in again["messages"]] == [_mid(i) for i in (8, 9, 10)]
  assert [m["unread"] for m in again["messages"]] == [True, True, True]
  assert again["watermark_id"] == _mid(10)
  assert again["more_unread"] == 0


@pytest.mark.asyncio
async def test_read_without_unread_returns_the_newest_limit(tmp_path: pathlib.Path) -> None:
  """Nothing unread: the window is the newest *limit* messages and nothing is marked."""
  cfg, session_blocks, client = _rig(tmp_path, channels={_THREAD: [_message(i, f"m{i}") for i in range(1, 9)]})
  session_id = await _make_session(session_blocks, watermark=_mid(8))

  with mock.patch(conftest.DISCORD_LISTENER_BOT_CLIENT_PATCH_TARGET, return_value=client):
    result = await discord_commands.read_thread(session_id, None, 3, cfg, session_blocks.store, session_blocks.events)

  assert [m["id"] for m in result["messages"]] == [_mid(i) for i in (6, 7, 8)]
  assert all(m["unread"] is False for m in result["messages"])
  assert result["watermark_id"] == _mid(8)
  assert result["more_unread"] == 0
  assert _ack_events(session_blocks, session_id) == []


@pytest.mark.asyncio
async def test_read_succeeds_when_a_message_lands_after_the_full_read(tmp_path: pathlib.Path) -> None:
  """A message arriving after the thread read stays out of the readback and breaks nothing.

  The unread set comes from the one fetched message list, so a message Discord
  serves only to a later read (nothing else unread) cannot name an id the
  window pick misses: the read returns, the latecomer is not in it, and
  nothing is marked.
  """
  cfg, session_blocks, client = _rig(tmp_path, channels={_THREAD: [_message(1, "first"), _message(2, "second")]})
  session_id = await _make_session(session_blocks, watermark=_mid(2))  # nothing unread at the read
  real_get = client.get_messages
  thread_reads = {"n": 0}

  async def get_messages(channel_id: str, *, after: str | None = None, limit: int = 100) -> list[dict]:
    page = await real_get(channel_id, after=after, limit=limit)
    if channel_id == _THREAD:
      thread_reads["n"] += 1
      if thread_reads["n"] == 1:  # the new message lands right after the first full read
        client.channels[_THREAD].append(_message(3, "arrived in between"))
    return page

  client.get_messages = get_messages
  with mock.patch(conftest.DISCORD_LISTENER_BOT_CLIENT_PATCH_TARGET, return_value=client):
    result = await discord_commands.read_thread(session_id, None, 50, cfg, session_blocks.store, session_blocks.events)

  assert [m["id"] for m in result["messages"]] == [_mid(1), _mid(2)]
  assert all(m["unread"] is False for m in result["messages"])
  assert result["watermark_id"] == _mid(2)
  assert result["more_unread"] == 0
  assert _ack_events(session_blocks, session_id) == []


@pytest.mark.asyncio
async def test_read_prepends_the_parent_starter_outside_the_limit(tmp_path: pathlib.Path) -> None:
  """A thread started from a parent message reads its starter first, not counted in *limit*."""
  starter = _message(0, "please plan the release", message_id=_THREAD, global_name="Display Name")
  cfg, session_blocks, client = _rig(
      tmp_path,
      channels={
          _PARENT: [starter],
          _THREAD: [_message(1, "first follow up"), _message(2, "second follow up")],
      })
  session_id = await _make_session(session_blocks)

  with mock.patch(conftest.DISCORD_LISTENER_BOT_CLIENT_PATCH_TARGET, return_value=client):
    result = await discord_commands.read_thread(session_id, None, 2, cfg, session_blocks.store, session_blocks.events)

  assert [m["id"] for m in result["messages"]] == [_THREAD, _mid(1), _mid(2)]
  # The starter rides first, unread false; limit=2 still delivered both unread.
  assert result["messages"][0] == {
      "id": _THREAD,
      "author_id": _USER,
      "author": "Display Name",
      "person": _PERSON,
      "timestamp": "2026-08-26T00:00:00Z",
      "content": "please plan the release",
      "attachments": [],
      "unread": False,
  }
  # A message view: global_name unset falls back to the username.
  assert result["messages"][1] == {
      "id": _mid(1),
      "author_id": _USER,
      "author": "plain-name",
      "person": _PERSON,
      "timestamp": "2026-08-26T00:00:01Z",
      "content": "first follow up",
      "attachments": [],
      "unread": True,
  }


@pytest.mark.asyncio
async def test_read_skips_a_404_starter(tmp_path: pathlib.Path) -> None:
  """A forum post or a thread not started from a message has no starter: the read goes on without one."""
  cfg, session_blocks, client = _rig(
      tmp_path, channels={_THREAD: [_message(1, "first follow up"),
                                    _message(2, "second follow up")]})
  session_id = await _make_session(session_blocks)

  with mock.patch(conftest.DISCORD_LISTENER_BOT_CLIENT_PATCH_TARGET, return_value=client):
    result = await discord_commands.read_thread(session_id, None, 2, cfg, session_blocks.store, session_blocks.events)

  assert [m["id"] for m in result["messages"]] == [_mid(1), _mid(2)]
  assert [m["unread"] for m in result["messages"]] == [True, True]


@pytest.mark.asyncio
async def test_read_names_the_author_person_for_listed_and_unlisted_authors(tmp_path: pathlib.Path) -> None:
  """person is the account's configured name; an author outside the map reads as None everywhere."""
  cfg, session_blocks, client = _rig(
      tmp_path,
      channels={
          _THREAD: [_message(1, "mine"), _message(2, "unlisted", author=_UNLISTED)],
          _OTHER_CHANNEL: [_message(3, "linked", author=_UNLISTED)],
      })
  session_id = await _make_session(session_blocks)

  with mock.patch(conftest.DISCORD_LISTENER_BOT_CLIENT_PATCH_TARGET, return_value=client):
    own = await discord_commands.read_thread(session_id, None, 5, cfg, session_blocks.store, session_blocks.events)
    linked = await discord_commands.read_thread(
        session_id, f"https://discord.com/channels/{_GUILD}/{_OTHER_CHANNEL}", 5, cfg, session_blocks.store,
        session_blocks.events)

  persons = {m["id"]: m["person"] for m in own["messages"]}
  assert persons[_mid(1)] == _PERSON
  assert persons[_mid(2)] is None
  # The --url readback carries person the same way.
  assert linked["messages"][0]["person"] is None


# ---------------------------------------------------------------------------
# Read: a linked channel, and the refusals
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_read_with_url_reads_the_linked_channel_and_marks_nothing(tmp_path: pathlib.Path) -> None:
  """--url reads the newest *limit* of the linked channel, all unread false, nothing acked."""
  cfg, session_blocks, client = _rig(tmp_path, channels={_OTHER_CHANNEL: [_message(i, f"m{i}") for i in range(1, 6)]})
  session_id = await _make_session(session_blocks)
  url = f"https://discord.com/channels/{_GUILD}/{_OTHER_CHANNEL}"

  with mock.patch(conftest.DISCORD_LISTENER_BOT_CLIENT_PATCH_TARGET, return_value=client):
    result = await discord_commands.read_thread(session_id, url, 3, cfg, session_blocks.store, session_blocks.events)

  assert [m["id"] for m in result["messages"]] == [_mid(i) for i in (3, 4, 5)]
  assert all(m["unread"] is False for m in result["messages"])
  assert result["watermark_id"] is None
  assert result["more_unread"] == 0
  # Nothing was marked: the session's watermark is untouched and no ack landed.
  meta = await session_blocks.store.get_session(session_id)
  assert meta is not None and metadata_slots.fields_of(meta, "discord").discord_watermark_id is None
  assert _ack_events(session_blocks, session_id) == []
  # Only the linked channel was read, newest-three shape (no ``after``).
  assert client.calls == [("get_messages", {"channel_id": _OTHER_CHANNEL, "after": None, "limit": 3})]


@pytest.mark.asyncio
async def test_read_with_url_refuses_hidden_channel_and_bad_link(tmp_path: pathlib.Path) -> None:
  """A 403 from Discord answers 404 'cannot see'; a non-discord.com link answers 422."""
  cfg, session_blocks, client = _rig(tmp_path, hidden=(_OTHER_CHANNEL,))
  session_id = await _make_session(session_blocks)

  with (
      mock.patch(conftest.DISCORD_LISTENER_BOT_CLIENT_PATCH_TARGET, return_value=client),
      conftest.make_internal_router_client(cfg, session_blocks) as http,
  ):
    hidden = http.post(
        "/api/internal/discord/read",
        json={
            "session_id": session_id,
            "url": f"https://discord.com/channels/{_GUILD}/{_OTHER_CHANNEL}"
        })
    bad_link = http.post("/api/internal/discord/read", json={"session_id": session_id, "url": "https://x.com/no"})

  assert hidden.status_code == 404
  assert hidden.json()["detail"] == "the bot cannot see this channel"
  assert bad_link.status_code == 422
  assert "not a discord.com channel/message link" in bad_link.json()["detail"]


@pytest.mark.asyncio
async def test_read_on_a_non_discord_session_answers_409(tmp_path: pathlib.Path) -> None:
  """A session without a Discord thread refuses with 409 before any Discord call."""
  cfg, session_blocks, client = _rig(tmp_path)
  meta = await conftest.create_root_session(session_blocks, models.CreateSessionRequest(name="plain"))

  with (
      mock.patch(conftest.DISCORD_LISTENER_BOT_CLIENT_PATCH_TARGET, return_value=client),
      conftest.make_internal_router_client(cfg, session_blocks) as http,
  ):
    resp = http.post("/api/internal/discord/read", json={"session_id": meta.id})

  assert resp.status_code == 409
  assert resp.json()["detail"] == "Session has no Discord thread"
  assert client.calls == []


# ---------------------------------------------------------------------------
# Reply: the stale-thread gate around the read
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_reply_refuses_412_while_unread_then_posts_after_a_read(tmp_path: pathlib.Path) -> None:
  """The reply gate fires before any post; the read marks the thread and the reply goes through."""
  cfg, session_blocks, client = _rig(
      tmp_path, channels={
          _THREAD: [_message(1, "first follow up"), _message(2, "second follow up")],
          _PARENT: []
      })
  session_id = await _make_session(session_blocks)

  with (
      mock.patch(conftest.DISCORD_LISTENER_BOT_CLIENT_PATCH_TARGET, return_value=client),
      conftest.make_internal_router_client(cfg, session_blocks) as http,
  ):
    refused = http.post("/api/internal/discord/reply", json={"session_id": session_id, "text": "the answer"})
    assert refused.status_code == 412
    detail = refused.json()["detail"]
    assert detail["error"] == "stale_thread"
    assert [m["id"] for m in detail["new_messages"]] == [_mid(1), _mid(2)]
    assert client.posts == []

    read = http.post("/api/internal/discord/read", json={"session_id": session_id})
    assert read.status_code == 200
    assert read.json()["more_unread"] == 0

    posted = http.post("/api/internal/discord/reply", json={"session_id": session_id, "text": "the answer"})
    assert posted.status_code == 200
    assert posted.json()["posted"] is True

  assert [p["content"] for p in client.posts] == ["the answer"]
  assert [p["channel_id"] for p in client.posts] == [_THREAD]


# ---------------------------------------------------------------------------
# Check: the bot token's setup
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_check_reports_missing_permissions_and_intent_off(tmp_path: pathlib.Path) -> None:
  """A guild missing ADD_REACTIONS names it, the intent off reads false, and ok is false."""
  every_permission_but_reactions = sum(
      discord_client.REQUIRED_PERMISSIONS.values()) - discord_client.REQUIRED_PERMISSIONS["ADD_REACTIONS"]
  cfg, session_blocks, client = _rig(
      tmp_path,
      user={
          "id": _BOT,
          "username": "charlie-bot"
      },
      application={
          "id": "600000000000000009",
          "flags": 0
      },
      guilds=[
          {
              "id": _GUILD,
              "name": "Research",
              "permissions": str(every_permission_but_reactions)
          },
          {
              "id": _OTHER_GUILD,
              "name": "Ops",
              "permissions": str(1 << 3)
          },  # ADMINISTRATOR
      ],
  )

  with (
      mock.patch(conftest.DISCORD_LISTENER_BOT_CLIENT_PATCH_TARGET, return_value=client),
      conftest.make_internal_router_client(cfg, session_blocks) as http,
  ):
    resp = http.post("/api/internal/discord/check", json={})

  assert resp.status_code == 200
  assert resp.json() == {
      "ok":
          False,
      "bot_user": {
          "id": _BOT,
          "username": "charlie-bot"
      },
      "application_id":
          "600000000000000009",
      "message_content_intent":
          False,
      "guilds":
          [
              {
                  "id": _GUILD,
                  "name": "Research",
                  "missing_permissions": ["ADD_REACTIONS"]
              },
              {
                  "id": _OTHER_GUILD,
                  "name": "Ops",
                  "missing_permissions": []
              },
          ],
  }
  assert "test-bot-token" not in json.dumps(resp.json())


@pytest.mark.asyncio
async def test_check_without_a_token_answers_409(tmp_path: pathlib.Path) -> None:
  """No bot token set: 409 naming the key, before any client could be built."""
  conftest.stub_credentials({})
  cfg = config.CharlieBotConfig(charliebot_home=tmp_path / "home", backends=conftest.fake_backends())

  with conftest.make_internal_router_client(cfg, conftest.build_session_blocks(cfg)) as http:
    resp = http.post("/api/internal/discord/check", json={})

  assert resp.status_code == 409
  assert resp.json()["detail"] == "credentials.discord.bot_token is not set"
