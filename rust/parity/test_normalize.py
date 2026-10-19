"""Normalization tests: a fake's recording compares only the headers its feature code sets explicitly, and the
stdin of each fake-agent start normalizes like any other recorded text.
"""

import json
import pathlib

import pytest

from rust.parity import fake_agent, normalize, parity


def _context() -> normalize.Context:
  return normalize.Context(tree_root="/tmp/charliebot-parity-synthetic/base")


def test_library_default_headers_are_left_out_of_slack_requests() -> None:
  headers = {
      "authorization": ["Bearer xoxb-parity-synthetic-token"],
      "host": ["slack.com"],
      "user-agent": ["python-httpx/0.28.1"],
  }
  compared = normalize.request_headers("slack", headers, _context(), websocket=False)
  assert compared == {"authorization": ["Bearer xoxb-parity-synthetic-token"]}


def test_content_type_compares_media_type_so_a_form_body_differs_from_json() -> None:
  json_request = normalize.request_headers(
      "slack", {"content-type": ["Application/JSON; charset=utf-8"]}, _context(), websocket=False)
  form_request = normalize.request_headers(
      "slack", {"content-type": ["application/x-www-form-urlencoded"]}, _context(), websocket=False)
  assert json_request == {"content-type": ["application/json"]}
  assert form_request != json_request


def test_discord_user_agent_is_compared_on_rest_only() -> None:
  headers = {"user-agent": ["DiscordBot (https://discord.com, charlie-bot)"]}
  assert normalize.request_headers("discord", headers, _context(), websocket=False) == headers
  assert normalize.request_headers("discord", headers, _context(), websocket=True) == {}


def test_unnamed_request_header_fails_fast() -> None:
  with pytest.raises(ValueError, match="'x-unexpected' is neither set explicitly"):
    normalize.request_headers("gemini", {"x-unexpected": ["1"]}, _context(), websocket=True)


def _spawn_side(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, label: str, stdin: bytes) -> parity.Side:
  """A side whose only fake-agent start is the spawn record the fake writes for *stdin*."""
  # The record reads the run token from the environment; a test run inside an agent session must not carry one.
  monkeypatch.delenv("CHARLIEBOT_RUN_TOKEN", raising=False)
  root = tmp_path / label
  (root / "fake-agent").mkdir(parents=True)
  record = fake_agent._spawn_record(0, "claude", scripted=True, stdin_bytes=stdin)
  (root / "fake-agent" / "spawns.jsonl").write_text(json.dumps(record) + "\n", encoding="utf-8")
  return parity.Side(label, parity.servers.Tree(label=label, root=root), port=18600)


def test_stdin_naming_only_the_tree_root_compares_equal(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  # Both tree roots have one length, as the harness gives them, so the stdin_bytes counts agree.
  baseline = _spawn_side(tmp_path, monkeypatch, "base", f"Read {tmp_path / 'base'}/home/notes.md".encode())
  candidate = _spawn_side(tmp_path, monkeypatch, "cand", f"Read {tmp_path / 'cand'}/home/notes.md".encode())
  assert parity._spawns_diff([baseline, candidate], expect_starts=1) == []


def test_stdin_text_difference_names_the_stdin_field(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  baseline = _spawn_side(tmp_path, monkeypatch, "base", b"Summarize the notes.")
  candidate = _spawn_side(tmp_path, monkeypatch, "cand", b"Summarize the draft.")
  lines = parity._spawns_diff([baseline, candidate], expect_starts=1)
  assert len(lines) == 1
  assert lines[0].startswith("fake-agent spawns $[0].stdin:")


PROXY_ROUTE = "POST /api/anthropic-proxy/openai-compatible/cc-glm52/v1/messages"
MINTED_ID = b'"id":"msg_0123456789abcdef0123456789abcdef"'


def test_a_proxy_route_masks_the_message_id_it_minted_in_json_and_in_a_stream() -> None:
  answer = b'{"id":"msg_0123456789abcdef0123456789abcdef","type":"message"}'
  frame = b'event: message_start\ndata: {"type":"message_start","message":{"id":"msg_0123456789abcdef0123456789abcdef"}}'
  assert normalize.mask_proxy_message_ids(answer, PROXY_ROUTE) == b'{"id":"<proxy-message-id>","type":"message"}'
  assert b'"message":{"id":"<proxy-message-id>"}' in normalize.mask_proxy_message_ids(frame, PROXY_ROUTE)


def test_an_upstream_supplied_id_stays_visible_on_the_proxy_route() -> None:
  upstream_id = b'{"id":"chatcmpl-synthetic","type":"message"}'
  quoted_text = b'{"type":"text_delta","text":"see \\"id\\":\\"msg_0123456789abcdef0123456789abcdef\\""}'
  assert normalize.mask_proxy_message_ids(upstream_id, PROXY_ROUTE) == upstream_id
  assert normalize.mask_proxy_message_ids(quoted_text, PROXY_ROUTE) == quoted_text


def test_the_rule_applies_to_the_proxy_routes_only() -> None:
  assert normalize.mask_proxy_message_ids(MINTED_ID, "GET /api/sessions/{session_id}/bootstrap") == MINTED_ID
