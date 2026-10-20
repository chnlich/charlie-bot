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
NOTICE = compare.EventSelector(event_type="web_reply_notice", match_fields={})
RUN_PATH = "home/sessions/synthetic-session/data/runs/synthetic-run/metadata.json"


def _chat_delta(events: list[dict]) -> compare.Delta:
  path = "home/sessions/synthetic-session/data/chat_events.jsonl"
  data = "".join(json.dumps(event) + "\n" for event in events).encode()
  nudge_ids = [event["id"] for event in events if event["type"] == "agent_message"]
  run = json.dumps({"input_event_ids": nudge_ids}).encode()
  return compare.Delta(
      added={}, removed=frozenset(), changed={
          path: compare.Entry("file", data),
          RUN_PATH: compare.Entry("file", run)
      })


def _chat_events(nudge_first: bool, nudge_id: str = "nudge-id", notice_first: bool = True) -> list[dict]:
  """The audit's writes: the turn's master_done, the nudge and the turn's run_finished, the nudged turn's master_done,
  then the notice and the nudged turn's run_finished. Each pair is written in the order its flag names."""
  nudge = {"type": "agent_message", "id": nudge_id, "from_session_name": "Web reply", "content": "Post the reply."}
  finished = {"type": "run_finished", "id": "finished-id", "outcome": "success", "run_id": "synthetic-run"}
  nudged_done = {"type": "master_done", "id": "nudged-done-id", "exit_code": 0, "input_event_ids": [nudge_id]}
  notice = {"type": "web_reply_notice", "id": "notice-id"}
  nudged_finished = {
      "type": "run_finished",
      "id": "nudged-finished-id",
      "outcome": "success",
      "run_id": "nudged-run",
      "input_event_ids": [nudge_id],
  }
  pair = [nudge, finished] if nudge_first else [finished, nudge]
  tail = [notice, nudged_finished] if notice_first else [nudged_finished, notice]
  events = [{"type": "master_done", "id": "done-id", "exit_code": 0}, *pair, nudged_done, *tail]
  for index, event in enumerate(events):
    event["event_index"] = index
  return events


def _ordered_diff(baseline: list[dict], candidate: list[dict], candidate_is_python: bool = False) -> list[str]:
  """The diff of two chat logs after each one's nudge pair and notice pair are ordered."""
  sides = []
  for events in (baseline, candidate):
    delta = compare.order_adjacent_pair(_chat_delta(events), NUDGE, FINISHED)
    sides.append(compare.order_adjacent_pair(delta, NOTICE, FINISHED))
  left, right = sides
  return compare.delta_diff(left, right, _context(), _context(), candidate_is_python=candidate_is_python)


def test_adjacent_pair_compares_equal_in_either_order_with_different_ids() -> None:
  orders = [(nudge_first, notice_first) for nudge_first in (True, False) for notice_first in (True, False)]
  for left_nudge_first, left_notice_first in orders:
    for right_nudge_first, right_notice_first in orders:
      baseline = _chat_events(left_nudge_first, "nudge-1", left_notice_first)
      candidate = _chat_events(right_nudge_first, "nudge-2", right_notice_first)
      assert _ordered_diff(baseline, candidate) == []
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


def test_adjacent_pair_rule_does_not_mask_event_ids_embedded_in_other_text() -> None:
  baseline = _chat_events(True, "nudge-1")
  candidate = _chat_events(False, "nudge-2")
  baseline[1]["content"] = "reply to nudge-1"
  candidate[2]["content"] = "reply to nudge-2"
  assert _ordered_diff(baseline, candidate) != []


def _event_stream_diff(baseline: list[dict], candidate: list[dict]) -> list[str]:
  event_types = ("improve_iteration_completed", "child_report", "master_done", "improve_failed")
  path = "home/sessions/synthetic-session/data/chat_events.jsonl"
  left = compare.order_event_type_streams_in_delta(_chat_delta(baseline), path, event_types)
  right = compare.order_event_type_streams_in_delta(_chat_delta(candidate), path, event_types)
  return compare.delta_diff(left, right, _context(), _context())


def test_event_type_streams_allow_cross_type_interleaving() -> None:
  iteration_one = {"type": "improve_iteration_completed", "id": "iteration-one", "iteration": 1}
  iteration_two = {"type": "improve_iteration_completed", "id": "iteration-two", "iteration": 2}
  report_one = {"type": "child_report", "id": "report-one", "summary": "first"}
  report_two = {"type": "child_report", "id": "report-two", "summary": "second"}
  manager_done = {"type": "master_done", "id": "manager-done", "exit_code": 0}
  loop_closed = {"type": "improve_failed", "id": "loop-closed", "iterations_completed": 2}
  assistant = {"type": "assistant", "id": "assistant", "content": "manager response"}
  baseline = [iteration_one, report_one, assistant, manager_done, iteration_two, report_two, loop_closed]
  candidate = [iteration_one, report_one, iteration_two, report_two, loop_closed, assistant, manager_done]

  assert _event_stream_diff(baseline, candidate) == []


def test_event_type_streams_keep_each_type_order_and_payload_compared() -> None:
  baseline = [
      {"type": "improve_iteration_completed", "id": "iteration-one", "iteration": 1},
      {"type": "child_report", "id": "report-one", "summary": "first"},
      {"type": "improve_iteration_completed", "id": "iteration-two", "iteration": 2},
      {"type": "child_report", "id": "report-two", "summary": "second"},
  ]
  reordered_iterations = [
      baseline[2],
      baseline[1],
      baseline[0],
      baseline[3],
  ]
  changed_report = list(baseline)
  changed_report[1] = {**changed_report[1], "summary": "changed"}

  assert _event_stream_diff(baseline, reordered_iterations) != []
  assert _event_stream_diff(baseline, changed_report) != []
