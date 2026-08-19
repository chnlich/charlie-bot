"""Acceptance tests for the Slack summon listener (src.core.slack_listener)."""

from __future__ import annotations

import asyncio
import shutil
from pathlib import Path
from typing import Optional
from unittest.mock import AsyncMock, patch

import pytest

from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, SlackOrigin
from src.core.sessions import SessionManager
from src.core.slack_listener import (
  CITATION_BOUNDARY,
  SlackClient,
  _build_summon_prompt,
  ensure_slack_group,
  handle_app_mention,
  summon_session_id,
)

_TS = "1700000000.000100"

# The approved red-line and reply-format texts, read from the same prompts docs
# the builder reads and stripped exactly like the builder, so the tail
# assertions pin exact bytes.
_RED_LINE_PATH = Path(__file__).resolve().parents[1] / "prompts" / "slack_reply_redline.md"
_RED_LINE = _RED_LINE_PATH.read_text(encoding="utf-8").strip()
_FORMAT_PATH = Path(__file__).resolve().parents[1] / "prompts" / "slack_reply_format.md"
_REPLY_FORMAT = _FORMAT_PATH.read_text(encoding="utf-8").strip()


def _spawn_round_tasks() -> list[asyncio.Task]:
  """Reset the task sink used by the patched create_logged_task."""
  _spawn_round_tasks.tasks = []
  return _spawn_round_tasks.tasks


_spawn_round_tasks.tasks: list[asyncio.Task] = []


class _FakeSlackClient:
  """Records calls and fabricates per-input permalinks; never touches the network.

  Implements only what the summon path may call: a regression to reading
  thread content fails here with an AttributeError by construction.
  """

  def __init__(self) -> None:
    self.calls: list[tuple[str, dict]] = []

  async def open_connection(self) -> str:
    self.calls.append(("open_connection", {"channel": None}))
    return "wss://fake.example/socket"

  async def post_message(self, channel: str, text: str, thread_ts: Optional[str] = None) -> dict:
    self.calls.append(("post_message", {"channel": channel, "text": text, "thread_ts": thread_ts}))
    return {"ok": True}

  async def add_reaction(self, channel: str, name: str, ts: str) -> dict:
    self.calls.append(("add_reaction", {"channel": channel, "name": name, "ts": ts}))
    return {"ok": True}

  async def get_permalink(self, channel: str, ts: str) -> str:
    self.calls.append(("get_permalink", {"channel": channel, "ts": ts}))
    return f"https://fake.slack.test/archives/{channel}/p{ts}"

  async def get_channel_name(self, channel_id: str) -> str | None:
    self.calls.append(("get_channel_name", {"channel": channel_id}))
    return f"name-of-{channel_id}"


def _make_event(**overrides: object) -> dict:
  """Build an allowed app_mention event, merging in per-test overrides."""
  base: dict = {
      "type": "app_mention",
      "user": "U_ALLOWED",
      "team": "T_TEST",
      "channel": "C_TEST",
      "ts": _TS,
      "text": "hey",
      "channel_type": "channel",
  }
  base.update(overrides)
  return base


def _cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      slack_bot_token="test-bot-token",
      slack_app_token="test-app-token",
      slack_allowed_user_ids=["U_ALLOWED"],
  )


def _cfg_with_repo(repo_root: Path) -> CharlieBotConfig:
  """A cfg-like object whose charlie_bot_repo points at *repo_root* (real CharlieBotConfig's
  charlie_bot_repo is a derived property tied to the installed package location, so a plain
  namespace stand-in is used to redirect it for these isolated fail-loud tests)."""

  class _Cfg:
    charlie_bot_repo = repo_root

  return _Cfg()  # type: ignore[return-value]


def _thread_ts(event: dict) -> str:
  return event.get("thread_ts") or event["ts"]


def _sid(event: dict) -> str:
  return summon_session_id(event["team"], event["channel"], _thread_ts(event))


def _origin(event: dict) -> SlackOrigin:
  return SlackOrigin(
      team_id=event["team"], channel_id=event["channel"], thread_ts=_thread_ts(event))


@pytest.mark.asyncio
async def test_allowed_user_creates_session_and_persists_agent_message(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  event = _make_event()

  def _spawn(coro, *, name=None):
    task = asyncio.get_running_loop().create_task(coro)
    _spawn_round_tasks.tasks.append(task)
    return task

  with (
      patch("src.core.slack_listener.trigger_master", new=AsyncMock()) as trigger,
      patch("src.core.slack_listener.create_logged_task", side_effect=_spawn),
  ):
    sid = await handle_app_mention(event, cfg, session_mgr, client)
    await asyncio.gather(*_spawn_round_tasks.tasks)

  assert sid == _sid(event)

  meta = await session_mgr.get_session(sid)
  assert meta is not None
  assert meta.slack_origin is not None
  assert meta.slack_origin.team_id == "T_TEST"
  assert meta.slack_origin.channel_id == "C_TEST"
  assert meta.slack_origin.thread_ts == _TS

  # The Slack traffic is exactly one eyes reaction on the mention, one
  # permalink lookup for the mention's own ts, plus one channel-name lookup
  # for the auto-grouping — no thread-content read of any kind, and no posted
  # acceptance message.
  posts = [c for name, c in client.calls if name == "post_message"]
  assert posts == []
  reactions = [c for name, c in client.calls if name == "add_reaction"]
  assert reactions == [{"channel": "C_TEST", "name": "eyes", "ts": _TS}]
  permalinks = [c for name, c in client.calls if name == "get_permalink"]
  assert permalinks == [{"channel": "C_TEST", "ts": _TS}]
  name_lookups = [c for name, c in client.calls if name == "get_channel_name"]
  assert name_lookups == [{"channel": "C_TEST"}]
  assert len(client.calls) == len(reactions) + len(permalinks) + len(name_lookups)

  # The persisted content carries exactly the URL the client produced for this
  # mention, and nothing else Slack-derived beyond the citation boundary.
  expected_url = f"https://fake.slack.test/archives/C_TEST/p{_TS}"

  events = session_mgr.load_chat_events_sync(sid)
  agent_messages = [ev for ev in events if ev.get("type") == ET.AGENT_MESSAGE]
  assert len(agent_messages) == 1
  assert agent_messages[0]["slack"] == {
      "channel_id": "C_TEST",
      "thread_ts": _TS,
      "mention_ts": _TS,
  }
  assert expected_url in agent_messages[0]["content"]
  assert agent_messages[0]["content"].endswith(f"{CITATION_BOUNDARY}\n{_RED_LINE}\n{_REPLY_FORMAT}")

  trigger.assert_awaited_once()
  assert trigger.await_args.kwargs["user_event_id"] == agent_messages[0]["id"]
  assert expected_url in trigger.await_args.args[1]


def test_build_summon_prompt_appends_both_notices_after_the_citation_boundary(tmp_path: Path) -> None:
  """Every Slack prompt ends citation boundary, then the red line, then the reply format."""
  prompt = _build_summon_prompt("https://fake.slack.test/archives/C_TEST/p1700000000.000100", _cfg(tmp_path))
  assert _REPLY_FORMAT in prompt
  assert prompt.index(CITATION_BOUNDARY) < prompt.index(_RED_LINE) < prompt.index(_REPLY_FORMAT)
  assert prompt.endswith(f"{CITATION_BOUNDARY}\n{_RED_LINE}\n{_REPLY_FORMAT}")


def test_build_summon_prompt_rereads_the_docs_on_every_call(tmp_path: Path) -> None:
  """No caching: an edit to a doc between two builds shows up in the second one."""
  shutil.copytree(_RED_LINE_PATH.parent, tmp_path / "prompts")
  red_doc = tmp_path / "prompts" / "slack_reply_redline.md"
  format_doc = tmp_path / "prompts" / "slack_reply_format.md"
  cfg = _cfg_with_repo(tmp_path)
  url = "https://fake.slack.test/archives/C_TEST/p1700000000.000100"

  first = _build_summon_prompt(url, cfg)
  red_doc.write_text("RED LINE VERSION TWO", encoding="utf-8")
  format_doc.write_text("REPLY FORMAT VERSION TWO", encoding="utf-8")
  second = _build_summon_prompt(url, cfg)

  assert first.endswith(f"{CITATION_BOUNDARY}\n{_RED_LINE}\n{_REPLY_FORMAT}")
  assert _RED_LINE not in second and _REPLY_FORMAT not in second
  assert "RED LINE VERSION TWO" in second
  assert "REPLY FORMAT VERSION TWO" in second


def test_build_summon_prompt_missing_red_line_doc_raises_with_path_and_cause(tmp_path: Path) -> None:
  """No embedded-text fallback: a missing doc fails the build, naming the path."""
  cfg = _cfg_with_repo(tmp_path)  # no prompts dir under this repo root
  missing_path = tmp_path / "prompts" / "slack_reply_redline.md"

  with pytest.raises(ValueError) as excinfo:
    _build_summon_prompt("https://fake.slack.test/archives/C_TEST/p1700000000.000100", cfg)

  assert str(missing_path) in str(excinfo.value)
  assert "most likely predates" in str(excinfo.value)


def test_build_summon_prompt_missing_reply_format_doc_raises_with_path_and_cause(tmp_path: Path) -> None:
  """A prompt is never assembled without the reply-format contract either."""
  shutil.copytree(_RED_LINE_PATH.parent, tmp_path / "prompts")
  (tmp_path / "prompts" / "slack_reply_format.md").unlink()
  cfg = _cfg_with_repo(tmp_path)
  missing_path = tmp_path / "prompts" / "slack_reply_format.md"

  with pytest.raises(ValueError) as excinfo:
    _build_summon_prompt("https://fake.slack.test/archives/C_TEST/p1700000000.000100", cfg)

  assert str(missing_path) in str(excinfo.value)
  assert "most likely predates" in str(excinfo.value)


@pytest.mark.asyncio
async def test_same_thread_twice_reuses_the_session(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  event = _make_event()

  with patch("src.core.slack_listener.trigger_master", new=AsyncMock()):
    first = await handle_app_mention(event, cfg, session_mgr, client)
    second = await handle_app_mention(event, cfg, session_mgr, client)

  assert first == second
  sessions = await session_mgr.list_sessions()
  assert len(sessions) == 1
  assert sessions[0].id == first


@pytest.mark.asyncio
async def test_disallowed_user_drops_with_no_side_effects(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  event = _make_event(user="U_OTHER")

  with patch("src.core.slack_listener.trigger_master", new=AsyncMock()):
    result = await handle_app_mention(event, cfg, session_mgr, client)

  assert result is None
  assert client.calls == []
  assert await session_mgr.get_session(_sid(event)) is None


@pytest.mark.asyncio
async def test_non_app_mention_message_drops_with_no_side_effects(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  event = _make_event(type="message")

  with patch("src.core.slack_listener.trigger_master", new=AsyncMock()):
    result = await handle_app_mention(event, cfg, session_mgr, client)

  assert result is None
  assert client.calls == []
  assert await session_mgr.get_session(_sid(event)) is None


@pytest.mark.asyncio
async def test_top_level_mention_uses_own_ts(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  event = _make_event()  # no thread_ts, so the mention's own ts is the thread
  tasks = _spawn_round_tasks()

  def _spawn(coro, *, name=None):
    task = asyncio.get_running_loop().create_task(coro)
    tasks.append(task)
    return task

  with (
      patch("src.core.slack_listener.trigger_master", new=AsyncMock()),
      patch("src.core.slack_listener.create_logged_task", side_effect=_spawn),
  ):
    sid = await handle_app_mention(event, cfg, session_mgr, client)
    await asyncio.gather(*tasks)

  assert sid == summon_session_id("T_TEST", "C_TEST", _TS)
  posts = [c for name, c in client.calls if name == "post_message"]
  assert posts == []
  reactions = [c for name, c in client.calls if name == "add_reaction"]
  assert reactions == [{"channel": "C_TEST", "name": "eyes", "ts": _TS}]


@pytest.mark.asyncio
async def test_reactions_add_failure_still_spawns_the_round(tmp_path: Path) -> None:
  """A failing reactions.add costs only the eyes: the round still spawns and persists."""
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  event = _make_event()

  class _FailingReactionClient(_FakeSlackClient):
    async def add_reaction(self, channel: str, name: str, ts: str) -> dict:
      raise RuntimeError("missing_scope")

  client = _FailingReactionClient()
  tasks = _spawn_round_tasks()

  def _spawn(coro, *, name=None):
    task = asyncio.get_running_loop().create_task(coro)
    tasks.append(task)
    return task

  with (
      patch("src.core.slack_listener.trigger_master", new=AsyncMock()) as trigger,
      patch("src.core.slack_listener.create_logged_task", side_effect=_spawn),
  ):
    sid = await handle_app_mention(event, cfg, session_mgr, client)
    results = await asyncio.gather(*tasks, return_exceptions=True)

  assert sid == _sid(event)
  assert len(tasks) == 2
  trigger.assert_awaited_once()
  failures = [r for r in results if isinstance(r, Exception)]
  assert len(failures) == 1
  assert isinstance(failures[0], RuntimeError)
  assert str(failures[0]) == "missing_scope"

  events = session_mgr.load_chat_events_sync(sid)
  agent_messages = [ev for ev in events if ev.get("type") == ET.AGENT_MESSAGE]
  assert len(agent_messages) == 1


@pytest.mark.asyncio
async def test_archived_session_is_unarchived_not_duplicated(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  event = _make_event()
  sid = _sid(event)
  await session_mgr.create_session(
      CreateSessionRequest(session_id=sid, name="slack-archived", slack_origin=_origin(event)))
  await session_mgr.archive_session(sid)
  assert (await session_mgr.get_session(sid)).status == "archived"

  client = _FakeSlackClient()
  with patch("src.core.slack_listener.trigger_master", new=AsyncMock()):
    result = await handle_app_mention(event, cfg, session_mgr, client)

  assert result == sid
  assert (await session_mgr.get_session(sid)).status == "active"
  sessions = await session_mgr.list_sessions()
  assert len(sessions) == 1
  assert sessions[0].id == sid


@pytest.mark.asyncio
async def test_new_summon_session_is_grouped_by_channel_name(tmp_path: Path) -> None:
  """A fresh summon session (slack_origin set, group empty) lands in `Slack #<name>`,
  and its session name carries the same resolved label."""
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  event = _make_event()

  with patch("src.core.slack_listener.trigger_master", new=AsyncMock()):
    sid = await handle_app_mention(event, cfg, session_mgr, client)

  assert sid == _sid(event)
  meta = await session_mgr.get_session(sid)
  assert meta is not None
  assert meta.group == "Slack #name-of-C_TEST"
  assert meta.name.startswith("Slack #name-of-C_TEST ")


@pytest.mark.asyncio
async def test_unresolvable_channel_name_groups_by_channel_id(tmp_path: Path) -> None:
  """A missing_scope-style resolution failure still groups, as `Slack #<channel_id>`,
  and the mention is still accepted and answered."""
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  event = _make_event()

  class _UnresolvingClient(_FakeSlackClient):
    async def get_channel_name(self, channel_id: str) -> str | None:
      self.calls.append(("get_channel_name", {"channel": channel_id}))
      return None

  client = _UnresolvingClient()
  tasks = _spawn_round_tasks()

  def _spawn(coro, *, name=None):
    task = asyncio.get_running_loop().create_task(coro)
    tasks.append(task)
    return task

  with (
      patch("src.core.slack_listener.trigger_master", new=AsyncMock()) as trigger,
      patch("src.core.slack_listener.create_logged_task", side_effect=_spawn),
  ):
    sid = await handle_app_mention(event, cfg, session_mgr, client)
    await asyncio.gather(*tasks)

  assert sid == _sid(event)
  meta = await session_mgr.get_session(sid)
  assert meta is not None
  assert meta.group == "Slack #C_TEST"
  assert meta.name.startswith("Slack #C_TEST ")
  trigger.assert_awaited_once()
  events = session_mgr.load_chat_events_sync(sid)
  agent_messages = [ev for ev in events if ev.get("type") == ET.AGENT_MESSAGE]
  assert len(agent_messages) == 1


@pytest.mark.asyncio
async def test_existing_group_is_never_overwritten(tmp_path: Path) -> None:
  """A repeat mention on a session with a non-empty group does not call set_group."""
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  event = _make_event()
  sid = _sid(event)
  await session_mgr.create_session(
      CreateSessionRequest(session_id=sid, name="slack-grouped", slack_origin=_origin(event)))
  await session_mgr.set_group(sid, "Manual Group")

  client = _FakeSlackClient()
  with (
      patch("src.core.slack_listener.trigger_master", new=AsyncMock()),
      patch.object(session_mgr, "set_group", new=AsyncMock()) as set_group,
  ):
    result = await handle_app_mention(event, cfg, session_mgr, client)

  assert result == sid
  set_group.assert_not_awaited()
  assert (await session_mgr.get_session(sid)).group == "Manual Group"


@pytest.mark.asyncio
async def test_unarchived_session_with_empty_group_is_grouped(tmp_path: Path) -> None:
  """The unarchive path groups an empty-group session exactly like the create path."""
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  event = _make_event()
  sid = _sid(event)
  await session_mgr.create_session(
      CreateSessionRequest(session_id=sid, name="slack-archived", slack_origin=_origin(event)))
  await session_mgr.archive_session(sid)

  client = _FakeSlackClient()
  with patch("src.core.slack_listener.trigger_master", new=AsyncMock()):
    result = await handle_app_mention(event, cfg, session_mgr, client)

  assert result == sid
  meta = await session_mgr.get_session(sid)
  assert meta is not None
  assert meta.status == "active"
  assert meta.group == "Slack #name-of-C_TEST"


@pytest.mark.asyncio
async def test_set_group_failure_does_not_break_handle_app_mention(tmp_path: Path) -> None:
  """A set_group failure is logged and swallowed: the mention is still accepted and answered."""
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  event = _make_event()

  with (
      patch("src.core.slack_listener.trigger_master", new=AsyncMock()),
      patch.object(
          session_mgr, "set_group", new=AsyncMock(side_effect=RuntimeError("disk full"))),
  ):
    sid = await handle_app_mention(event, cfg, session_mgr, client)

  assert sid == _sid(event)
  assert (await session_mgr.get_session(sid)).group is None
  events = session_mgr.load_chat_events_sync(sid)
  agent_messages = [ev for ev in events if ev.get("type") == ET.AGENT_MESSAGE]
  assert len(agent_messages) == 1


@pytest.mark.asyncio
async def test_ensure_slack_group_skips_sessions_without_slack_origin(tmp_path: Path) -> None:
  """The label form of ensure_slack_group writes nothing when the session has no slack_origin."""
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="web-session"))

  with patch.object(session_mgr, "set_group", new=AsyncMock()) as set_group:
    await ensure_slack_group(session_mgr, meta.id, "Slack #name-of-C_TEST")

  set_group.assert_not_awaited()


class _StubHttp:
  """Minimal httpx.AsyncClient stand-in for SlackClient.get_channel_name tests."""

  def __init__(self, payload: dict) -> None:
    self.payload = payload
    self.gets: list[dict] = []

  async def get(self, url: str, *, headers: dict, params: dict):
    self.gets.append({"url": url, "params": params})

    class _Resp:
      def __init__(self, body: dict) -> None:
        self._body = body

      def raise_for_status(self) -> None:
        return None

      def json(self) -> dict:
        return self._body

    return _Resp(self.payload)


@pytest.mark.asyncio
async def test_get_channel_name_resolves_and_caches(tmp_path: Path) -> None:
  http = _StubHttp({"ok": True, "channel": {"name": "general"}})
  client = SlackClient(http, bot_token="test-bot-token", app_token="test-app-token")  # type: ignore[arg-type]

  assert await client.get_channel_name("C_TEST") == "general"
  assert await client.get_channel_name("C_TEST") == "general"
  assert http.gets == [{
      "url": "https://slack.com/api/conversations.info",
      "params": {"channel": "C_TEST"},
  }]


@pytest.mark.asyncio
async def test_get_channel_name_missing_scope_caches_none(tmp_path: Path) -> None:
  """A failure resolves to None without raising and is cached for the process lifetime."""
  http = _StubHttp({"ok": False, "error": "missing_scope"})
  client = SlackClient(http, bot_token="test-bot-token", app_token="test-app-token")  # type: ignore[arg-type]

  assert await client.get_channel_name("C_TEST") is None
  assert await client.get_channel_name("C_TEST") is None
  assert len(http.gets) == 1


@pytest.mark.asyncio
async def test_trigger_master_forwards_user_event_id(tmp_path: Path) -> None:
  from src.core import master_trigger

  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="t"))

  with patch.object(master_trigger, "run_message", new=AsyncMock(return_value=None)) as run_mock:
    await master_trigger.trigger_master(meta.id, "s", cfg, session_mgr, user_event_id="evt-1")
    await master_trigger.trigger_master(meta.id, "s", cfg, session_mgr)

  assert run_mock.call_count == 2
  assert run_mock.await_args_list[0].kwargs["user_event_id"] == "evt-1"
  assert run_mock.await_args_list[1].kwargs["user_event_id"] is None