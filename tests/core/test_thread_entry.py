"""Unit tests of the shared thread core, exercised under a synthetic second platform.

The platform here is "fakechat" with integer-ordered message ids: everything
the shared helpers need from a platform beyond Slack — a different summon
block key, marker keys derived from the name, and an id sort that is not the
string sort — is covered without any Slack fixture.
"""

import dataclasses

import pytest
from conftest import make_home_config

from src.core import event_types as ET
from src.core.thread_entry import (
    ThreadPlatform,
    ThreadReplyError,
    chunk_text,
    follow_floor,
    lost_summons,
    newest_thread_input,
    noticed,
    nudged,
    operator_only_note,
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
    follow_trigger_prefix="fakechat-thread-follow",
    id_key=int,
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
  events = [
      {"id": "b1", "type": ET.AGENT_MESSAGE},  # no summon block
      {"id": "s1", "type": ET.AGENT_MESSAGE, "fakechat": {"thread": "t1"}},
      {"id": "s2", "type": ET.AGENT_MESSAGE, "fakechat": {"thread": "t1", "nudge_of": "s1"}},
      {"id": "s3", "type": ET.AGENT_MESSAGE, "slack": {"thread": "t1"}},  # another platform's block
  ]
  bound = newest_thread_input(FAKECHAT, events, ["b1", "s1", "s2", "s3"])
  assert bound == ("s2", {"thread": "t1", "nudge_of": "s1"})
  assert newest_thread_input(FAKECHAT, events, ["b1"]) is None
  assert newest_thread_input(FAKECHAT, events, ["s3"]) is None


def test_replied_nudged_noticed_read_the_fakechat_shapes() -> None:
  events = [
      {"id": "r1", "type": "fakechat_reply", "fakechat_reply": {"answers": "s1"}},
      {"id": "r2", "type": "fakechat_reply", "fakechat_reply": {"answers": "s2"}},
      {"id": "n1", "type": ET.AGENT_MESSAGE, "fakechat": {"nudge_of": "s1"}},
      {"id": "x1", "type": ET.ASSISTANT_ERROR, "fakechat_notice": {ET.INPUT_EVENT_ID: "s1"}},
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
      {"id": "d1", "type": ET.MASTER_DONE, ET.INPUT_EVENT_IDS: ["s1"]},
      {"id": "m2", "type": ET.ASSISTANT_ERROR, "fakechat_backfill": {ET.INPUT_EVENT_ID: "s2"}},
  ]
  lost = lost_summons(FAKECHAT, events, owned={"s3"}, running=set())
  assert [ev["id"] for ev in lost] == ["s4"]
  # The running set keeps its summon off the report the same way.
  assert lost_summons(FAKECHAT, events, owned={"s3"}, running={"s4"}) == []
  # A marker under another platform's backfill key marks nothing here.
  events_slack_marked = [
      *events,
      {"id": "m3", "type": ET.ASSISTANT_ERROR, "slack_backfill": {ET.INPUT_EVENT_ID: "s4"}},
  ]
  assert [ev["id"] for ev in lost_summons(FAKECHAT, events_slack_marked, owned={"s3"}, running=set())] == ["s4"]


def test_unread_after_orders_ids_by_the_platform_key() -> None:
  messages = [
      {"id": "98", "human": True},
      {"id": "99", "human": True},
      {"id": "100", "human": True},
      {"id": "101", "human": False},
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
