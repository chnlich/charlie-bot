"""Acceptance tests for the Discord summon listener (src.core.discord_listener)."""

from __future__ import annotations

import asyncio
from pathlib import Path
from unittest.mock import patch

import pytest
from conftest import (
    ROOT,
    fake_backends,
    mention_seam,
    shut_down_trigger_tasks,
    stub_credentials,
)

from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.discord_client import snowflake_key
from src.core.discord_listener import (
    _DM_NOTICE,
    _REPLY_COMMAND,
    DiscordThreadAdapter,
    _build_follow_wake_message,
    deliver_done,
    handle_message_create,
    post_reply,
    summon_session_id,
)
from src.core.models import CreateSessionRequest, DiscordOrigin, SessionStatus, TriggerStatus
from src.core.sessions import SessionManager
from src.core.triggers import TriggerManager

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
    self.started_threads: list[dict] = []
    self.reactions: dict[tuple[str, str], set[str]] = {}

  async def get_channel(self, channel_id: str) -> dict:
    self.calls.append(("get_channel", {"channel_id": channel_id}))
    return self.channels[channel_id]

  async def start_thread_from_message(self, channel_id: str, message_id: str, name: str) -> dict:
    self.calls.append(("start_thread_from_message", {"channel_id": channel_id, "message_id": message_id, "name": name}))
    self.started_threads.append({"channel_id": channel_id, "message_id": message_id, "name": name})
    return {"id": self.thread_id, "name": name, "type": 11}

  async def create_message(self, channel_id: str, content: str, *, files=()) -> dict:
    self.calls.append(("create_message", {"channel_id": channel_id, "content": content, "files": list(files)}))
    self.posts.append({"channel_id": channel_id, "content": content, "files": list(files)})
    return {"id": "0", "channel_id": channel_id, "content": content}

  async def add_reaction(self, channel_id: str, message_id: str, emoji: str) -> None:
    self.calls.append(("add_reaction", {"channel_id": channel_id, "message_id": message_id, "emoji": emoji}))
    self.reactions.setdefault((channel_id, message_id), set()).add(emoji)

  async def remove_own_reaction(self, channel_id: str, message_id: str, emoji: str) -> None:
    self.calls.append(("remove_own_reaction", {"channel_id": channel_id, "message_id": message_id, "emoji": emoji}))
    self.reactions.setdefault((channel_id, message_id), set()).discard(emoji)

  async def get_messages(self, channel_id: str, *, after: str | None = None, limit: int = 100) -> list[dict]:
    self.calls.append(("get_messages", {"channel_id": channel_id, "after": after, "limit": limit}))
    floor = snowflake_key(after) if after is not None else 0
    return [m for m in self.thread if snowflake_key(m["id"]) > floor][:limit]


def _build_cfg(tmp_path: Path) -> CharlieBotConfig:
  """CharlieBotConfig for discord tests: the home dir lives under tmp_path so each test owns its own
  tree, and the stubbed test token plus the single allowed user id wire the delivery and listener
  paths under src.core.discord_listener."""
  stub_credentials({"discord": {"bot_token": "test-bot-token"}})
  return CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      discord={"allowed_users": {
          _USER: "tester"
      }},
      backends=fake_backends(),
  )


def _rig(
    tmp_path: Path,
    *,
    channels: dict[str, dict] | None = None,
    thread: list[dict] | None = None,
) -> tuple[CharlieBotConfig, SessionManager, TriggerManager, FakeDiscordClient]:
  """Discord rig: cfg, managers, and session home rooted at tmp_path, plus the recording fake client."""
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  return cfg, session_mgr, TriggerManager(cfg, session_mgr), FakeDiscordClient(channels=channels, thread=thread)


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
async def test_text_channel_summon_starts_thread_and_session(tmp_path: Path) -> None:
  cfg, session_mgr, trigger_mgr, client = _rig(
      tmp_path, channels={_PARENT: {
          "id": _PARENT,
          "type": 0,
          "name": "general"
      }})
  tasks: list[asyncio.Task] = []

  with mention_seam(tasks) as trigger:
    sid = await handle_message_create(_message(), cfg, session_mgr, client, trigger_mgr, bot_user_id=_BOT_USER)
    await _drain(tasks)

  assert sid == summon_session_id(_GUILD, _THREAD)

  meta = await session_mgr.get_session(sid)
  assert meta is not None
  assert meta.discord_origin == DiscordOrigin(guild_id=_GUILD, parent_channel_id=_PARENT, thread_id=_THREAD)
  assert meta.name.startswith("Discord #general ")
  assert meta.group == "Discord #general"

  # The thread is named from the mention's content minus its mention token,
  # and the only Discord traffic is the thread start plus the 👀 ack on the
  # mention — no posted acceptance message. The completeness assertions below
  # fail on any other client call.
  assert client.started_threads == [{
      "channel_id": _PARENT,
      "message_id": _MENTION,
      "name": "plan the release",
  }]
  reactions = [c for name, c in client.calls if name == "add_reaction"]
  assert reactions == [{"channel_id": _PARENT, "message_id": _MENTION, "emoji": "👀"}]
  assert not [name for name, _ in client.calls if name == "create_message"]
  assert client.reactions[(_PARENT, _MENTION)] == {"👀"}

  # The summon is persisted under the platform key with the block, and its
  # content carries the mention-message link plus the reply command the
  # round-end audit gates on.
  events = session_mgr.load_chat_events_sync(sid)
  agent_messages = [ev for ev in events if ev.get("type") == ET.AGENT_MESSAGE]
  assert len(agent_messages) == 1
  assert agent_messages[0]["discord"] == {
      "guild_id": _GUILD,
      "channel_id": _PARENT,
      "thread_id": _THREAD,
      "mention_id": _MENTION,
  }
  link = f"https://discord.com/channels/{_GUILD}/{_PARENT}/{_MENTION}"
  assert agent_messages[0]["content"].startswith(f"Discord 线程召唤：{link}\n\n")
  assert f"Reply command: `{_REPLY_COMMAND} --file <path>`" in agent_messages[0]["content"]

  trigger.assert_awaited_once()
  assert trigger.await_args.kwargs["user_event_id"] == agent_messages[0]["id"]
  assert link in trigger.await_args.args[1]


@pytest.mark.asyncio
async def test_summon_prompt_carries_the_discord_scope_doc_and_not_the_old_boundary(tmp_path: Path) -> None:
  """The tail's scope slot holds the Discord scope doc verbatim; the old fixed citation boundary is gone."""
  cfg, session_mgr, trigger_mgr, client = _rig(
      tmp_path, channels={_PARENT: {
          "id": _PARENT,
          "type": 0,
          "name": "general"
      }})
  tasks: list[asyncio.Task] = []

  with mention_seam(tasks):
    sid = await handle_message_create(_message(), cfg, session_mgr, client, trigger_mgr, bot_user_id=_BOT_USER)
    await _drain(tasks)

  events = session_mgr.load_chat_events_sync(sid)
  content = [ev for ev in events if ev.get("type") == ET.AGENT_MESSAGE][0]["content"]
  scope = (ROOT / "prompts" / "discord_reply_scope.md").read_text(encoding="utf-8").strip()
  red_line = (ROOT / "prompts" / "thread_reply_redline.md").read_text(encoding="utf-8").strip()
  reply_format = (ROOT / "prompts" / "thread_reply_format.md").read_text(encoding="utf-8").strip()
  assert content.endswith(f"{scope}\n{red_line}\n{reply_format}")
  # The old platform-neutral citation boundary no longer rides the Discord prompt.
  assert "已成文的私有内容不引用" not in content


def test_follow_wake_names_the_discord_scope_doc_and_the_shared_docs() -> None:
  """The wake orders the scope doc, the red line, and the reply format re-read before any reply."""
  msg = _build_follow_wake_message("1000000000000000100", f"https://discord.com/channels/{_GUILD}/{_PARENT}")
  assert "prompts/discord_reply_scope.md" in msg
  assert "prompts/thread_reply_redline.md" in msg
  assert "prompts/thread_reply_format.md" in msg


@pytest.mark.asyncio
async def test_thread_summon_binds_the_thread_and_labels_from_the_parent(tmp_path: Path) -> None:
  cfg, session_mgr, trigger_mgr, client = _rig(
      tmp_path,
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

  with mention_seam(tasks):
    sid = await handle_message_create(
        _message(channel_id=_THREAD), cfg, session_mgr, client, trigger_mgr, bot_user_id=_BOT_USER)
    await _drain(tasks)

  assert sid == summon_session_id(_GUILD, _THREAD)
  meta = await session_mgr.get_session(sid)
  assert meta is not None
  assert meta.discord_origin == DiscordOrigin(guild_id=_GUILD, parent_channel_id=_PARENT, thread_id=_THREAD)
  assert meta.name.startswith("Discord #general ")
  assert meta.group == "Discord #general"
  # No thread was started: the mention's own channel is the thread, and the
  # block names it as both the holding channel and the thread.
  assert client.started_threads == []
  events = session_mgr.load_chat_events_sync(sid)
  agent_messages = [ev for ev in events if ev.get("type") == ET.AGENT_MESSAGE]
  assert agent_messages[0]["discord"] == {
      "guild_id": _GUILD,
      "channel_id": _THREAD,
      "thread_id": _THREAD,
      "mention_id": _MENTION,
  }


@pytest.mark.asyncio
async def test_second_summon_reuses_and_unarchives(tmp_path: Path) -> None:
  cfg, session_mgr, trigger_mgr, client = _rig(
      tmp_path, channels={_PARENT: {
          "id": _PARENT,
          "type": 0,
          "name": "general"
      }})
  tasks: list[asyncio.Task] = []

  with mention_seam(tasks):
    first = await handle_message_create(_message(), cfg, session_mgr, client, trigger_mgr, bot_user_id=_BOT_USER)
    await _drain(tasks)
    second = await handle_message_create(_message(), cfg, session_mgr, client, trigger_mgr, bot_user_id=_BOT_USER)
    await _drain(tasks)
    await session_mgr.archive_session(first)
    third = await handle_message_create(_message(), cfg, session_mgr, client, trigger_mgr, bot_user_id=_BOT_USER)
    await _drain(tasks)

  assert first == second == third
  sessions = await session_mgr.list_sessions()
  assert len(sessions) == 1
  meta = await session_mgr.get_session(first)
  assert meta is not None and meta.status == SessionStatus.ACTIVE


@pytest.mark.asyncio
async def test_disallowed_user_and_bot_author_create_nothing(tmp_path: Path) -> None:
  cfg, session_mgr, trigger_mgr, client = _rig(
      tmp_path, channels={_PARENT: {
          "id": _PARENT,
          "type": 0,
          "name": "general"
      }})

  with mention_seam():
    disallowed = await handle_message_create(
        _message(author={"id": _OTHER}), cfg, session_mgr, client, trigger_mgr, bot_user_id=_BOT_USER)
    # A bot-flagged author drops even when its id is on the allow-list.
    bot = await handle_message_create(
        _message(author={
            "id": _USER,
            "bot": True
        }), cfg, session_mgr, client, trigger_mgr, bot_user_id=_BOT_USER)

  assert disallowed is None and bot is None
  assert not client.calls
  assert await session_mgr.list_sessions() == []


@pytest.mark.asyncio
async def test_allowed_dm_mention_gets_the_notice_only(tmp_path: Path) -> None:
  cfg, session_mgr, trigger_mgr, client = _rig(tmp_path)
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

  with mention_seam():
    sid = await handle_message_create(message, cfg, session_mgr, client, trigger_mgr, bot_user_id=_BOT_USER)

  assert sid is None
  assert client.posts == [{"channel_id": _DM_CHANNEL, "content": _DM_NOTICE, "files": []}]
  assert not [name for name, _ in client.calls if name == "add_reaction"]
  assert await session_mgr.list_sessions() == []


# ---------------------------------------------------------------------------
# Thread follow
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_unmentioned_message_arms_follow_and_compares_ids_as_integers(tmp_path: Path) -> None:
  cfg, session_mgr, trigger_mgr, client = _rig(tmp_path)
  meta = await session_mgr.create_session(
      CreateSessionRequest(
          session_id=summon_session_id(_GUILD, _THREAD),
          name="discord session",
          discord_origin=DiscordOrigin(guild_id=_GUILD, parent_channel_id=_PARENT, thread_id=_THREAD)))
  # A 3-digit watermark: string order would flip it against the 19-digit
  # message id below, so arming at all proves the comparison went through
  # snowflake_key.
  meta.discord_watermark_id = "999"
  await session_mgr.save_metadata(meta)
  message_id = "1000000000000000100"
  message = {
      "id": message_id,
      "guild_id": _GUILD,
      "channel_id": _THREAD,
      "author": {
          "id": _USER
      },
      "type": 0,
      "content": "the follow-up",
      "mentions": [],
  }

  try:
    sid = await handle_message_create(message, cfg, session_mgr, client, trigger_mgr, bot_user_id=_BOT_USER)

    armed = [
        t for t in await trigger_mgr.list_triggers(meta.id)
        if t.status == TriggerStatus.PENDING and t.message.startswith("discord-thread-follow")
    ]
    assert sid == meta.id
    assert len(armed) == 1
    assert armed[0].message.startswith(f"discord-thread-follow floor={message_id}\n")
    assert f"https://discord.com/channels/{_GUILD}/{_THREAD}" in armed[0].message
    # The arm reads no thread content: the wake does that when it fires.
    assert not client.calls
  finally:
    shut_down_trigger_tasks(trigger_mgr)


@pytest.mark.asyncio
async def test_archived_session_revives_and_arms_on_an_unmentioned_message(tmp_path: Path) -> None:
  """An archived thread session's eligible unmentioned message revives it: the
  unarchive precedes the arm, the session is ACTIVE again, and the follow
  trigger is armed exactly as for an active session (the revived session
  returns to the Threads view)."""
  cfg, session_mgr, trigger_mgr, client = _rig(tmp_path)
  meta = await session_mgr.create_session(
      CreateSessionRequest(
          session_id=summon_session_id(_GUILD, _THREAD),
          name="discord session",
          discord_origin=DiscordOrigin(guild_id=_GUILD, parent_channel_id=_PARENT, thread_id=_THREAD)))
  await session_mgr.archive_session(meta.id)
  message = {
      "id": "1000000000000000100",
      "guild_id": _GUILD,
      "channel_id": _THREAD,
      "author": {
          "id": _USER
      },
      "type": 0,
      "content": "the follow-up",
      "mentions": [],
  }

  try:
    sid = await handle_message_create(message, cfg, session_mgr, client, trigger_mgr, bot_user_id=_BOT_USER)

    armed = [
        t for t in await trigger_mgr.list_triggers(meta.id)
        if t.status == TriggerStatus.PENDING and t.message.startswith("discord-thread-follow")
    ]
    assert sid == meta.id
    revived = await session_mgr.get_session(meta.id)
    assert revived is not None and revived.status == SessionStatus.ACTIVE
    assert len(armed) == 1
  finally:
    shut_down_trigger_tasks(trigger_mgr)


# ---------------------------------------------------------------------------
# Round side
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_read_eligible_pages_two_calls_and_drops_bots(tmp_path: Path) -> None:
  cfg, _session_mgr, _trigger_mgr, client = _rig(tmp_path)
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
  client.thread = sorted([*human, bot], key=lambda m: snowflake_key(m["id"]))
  adapter = DiscordThreadAdapter(client)
  origin = DiscordOrigin(guild_id=_GUILD, parent_channel_id=_PARENT, thread_id=_THREAD)

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


@pytest.mark.asyncio
async def test_post_reply_uploads_linked_file_on_the_last_chunk(tmp_path: Path) -> None:
  cfg, session_mgr, _trigger_mgr, client = _rig(tmp_path)
  meta = await session_mgr.create_session(
      CreateSessionRequest(
          session_id=summon_session_id(_GUILD, _THREAD),
          name="discord session",
          discord_origin=DiscordOrigin(guild_id=_GUILD, parent_channel_id=_PARENT, thread_id=_THREAD)))
  page = tmp_path / "page.html"
  page.write_text("<p>hi</p>", encoding="utf-8")
  file_url = f"http://127.0.0.1:{cfg.server.port}/absolute_filepath{page}"
  # One linked file, but over the 2000-char per-message limit once the URL is
  # rewritten, so the reply splits into several chunks.
  text = ("filler paragraph\n\n" * 150) + f"see {file_url} for details"

  with patch("src.core.discord_listener._bot_client", return_value=client):
    readback = await post_reply(meta.id, text, cfg, session_mgr)

  assert len(client.posts) > 1
  assert all(p["files"] == [] for p in client.posts[:-1])
  assert client.posts[-1]["files"] == [page]
  # The URL became the bare file name, so the chunk stands on its own.
  assert client.posts[-1]["content"].endswith("see page.html for details")
  assert "http" not in client.posts[-1]["content"]
  assert readback["attachments"] == ["page.html"]
  reply_events = [ev for ev in session_mgr.load_chat_events_sync(meta.id) if ev.get("type") == ET.DISCORD_REPLY]
  assert len(reply_events) == 1
  assert reply_events[0]["discord_reply"]["attachments"] == ["page.html"]


@pytest.mark.asyncio
async def test_deliver_done_skips_a_session_without_discord_origin(tmp_path: Path) -> None:
  # An empty stub: any client build would fail on the missing token, so False
  # here proves the audit never reached for one.
  stub_credentials({})
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", backends=fake_backends())
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="plain"))

  assert await deliver_done(meta.id, {"type": ET.MASTER_DONE, "input_event_id": "e1"}, cfg, session_mgr) is False
  assert session_mgr.load_chat_events_sync(meta.id) == []
