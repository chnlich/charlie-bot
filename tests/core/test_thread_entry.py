"""Unit tests of the shared thread core, exercised under a synthetic second platform.

The platform here is "fakechat" with integer-ordered message ids: everything
the shared helpers need from a platform beyond Slack — a different summon
block key, marker keys derived from the name, and an id sort that is not the
string sort — is covered without any Slack fixture.
"""

import asyncio
import dataclasses
import uuid
from collections.abc import Callable, Sequence
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

import pytest
from conftest import (
    THREAD_ENTRY_CREATE_LOGGED_TASK_PATCH_TARGET,
    THREAD_ENTRY_TRIGGER_MASTER_PATCH_TARGET,
    make_home_config,
    make_task_spawner,
)

from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import SessionStatus
from src.core.thread_entry import (
    ThreadAdapter,
    ThreadMessage,
    ThreadPlatform,
    ThreadReplyError,
    accept_summon,
    ack_messages,
    chunk_text,
    consume_mention,
    deliver_done,
    follow_floor,
    follow_message,
    lost_summons,
    newest_thread_input,
    noticed,
    nudged,
    operator_only_note,
    post_reply,
    replied,
    rewrite_file_links,
    unread_after,
)

FAKECHAT = ThreadPlatform(
    name="fakechat",
    display_name="Fakechat",
    reply_event_type="fakechat_reply",
    reply_command="charliebot fakechat reply",
    max_post_chars=2000,
    scope_doc="fakechat_reply_scope.md",
    follow_trigger_prefix="fakechat-thread-follow",
    id_key=int,
    origin_field="fakechat_origin",
    watermark_field="fakechat_watermark_id",
    id_label="id",
    mention_key="mention_id",
    block_keys=("channel_id", "thread_ts", "mention_id"),
    thread_fallback="(channel {channel_id}, thread {thread_ts})",
    attaches_files=True,
)


def _summon(event_id: str) -> dict:
  """A fakechat summon injection as the log holds it."""
  return {"id": event_id, "type": ET.AGENT_MESSAGE, "content": "summon", "fakechat": {"thread": "t1"}}


def test_fakechat_marker_keys_derive_from_the_name() -> None:
  assert FAKECHAT.notice_key == "fakechat_notice"
  assert FAKECHAT.backfill_key == "fakechat_backfill"
  assert FAKECHAT.ack_event_type == "fakechat_ack"
  with pytest.raises(dataclasses.FrozenInstanceError):
    FAKECHAT.name = "other"  # type: ignore[misc]


def test_newest_thread_input_picks_the_newest_fakechat_block() -> None:
  # b1 carries no summon block; s3 carries another platform's block.
  events = [
      {
          "id": "b1",
          "type": ET.AGENT_MESSAGE
      },
      {
          "id": "s1",
          "type": ET.AGENT_MESSAGE,
          "fakechat": {
              "thread": "t1"
          }
      },
      {
          "id": "s2",
          "type": ET.AGENT_MESSAGE,
          "fakechat": {
              "thread": "t1",
              "nudge_of": "s1"
          }
      },
      {
          "id": "s3",
          "type": ET.AGENT_MESSAGE,
          "slack": {
              "thread": "t1"
          }
      },
  ]
  bound = newest_thread_input(FAKECHAT, events, ["b1", "s1", "s2", "s3"])
  assert bound == ("s2", {"thread": "t1", "nudge_of": "s1"})
  assert newest_thread_input(FAKECHAT, events, ["b1"]) is None
  assert newest_thread_input(FAKECHAT, events, ["s3"]) is None


def test_replied_nudged_noticed_read_the_fakechat_shapes() -> None:
  events = [
      {
          "id": "r1",
          "type": "fakechat_reply",
          "fakechat_reply": {
              "answers": "s1"
          }
      },
      {
          "id": "r2",
          "type": "fakechat_reply",
          "fakechat_reply": {
              "answers": "s2"
          }
      },
      {
          "id": "n1",
          "type": ET.AGENT_MESSAGE,
          "fakechat": {
              "nudge_of": "s1"
          }
      },
      {
          "id": "x1",
          "type": ET.ASSISTANT_ERROR,
          "fakechat_notice": {
              ET.INPUT_EVENT_ID: "s1"
          }
      },
  ]
  assert replied(FAKECHAT, events, "s1")
  assert not replied(FAKECHAT, events, "s9")
  assert nudged(FAKECHAT, events, "s1")
  assert not nudged(FAKECHAT, events, "s2")
  assert noticed(FAKECHAT, events, "s1")
  assert not noticed(FAKECHAT, events, "s2")


def test_lost_summons_reports_only_the_unanswered_unowned_unmarked() -> None:
  events = [
      _summon("s1"),
      _summon("s2"),
      _summon("s3"),
      _summon("s4"),
      {
          "id": "d1",
          "type": ET.MASTER_DONE,
          ET.INPUT_EVENT_IDS: ["s1"]
      },
      {
          "id": "m2",
          "type": ET.ASSISTANT_ERROR,
          "fakechat_backfill": {
              ET.INPUT_EVENT_ID: "s2"
          }
      },
  ]
  lost = lost_summons(FAKECHAT, events, owned={"s3"}, running=set())
  assert [ev["id"] for ev in lost] == ["s4"]
  # The running set keeps its summon off the report the same way.
  assert lost_summons(FAKECHAT, events, owned={"s3"}, running={"s4"}) == []
  # A marker under another platform's backfill key marks nothing here.
  events_slack_marked = [
      *events,
      {
          "id": "m3",
          "type": ET.ASSISTANT_ERROR,
          "slack_backfill": {
              ET.INPUT_EVENT_ID: "s4"
          }
      },
  ]
  assert [ev["id"] for ev in lost_summons(FAKECHAT, events_slack_marked, owned={"s3"}, running=set())] == ["s4"]


def test_unread_after_orders_ids_by_the_platform_key() -> None:
  messages = [
      {
          "id": "98",
          "human": True
      },
      {
          "id": "99",
          "human": True
      },
      {
          "id": "100",
          "human": True
      },
      {
          "id": "101",
          "human": False
      },
  ]
  kw = {"eligible": lambda m: m["human"], "message_id": lambda m: m["id"], "id_key": int}
  # Integer order: "100" sorts above the "99" watermark (the string sort would not).
  assert [m["id"] for m in unread_after(messages, watermark="99", **kw)] == ["100"]
  # A None watermark passes every eligible message; the ineligible one still drops.
  assert [m["id"] for m in unread_after(messages, watermark=None, **kw)] == ["98", "99", "100"]
  assert [m["id"] for m in unread_after(messages, watermark="100", **kw)] == []


def test_follow_floor_parses_the_label_floor() -> None:
  label = ("fakechat-thread-follow floor=1700000000.000100\n"
           "Fakechat thread follow: https://fakechat.test/t1")
  assert follow_floor(label) == "1700000000.000100"
  assert follow_floor("no floor on this label") is None
  assert follow_floor("floor=not-numeric") is None


def test_chunk_text_respects_the_limit_and_keeps_the_content() -> None:
  text = "first paragraph\n\nsecond paragraph\n\nthird"
  chunks = chunk_text(text, 20)
  assert all(len(c) <= 20 for c in chunks)
  assert "".join(chunks) == text
  assert chunk_text("short", 20) == ["short"]
  assert chunk_text("x" * 25, 10) == ["x" * 10, "x" * 10, "x" * 5]


def test_rewrite_file_links_swaps_each_link_through_the_swap(tmp_path) -> None:
  cfg = make_home_config(tmp_path)
  page = tmp_path / "page.html"
  page.write_text("<p>hi</p>", encoding="utf-8")
  file_url = f"http://127.0.0.1:{cfg.server.port}/absolute_filepath{page}"
  route_url = f"http://127.0.0.1:{cfg.server.port}/diff"
  text = f"see {file_url}?q=1#frag and open {route_url}"

  out, routes = rewrite_file_links(text, cfg, swap=lambda p: f"published:{p.name}")

  assert out == f"see published:page.html?q=1#frag and open {route_url}"
  assert routes == [route_url]
  note = operator_only_note(routes)
  assert note is not None and route_url in note
  assert operator_only_note([]) is None


def test_rewrite_file_links_refuses_when_the_linked_file_is_gone(tmp_path) -> None:
  cfg = make_home_config(tmp_path)
  gone = tmp_path / "gone.html"
  file_url = f"http://127.0.0.1:{cfg.server.port}/absolute_filepath{gone}"

  with pytest.raises(ThreadReplyError) as excinfo:
    rewrite_file_links(file_url, cfg, swap=lambda p: "never")

  assert excinfo.value.status == 422
  assert file_url in str(excinfo.value.detail)


# ---------------------------------------------------------------------------
# The round side under the fakechat platform: one fake adapter records what
# the shared machinery posts, acks, and persists; nothing Slack-shaped runs.
# ---------------------------------------------------------------------------


class FakeAdapter(ThreadAdapter):
  """The fakechat adapter over an in-memory double: posts and acks record, and
  ``read_eligible`` returns the canned ``thread``."""

  platform = FAKECHAT

  def __init__(self) -> None:
    self.posts: list[tuple[dict, str, list]] = []
    self.acks: list[dict] = []
    self.removed: list[dict] = []
    self.thread: list[ThreadMessage] = []
    self.files: list = []

  async def post(self, address: dict, text: str, files: Sequence) -> None:
    self.posts.append((address, text, list(files)))

  async def add_ack(self, block: dict) -> None:
    self.acks.append(block)

  async def remove_ack(self, block: dict) -> None:
    self.removed.append(block)

  async def thread_link(self, origin) -> str:
    return f"https://fakechat.test/{origin['thread_ts']}"

  def follow_wake_message(self, floor: str, link: str) -> str:
    return f"{self.platform.follow_trigger_prefix} floor={floor}\n{link}"

  async def read_eligible(self, origin, cfg: CharlieBotConfig) -> list[ThreadMessage]:
    return list(self.thread)

  def address_of(self, origin) -> dict:
    return dict(origin)

  def link_swap(self, cfg: CharlieBotConfig) -> tuple[Callable, list]:
    # The attaching swap appends each linked file itself and returns the
    # replacement text; the returned list is what rides the last chunk.
    def swap(fs_path) -> str:
      self.files.append(fs_path)
      return f"published:{fs_path.name}"

    return swap, self.files

  def log_fields(self, address: dict) -> dict:
    return {"channel": address["channel_id"]}


class FakeTriggers:
  """The trigger-manager surface the summon and follow sides touch: armed
  records list, cancels, and creates record; a created record carries the
  label the core built and answers the armed log's ``fire_at`` read."""

  def __init__(self, armed: list | None = None) -> None:
    self.armed = list(armed or [])
    self.cancelled: list[tuple[str, str]] = []
    self.created: list[SimpleNamespace] = []

  async def list_triggers(self, session_id: str) -> list:
    return list(self.armed)

  async def cancel_trigger(self, session_id: str, trigger_id: str) -> None:
    self.cancelled.append((session_id, trigger_id))

  async def create_trigger(
      self,
      session_id: str,
      delay: int,
      message: str,
      *,
      created_at,
      enforce_pending_limit: bool = False,
  ) -> SimpleNamespace:
    record = SimpleNamespace(id=f"tr{len(self.created) + 1}", message=message, fire_at=created_at)
    self.created.append(record)
    return record


class FakeSessions:
  """The session-manager surface the round side and the summon side touch, over
  one in-memory metadata; persisted events land in ``persisted`` for the
  readback asserts and the summon create/group writes land in ``created`` and
  ``groups``. A None *meta* is the no-session-yet state the summon create
  resolves."""

  def __init__(self, meta: SimpleNamespace | None) -> None:
    self.meta = meta
    self.events: list[dict] = []
    self.persisted: list[dict] = []
    self.created: list = []
    self.groups: list[tuple[str, str]] = []

  async def get_session(self, session_id: str) -> SimpleNamespace | None:
    return self.meta

  async def create_session(self, request) -> None:
    self.created.append(request)
    self.meta = SimpleNamespace(
        id=request.session_id,
        name=request.name,
        group=getattr(request, "group", None),
        status=SessionStatus.ACTIVE,
        updated_at="2026-01-01T00:00:00Z",
        fakechat_origin=getattr(request, "fakechat_origin", None),
        fakechat_watermark_id=None,
    )

  async def set_group(self, session_id: str, group: str | None) -> None:
    self.groups.append((session_id, group))

  async def save_metadata(self, meta: SimpleNamespace) -> None:
    pass

  async def persist_and_broadcast(self, session_id: str, event: dict) -> None:
    # The store injects a missing id/timestamp before the append; the round
    # side reads the nudge's id back after persisting, so the double does too.
    event.setdefault("id", str(uuid.uuid4()))
    event.setdefault("timestamp", "2026-01-01T00:00:00Z")
    self.persisted.append(event)

  def load_chat_events_sync(self, session_id: str) -> list[dict]:
    return list(self.events)

  async def read_metadata_fresh(self, session_id: str) -> None:
    return None


def _fake_meta() -> SimpleNamespace:
  """One fakechat thread-bound session: the origin names the thread, no watermark yet."""
  return SimpleNamespace(fakechat_origin={"channel_id": "c1", "thread_ts": "t1"}, fakechat_watermark_id=None)


@pytest.mark.asyncio
async def test_ack_messages_orders_ids_by_the_platform_id_key() -> None:
  adapter = FakeAdapter()
  # "100" sorts above "99" only as an integer; the string sort would pick "99".
  adapter.thread = [ThreadMessage("99", "u", "a"), ThreadMessage("100", "u", "b")]
  sessions = FakeSessions(_fake_meta())

  readback = await ack_messages(adapter, "s1", ["100", "99"], None, sessions)

  assert readback == {"acked": 2, "watermark_id": "100"}
  assert sessions.meta.fakechat_watermark_id == "100"
  ack_event = sessions.persisted[0]
  assert ack_event["type"] == "fakechat_ack"
  assert ack_event["content"] == "Fakechat thread ack: 2 message(s) read through 100"
  assert ack_event["fakechat_ack"] == {"message_ids": ["99", "100"], "watermark_id": "100"}


@pytest.mark.asyncio
async def test_post_reply_carries_the_linked_file_on_the_last_chunk_only(tmp_path) -> None:
  cfg = make_home_config(tmp_path)
  page = tmp_path / "page.html"
  page.write_text("<p>hi</p>", encoding="utf-8")
  file_url = f"http://127.0.0.1:{cfg.server.port}/absolute_filepath{page}"
  # One linked file, but over the 2000-char per-message limit once the URL is
  # rewritten, so the reply splits into several chunks.
  text = ("filler paragraph\n\n" * 150) + f"see {file_url} for details"
  adapter = FakeAdapter()
  sessions = FakeSessions(_fake_meta())

  readback = await post_reply(adapter, "s1", text, cfg, sessions)

  assert len(adapter.posts) > 1
  assert all(files == [] for _, _, files in adapter.posts[:-1])
  assert adapter.posts[-1][2] == [page]
  assert readback["attachments"] == ["page.html"]
  reply_event = sessions.persisted[0]
  assert reply_event["type"] == "fakechat_reply"
  assert reply_event["fakechat_reply"]["attachments"] == ["page.html"]


@pytest.mark.asyncio
async def test_audit_nudge_names_the_platform_and_its_reply_command(tmp_path) -> None:
  cfg = make_home_config(tmp_path)
  adapter = FakeAdapter()
  sessions = FakeSessions(_fake_meta())
  sessions.events = [
      {
          "id": "s1",
          "type": ET.AGENT_MESSAGE,
          "content": "Fakechat summon; post the reply with `charliebot fakechat reply --file <path>`.",
          "fakechat": {
              "channel_id": "c1",
              "thread_ts": "t1",
              "mention_id": "m1"
          },
      }
  ]
  done = {"type": ET.MASTER_DONE, "input_event_id": "s1", "exit_code": 0, "still_thinking": False}
  tasks: list[asyncio.Task] = []

  with (
      patch(THREAD_ENTRY_TRIGGER_MASTER_PATCH_TARGET, new=AsyncMock()) as mock_trigger,
      patch(THREAD_ENTRY_CREATE_LOGGED_TASK_PATCH_TARGET, side_effect=make_task_spawner(tasks)),
  ):
    acted = await deliver_done(adapter, "s1", done, cfg, sessions)
    await asyncio.gather(*tasks)

  assert acted is True
  mock_trigger.assert_awaited_once()
  nudge = sessions.persisted[0]
  assert nudge["type"] == ET.AGENT_MESSAGE
  assert nudge["from_session_name"] == "Fakechat"
  # The link-less summon falls back to the platform's own thread naming.
  assert nudge["content"].startswith("Fakechat thread (channel c1, thread t1): the round answering this mention")
  assert "(no `charliebot fakechat reply` call)" in nudge["content"]
  assert "`charliebot fakechat reply --file <path>`" in nudge["content"]
  assert nudge["fakechat"] == {"channel_id": "c1", "thread_ts": "t1", "mention_id": "m1", "nudge_of": "s1"}


# ---------------------------------------------------------------------------
# The summon and follow side under the fakechat platform: the fake adapter's
# ack/link/wake face and the fake trigger manager record what the shared
# machinery consumes, groups, arms, and fires.
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_consume_mention_advances_the_watermark_by_the_platform_id_key() -> None:
  # Integer order: "100" sorts above the "99" watermark only as an integer; the
  # string sort would leave it unread.
  sessions = FakeSessions(_fake_meta())
  sessions.meta.fakechat_watermark_id = "99"
  triggers = FakeTriggers()

  await consume_mention(FAKECHAT, sessions, triggers, "s1", "100")

  assert sessions.meta.fakechat_watermark_id == "100"
  assert triggers.cancelled == []


@pytest.mark.asyncio
async def test_follow_message_drops_below_the_watermark_and_arms_above_it() -> None:
  adapter = FakeAdapter()
  sessions = FakeSessions(_fake_meta())
  sessions.meta.status = SessionStatus.ACTIVE
  sessions.meta.fakechat_watermark_id = "100"
  triggers = FakeTriggers()
  origin_matches = lambda origin: origin == sessions.meta.fakechat_origin  # noqa: E731  (a one-line guard shape)

  # Integer order: "99" sits below the "100" watermark even though the string sorts above.
  dropped = await follow_message(adapter, sessions, triggers, "s1", "99", origin_matches=origin_matches)
  assert dropped is None
  assert triggers.created == []

  armed = await follow_message(adapter, sessions, triggers, "s1", "101", origin_matches=origin_matches)

  assert armed == "s1"
  assert [rec.message for rec in triggers.created] == ["fakechat-thread-follow floor=101\nhttps://fakechat.test/t1"]


@pytest.mark.asyncio
async def test_accept_summon_creates_the_session_and_spawns_the_round_and_ack() -> None:
  adapter = FakeAdapter()
  sessions = FakeSessions(None)  # no session yet: the summon create resolves it
  triggers = FakeTriggers()
  block = {"channel_id": "c1", "thread_ts": "t1", "mention_id": "m1"}
  tasks: list[asyncio.Task] = []

  with (
      patch(THREAD_ENTRY_TRIGGER_MASTER_PATCH_TARGET, new=AsyncMock()) as mock_trigger,
      patch(THREAD_ENTRY_CREATE_LOGGED_TASK_PATCH_TARGET, side_effect=make_task_spawner(tasks)),
      # The model drops unknown fields again (extra="allow" is gone), so the
      # stand-in records the keyword arguments the shared core passes and the
      # assert reads the platform's origin field off them by name.
      patch("src.core.thread_entry.CreateSessionRequest", lambda **kw: SimpleNamespace(**kw)),
  ):
    sid = await accept_summon(
        adapter,
        None,
        sessions,
        triggers,
        session_id="s1",
        label="Fakechat #c1",
        origin={
            "channel_id": "c1",
            "thread_ts": "t1"
        },
        block=block,
        content="fakechat summon",
        user="u1",
    )
    await asyncio.gather(*tasks)

  assert sid == "s1"
  request = sessions.created[0]
  assert request.name.startswith("Fakechat #c1 ")
  # The origin rides the request under the platform's origin field name.
  assert request.fakechat_origin == {"channel_id": "c1", "thread_ts": "t1"}
  assert sessions.groups == [("s1", "Fakechat #c1")]
  summon_event = sessions.persisted[0]
  assert summon_event["type"] == ET.AGENT_MESSAGE
  assert summon_event["from_session_name"] == "Fakechat"
  assert summon_event["fakechat"] == block
  mock_trigger.assert_awaited_once()
  assert mock_trigger.await_args.args[0] == "s1"
  assert mock_trigger.await_args.kwargs["user_event_id"] == summon_event["id"]
  assert adapter.acks == [block]
  assert sorted(task.get_name() for task in tasks) == ["fakechat-ack-s1", "fakechat-round-s1"]
