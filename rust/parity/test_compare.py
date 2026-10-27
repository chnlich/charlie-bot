"""Malformed-input comparison tests: only the status and each error's type and loc count."""

import json
import pathlib

from rust.parity import compare, normalize, parity, scenario_file


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


REMINDER = compare.EventSelector(event_type="agent_message", match_fields={"from_session_name": "Session status"})


def _reminder_ordered_diff(baseline: list[dict], candidate: list[dict]) -> list[str]:
  """The diff of two chat logs after each one's reminder pair is ordered."""
  sides = [compare.order_adjacent_pair(_chat_delta(events), REMINDER, FINISHED) for events in (baseline, candidate)]
  return compare.delta_diff(sides[0], sides[1], _context(), _context())


def test_adjacent_pair_rule_orders_every_pair_of_a_multi_turn_log() -> None:
  """A multi-turn session's log holds one pair per audited turn, and each side may write each pair its own way."""
  reminder = {"type": "agent_message", "id": "reminder-id", "from_session_name": "Session status"}
  finished = {"type": "run_finished", "id": "finished-id", "outcome": "success"}
  other = {"type": "user", "id": "user-id", "content": "next"}
  for baseline_order, candidate_order in [(True, False), (False, True)]:
    baseline = [reminder, finished, other] if baseline_order else [finished, reminder, other]
    candidate = [reminder, finished, other] if candidate_order else [finished, reminder, other]
    assert _reminder_ordered_diff(baseline, candidate) == [], (baseline_order, candidate_order)


def test_adjacent_pair_rule_orders_two_independent_pairs_of_one_log() -> None:
  first = {"type": "agent_message", "id": "reminder-1", "from_session_name": "Session status"}
  second = {"type": "agent_message", "id": "reminder-2", "from_session_name": "Session status"}
  first_finished = {"type": "run_finished", "id": "finished-1", "outcome": "success"}
  second_finished = {"type": "run_finished", "id": "finished-2", "outcome": "success"}
  gap = {"type": "master_done", "id": "done-2"}
  baseline = [first_finished, first, gap, second, second_finished]
  candidate = [first, first_finished, gap, second_finished, second]
  assert _reminder_ordered_diff(baseline, candidate) == []


def test_adjacent_pair_rule_keeps_two_seconds_around_one_first_unpaired() -> None:
  reminder = {"type": "agent_message", "id": "reminder-id", "from_session_name": "Session status"}
  left = {"type": "run_finished", "id": "finished-1", "outcome": "success"}
  right = {"type": "run_finished", "id": "finished-2", "outcome": "success"}
  assert _reminder_ordered_diff([left, reminder, right], [reminder, left, right]) != []


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


HANDOVER_SCENARIO = "handover-stop-request.yaml"
BASELINE_RUN = "10000000-0000-4000-8000-0000000000a1"
CANDIDATE_RUN = "20000000-0000-4000-8000-0000000000b2"
OTHER_RUN = "30000000-0000-4000-8000-0000000000c3"


def _handover_side(label: str, run_id: str) -> parity.Side:
  """A side whose stop target captured *run_id*, as the scenario's retry step records it."""
  tree = parity.servers.Tree(label=label, root=pathlib.Path(f"/synthetic/{label}"))
  side = parity.Side(label, tree, port=18600, strace=parity.servers.STRACE_BIN)
  side.captures["run_id"] = run_id
  return side


def _handover_pair(run_id: str, finish_first: bool, outcome: str = "interrupted") -> list[dict]:
  """The run_finished and master_done of one stopped run, written in the order its flag names."""
  finished = {
      "type": "run_finished",
      "id": f"finished-{run_id}",
      "actor": "system",
      "run_id": run_id,
      "outcome": outcome,
      "input_event_ids": ["synthetic-input"],
  }
  done = {
      "type": "master_done",
      "id": f"done-{run_id}",
      "exit_code": -9,
      "still_thinking": False,
      "input_event_ids": ["synthetic-input"],
  }
  return [finished, done] if finish_first else [done, finished]


def _handover_log(*pairs: list[dict]) -> list[dict]:
  """An assistant message, then each pair in turn, then a closing assistant message."""
  events = [{"type": "assistant", "id": "before-id", "text": "same"}]
  for pair in pairs:
    events.extend(pair)
  events.append({"type": "assistant", "id": "after-id", "text": "same"})
  return events


def _handover_diff(baseline: list[dict], candidate: list[dict], candidate_is_python: bool = False) -> list[str]:
  """The differences between two chat logs of handover-stop-request, with the scenario's own comparison rules."""
  scenario = scenario_file.load(scenario_file.SCENARIO_DIR / HANDOVER_SCENARIO)
  sides = [_handover_side("baseline", BASELINE_RUN), _handover_side("candidate", CANDIDATE_RUN)]
  deltas = [_chat_delta(baseline), _chat_delta(candidate)]
  return parity._delta_diff_lines(scenario, sides, {}, deltas, candidate_is_python)


def test_handover_pair_of_the_stopped_run_compares_equal_in_either_order() -> None:
  for baseline_first in (True, False):
    for candidate_first in (True, False):
      baseline = _handover_log(_handover_pair(BASELINE_RUN, baseline_first))
      candidate = _handover_log(_handover_pair(CANDIDATE_RUN, candidate_first))
      assert _handover_diff(baseline, candidate) == []
  baseline = _handover_log(_handover_pair(BASELINE_RUN, True))
  candidate = _handover_log(_handover_pair(CANDIDATE_RUN, False))
  assert _handover_diff(baseline, candidate, candidate_is_python=True) == []


def test_handover_swapped_pair_of_another_run_stays_a_difference() -> None:
  baseline = _handover_log(_handover_pair(BASELINE_RUN, True), _handover_pair(OTHER_RUN, True))
  candidate = _handover_log(_handover_pair(CANDIDATE_RUN, False), _handover_pair(OTHER_RUN, False))
  assert any("$[3].type" in line for line in _handover_diff(baseline, candidate))


def test_handover_swapped_pair_with_another_outcome_stays_a_difference() -> None:
  baseline = _handover_log(_handover_pair(BASELINE_RUN, True, outcome="failed"))
  candidate = _handover_log(_handover_pair(CANDIDATE_RUN, False, outcome="failed"))
  assert any("$[1].type" in line for line in _handover_diff(baseline, candidate))


def test_handover_run_finished_swapped_with_another_event_type_stays_a_difference() -> None:
  finished = _handover_pair(BASELINE_RUN, True)[0]
  assistant = {"type": "assistant", "id": "between-id", "text": "same"}
  baseline = [finished, assistant]
  candidate = [assistant, {**finished, "run_id": CANDIDATE_RUN}]
  assert any("$[0].type" in line for line in _handover_diff(baseline, candidate))


def test_handover_pinned_run_id_resolves_from_each_sides_own_capture() -> None:
  scenario = scenario_file.load(scenario_file.SCENARIO_DIR / HANDOVER_SCENARIO)
  for label, run_id in (("baseline", BASELINE_RUN), ("candidate", CANDIDATE_RUN)):
    rules = parity._adjacent_order_rules(scenario, _handover_side(label, run_id), {})
    assert len(rules) == 1
    first, second = rules[0]
    assert first.event_type == "run_finished"
    assert first.match_fields == {"run_id": run_id, "outcome": "interrupted"}
    assert second.event_type == "master_done"
    assert second.match_fields == {"exit_code": -9}


def _event_stream_diff(baseline: list[dict], candidate: list[dict]) -> list[str]:
  event_types = (
      "system", "assistant", "result", "master_done", "run_finished", "backend_overlay_inactive",
      "resume_context_dropped")
  path = "home/sessions/synthetic-session/data/chat_events.jsonl"
  left = compare.order_event_type_streams_in_delta(_chat_delta(baseline), path, event_types)
  right = compare.order_event_type_streams_in_delta(_chat_delta(candidate), path, event_types)
  return compare.delta_diff(left, right, _context(), _context())


def test_event_type_streams_allow_parent_turns_to_interleave_with_loop_events() -> None:
  iteration_one = {"type": "improve_iteration_completed", "id": "iteration-one", "iteration": 1}
  iteration_two = {"type": "improve_iteration_completed", "id": "iteration-two", "iteration": 2}
  report_one = {"type": "child_report", "id": "report-one", "summary": "first"}
  report_two = {"type": "child_report", "id": "report-two", "summary": "second"}
  parent_turn = [
      {
          "type": "system",
          "id": "manager-system"
      },
      {
          "type": "assistant",
          "id": "manager-assistant"
      },
      {
          "type": "result",
          "id": "manager-result"
      },
      {
          "type": "master_done",
          "id": "manager-done",
          "exit_code": 0
      },
      {
          "type": "run_finished",
          "id": "manager-run-finished",
          "outcome": "success"
      },
      {
          "type": "backend_overlay_inactive",
          "id": "manager-overlay"
      },
      {
          "type": "resume_context_dropped",
          "id": "manager-resume"
      },
  ]
  loop_closed = {"type": "improve_failed", "id": "loop-closed", "iterations_completed": 2}
  baseline = [iteration_one, report_one, *parent_turn[:3], iteration_two, *parent_turn[3:], report_two, loop_closed]
  candidate = [iteration_one, report_one, *parent_turn, iteration_two, report_two, loop_closed]
  for events in (baseline, candidate):
    for index, event in enumerate(events):
      event["event_index"] = index + 1

  assert _event_stream_diff(baseline, candidate) == []
  out_of_order_parent_turn = [*candidate[:5], parent_turn[4], parent_turn[3], *candidate[7:]]
  assert _event_stream_diff(baseline, out_of_order_parent_turn) != []


def test_event_type_streams_keep_python_order_between_loop_event_types() -> None:
  baseline = [
      {
          "type": "improve_iteration_completed",
          "id": "iteration-one"
      },
      {
          "type": "child_report",
          "id": "report-one"
      },
      {
          "type": "master_done",
          "id": "manager-done"
      },
      {
          "type": "improve_iteration_completed",
          "id": "iteration-two"
      },
      {
          "type": "child_report",
          "id": "report-two"
      },
  ]
  candidate = [baseline[0], baseline[3], baseline[1], baseline[2], baseline[4]]

  assert _event_stream_diff(baseline, candidate) != []


def test_event_type_streams_keep_each_type_order_and_payload_compared() -> None:
  baseline = [
      {
          "type": "improve_iteration_completed",
          "id": "iteration-one",
          "iteration": 1
      },
      {
          "type": "child_report",
          "id": "report-one",
          "summary": "first"
      },
      {
          "type": "improve_iteration_completed",
          "id": "iteration-two",
          "iteration": 2
      },
      {
          "type": "child_report",
          "id": "report-two",
          "summary": "second"
      },
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
