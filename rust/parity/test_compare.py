"""Malformed-input comparison tests: only the status and each error's type and loc count."""

import json

from rust.parity import compare, normalize


def _context() -> normalize.Context:
  return normalize.Context(tree_root="/tmp/charliebot-parity-synthetic/base")


def _response(status: int, body: dict | list | str) -> compare.Response:
  if isinstance(body, str):
    payload = body.encode()
    content_type = "text/plain"
  else:
    payload = json.dumps(body).encode()
    content_type = "application/json"
  return compare.Response(status, [("content-type", content_type)], payload)


def _diff(left: compare.Response, right: compare.Response) -> list[str]:
  return compare.status_and_shape_diff("POST /x", left, right, _context(), _context())


def test_bodies_differing_only_in_msg_match() -> None:
  left = _response(422, {"detail": [{"type": "json_invalid", "loc": ["body", 0], "msg": "a"},]})
  right = _response(422, {"detail": [{"type": "json_invalid", "loc": ["body", 0], "msg": "b"}]})
  assert _diff(left, right) == []


def test_different_type_fails() -> None:
  left = _response(422, {"detail": [{"type": "json_invalid", "loc": ["body"]}]})
  right = _response(422, {"detail": [{"type": "model_attributes_type", "loc": ["body"]}]})
  assert any("detail[0]" in line for line in _diff(left, right))


def test_different_loc_fails() -> None:
  left = _response(422, {"detail": [{"type": "missing", "loc": ["body", "a"]}]})
  right = _response(422, {"detail": [{"type": "missing", "loc": ["body", "b"]}]})
  assert any("detail[0]" in line for line in _diff(left, right))


def test_different_list_length_fails() -> None:
  left = _response(422, {"detail": [{"type": "missing", "loc": ["body"]}]})
  right = _response(422, {"detail": [{"type": "missing", "loc": ["body"]}, {"type": "missing", "loc": ["query"]}]})
  assert any("1 errors != 2 errors" in line for line in _diff(left, right))


def test_different_status_fails() -> None:
  left = _response(422, {"detail": [{"type": "json_invalid", "loc": ["body"]}]})
  right = _response(400, {"detail": [{"type": "json_invalid", "loc": ["body"]}]})
  assert "status: 422 != 400" in _diff(left, right)


def test_non_json_bodies_on_both_sides_match() -> None:
  assert _diff(_response(400, "bad"), _response(400, "worse")) == []


def test_json_on_one_side_only_fails() -> None:
  lines = _diff(_response(400, "bad"), _response(400, {"detail": "x"}))
  assert lines == ["body: JSON on one side only"]


def test_detail_object_on_both_sides_matches_without_the_text() -> None:
  assert _diff(_response(404, {"detail": "one"}), _response(404, {"detail": "two"})) == []


def test_detail_object_on_one_side_only_fails() -> None:
  lines = _diff(_response(404, {"detail": "one"}), _response(404, {"error": "two"}))
  assert lines == ["body: both sides must be JSON objects with a detail key"]


def test_json_objects_without_detail_on_both_sides_fail() -> None:
  lines = _diff(_response(404, {"error": "one"}), _response(404, {"error": "two"}))
  assert lines == ["body: both sides must be JSON objects with a detail key"]


def test_detail_list_against_detail_text_fails_in_both_orders() -> None:
  errors = _response(422, {"detail": [{"type": "json_invalid", "loc": ["body"]}]})
  text = _response(422, {"detail": "invalid json"})
  assert _diff(errors, text) == ["body detail: a list on one side only"]
  assert _diff(text, errors) == ["body detail: a list on one side only"]


NUDGE = compare.EventSelector(event_type="agent_message", match_fields={"from_session_name": "Web reply"})
FINISHED = compare.EventSelector(event_type="run_finished", match_fields={"outcome": "success"})


def _chat_delta(events: list[dict]) -> compare.Delta:
  path = "home/sessions/synthetic-session/data/chat_events.jsonl"
  data = "".join(json.dumps(event) + "\n" for event in events).encode()
  return compare.Delta(added={}, removed=frozenset(), changed={path: compare.Entry("file", data)})


def _chat_events(nudge_first: bool, nudge_id: str = "nudge-id") -> list[dict]:
  nudge = {"type": "agent_message", "id": nudge_id, "from_session_name": "Web reply", "content": "Post the reply."}
  finished = {"type": "run_finished", "id": "finished-id", "outcome": "success", "run_id": "synthetic-run"}
  pair = [nudge, finished] if nudge_first else [finished, nudge]
  events = [{"type": "master_done", "id": "done-id", "exit_code": 0}, *pair, {"type": "web_reply_notice"}]
  for index, event in enumerate(events):
    event["event_index"] = index
  return events


def _ordered_diff(baseline: list[dict], candidate: list[dict], candidate_is_python: bool = False) -> list[str]:
  left, right = (compare.order_adjacent_pair(_chat_delta(events), NUDGE, FINISHED) for events in (baseline, candidate))
  return compare.delta_diff(left, right, _context(), _context(), candidate_is_python=candidate_is_python)


def test_adjacent_pair_compares_equal_in_either_order_with_different_ids() -> None:
  assert _ordered_diff(_chat_events(True, "nudge-1"), _chat_events(False, "nudge-2")) == []
  assert _ordered_diff(_chat_events(False, "nudge-1"), _chat_events(True, "nudge-2")) == []
  assert _ordered_diff(_chat_events(True), _chat_events(False), candidate_is_python=True) == []


def test_adjacent_pair_rule_still_reports_a_difference_outside_the_pair() -> None:
  candidate = _chat_events(False)
  candidate[0]["exit_code"] = 1
  assert any("exit_code" in line for line in _ordered_diff(_chat_events(True), candidate))


def test_adjacent_pair_rule_keeps_a_pair_that_is_not_adjacent() -> None:
  candidate = _chat_events(False)
  candidate.insert(2, {"type": "web_reply", "id": "between"})
  assert _ordered_diff(_chat_events(True), candidate) != []


def test_adjacent_pair_rule_keeps_a_log_without_the_first_event() -> None:
  candidate = [event for event in _chat_events(True) if event["type"] != "agent_message"]
  assert _ordered_diff(_chat_events(True), candidate) != []
