"""Acceptance tests for the Discord summon listener (src.features.discord.discord_listener)."""

from __future__ import annotations

import asyncio
import datetime
import hashlib
import json
import pathlib
import time
from unittest import mock

import conftest
import httpx
import pytest

from src.features.chat_threads import thread_entry
from src.features.discord import discord_client, discord_listener, event_types, metadata, turn_contribution
from src.infra import config, metadata_slots, models
from src.infra import event_types as ET
from src.runtime import triggers

_GUILD = "900000000000000001"
_PARENT = "900000000000000002"
_THREAD = "900000000000000003"
_MENTION = "900000000000000004"
_DM_CHANNEL = "900000000000000005"
_USER = "700000000000000001"
_OTHER = "700000000000000002"
_BOT_USER = "600000000000000001"


class FakeDiscordClient:
  """Recording Discord REST double for discord_listener tests; never touches the network.

  Every call lands in ``calls`` as ``(method, kwargs)`` — the completeness
  assertions built on it fail on any call a path was not expected to make.
  ``channels`` answers get_channel, a started thread is fabricated with
  ``thread_id``, and ``thread`` is what the paged readback serves (sliced by
  ``after`` and the limit, oldest first, the shape
  DiscordClient.get_messages returns). Implements only what the listener paths
  may call; a regression to reading anything else fails here with an
  AttributeError by construction.
  """

  def __init__(
      self,
      *,
      channels: dict[str, dict] | None = None,
      thread: list[dict] | None = None,
      thread_id: str = _THREAD,
  ) -> None:
    self.calls: list[tuple[str, dict]] = []
    self.channels = channels or {}
    self.thread = thread or []
    self.thread_id = thread_id
    self.posts: list[dict] = []
    self._nonce_messages: dict[str, dict] = {}
    self.nonce_deduplication = True
    self.raise_after_accept_once = False
    self.started_threads: list[dict] = []
    self.reactions: dict[tuple[str, str], set[str]] = {}
    self._bot_user_id = "bot-user"

  async def get_channel(self, channel_id: str) -> dict:
    self.calls.append(("get_channel", {"channel_id": channel_id}))
    return self.channels[channel_id]

  async def start_thread_from_message(
      self, channel_id: str, message_id: str, name: str, *, auto_archive_duration: int = 10080) -> dict:
    payload = {
        "channel_id": channel_id,
        "message_id": message_id,
        "name": name,
        "auto_archive_duration": auto_archive_duration,
    }
    self.calls.append(("start_thread_from_message", payload))
    self.started_threads.append(dict(payload))
    return {"id": self.thread_id, "name": name, "type": 11}

  async def create_message(self, channel_id: str, content: str, *, post_key: str) -> dict:
    nonce = hashlib.sha256(post_key.encode("utf-8")).hexdigest()[:25]
    body = {
        "channel_id": channel_id,
        "content": content,
        "nonce": nonce,
        "enforce_nonce": True,
    }
    self.calls.append(("create_message", body))
    existing = self._nonce_messages.get(nonce) if self.nonce_deduplication else None
    if existing is not None:
      return existing
    timestamp_ms = int(time.time() * 1000) + 1000 + len(self.posts)
    message_id = str((timestamp_ms - 1420070400000) << 22)
    post = {
        "id": message_id,
        "channel_id": channel_id,
        "content": content,
        "nonce": nonce,
        "author": {
            "id": self._bot_user_id,
            "bot": True
        },
    }
    self.posts.append({"channel_id": channel_id, "content": content})
    self._nonce_messages[nonce] = post
    self.thread.append({key: value for key, value in post.items() if key != "nonce"})
    if self.raise_after_accept_once:
      self.raise_after_accept_once = False
      raise TimeoutError("read timed out after Discord accepted the post")
    return post

  async def get_current_user(self) -> dict:
    self.calls.append(("get_current_user", {}))
    return {"id": self._bot_user_id}

  async def add_reaction(self, channel_id: str, message_id: str, emoji: str) -> None:
    self.calls.append(("add_reaction", {"channel_id": channel_id, "message_id": message_id, "emoji": emoji}))
    self.reactions.setdefault((channel_id, message_id), set()).add(emoji)

  async def remove_own_reaction(self, channel_id: str, message_id: str, emoji: str) -> None:
    self.calls.append(("remove_own_reaction", {"channel_id": channel_id, "message_id": message_id, "emoji": emoji}))
    self.reactions.setdefault((channel_id, message_id), set()).discard(emoji)

  async def get_messages(self, channel_id: str, *, after: str | None = None, limit: int = 100) -> list[dict]:
    self.calls.append(("get_messages", {"channel_id": channel_id, "after": after, "limit": limit}))
    floor = discord_client.snowflake_key(after) if after is not None else 0
    return [m for m in self.thread if discord_client.snowflake_key(m["id"]) > floor][:limit]


def _build_cfg(tmp_path: pathlib.Path) -> config.CharlieBotConfig:
  """CharlieBotConfig for discord tests: the home dir lives under tmp_path so each test owns its own
  tree, and the stubbed test token plus the single allowed user id wire the delivery and listener
  paths under src.features.discord.discord_listener."""
  conftest.stub_credentials({"discord": {"bot_token": "test-bot-token"}})
  return config.CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      discord={"allowed_users": {
          _USER: "tester"
      }},
      backends=conftest.fake_backends(),
  )


def _rig(
    tmp_path: pathlib.Path,
    *,
    channels: dict[str, dict] | None = None,
    thread: list[dict] | None = None,
) -> tuple[config.CharlieBotConfig, conftest.SessionBlocks, triggers.TriggerManager, FakeDiscordClient]:
  """Discord rig: cfg, managers, and session home rooted at tmp_path, plus the recording fake client."""
  cfg = _build_cfg(tmp_path)
  cfg.sessions_dir.mkdir(parents=True)  # the task tree's index reads it, so an empty home still has it
  session_blocks = conftest.build_session_blocks(cfg)
  tree = conftest.build_task_tree(cfg, session_blocks)
  return cfg, session_blocks, triggers.TriggerManager(cfg, tree), FakeDiscordClient(channels=channels, thread=thread)


def _summon_rig(
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
    *,
    channels: dict[str, dict] | None = None,
    thread: list[dict] | None = None,
) -> tuple[config.CharlieBotConfig, conftest.SessionBlocks, triggers.TriggerManager, FakeDiscordClient]:
  """_rig for a summon that creates its session: the summon opens its manager root through the deps
  task-tree singleton, so this binds a tree over the rig's session blocks."""
  cfg, session_blocks, trigger_mgr, client = _rig(tmp_path, channels=channels, thread=thread)
  conftest.bind_deps_blocks(monkeypatch, session_blocks.tree, session_blocks)
  return cfg, session_blocks, trigger_mgr, client


def _message(**overrides: object) -> dict:
  """Build an allowed mention message in the parent channel, merging in per-test overrides."""
  base: dict = {
      "id": _MENTION,
      "guild_id": _GUILD,
      "channel_id": _PARENT,
      "author": {
          "id": _USER
      },
      "type": 0,
      "content": f"<@{_BOT_USER}> plan the release",
      "mentions": [{
          "id": _BOT_USER
      }],
  }
  base.update(overrides)
  return base


async def _drain(tasks: list[asyncio.Task]) -> None:
  """Await the round and ack tasks the patched create_logged_task captured, then reset the sink."""
  await asyncio.gather(*tasks)
  tasks.clear()


# ---------------------------------------------------------------------------
# Summon
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_text_channel_summon_starts_thread_and_session(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, trigger_mgr, client = _summon_rig(
      tmp_path, monkeypatch, channels={_PARENT: {
          "id": _PARENT,
          "type": 0,
          "name": "general"
      }})
  tasks: list[asyncio.Task] = []

  with conftest.mention_seam(tasks) as trigger:
    sid = await discord_listener.handle_message_create(
        _message(), cfg, *conftest.thread_blocks(session_blocks), client, trigger_mgr, bot_user_id=_BOT_USER)
    await _drain(tasks)

  assert sid == discord_listener.summon_session_id(_GUILD, _THREAD)

  meta = await session_blocks.store.get_session(sid)
  assert meta is not None
  assert metadata_slots.fields_of(meta, "discord").discord_origin == metadata.DiscordOrigin(
      guild_id=_GUILD, parent_channel_id=_PARENT, thread_id=_THREAD)
  assert meta.name.startswith("Discord #general ")
  assert meta.group == "Discord #general"

  # The thread is named from the mention's content minus its mention token,
  # and the only Discord traffic is the thread start plus the 👀 ack on the
  # mention — no posted acceptance message. The completeness assertions below
  # fail on any other client call.
  assert client.started_threads == [
      {
          "channel_id": _PARENT,
          "message_id": _MENTION,
          "name": "plan the release",
          "auto_archive_duration": 10080,
      }
  ]
  reactions = [c for name, c in client.calls if name == "add_reaction"]
  assert reactions == [{"channel_id": _PARENT, "message_id": _MENTION, "emoji": "👀"}]
  assert not [name for name, _ in client.calls if name == "create_message"]
  assert client.reactions[(_PARENT, _MENTION)] == {"👀"}

  events = session_blocks.events.load_chat_events_sync(sid)
  agent_messages = [ev for ev in events if ev.get("type") == ET.AGENT_MESSAGE]
  assert len(agent_messages) == 1
  assert agent_messages[0]["discord"] == {
      "guild_id": _GUILD,
      "channel_id": _PARENT,
      "thread_id": _THREAD,
      "mention_id": _MENTION,
  }
  assert agent_messages[0]["content"] == discord_listener.DISCORD.summon_line
  trigger.assert_awaited_once()
  assert trigger.await_args.kwargs["input_id"] == agent_messages[0]["id"]
  assert trigger.await_args.args[1] == discord_listener.DISCORD.summon_line


@pytest.mark.asyncio
async def test_duplicate_discord_message_create_accepts_one_thread_summon(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, trigger_mgr, client = _summon_rig(
      tmp_path,
      monkeypatch,
      channels={
          _THREAD: {
              "id": _THREAD,
              "type": 11,
              "parent_id": _PARENT,
          },
          _PARENT: {
              "id": _PARENT,
              "type": 0,
              "name": "general",
          },
      })
  message = _message(channel_id=_THREAD)
  thread_reads = 0
  both_read = asyncio.Event()
  original_get_channel = client.get_channel

  async def synchronize_channel(channel_id: str) -> dict:
    nonlocal thread_reads
    if channel_id == _THREAD:
      thread_reads += 1
      if thread_reads == 2:
        both_read.set()
      await both_read.wait()
    return await original_get_channel(channel_id)

  client.get_channel = synchronize_channel
  tasks: list[asyncio.Task] = []
  with conftest.mention_seam(tasks) as trigger:
    pair = await asyncio.gather(
        *[
            discord_listener.handle_message_create(
                message, cfg, *conftest.thread_blocks(session_blocks), client, trigger_mgr, bot_user_id=_BOT_USER)
            for _ in range(2)
        ])
    for _ in range(2):
      assert await discord_listener.handle_message_create(
          message, cfg, *conftest.thread_blocks(session_blocks), client, trigger_mgr, bot_user_id=_BOT_USER) == pair[0]
    await _drain(tasks)

  assert pair[0] == pair[1]
  sid = pair[0]
  assert sid is not None
  summons = [
      ev for ev in session_blocks.tree.fact_history(sid) if ev.get("type") == ET.AGENT_MESSAGE and "discord" in ev
  ]
  assert len(summons) == 1
  assert summons[0]["request_id"] == f"discord-mention:{_MENTION}"
  trigger.assert_awaited_once()
  assert len([call for method, call in client.calls if method == "add_reaction"]) == 1
  assert not client.started_threads


def test_contract_part_names_the_read_command_and_the_summon_mark() -> None:
  """The Discord contract part states the server-side read and flags the pending summon."""
  part = discord_listener.thread_contract_part()
  assert f"Platform: Discord. Reply command: `{discord_listener._REPLY_COMMAND} --file <path>`" in part
  assert f"用 `{discord_listener._READ_COMMAND}` 读这条线程" in part
  assert "`summon: true`" in part
  assert f"`{discord_listener._FOLLOW_TRIGGER_PREFIX} floor=<id>`" in part
  # Both read rules ride the part: the summon round's read and the wake's read.
  assert "读这条线程" in part
  assert "读线程里的新消息" in part


@pytest.mark.asyncio
async def test_thread_summon_binds_the_thread_and_labels_from_the_parent(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, trigger_mgr, client = _summon_rig(
      tmp_path,
      monkeypatch,
      channels={
          _THREAD: {
              "id": _THREAD,
              "type": 11,
              "name": "release-thread",
              "parent_id": _PARENT
          },
          _PARENT: {
              "id": _PARENT,
              "type": 0,
              "name": "general"
          },
      },
  )
  tasks: list[asyncio.Task] = []

  with conftest.mention_seam(tasks):
    sid = await discord_listener.handle_message_create(
        _message(channel_id=_THREAD),
        cfg,
        *conftest.thread_blocks(session_blocks),
        client,
        trigger_mgr,
        bot_user_id=_BOT_USER)
    await _drain(tasks)

  assert sid == discord_listener.summon_session_id(_GUILD, _THREAD)
  meta = await session_blocks.store.get_session(sid)
  assert meta is not None
  assert metadata_slots.fields_of(meta, "discord").discord_origin == metadata.DiscordOrigin(
      guild_id=_GUILD, parent_channel_id=_PARENT, thread_id=_THREAD)
  assert meta.name.startswith("Discord #general ")
  assert meta.group == "Discord #general"
  # No thread was started: the mention's own channel is the thread, and the
  # block names it as both the holding channel and the thread.
  assert client.started_threads == []
  events = session_blocks.events.load_chat_events_sync(sid)
  agent_messages = [ev for ev in events if ev.get("type") == ET.AGENT_MESSAGE]
  assert agent_messages[0]["discord"] == {
      "guild_id": _GUILD,
      "channel_id": _THREAD,
      "thread_id": _THREAD,
      "mention_id": _MENTION,
  }


@pytest.mark.asyncio
async def test_second_summon_reuses_and_unarchives(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, trigger_mgr, client = _summon_rig(
      tmp_path, monkeypatch, channels={_PARENT: {
          "id": _PARENT,
          "type": 0,
          "name": "general"
      }})
  tasks: list[asyncio.Task] = []

  with conftest.mention_seam(tasks):
    first = await discord_listener.handle_message_create(
        _message(), cfg, *conftest.thread_blocks(session_blocks), client, trigger_mgr, bot_user_id=_BOT_USER)
    await _drain(tasks)
    second = await discord_listener.handle_message_create(
        _message(), cfg, *conftest.thread_blocks(session_blocks), client, trigger_mgr, bot_user_id=_BOT_USER)
    await _drain(tasks)
    await session_blocks.lifecycle.archive_session(first)
    third = await discord_listener.handle_message_create(
        _message(), cfg, *conftest.thread_blocks(session_blocks), client, trigger_mgr, bot_user_id=_BOT_USER)
    await _drain(tasks)

  assert first == second == third
  sessions = await session_blocks.listing.list_sessions()
  assert len(sessions) == 1
  meta = await session_blocks.store.get_session(first)
  assert meta is not None and meta.status == models.SessionStatus.ARCHIVED
  summon_events = [
      ev for ev in session_blocks.events.load_chat_events_sync(first) if ev.get("type") == ET.AGENT_MESSAGE
  ]
  assert len(summon_events) == 1


@pytest.mark.asyncio
async def test_disallowed_user_and_bot_author_create_nothing(tmp_path: pathlib.Path) -> None:
  cfg, session_blocks, trigger_mgr, client = _rig(
      tmp_path, channels={_PARENT: {
          "id": _PARENT,
          "type": 0,
          "name": "general"
      }})

  with conftest.mention_seam():
    disallowed = await discord_listener.handle_message_create(
        _message(author={"id": _OTHER}),
        cfg,
        *conftest.thread_blocks(session_blocks),
        client,
        trigger_mgr,
        bot_user_id=_BOT_USER)
    # A bot-flagged author drops even when its id is on the allow-list.
    bot = await discord_listener.handle_message_create(
        _message(author={
            "id": _USER,
            "bot": True
        }),
        cfg,
        *conftest.thread_blocks(session_blocks),
        client,
        trigger_mgr,
        bot_user_id=_BOT_USER)

  assert disallowed is None and bot is None
  assert not client.calls
  assert await session_blocks.listing.list_sessions() == []


@pytest.mark.asyncio
async def test_allowed_dm_mention_gets_the_notice_only(tmp_path: pathlib.Path) -> None:
  cfg, session_blocks, trigger_mgr, client = _rig(tmp_path)
  message = {
      "id": _MENTION,
      "channel_id": _DM_CHANNEL,
      "author": {
          "id": _USER
      },
      "type": 0,
      "content": f"<@{_BOT_USER}> hello",
      "mentions": [{
          "id": _BOT_USER
      }],
  }

  with conftest.mention_seam():
    sid = await discord_listener.handle_message_create(
        message, cfg, *conftest.thread_blocks(session_blocks), client, trigger_mgr, bot_user_id=_BOT_USER)

  assert sid is None
  assert client.posts == [{"channel_id": _DM_CHANNEL, "content": discord_listener._DM_NOTICE}]
  assert not [name for name, _ in client.calls if name == "add_reaction"]
  assert await session_blocks.listing.list_sessions() == []


# ---------------------------------------------------------------------------
# Thread follow
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_unmentioned_message_arms_follow_and_compares_ids_as_integers(tmp_path: pathlib.Path) -> None:
  cfg, session_blocks, trigger_mgr, client = _rig(tmp_path)
  meta = await _discord_session(session_blocks)
  # A 3-digit watermark: string order would flip it against the 19-digit
  # message id below, so arming at all proves the comparison went through
  # snowflake_key.
  metadata_slots.set_fields(meta, "discord", discord_watermark_id="999")
  await session_blocks.store.save_metadata(meta)
  message_id = "1000000000000000100"
  message = _message(id=message_id, channel_id=_THREAD, content="the follow-up", mentions=[])

  try:
    sid = await discord_listener.handle_message_create(
        message, cfg, *conftest.thread_blocks(session_blocks), client, trigger_mgr, bot_user_id=_BOT_USER)

    armed = [
        t for t in await trigger_mgr.list_triggers(meta.id)
        if t.status == models.TriggerStatus.PENDING and t.message.startswith("discord-thread-follow")
    ]
    assert sid == meta.id
    assert len(armed) == 1
    # The label is the one machine-read line; the rules ride the session's
    # managed instructions.
    assert armed[0].message == f"discord-thread-follow floor={message_id}"
    # The arm reads no thread content: the wake does that when it fires.
    assert not client.calls
  finally:
    conftest.shut_down_trigger_tasks(trigger_mgr)


@pytest.mark.asyncio
async def test_archived_session_revives_and_arms_on_an_unmentioned_message(tmp_path: pathlib.Path) -> None:
  """An archived thread session's eligible unmentioned message revives it: the
  unarchive precedes the arm, the session is ACTIVE again, and the follow
  trigger is armed exactly as for an active session (the revived session
  returns to the Threads view)."""
  cfg, session_blocks, trigger_mgr, client = _rig(tmp_path)
  meta = await _discord_session(session_blocks)
  await session_blocks.lifecycle.archive_session(meta.id)
  message = _message(id="1000000000000000100", channel_id=_THREAD, content="the follow-up", mentions=[])

  try:
    sid = await discord_listener.handle_message_create(
        message, cfg, *conftest.thread_blocks(session_blocks), client, trigger_mgr, bot_user_id=_BOT_USER)

    armed = [
        t for t in await trigger_mgr.list_triggers(meta.id)
        if t.status == models.TriggerStatus.PENDING and t.message.startswith("discord-thread-follow")
    ]
    assert sid == meta.id
    revived = await session_blocks.store.get_session(meta.id)
    assert revived is not None and revived.status == models.SessionStatus.ACTIVE
    assert len(armed) == 1
  finally:
    conftest.shut_down_trigger_tasks(trigger_mgr)


# ---------------------------------------------------------------------------
# Round side
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_read_eligible_pages_two_calls_and_drops_bots(tmp_path: pathlib.Path) -> None:
  cfg, _session_blocks, _trigger_mgr, client = _rig(tmp_path)
  human = [
      {
          "id": f"500000000000000{i:03d}",
          "author": {
              "id": _USER
          },
          "content": f"m{i}",
          "type": 0
      } for i in range(101)
  ]
  bot = {"id": "50000000000000000050", "author": {"id": _OTHER, "bot": True}, "content": "noise", "type": 0}
  client.thread = sorted([*human, bot], key=lambda m: discord_client.snowflake_key(m["id"]))
  adapter = discord_listener.DiscordThreadAdapter(client)
  origin = metadata.DiscordOrigin(guild_id=_GUILD, parent_channel_id=_PARENT, thread_id=_THREAD)

  messages = await adapter.read_eligible(origin, cfg)

  assert [m.id for m in messages] == [m["id"] for m in human]
  assert all(m.user == _USER for m in messages)
  calls = [c for name, c in client.calls if name == "get_messages"]
  assert calls == [
      {
          "channel_id": _THREAD,
          "after": "0",
          "limit": 100
      },
      # The second page continues after the first page's last id (human[99],
      # the 100th oldest); the short second page stops the paging.
      {
          "channel_id": _THREAD,
          "after": human[99]["id"],
          "limit": 100
      },
  ]


async def _discord_session(session_blocks: conftest.SessionBlocks) -> models.SessionMetadata:
  """Create the session bound to the test thread and return its metadata."""
  return await conftest.create_root_session(
      session_blocks,
      models.CreateSessionRequest(
          session_id=discord_listener.summon_session_id(_GUILD, _THREAD),
          name="discord session",
          discord_origin=metadata.DiscordOrigin(guild_id=_GUILD, parent_channel_id=_PARENT, thread_id=_THREAD)))


def _wire_client(text: str) -> tuple[discord_client.DiscordClient, list[httpx.Request]]:
  """The real REST client over an in-memory transport, plus the wire requests it records.

  The recorded request shows whether a post went out as JSON or as a multipart upload.
  """
  requests: list[httpx.Request] = []

  def handler(request: httpx.Request) -> httpx.Response:
    requests.append(request)
    if request.url.path == "/api/v10/users/@me":
      return httpx.Response(200, json={"id": "bot-user"})
    if request.method == "GET":
      return httpx.Response(200, json=[])
    message_id = str((int(time.time() * 1000) - 1420070400000 + 1000) << 22)
    return httpx.Response(200, json={"id": message_id, "channel_id": _THREAD, "content": text})

  client = discord_client.DiscordClient(
      httpx.AsyncClient(transport=httpx.MockTransport(handler)), bot_token="test-bot-token")
  return client, requests


@pytest.mark.asyncio
async def test_post_reply_posts_a_published_url_byte_identical(tmp_path: pathlib.Path) -> None:
  """A reply whose page link is the published URL goes out exactly as written."""
  cfg, session_blocks, _trigger_mgr, _client = _rig(tmp_path)
  sid = (await _discord_session(session_blocks)).id
  text = f"see {conftest.PUBLISH_BASE_URL}/Ab3dEf6hIj8kLm1nOp2q/page.html for details"
  client, requests = _wire_client(text)
  with mock.patch(conftest.DISCORD_LISTENER_BOT_CLIENT_PATCH_TARGET, return_value=client):
    readback = await discord_listener.post_reply(
        sid, text, cfg, session_blocks.store, session_blocks.events, tree=session_blocks.tree)

  assert len(requests) == 3
  request = next(request for request in requests if request.method == "POST")
  assert (request.method, str(request.url)) == ("POST", f"{discord_client.BASE_URL}/channels/{_THREAD}/messages")
  assert request.headers["content-type"] == "application/json"
  body = json.loads(request.content)
  assert body["content"] == text
  assert len(body["nonce"]) == 25
  assert body["enforce_nonce"] is True
  assert readback["text"] == text
  assert readback["operator_only_note"] is None


@pytest.mark.asyncio
async def test_discord_accepted_post_timeout_retries_with_same_nonce_once(tmp_path: pathlib.Path) -> None:
  cfg, session_blocks, _trigger_mgr, client = _rig(tmp_path)
  sid = (await _discord_session(session_blocks)).id
  client.raise_after_accept_once = True

  with (
      mock.patch(conftest.DISCORD_LISTENER_BOT_CLIENT_PATCH_TARGET, return_value=client),
      mock.patch("src.features.chat_threads.thread_entry._RETRY_DELAYS", (0.0, 0.0)),
  ):
    result = await discord_listener.post_reply(
        sid,
        "accepted then timed out",
        cfg,
        session_blocks.store,
        session_blocks.events,
        request_id="discord-timeout",
        run_started_at=datetime.datetime.now(datetime.UTC),
        tree=session_blocks.tree,
    )

  assert result["posted"] is True
  assert len(client.posts) == 1
  calls = [body for method, body in client.calls if method == "create_message"]
  assert len(calls) == 2
  assert calls[0]["nonce"] == calls[1]["nonce"]
  assert all(body["enforce_nonce"] is True for body in calls)
  assert all("nonce" not in message for message in client.thread)


@pytest.mark.asyncio
async def test_discord_existing_content_blocks_rerun_without_nonce_deduplication(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, _trigger_mgr, client = _rig(tmp_path)
  sid = (await _discord_session(session_blocks)).id
  client.nonce_deduplication = False
  started_at = datetime.datetime.now(datetime.UTC) - datetime.timedelta(minutes=10)
  original_save = session_blocks.events.save_chat_event
  failed = False

  async def fail_reply_write(session_id: str, event: dict) -> None:
    nonlocal failed
    if not failed and event.get("type") == event_types.DISCORD_REPLY:
      failed = True
      raise RuntimeError("injected event write failure")
    await original_save(session_id, event)

  monkeypatch.setattr(session_blocks.events, "save_chat_event", fail_reply_write)
  with mock.patch(conftest.DISCORD_LISTENER_BOT_CLIENT_PATCH_TARGET,
                  return_value=client), pytest.raises(RuntimeError, match="injected event write failure"):
    await discord_listener.post_reply(
        sid,
        "persist after post",
        cfg,
        session_blocks.store,
        session_blocks.events,
        request_id="discord-write",
        run_started_at=started_at,
        tree=session_blocks.tree)

  monkeypatch.setattr(session_blocks.events, "save_chat_event", original_save)
  with mock.patch(conftest.DISCORD_LISTENER_BOT_CLIENT_PATCH_TARGET, return_value=client):
    result = await discord_listener.post_reply(
        sid,
        "persist after post",
        cfg,
        session_blocks.store,
        session_blocks.events,
        request_id="discord-write",
        run_started_at=started_at,
        tree=session_blocks.tree)

  assert result["posted"] is True
  assert len(client.posts) == 1
  assert len(
      [ev for ev in session_blocks.events.load_chat_events_sync(sid) if ev.get("type") == event_types.DISCORD_REPLY
      ]) == 1
  assert all("nonce" not in message for message in client.thread)


@pytest.mark.asyncio
async def test_post_reply_names_an_application_route_link_for_the_operator(tmp_path: pathlib.Path) -> None:
  """A server-port application-route link posts as written and rides the operator-alone note."""
  cfg, session_blocks, _trigger_mgr, _client = _rig(tmp_path)
  sid = (await _discord_session(session_blocks)).id
  route_url = f"http://127.0.0.1:{cfg.server.port}/diff"
  text = f"open {conftest.PUBLISH_BASE_URL}/Ab3dEf6hIj8kLm1nOp2q/page.html and {route_url}"
  client, requests = _wire_client(text)
  with mock.patch(conftest.DISCORD_LISTENER_BOT_CLIENT_PATCH_TARGET, return_value=client):
    readback = await discord_listener.post_reply(
        sid, text, cfg, session_blocks.store, session_blocks.events, tree=session_blocks.tree)

  body = json.loads(next(request for request in requests if request.method == "POST").content)
  assert body["content"] == text
  assert len(body["nonce"]) == 25
  assert body["enforce_nonce"] is True
  assert readback["operator_only_note"] is not None and route_url in readback["operator_only_note"]


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "link_template, named_path",
    [
        # This server's port: the shape the old rewrite matched.
        ("http://localhost:{port}/absolute_filepath/home/u/artifacts/page.html", "/home/u/artifacts/page.html"),
        # Portless: the shape the rewrite silently let through to the password page.
        ("https://charliebot.example/absolute_filepath/home/u/artifacts/page.html", "/home/u/artifacts/page.html"),
        # Percent-encoded: the command names the decoded path.
        (
            "https://charliebot.example/absolute_filepath/home/u/caf%C3%A9/page.html?q=1#frag",
            "/home/u/café/page.html",
        ),
    ])
async def test_post_reply_refuses_422_and_posts_nothing_on_a_file_server_link(
    tmp_path: pathlib.Path, link_template: str, named_path: str) -> None:
  cfg, session_blocks, _trigger_mgr, client = _rig(tmp_path)
  sid = (await _discord_session(session_blocks)).id
  link = link_template.format(port=cfg.server.port)

  with (
      mock.patch(conftest.DISCORD_LISTENER_BOT_CLIENT_PATCH_TARGET, return_value=client),
      pytest.raises(thread_entry.ThreadReplyError) as excinfo,
  ):
    await discord_listener.post_reply(
        sid, f"see {link} for details", cfg, session_blocks.store, session_blocks.events, tree=session_blocks.tree)

  assert excinfo.value.status == 422
  assert link in excinfo.value.detail
  assert f"charliebot publish {named_path}" in excinfo.value.detail
  assert client.posts == []
  assert not [
      ev for ev in session_blocks.events.load_chat_events_sync(sid) if ev.get("type") == event_types.DISCORD_REPLY
  ]


@pytest.mark.asyncio
async def test_after_turn_audits_the_round_on_the_bound_blocks_and_persists_the_nudge(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The funnel's hook reads the session's log through the block accessors and writes the nudge event there."""
  cfg, session_blocks, _trigger_mgr, _client = _rig(tmp_path)
  block = {"guild_id": _GUILD, "channel_id": _PARENT, "thread_id": _THREAD, "mention_id": _MENTION}
  meta = await conftest.create_root_session(
      session_blocks,
      models.CreateSessionRequest(
          name="discord session",
          discord_origin=metadata.DiscordOrigin(guild_id=_GUILD, parent_channel_id=_PARENT, thread_id=_THREAD)))
  conftest.bind_deps_blocks(monkeypatch, session_blocks.tree, session_blocks)
  summon = {
      "type": ET.AGENT_MESSAGE,
      "content": "Discord summon; post the reply with `charliebot discord reply --file <path>`.",
      "discord": block,
  }
  await session_blocks.events.save_chat_event(meta.id, summon)
  done = {"type": ET.MASTER_DONE, "exit_code": 0, "still_thinking": False, "input_event_id": summon["id"]}
  await session_blocks.events.save_chat_event(meta.id, done)
  tasks: list[asyncio.Task] = []

  with conftest.mention_seam(tasks):
    await turn_contribution.DiscordTurnContribution().after_turn(meta, done, cfg=cfg)
    await _drain(tasks)

  nudges = [
      ev for ev in session_blocks.events.load_chat_events_sync(meta.id)
      if ev.get("type") == ET.AGENT_MESSAGE and "nudge_of" in (ev.get("discord") or {})
  ]
  assert [nudge["discord"] for nudge in nudges] == [{**block, "nudge_of": summon["id"]}]


@pytest.mark.asyncio
async def test_deliver_done_skips_a_session_without_discord_origin(tmp_path: pathlib.Path) -> None:
  # An empty stub: any client build would fail on the missing token, so False
  # here proves the audit never reached for one.
  conftest.stub_credentials({})
  cfg = config.CharlieBotConfig(charliebot_home=tmp_path / "home", backends=conftest.fake_backends())
  session_blocks = conftest.build_session_blocks(cfg)
  meta = await conftest.create_root_session(session_blocks, models.CreateSessionRequest(name="plain"))
  events_before = session_blocks.events.load_chat_events_sync(meta.id)

  assert await discord_listener.deliver_done(
      meta.id, {
          "type": ET.MASTER_DONE,
          "input_event_id": "e1"
      },
      cfg,
      *conftest.thread_blocks(session_blocks),
      tree=session_blocks.tree) is False
  assert session_blocks.events.load_chat_events_sync(meta.id) == events_before
