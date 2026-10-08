"""Unit tests of the shared thread core, exercised under a synthetic second platform.

The platform here is "fakechat" with integer-ordered message ids: everything
the shared helpers need from a platform beyond Slack — a different summon
block key, marker keys derived from the name, and an id sort that is not the
string sort — is covered without any Slack fixture.
"""

import asyncio
import dataclasses
import uuid
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

import pytest
from conftest import (
    PUBLISH_BASE_URL,
    THREAD_ENTRY_CREATE_LOGGED_TASK_PATCH_TARGET,
    THREAD_ENTRY_TRIGGER_MASTER_PATCH_TARGET,
    make_home_config,
    make_task_spawner,
    thread_blocks,
)

from src.features.chat_threads.thread_entry import (
    ThreadAdapter,
    ThreadMessage,
    ThreadPlatform,
    ThreadReplyError,
    accept_summon,
    ack_messages,
    application_route_links,
    assert_no_file_server_links,
    backfill_followed_threads,
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
    replied,
    unread_after,
)
from src.infra import event_types as ET
from src.infra import metadata_slots
from src.infra.config import CharlieBotConfig
from src.infra.models import SessionStatus

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
)


@pytest.fixture(autouse=True)
def fakechat_metadata_access(monkeypatch: pytest.MonkeyPatch) -> None:
  """Adapt the fake platform's lightweight session double to the slot API."""
  real_fields_of = metadata_slots.fields_of
  real_set_fields = metadata_slots.set_fields

  def fields_of(meta, owner: str):
    if owner == "fakechat":
      return SimpleNamespace(fakechat_origin=meta.fakechat_origin, fakechat_watermark_id=meta.fakechat_watermark_id)
    return real_fields_of(meta, owner)

  def set_fields(meta, owner: str, **values: object) -> None:
    if owner == "fakechat":
      for name, value in values.items():
        setattr(meta, name, value)
      return
    real_set_fields(meta, owner, **values)

  monkeypatch.setattr(metadata_slots, "fields_of", fields_of)
  monkeypatch.setattr(metadata_slots, "set_fields", set_fields)


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


def test_no_file_server_links_refuses_the_first_link_and_names_the_publish_command() -> None:
  link = "http://localhost:18498/absolute_filepath/home/u/caf%C3%A9/page.html"
  later = "https://charliebot.example/absolute_filepath/home/u/other.html"

  with pytest.raises(ThreadReplyError) as excinfo:
    assert_no_file_server_links(f"see {link} or {later}")

  assert excinfo.value.status == 422
  # The first link is named with its query and fragment as written, and the
  # command carries the path the link names, percent-decoded.
  assert link in excinfo.value.detail
  assert "charliebot publish /home/u/café/page.html" in excinfo.value.detail


def test_no_file_server_links_refuses_a_portless_link_the_same_way() -> None:
  link = "https://charliebot.example/absolute_filepath/home/u/page.html"

  with pytest.raises(ThreadReplyError) as excinfo:
    assert_no_file_server_links(f"see {link}")

  assert excinfo.value.status == 422
  assert link in excinfo.value.detail
  assert "charliebot publish /home/u/page.html" in excinfo.value.detail


def test_no_file_server_links_passes_a_published_url_and_names_route_links(tmp_path) -> None:
  cfg = make_home_config(tmp_path)
  published = f"{PUBLISH_BASE_URL}/Ab3dEf6hIj8kLm1nOp2q/page.html"
  route_url = f"http://127.0.0.1:{cfg.server.port}/diff"
  text = f"open {published} and {route_url}"

  assert assert_no_file_server_links(text) is None

  routes = application_route_links(text, cfg)
  assert routes == [route_url]
  note = operator_only_note(routes)
  assert note is not None and route_url in note
  assert operator_only_note([]) is None


# ---------------------------------------------------------------------------
# The round side under the fakechat platform: one fake adapter records what
# the shared machinery posts, acks, and persists; nothing Slack-shaped runs.
# ---------------------------------------------------------------------------


class FakeAdapter(ThreadAdapter):
  """The fakechat adapter over an in-memory double: posts and acks record, and
  ``read_eligible`` returns the canned ``thread``."""

  platform = FAKECHAT

  def __init__(self) -> None:
    self.posts: list[tuple[dict, str]] = []
    self.acks: list[dict] = []
    self.removed: list[dict] = []
    self.thread: list[ThreadMessage] = []

  async def post(self, address: dict, text: str) -> None:
    self.posts.append((address, text))

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

  def log_fields(self, address: dict) -> dict:
    return {"channel": address["channel_id"]}


class FakeTriggers:
  """The trigger-manager surface the summon and follow sides touch: armed
  records list, cancels, and creates record; a created record carries the
  label the core built and answers the armed log's ``fire_at`` read."""

  def __init__(self, armed: list | None = None, order: list[str] | None = None) -> None:
    self.armed = list(armed or [])
    self.cancelled: list[tuple[str, str]] = []
    self.created: list[SimpleNamespace] = []
    # Shared with the sessions double by the revival tests: the append order
    # across the two doubles is the unarchive-before-arm ordering proof.
    self.order = order if order is not None else []

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
    self.order.append("create")
    record = SimpleNamespace(id=f"tr{len(self.created) + 1}", message=message, fire_at=created_at)
    self.created.append(record)
    return record


class FakeTree:
  """The task-tree surface the summon create touches: create_task records its keyword arguments
  (the origin in slot_values among them) and resolves the doubled session."""

  def __init__(self, sessions: FakeSessions) -> None:
    self._sessions = sessions

  async def create_task(self, **kwargs) -> None:
    self._sessions.created.append(SimpleNamespace(**kwargs))
    slot_values = kwargs["slot_values"]
    self._sessions.meta = SimpleNamespace(
        id=kwargs["session_id"],
        name=kwargs["name"],
        group=kwargs["group"],
        status=SessionStatus.ACTIVE,
        updated_at="2026-01-01T00:00:00Z",
        fakechat_origin=slot_values["fakechat_origin"],
        fakechat_watermark_id=None,
    )


class FakeSessions:
  """The store, events, listing, lifecycle and successor surface the round side and the summon side touch,
  over one in-memory metadata (each of those five attributes is the double itself);
  persisted events land in ``persisted`` for the readback asserts and the summon create/group writes land in
  ``created`` and ``groups``. A None *meta* is the no-session-yet state the summon create resolves."""

  def __init__(self, meta: SimpleNamespace | None, order: list[str] | None = None) -> None:
    self.meta = meta
    self.store = self
    self.events = self
    self.listing = self
    self.lifecycle = self
    self.successor = self
    self.chat_events: list[dict] = []
    self.persisted: list[dict] = []
    self.created: list = []
    self.groups: list[tuple[str, str]] = []
    self.unarchived: list[str] = []
    self.broadcasts: list[tuple[str, str | None]] = []
    # Shared with the triggers double by the revival tests: the append order
    # across the two doubles is the unarchive-before-arm ordering proof.
    self.order = order if order is not None else []

  async def get_session(self, session_id: str) -> SimpleNamespace | None:
    return self.meta

  async def unarchive_session(self, session_id: str) -> None:
    self.unarchived.append(session_id)
    if self.meta is not None:
      self.meta.status = SessionStatus.ACTIVE
    self.order.append("unarchive")

  async def broadcast_task_tree_changed(self, session_id: str, event_type: str | None) -> None:
    self.broadcasts.append((session_id, event_type))
    self.order.append("broadcast")

  async def list_sessions_readonly(self, status: SessionStatus | None = None, **_: object) -> tuple[list, dict]:
    if self.meta is None or (status is not None and self.meta.status != status):
      return [], {}
    return [self.meta], {}

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
    return list(self.chat_events)

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

  readback = await ack_messages(adapter, "s1", ["100", "99"], None, sessions.store, sessions.events)

  assert readback == {"acked": 2, "watermark_id": "100"}
  assert sessions.meta.fakechat_watermark_id == "100"
  ack_event = sessions.persisted[0]
  assert ack_event["type"] == "fakechat_ack"
  assert ack_event["content"] == "Fakechat thread ack: 2 message(s) read through 100"
  assert ack_event["fakechat_ack"] == {"message_ids": ["99", "100"], "watermark_id": "100"}


@pytest.mark.asyncio
async def test_audit_nudge_names_the_platform_and_its_reply_command(tmp_path) -> None:
  cfg = make_home_config(tmp_path)
  adapter = FakeAdapter()
  sessions = FakeSessions(_fake_meta())
  sessions.chat_events = [
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
    acted = await deliver_done(adapter, "s1", done, cfg, *thread_blocks(sessions))
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
  dropped = await follow_message(
      adapter, sessions.store, sessions.lifecycle, sessions.events, triggers, "s1", "99", origin_matches=origin_matches)
  assert dropped is None
  assert triggers.created == []

  armed = await follow_message(
      adapter,
      sessions.store,
      sessions.lifecycle,
      sessions.events,
      triggers,
      "s1",
      "101",
      origin_matches=origin_matches)

  assert armed == "s1"
  assert [rec.message for rec in triggers.created] == ["fakechat-thread-follow floor=101\nhttps://fakechat.test/t1"]


@pytest.mark.asyncio
async def test_follow_message_revives_an_archived_session_and_arms_after_the_unarchive() -> None:
  adapter = FakeAdapter()
  order: list[str] = []
  sessions = FakeSessions(_fake_meta(), order=order)
  sessions.meta.status = SessionStatus.ARCHIVED
  triggers = FakeTriggers(order=order)
  origin_matches = lambda origin: origin == sessions.meta.fakechat_origin  # noqa: E731  (a one-line guard shape)

  armed = await follow_message(
      adapter,
      sessions.store,
      sessions.lifecycle,
      sessions.events,
      triggers,
      "s1",
      "105",
      origin_matches=origin_matches)

  assert armed == "s1"
  assert sessions.meta.status == SessionStatus.ACTIVE
  assert sessions.unarchived == ["s1"]
  assert sessions.broadcasts == [("s1", "session_unarchived")]
  assert len(triggers.created) == 1
  # The order is forced: create_trigger rejects an archived session, so the
  # unarchive (and its sidebar-refresh broadcast) must land before the arm.
  assert order == ["unarchive", "broadcast", "create"]


# The summon payload every summon-side test sends and reads back in the acks.
_SUMMON_BLOCK = {"channel_id": "c1", "thread_ts": "t1", "mention_id": "m1"}


async def _run_accept_summon(sessions: FakeSessions, adapter: FakeAdapter,
                             tasks: list[asyncio.Task]) -> tuple[str, AsyncMock]:
  """One fakechat summon under the file's standard double set: the trigger-master
  mock, the logged-task spawner, and the field-recording CreateSessionRequest
  stand-in. Drains the spawned round tasks and returns (session id, the awaited
  trigger mock) for the summon-side asserts."""
  with (
      patch(THREAD_ENTRY_TRIGGER_MASTER_PATCH_TARGET, new=AsyncMock()) as mock_trigger,
      patch(THREAD_ENTRY_CREATE_LOGGED_TASK_PATCH_TARGET, side_effect=make_task_spawner(tasks)),
      # The stand-in tree records the keyword arguments the shared core passes
      # and the assert reads the registered origin from slot_values.
      patch("src.features.chat_threads.thread_entry.task_execution.task_manager", return_value=FakeTree(sessions)),
  ):
    sid = await accept_summon(
        adapter,
        None,
        *thread_blocks(sessions),
        FakeTriggers(),
        session_id="s1",
        label="Fakechat #c1",
        origin={
            "channel_id": "c1",
            "thread_ts": "t1"
        },
        block=_SUMMON_BLOCK,
        content="fakechat summon",
        user="u1",
    )
    await asyncio.gather(*tasks)
  return sid, mock_trigger


@pytest.mark.asyncio
async def test_accept_summon_unarchive_broadcasts_the_task_tree_change() -> None:
  sessions = FakeSessions(_fake_meta())
  sessions.meta.status = SessionStatus.ARCHIVED
  tasks: list[asyncio.Task] = []

  sid, _ = await _run_accept_summon(sessions, FakeAdapter(), tasks)

  assert sid == "s1"
  assert sessions.meta.status == SessionStatus.ACTIVE
  assert sessions.unarchived == ["s1"]
  # The mention path's unarchive notifies the sidebar the same way the follow
  # path's does: an open Threads view refetches on the task-tree notification.
  assert sessions.broadcasts == [("s1", "session_unarchived")]


@pytest.mark.asyncio
async def test_backfill_revives_an_archived_session_with_unread_messages() -> None:
  adapter = FakeAdapter()
  adapter.thread = [ThreadMessage("105", "u", "posted while the socket was down")]
  order: list[str] = []
  sessions = FakeSessions(_fake_meta(), order=order)
  sessions.meta.id = "s1"
  sessions.meta.status = SessionStatus.ARCHIVED
  triggers = FakeTriggers(order=order)

  armed_count = await backfill_followed_threads(
      adapter, None, sessions.listing, sessions.lifecycle, sessions.events, triggers)

  assert armed_count == 1
  assert sessions.meta.status == SessionStatus.ACTIVE
  assert sessions.unarchived == ["s1"]
  assert sessions.broadcasts == [("s1", "session_unarchived")]
  assert len(triggers.created) == 1
  assert order == ["unarchive", "broadcast", "create"]


@pytest.mark.asyncio
async def test_backfill_arms_an_active_session_without_a_revival() -> None:
  adapter = FakeAdapter()
  adapter.thread = [ThreadMessage("105", "u", "hello")]
  order: list[str] = []
  sessions = FakeSessions(_fake_meta(), order=order)
  sessions.meta.id = "s1"
  sessions.meta.status = SessionStatus.ACTIVE
  triggers = FakeTriggers(order=order)

  armed_count = await backfill_followed_threads(
      adapter, None, sessions.listing, sessions.lifecycle, sessions.events, triggers)

  assert armed_count == 1
  assert sessions.unarchived == [] and sessions.broadcasts == []
  assert len(triggers.created) == 1
  assert order == ["create"]


@pytest.mark.asyncio
async def test_backfill_leaves_an_archived_session_without_unread_archived() -> None:
  adapter = FakeAdapter()
  order: list[str] = []
  sessions = FakeSessions(_fake_meta(), order=order)
  sessions.meta.id = "s1"
  sessions.meta.status = SessionStatus.ARCHIVED
  triggers = FakeTriggers(order=order)

  armed_count = await backfill_followed_threads(
      adapter, None, sessions.listing, sessions.lifecycle, sessions.events, triggers)

  assert armed_count == 0
  assert sessions.meta.status == SessionStatus.ARCHIVED
  assert sessions.unarchived == [] and sessions.broadcasts == []
  assert triggers.created == []
  assert order == []


@pytest.mark.asyncio
async def test_accept_summon_creates_the_session_and_spawns_the_round_and_ack() -> None:
  adapter = FakeAdapter()
  sessions = FakeSessions(None)  # no session yet: the summon create resolves it
  tasks: list[asyncio.Task] = []

  sid, mock_trigger = await _run_accept_summon(sessions, adapter, tasks)

  assert sid == "s1"
  request = sessions.created[0]
  assert request.name.startswith("Fakechat #c1 ")
  # The summon opens a manager root bound to the deterministic session id, written by the
  # server itself, and the origin rides the create in the platform's slot values.
  assert (request.session_id, request.profile, request.task_parent_id, request.task) == ("s1", "manager", None, None)
  assert request.caller == "system"
  assert request.slot_values["fakechat_origin"] == {"channel_id": "c1", "thread_ts": "t1"}
  assert sessions.groups == [("s1", "Fakechat #c1")]
  summon_event = sessions.persisted[0]
  assert summon_event["type"] == ET.AGENT_MESSAGE
  assert summon_event["from_session_name"] == "Fakechat"
  assert summon_event["fakechat"] == _SUMMON_BLOCK
  mock_trigger.assert_awaited_once()
  assert mock_trigger.await_args.args[0] == "s1"
  assert mock_trigger.await_args.kwargs["input_id"] == summon_event["id"]
  assert adapter.acks == [_SUMMON_BLOCK]
  assert sorted(task.get_name() for task in tasks) == ["fakechat-ack-s1", "fakechat-round-s1"]
