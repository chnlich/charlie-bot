"""Normalization tests: a fake's recording compares only the headers its feature code sets explicitly, and the
stdin of each fake-agent start normalizes like any other recorded text.
"""

import json
import pathlib

import pytest

from rust.parity import compare, fake_agent, normalize, parity


def _context() -> normalize.Context:
  return normalize.Context(tree_root="/tmp/charliebot-parity-synthetic/base")


def _cancel_event_rule() -> compare.EventFieldRule:
  return compare.EventFieldRule(
      event_type="run_finished",
      match_fields={"run_id": "synthetic-run"},
      field="outcome",
      allowed_baseline_values=("failed", "interrupted"),
      required_candidate_value="interrupted",
  )


def _cancel_event_delta(events: list[dict]) -> compare.Delta:
  path = "sessions/synthetic-session/data/chat_events.jsonl"
  data = "".join(json.dumps(event) + "\n" for event in events).encode()
  return compare.Delta(added={}, removed=frozenset(), changed={path: compare.Entry("file", data)})


def _cancel_event_diff(baseline: list[dict], candidate: list[dict], candidate_is_python: bool = False) -> list[str]:
  return compare.delta_diff(
      _cancel_event_delta(baseline),
      _cancel_event_delta(candidate),
      normalize.Context(tree_root="/synthetic/base"),
      normalize.Context(tree_root="/synthetic/candidate"),
      ((_cancel_event_rule(), _cancel_event_rule()),),
      candidate_is_python,
  )


def _cancel_events(outcome: str, finish_first: bool, unrelated_text: str = "same") -> list[dict]:
  finished = {
      "type": "run_finished",
      "id": "10000000-0000-4000-8000-000000000001",
      "run_id": "synthetic-run",
      "input_event_ids": ["synthetic-input"],
      "outcome": outcome,
  }
  done = {
      "type": "master_done",
      "id": "10000000-0000-4000-8000-000000000002",
      "input_event_ids": ["synthetic-input"],
      "exit_code": -15,
  }
  unrelated = {
      "type": "assistant",
      "id": "10000000-0000-4000-8000-000000000003",
      "message": {
          "content": [{
              "type": "text",
              "text": unrelated_text
          }]
      },
  }
  if finish_first:
    finished["event_index"], done["event_index"], unrelated["event_index"] = 7, 8, 9
  else:
    done["event_index"], finished["event_index"], unrelated["event_index"] = 7, 8, 9
  return [finished, done, unrelated] if finish_first else [done, finished, unrelated]


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


def test_event_field_rule_accepts_allowed_baseline_and_required_candidate_and_masks_pair_order() -> None:
  differences = _cancel_event_diff(_cancel_events("failed", False), _cancel_events("interrupted", True))
  assert differences == []


def test_event_field_rule_rejects_wrong_candidate_value() -> None:
  differences = _cancel_event_diff(_cancel_events("interrupted", False), _cancel_events("failed", True))
  assert differences
  assert any("candidate" in line for line in differences)


def test_event_field_rule_rejects_baseline_value_outside_allowed_set() -> None:
  differences = _cancel_event_diff(_cancel_events("success", False), _cancel_events("interrupted", True))
  assert differences
  assert any("baseline" in line for line in differences)


def test_event_field_rule_leaves_unrelated_events_in_the_comparison() -> None:
  differences = _cancel_event_diff(
      _cancel_events("failed", False, "baseline unrelated"),
      _cancel_events("interrupted", True, "candidate unrelated"),
  )
  assert any("baseline unrelated" in line and "candidate unrelated" in line for line in differences)


def test_python_vs_python_applies_the_baseline_value_set_to_both_sides() -> None:
  differences = _cancel_event_diff(
      _cancel_events("failed", False), _cancel_events("interrupted", True), candidate_is_python=True)
  assert differences == []


def _cancel_response(outcome: str, extra: str = "same") -> compare.Response:
  body = {"run_id": "synthetic-run", "stop_requested": True, "outcome": outcome, "extra": extra}
  return compare.Response(200, [("content-type", "application/json")], json.dumps(body).encode())


def test_event_field_rule_applies_to_matching_cancel_response_and_keeps_other_fields() -> None:
  differences = compare.response_diff(
      "POST /api/sessions/{session_id}/runs/{run_id}/cancel",
      _cancel_response("failed"),
      _cancel_response("interrupted"),
      normalize.Context(tree_root="/synthetic/base"),
      normalize.Context(tree_root="/synthetic/candidate"),
      event_field_rules=((_cancel_event_rule(), _cancel_event_rule()),),
  )
  assert differences == []


def test_event_field_rule_rejects_wrong_cancel_response_candidate_and_baseline_values() -> None:
  candidate_differences = compare.response_diff(
      "POST /api/sessions/{session_id}/runs/{run_id}/cancel",
      _cancel_response("failed"),
      _cancel_response("failed"),
      normalize.Context(tree_root="/synthetic/base"),
      normalize.Context(tree_root="/synthetic/candidate"),
      event_field_rules=((_cancel_event_rule(), _cancel_event_rule()),),
  )
  assert any("candidate response outcome" in line for line in candidate_differences)
  baseline_differences = compare.response_diff(
      "POST /api/sessions/{session_id}/runs/{run_id}/cancel",
      _cancel_response("success"),
      _cancel_response("interrupted"),
      normalize.Context(tree_root="/synthetic/base"),
      normalize.Context(tree_root="/synthetic/candidate"),
      event_field_rules=((_cancel_event_rule(), _cancel_event_rule()),),
  )
  assert any("baseline response outcome" in line for line in baseline_differences)


def test_event_field_rule_still_compares_unrelated_cancel_response_fields() -> None:
  differences = compare.response_diff(
      "POST /api/sessions/{session_id}/runs/{run_id}/cancel",
      _cancel_response("failed", "baseline extra"),
      _cancel_response("interrupted", "candidate extra"),
      normalize.Context(tree_root="/synthetic/base"),
      normalize.Context(tree_root="/synthetic/candidate"),
      event_field_rules=((_cancel_event_rule(), _cancel_event_rule()),),
  )
  assert any("$.extra" in line for line in differences)


def test_status_and_shape_response_comparison_ignores_json_leaf_values() -> None:
  baseline = compare.Response(
      422,
      [("content-type", "application/json")],
      b'{"detail":[{"type":"int_parsing","loc":["query","limit"],"msg":"baseline"}]}',
  )
  candidate = compare.Response(
      422,
      [("content-type", "application/json")],
      b'{"detail":[{"type":"int_parsing","loc":["query","limit"],"msg":"candidate"}]}',
  )
  assert compare.response_diff(
      "POST /api/sessions/{session_id}/runs",
      baseline,
      candidate,
      _context(),
      normalize.Context(tree_root="/synthetic/candidate"),
      "status_and_shape",
  ) == []


def test_status_and_shape_compares_error_type_and_location() -> None:
  baseline = compare.Response(
      422, [("content-type", "application/json")], b'{"detail":[{"type":"int_parsing","loc":["query","limit"]}]}')
  candidate = compare.Response(
      422, [("content-type", "application/json")], b'{"detail":[{"type":"float_parsing","loc":["path","limit"]}]}')
  differences = compare.response_diff(
      "POST /api/sessions/{session_id}/runs",
      baseline,
      candidate,
      _context(),
      normalize.Context(tree_root="/synthetic/candidate"),
      "status_and_shape",
  )
  assert differences
  assert any("body detail[0] $.type" in line for line in differences)
  assert any("body detail[0] $.loc" in line for line in differences)


def test_status_and_shape_requires_detail_objects_on_both_sides() -> None:
  baseline = compare.Response(422, [("content-type", "application/json")], b'{"error":"baseline"}')
  candidate = compare.Response(422, [("content-type", "application/json")], b'{"error":"candidate"}')
  assert compare.response_diff(
      "POST /api/sessions/{session_id}/runs",
      baseline,
      candidate,
      _context(),
      normalize.Context(tree_root="/synthetic/candidate"),
      "status_and_shape",
  ) == ["body: both sides must be JSON objects with a detail key"]


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
  assert normalize.mask_proxy_message_ids(MINTED_ID, "POST /api/anthropic-proxy/other/v1/messages") == MINTED_ID
