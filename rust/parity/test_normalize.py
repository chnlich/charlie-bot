"""Normalization tests: a fake's recording compares only the headers its feature code sets explicitly, and each
fake-agent start's stdin and stdout_path normalize under the rules that name them.
"""

import hashlib
import json
import pathlib
import re
from typing import Any

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


def _raw_chat_delta(data: bytes) -> compare.Delta:
  path = "sessions/synthetic-session/data/chat_events.jsonl"
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


def test_a_chat_line_json_refuses_compares_as_malformed_text_without_raising() -> None:
  """A chat line that json.loads refuses compares as its text under malformed_jsonl_line. No event rule matches it,
  so two logs that hold the same bad line agree, and two different bad lines are reported as a difference."""
  finished = b"".join(json.dumps(event).encode() + b"\n" for event in _cancel_events("interrupted", finish_first=True))
  rules = ((_cancel_event_rule(), _cancel_event_rule()),)
  base_ctx = normalize.Context(tree_root="/synthetic/base")
  candidate_ctx = normalize.Context(tree_root="/synthetic/candidate")
  same = compare.delta_diff(
      _raw_chat_delta(finished + b"not json {\n"),
      _raw_chat_delta(finished + b"not json {\n"),
      base_ctx,
      candidate_ctx,
      rules,
      False,
  )
  assert same == []
  different = compare.delta_diff(
      _raw_chat_delta(finished + b"not json {\n"),
      _raw_chat_delta(finished + b"not json [\n"),
      base_ctx,
      candidate_ctx,
      rules,
      False,
  )
  assert different


def test_a_chat_line_that_is_not_utf8_compares_as_text_on_both_paths() -> None:
  """json.loads raises UnicodeDecodeError on a line with invalid UTF-8, which is no JSONDecodeError. The file
  normalizer and the event-log diff both take that line as its text under malformed_jsonl_line and raise nothing."""
  path = "sessions/synthetic-session/data/chat_events.jsonl"
  finished = b"".join(json.dumps(event).encode() + b"\n" for event in _cancel_events("interrupted", finish_first=True))
  rules = ((_cancel_event_rule(), _cancel_event_rule()),)

  def contexts() -> tuple[normalize.Context, normalize.Context]:
    # A context numbers the minted UUIDs it meets, so each comparison takes its own pair.
    return normalize.Context(tree_root="/synthetic/base"), normalize.Context(tree_root="/synthetic/candidate")

  bad_line = finished + b"\xff{\n"
  value = compare._normalized_file(path, compare.Entry("file", bad_line), contexts()[0])
  assert value[-1] == {"malformed_jsonl_line": b"\xff{".decode(errors="surrogateescape")}
  assert compare.delta_diff(_raw_chat_delta(bad_line), _raw_chat_delta(bad_line), *contexts(), rules, False) == []
  assert compare.delta_diff(
      _raw_chat_delta(bad_line), _raw_chat_delta(finished + b"\xfe{\n"), *contexts(), rules, False)


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


def _spawn_side(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, label: str, records: list[dict]) -> parity.Side:
  """A side whose fake-agent directory contains the given spawn records."""
  # The record reads the run token from the environment; a test run inside an agent session must not carry one.
  monkeypatch.delenv("CHARLIEBOT_RUN_TOKEN", raising=False)
  root = tmp_path / label
  (root / "fake-agent").mkdir(parents=True)
  data = "".join(json.dumps(record) + "\n" for record in records)
  (root / "fake-agent" / "spawns.jsonl").write_text(data, encoding="utf-8")
  return parity.Side(label, parity.servers.Tree(label=label, root=root), port=18600, strace=parity.servers.STRACE_BIN)


def _spawn_record(index: int, session: str, run: str, stdin: bytes) -> dict:
  record = fake_agent._spawn_record(index, "claude", scripted=True, stdin_bytes=stdin)
  record["run_token_claims"] = {"session_id": session, "run_id": run, "agent": "claude"}
  return record


def _task_metadata(task_parent_id: str | None) -> compare.Entry:
  """A session's metadata.json entry carrying only the task parent the spawn aliases read."""
  return compare.Entry("file", json.dumps({"task_parent_id": task_parent_id}).encode())


def test_stdin_naming_only_the_tree_root_compares_equal(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  # Both tree roots have one length, as the harness gives them, so the stdin_bytes counts agree.
  monkeypatch.delenv("CHARLIEBOT_RUN_TOKEN", raising=False)
  baseline = _spawn_side(
      tmp_path, monkeypatch, "base", [
          fake_agent._spawn_record(
              0, "claude", scripted=True, stdin_bytes=f"Read {tmp_path / 'base'}/home/notes.md".encode())
      ])
  candidate = _spawn_side(
      tmp_path, monkeypatch, "cand", [
          fake_agent._spawn_record(
              0, "claude", scripted=True, stdin_bytes=f"Read {tmp_path / 'cand'}/home/notes.md".encode())
      ])
  assert parity._spawns_diff([baseline, candidate], expect_starts=1) == []


def test_stdin_text_difference_names_the_stdin_field(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.delenv("CHARLIEBOT_RUN_TOKEN", raising=False)
  baseline = _spawn_side(
      tmp_path, monkeypatch, "base",
      [fake_agent._spawn_record(0, "claude", scripted=True, stdin_bytes=b"Summarize the notes.")])
  candidate = _spawn_side(
      tmp_path, monkeypatch, "cand",
      [fake_agent._spawn_record(0, "claude", scripted=True, stdin_bytes=b"Summarize the draft.")])
  lines = parity._spawns_diff([baseline, candidate], expect_starts=1)
  assert len(lines) == 1
  assert lines[0].startswith("fake-agent spawns $[0][0].stdin:")


def test_spawn_groups_compare_each_sessions_order_and_launch_context(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.delenv("CHARLIEBOT_RUN_TOKEN", raising=False)
  baseline = _spawn_side(
      tmp_path, monkeypatch, "base", [
          _spawn_record(0, "manager", "manager-run-1", b"Parent audit one."),
          _spawn_record(1, "loop-child", "iteration-run-1", b"Iteration prompt one."),
          _spawn_record(2, "manager", "manager-run-2", b"Parent audit two."),
      ])
  candidate = _spawn_side(
      tmp_path, monkeypatch, "cand", [
          _spawn_record(0, "loop-child", "iteration-run-1", b"Iteration prompt one."),
          _spawn_record(1, "manager", "manager-run-1", b"Parent audit one."),
          _spawn_record(2, "manager", "manager-run-2", b"Parent audit two."),
      ])
  assert parity._spawns_diff([baseline, candidate], expect_starts=3) == []

  records = parity._read_spawns(candidate.tree)
  records[1]["argv"].append("--different-launch")
  parity._spawns_path(candidate.tree).write_text(
      "".join(json.dumps(record) + "\n" for record in records), encoding="utf-8")
  assert parity._spawns_diff([baseline, candidate], expect_starts=3)


def test_interleaved_session_spawns_seed_uuid_normalization_in_group_order(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.delenv("CHARLIEBOT_RUN_TOKEN", raising=False)
  manager_id = "10000000-0000-4000-8000-000000000001"
  child_ids = ("10000000-0000-4000-8000-000000000011", "10000000-0000-4000-8000-000000000012")
  manager_runs = ("10000000-0000-4000-8000-000000000021", "10000000-0000-4000-8000-000000000022")
  child_runs = ("10000000-0000-4000-8000-000000000031", "10000000-0000-4000-8000-000000000032")
  snapshots = []
  sides = []
  for label, child_id, manager_run, child_run, records in (
      ("base", child_ids[0], manager_runs[0], child_runs[0], [
          _spawn_record(0, manager_id, manager_runs[0], b"Parent prompt."),
          _spawn_record(1, child_ids[0], child_runs[0], b"Iteration prompt."),
      ]),
      ("cand", child_ids[1], manager_runs[1], child_runs[1], [
          _spawn_record(0, child_ids[1], child_runs[1], b"Iteration prompt."),
          _spawn_record(1, manager_id, manager_runs[1], b"Parent prompt."),
      ]),
  ):
    side = _spawn_side(tmp_path, monkeypatch, label, records)
    side.ctx.fixture_uuids = frozenset({manager_id})
    sides.append(side)
    snapshots.append(
        {
            f"home/sessions/{child_id}/metadata.json": _task_metadata(manager_id),
            f"home/sessions/{child_id}/data/chat_events.jsonl": compare.Entry("file", b"{}\n"),
        })

  parity._prepare_contexts(sides, snapshots)

  for side, child_id, manager_run, child_run in zip(sides, child_ids, manager_runs, child_runs, strict=True):
    # The child is a spawned session, so its id gets a stable alias under its task parent, not a minted one.
    assert side.ctx.stable_uuids[child_id] == sides[0].ctx.stable_uuids[child_ids[0]]
    assert child_id not in side.ctx.minted_uuids
    assert manager_run not in side.ctx.minted_uuids
    assert child_run not in side.ctx.minted_uuids
    assert side.ctx.stable_uuids[manager_run] == sides[0].ctx.stable_uuids[manager_runs[0]]
    assert side.ctx.stable_uuids[child_run] == sides[0].ctx.stable_uuids[child_runs[0]]
    assert normalize.normalize_text(manager_run, side.ctx) == side.ctx.stable_uuids[manager_run]
    assert normalize.normalize_text(child_run, side.ctx) == side.ctx.stable_uuids[child_run]
  assert sides[0].ctx.stable_uuids[manager_runs[0]] != sides[0].ctx.stable_uuids[child_runs[0]]
  assert parity._spawns_diff(sides, expect_starts=2) == []


def test_children_of_two_parents_that_start_in_opposite_order_keep_their_aliases(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A child's alias names its task parent and its place among that parent's spawns, so no order across parents
  reaches it: the baseline starts a child of the first parent before the second parent's child, and the candidate
  starts them the other way round."""
  monkeypatch.delenv("CHARLIEBOT_RUN_TOKEN", raising=False)
  first_parent = "10000000-0000-4000-8000-000000000001"
  second_parent = "10000000-0000-4000-8000-000000000002"
  first_child = "10000000-0000-4000-8000-000000000011"
  second_child = "10000000-0000-4000-8000-000000000012"
  first_child_again = "10000000-0000-4000-8000-000000000013"
  parent_of = {first_child: first_parent, second_child: second_parent, first_child_again: first_parent}
  runs = {
      first_child: "20000000-0000-4000-8000-000000000011",
      second_child: "20000000-0000-4000-8000-000000000012",
      first_child_again: "20000000-0000-4000-8000-000000000013",
  }
  starts = {
      "base": (first_child, second_child, first_child_again),
      "cand": (second_child, first_child, first_child_again),
  }
  sides = []
  snapshots = []
  for label, order in starts.items():
    records = [
        _spawn_record(index, session, runs[session], b"Iteration prompt.") for index, session in enumerate(order)
    ]
    side = _spawn_side(tmp_path, monkeypatch, label, records)
    side.ctx.fixture_uuids = frozenset({first_parent, second_parent})
    sides.append(side)
    snapshots.append(
        {
            f"home/sessions/{session}/metadata.json": _task_metadata(parent) for session, parent in parent_of.items()
        })

  parity._prepare_contexts(sides, snapshots)

  base, cand = sides
  first_group = hashlib.sha256(first_parent.encode()).hexdigest()[:12]
  second_group = hashlib.sha256(second_parent.encode()).hexdigest()[:12]
  assert base.ctx.stable_uuids[first_child] == f"<spawn-session-{first_group}-1>"
  assert base.ctx.stable_uuids[first_child_again] == f"<spawn-session-{first_group}-2>"
  assert base.ctx.stable_uuids[second_child] == f"<spawn-session-{second_group}-1>"
  for session in parent_of:
    assert base.ctx.stable_uuids[session] == cand.ctx.stable_uuids[session]
  assert len({base.ctx.stable_uuids[session] for session in parent_of}) == 3
  assert parity._spawns_diff(sides, expect_starts=3) == []


def test_children_of_a_bound_manager_group_under_its_name(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A cron task's manager is created by the scenario, so it is neither a fixture nor a spawn. Its leaf groups under
  the manager's name, so a child of another parent that starts first in one tree does not move the leaf's alias."""
  monkeypatch.delenv("CHARLIEBOT_RUN_TOKEN", raising=False)
  fixture_parent = "10000000-0000-4000-8000-000000000001"
  bound_manager = "10000000-0000-4000-8000-000000000002"
  iteration = "10000000-0000-4000-8000-000000000011"
  leaf = "10000000-0000-4000-8000-000000000012"
  parent_of = {iteration: fixture_parent, leaf: bound_manager}
  runs = {iteration: "20000000-0000-4000-8000-000000000011", leaf: "20000000-0000-4000-8000-000000000012"}
  sides = []
  snapshots = []
  for label, order in (("base", (leaf, iteration)), ("cand", (iteration, leaf))):
    records = [_spawn_record(index, session, runs[session], b"Firing prompt.") for index, session in enumerate(order)]
    side = _spawn_side(tmp_path, monkeypatch, label, records)
    side.ctx.fixture_uuids = frozenset({fixture_parent})
    sides.append(side)
    snapshots.append(
        {
            f"home/sessions/{bound_manager}/metadata.json":
                compare.Entry("file",
                              json.dumps({
                                  "name": "synthetic-cron",
                                  "task_parent_id": None
                              }).encode()),
            **{
                f"home/sessions/{session}/metadata.json": _task_metadata(parent) for session, parent in parent_of.items(
                )
            },
        })

  parity._prepare_contexts(sides, snapshots)

  base, cand = sides
  for session in parent_of:
    assert base.ctx.stable_uuids[session] == cand.ctx.stable_uuids[session]
  assert parity._spawns_diff(sides, expect_starts=2) == []


def test_interleaved_event_streams_do_not_seed_different_uuid_orders(tmp_path: pathlib.Path) -> None:
  ids = (
      "10000000-0000-4000-8000-000000000011",
      "10000000-0000-4000-8000-000000000012",
      "10000000-0000-4000-8000-000000000013",
      "10000000-0000-4000-8000-000000000014",
      "10000000-0000-4000-8000-000000000015",
      "10000000-0000-4000-8000-000000000016",
  )
  event_path = "home/sessions/synthetic-session/data/chat_events.jsonl"
  event_types = ("master_done",)
  sequences = (
      [
          {
              "type": "improve_iteration_completed",
              "id": ids[0]
          },
          {
              "type": "master_done",
              "id": ids[1]
          },
          {
              "type": "improve_iteration_completed",
              "id": ids[2]
          },
      ],
      [
          {
              "type": "improve_iteration_completed",
              "id": ids[3]
          },
          {
              "type": "improve_iteration_completed",
              "id": ids[4]
          },
          {
              "type": "master_done",
              "id": ids[5]
          },
      ],
  )
  sides = []
  snapshots = []
  for label, events in zip(("base", "cand"), sequences, strict=True):
    root = tmp_path / label
    root.mkdir()
    sides.append(
        parity.Side(label, parity.servers.Tree(label=label, root=root), port=18600, strace=parity.servers.STRACE_BIN))
    data = "".join(json.dumps(event) + "\n" for event in events).encode()
    snapshots.append({event_path: compare.Entry("file", data)})

  rules = [[(event_path, event_types)], [(event_path, event_types)]]
  parity._prepare_contexts(sides, snapshots, event_type_stream_rules=rules)

  assert list(sides[0].ctx.minted_uuids.values()) == ["<minted-1>", "<minted-2>", "<minted-3>"]
  assert list(sides[1].ctx.minted_uuids.values()) == ["<minted-1>", "<minted-2>", "<minted-3>"]


def test_probe_stdout_pipe_inodes_compare_equal_and_a_file_path_still_differs() -> None:
  rules = normalize.file_rules("fake-agent/spawns.jsonl")
  baseline = normalize.normalize_json({"stdout_path": "pipe:[100017684]"}, rules, _context())
  candidate = normalize.normalize_json({"stdout_path": "pipe:[100007930]"}, rules, _context())
  file_path = normalize.normalize_json(
      {"stdout_path": "/tmp/charliebot-parity-synthetic/base/logs/probe.txt"}, rules, _context())
  assert baseline == candidate == {"stdout_path": "pipe:[<fake-agent-stdout-inode>]"}
  assert baseline != file_path


def test_task_branch_normalization_masks_only_the_wall_clock_second() -> None:
  earlier = normalize.normalize_text("charliebot/task-1792480595-81e5922b", _context())
  later = normalize.normalize_text("charliebot/task-1792480596-81e5922b", _context())
  assert earlier == later == "charliebot/task-<task-branch-timestamp>-81e5922b"
  other_run = normalize.normalize_text("charliebot/task-1792480596-81e5922c", _context())
  assert later != other_run
  assert normalize.normalize_text("task-1792480595-81e5922b", _context()) == "task-1792480595-81e5922b"
  earlier_path = normalize.normalize_path("worktrees/charliebot-task-1792480595-81e5922b", _context())
  later_path = normalize.normalize_path("worktrees/charliebot-task-1792480596-81e5922b", _context())
  assert earlier_path == later_path == "worktrees/charliebot-task-<task-branch-timestamp>-81e5922b"


def test_opencode_normalization_masks_only_the_fake_debug_endpoint_port() -> None:
  earlier = normalize.normalize_text("http://127.0.0.1:39881/api/debug/location", _context())
  later = normalize.normalize_text("http://127.0.0.1:44973/api/debug/location", _context())
  assert earlier == later == "http://127.0.0.1:<opencode-debug-port>/api/debug/location"
  assert normalize.normalize_text("http://127.0.0.1:39881/api/session", _context()) != normalize.normalize_text(
      "http://127.0.0.1:44973/api/session", _context())


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


TOKEN_USAGE_ROUTE = "GET /token-usage"


def test_the_token_usage_render_stamp_is_masked_and_the_as_of_text_stays() -> None:
  body = b'<span class="mtag">generated &middot; 2026-10-19 09:41 UTC</span><p>ledger as of 2026-10-18 09:30 UTC</p>'
  assert normalize.normalize_body_bytes(body, TOKEN_USAGE_ROUTE, _context()) == (
      b'<span class="mtag">generated &middot; "<token-usage-render-stamp>"</span>'
      b'<p>ledger as of 2026-10-18 09:30 UTC</p>')


def test_the_token_usage_read_time_is_masked_and_the_rest_of_the_line_stays() -> None:
  body = b'<p class="foot">Read from the ledger in <b>0.01&thinsp;s</b>; the ledger is as of 2026-10-18 09:30 UTC.</p>'
  assert normalize.normalize_body_bytes(body, TOKEN_USAGE_ROUTE, _context()) == (
      b'<p class="foot">Read from the ledger in <b>"<token-usage-read-time>"&thinsp;s</b>; '
      b'the ledger is as of 2026-10-18 09:30 UTC.</p>')


PLAN_REGISTRY = "home/sessions/synthetic-session/plans.json"


def _plan_registry(takeoff: dict | None, closed: dict | None) -> dict:
  return {"plans": [{"id": 1, "title": "Synthetic plan", "takeoff": takeoff, "closed": closed}]}


def _masked_plan_registry(registry: dict) -> dict:
  return normalize.normalize_json(registry, normalize.file_rules(PLAN_REGISTRY), _context())


_SESSION_LIST = "GET /api/sessions/"


def _session_list(child_stamp: str, field: str) -> list[dict]:
  """The child row with *child_stamp* in *field*, above its root row."""
  child = {
      "id": "synthetic-child",
      "name": "Atlas survey",
      "created_at": "2026-01-05T09:00:07Z",
      "updated_at": "2026-01-05T09:00:07Z",
      "cc_session_started_at": "2026-01-05T09:00:07Z",
  }
  child[field] = child_stamp
  root = {
      "id": "synthetic-root",
      "name": "Atlas planning",
      "created_at": "2026-01-05T09:00:00Z",
      "updated_at": "2026-01-05T09:00:00Z",
      "cc_session_started_at": None,
  }
  return [child, root]


@pytest.mark.parametrize(
    ("field", "placeholder"), [
        ("updated_at", "<session-list-updated-time>"),
        ("cc_session_started_at", "<session-list-conversation-start-time>"),
    ])
def test_a_session_list_wall_clock_stamp_is_masked_and_the_row_order_still_compares(
    field: str, placeholder: str) -> None:
  earlier = normalize.normalize_json(
      _session_list("2026-10-26T20:32:28.473956Z", field), normalize.response_rules(_SESSION_LIST), _context())
  later = normalize.normalize_json(
      _session_list("2026-10-26T20:32:28.528709Z", field), normalize.response_rules(_SESSION_LIST), _context())
  assert earlier == later
  assert later[0][field] == placeholder
  assert later[1]["updated_at"] == "2026-01-05T09:00:00Z"
  assert later[1]["cc_session_started_at"] is None
  reordered = normalize.normalize_json(
      list(reversed(_session_list("2026-10-26T20:32:28.528709Z", field))), normalize.response_rules(_SESSION_LIST),
      _context())
  assert reordered != later


@pytest.mark.parametrize("field", ["updated_at", "cc_session_started_at"])
def test_a_session_list_fixture_clock_stamp_stays_exact(field: str) -> None:
  earlier = normalize.normalize_json(
      _session_list("2026-01-05T09:00:07Z", field), normalize.response_rules(_SESSION_LIST), _context())
  later = normalize.normalize_json(
      _session_list("2026-01-05T09:00:14Z", field), normalize.response_rules(_SESSION_LIST), _context())
  assert earlier[0][field] == "2026-01-05T09:00:07Z"
  assert later[0][field] == "2026-01-05T09:00:14Z"
  assert earlier != later


def test_plan_takeoff_time_masks_only_the_stamp_and_its_version_still_differs() -> None:
  earlier = _plan_registry({"v": 1, "at": "2026-10-19T09:41:00+00:00"}, None)
  later = _plan_registry({"v": 1, "at": "2026-10-19T09:42:07.512904+00:00"}, None)
  assert _masked_plan_registry(earlier) == _masked_plan_registry(later)
  assert _masked_plan_registry(earlier)["plans"][0]["takeoff"] == {"v": 1, "at": "<plan-takeoff-time>"}
  other_version = _plan_registry({"v": 2, "at": "2026-10-19T09:42:07.512904+00:00"}, None)
  assert _masked_plan_registry(later) != _masked_plan_registry(other_version)


def test_plan_version_created_time_masks_each_stamp_and_each_version_number_still_differs() -> None:

  def registry(stamp: str, numbers: tuple[int, int]) -> dict:
    versions = [{"v": number, "created_at": stamp} for number in numbers]
    return {
        "plans": [{
            "id": plan_id,
            "title": "Synthetic plan",
            "versions": versions
        } for plan_id in (1, 2)],
    }

  earlier = _masked_plan_registry(registry("2026-10-19T09:41:00+00:00", (1, 2)))
  later = _masked_plan_registry(registry("2026-10-19T09:42:07.512904+00:00", (1, 2)))
  assert earlier == later
  assert [[version["created_at"] for version in plan["versions"]] for plan in earlier["plans"]] == [
      ["<plan-version-created-time>"] * 2,
      ["<plan-version-created-time>"] * 2,
  ]
  assert [[version["v"] for version in plan["versions"]] for plan in earlier["plans"]] == [[1, 2], [1, 2]]
  renumbered = _masked_plan_registry(registry("2026-10-19T09:42:07.512904+00:00", (1, 3)))
  assert later != renumbered


def test_plan_close_time_masks_only_the_stamp_and_its_mode_still_differs() -> None:
  earlier = _plan_registry(None, {"as": "completed", "at": "2026-10-19T09:41:00+00:00"})
  later = _plan_registry(None, {"as": "completed", "at": "2026-10-19T09:42:07.512904+00:00"})
  assert _masked_plan_registry(earlier) == _masked_plan_registry(later)
  assert _masked_plan_registry(earlier)["plans"][0]["closed"] == {"as": "completed", "at": "<plan-close-time>"}
  other_mode = _plan_registry(None, {"as": "abandoned", "at": "2026-10-19T09:42:07.512904+00:00"})
  assert _masked_plan_registry(later) != _masked_plan_registry(other_mode)


def test_cron_firing_masks_the_clock_stamp_and_keeps_the_task_identity() -> None:
  earlier = (
      "- Session: synthetic-cron · 2026-10-20T09:26:06.696542+00:00\n"
      "cron:synthetic-cron:2026-10-20T09:26:06.696542+00:00:leaf")
  later = (
      "- Session: synthetic-cron · 2026-10-20T09:26:06.735915+00:00\n"
      "cron:synthetic-cron:2026-10-20T09:26:06.735915+00:00:leaf")
  assert normalize.normalize_text(earlier, _context()) == normalize.normalize_text(later, _context())
  leaf = {
      "id": "synthetic-session",
      "name": "synthetic-cron · 2026-10-20T09:26:06.696542+00:00",
      "profile": "worker",
      "task_parent_id": "synthetic-parent",
  }
  normalized_leaf = normalize.normalize_json(leaf, (), _context())
  assert normalized_leaf["name"] == "synthetic-cron · 2026-10-20T<cron-firing-time>+00:00"
  claims = {
      "session_id": "synthetic-session",
      "run_id": "synthetic-run",
      "agent": leaf["name"],
  }
  normalized_claims = normalize.normalize_json({"run_token_claims": claims}, (), _context())
  assert normalized_claims["run_token_claims"]["agent"] == "synthetic-cron · 2026-10-20T<cron-firing-time>+00:00"
  assert normalize.normalize_text("cron:another-task:2026-10-20T09:26:06.696542+00:00:leaf", _context()) != (
      normalize.normalize_text("cron:synthetic-cron:2026-10-20T09:26:06.696542+00:00:leaf", _context()))
  assert normalize.normalize_text("cron:synthetic-cron:2026-10-20T09:26:06.696542+00:00:leaf",
                                  _context()) != normalize.normalize_text(
                                      "cron:synthetic-cron:2026-10-20T02:26:06.696542-07:00:leaf", _context())
  assert normalize.normalize_text(
      "2026-10-20T09:26:06.696542+00:00",
      _context()) != (normalize.normalize_text("2026-10-20T02:26:06.735915-07:00", _context()))
  assert normalize.normalize_text("cron:synthetic-cron:2026-10-20T09:26:06.696542+00:00:leaf",
                                  _context()) != normalize.normalize_text(
                                      "cron:synthetic-cron:2026-10-20T09:26:06.696542001+00:00:leaf", _context())


def test_dot_separated_stamp_in_other_text_stays_visible() -> None:
  # A memory index line is `<topic>/<slug> · <title>` (src/features/memory/memory.py:375), and no cron firing writes
  # it, so its stamp is not a leaf name's wall-clock time.
  memory_line = "charliebot/local-checks · 2026-10-20T09:26:06.696542+00:00"
  assert normalize.normalize_text(memory_line, _context()) == memory_line
  leaf_shaped_text = "synthetic-cron · 2026-10-20T09:26:06.696542+00:00"
  assert normalize.normalize_text(leaf_shaped_text, _context()) == leaf_shaped_text
  note = "Operator note: synthetic-cron · 2026-10-20T09:26:06.696542+00:00 should remain visible."
  assert normalize.normalize_text(note, _context()) == note
  assert normalize.normalize_json({"name": leaf_shaped_text}, (), _context()) == {"name": leaf_shaped_text}


def test_cron_last_scheduled_run_masks_the_stamp_and_keeps_the_expression() -> None:
  rules = normalize.file_rules("home/sessions/synthetic-session/metadata.json")
  earlier = normalize.normalize_json(
      {
          "last_scheduled_run": "2026-10-20T09:26:06.696542+00:00",
          "last_scheduled_cron": "0 8 * * *"
      },
      rules,
      _context(),
  )
  later = normalize.normalize_json(
      {
          "last_scheduled_run": "2026-10-20T09:26:06.735915+00:00",
          "last_scheduled_cron": "0 8 * * *"
      },
      rules,
      _context(),
  )
  assert earlier == later
  assert earlier == {
      "last_scheduled_run": "2026-10-20T<cron-last-scheduled-run>+00:00",
      "last_scheduled_cron": "0 8 * * *",
  }
  assert normalize.normalize_json({"last_scheduled_run": None}, rules, _context()) == {"last_scheduled_run": None}
  nine_fraction_digits = normalize.normalize_json(
      {"last_scheduled_run": "2026-10-20T09:26:06.696542001+00:00"}, rules, _context())
  assert earlier != nine_fraction_digits


def test_field_rule_only_passes_null_through_as_a_non_string() -> None:
  rule = normalize.FieldRule(
      name="synthetic", target="", field="stamp", reason="test", when=(), pattern=re.compile(r"(?P<value>.+)"))
  assert rule.mask(None) is None
  with pytest.raises(TypeError, match="expects a string or null"):
    rule.mask(1)


SESSIONS_RUNS_ROUTE = "GET /api/sessions/{session_id}/runs"
WORKER_REPO = f"{_context().tree_root}/workspace/worker-repo"
ANOTHER_REPO = f"{_context().tree_root}/workspace/another-repo"
DELEGATE_REQUEST = "synthetic-delegate-implement-failure"


def _masked_run_item(item: dict) -> dict:
  rules = normalize.response_rules(SESSIONS_RUNS_ROUTE)
  return normalize.normalize_json({"items": [item]}, rules, _context())["items"][0]


def _masked_run_metadata(metadata: dict) -> dict:
  rules = normalize.file_rules("home/sessions/synthetic-session/data/runs/synthetic-run/metadata.json")
  return normalize.normalize_json(metadata, rules, _context())


def _masked_chat_event(event: dict) -> dict:
  rules = normalize.file_rules("home/sessions/synthetic-session/data/chat_events.jsonl")
  return normalize.normalize_json(event, rules, _context())


def test_improve_loop_state_masks_only_creation_time_and_server_pid() -> None:
  path = "home/sessions/synthetic-session/loops/1/state.json"
  rules = normalize.file_rules(path)
  baseline = normalize.normalize_json(
      {
          "created_at": "2026-10-20T10:00:00Z",
          "server_pid": 100,
          "iteration": 2,
          "status": "running"
      },
      rules,
      _context(),
  )
  candidate = normalize.normalize_json(
      {
          "created_at": "2026-10-20T10:01:00Z",
          "server_pid": 200,
          "iteration": 2,
          "status": "blocked"
      },
      rules,
      _context(),
  )

  assert baseline["created_at"] == candidate["created_at"] == "<improve-loop-created-time>"
  assert baseline["server_pid"] == candidate["server_pid"] == "<improve-loop-server-pid>"
  assert baseline["iteration"] == candidate["iteration"]
  assert baseline["status"] != candidate["status"]


def test_improve_child_spec_hash_rule_does_not_mask_another_request_id() -> None:
  path = "home/sessions/synthetic-session/data/chat_events.jsonl"
  rules = normalize.file_rules(path)
  assert normalize.normalize_json({
      "request_id": "improve:1",
      "task_spec_hash": "synthetic-hash"
  }, rules, _context()) == normalize.normalize_json(
      {
          "request_id": "improve:1",
          "task_spec_hash": "another-hash"
      }, rules, _context())

  other_request = {"request_id": "improve:2", "task_spec_hash": "synthetic-hash"}
  other_request_changed = {"request_id": "improve:2", "task_spec_hash": "another-hash"}
  assert normalize.normalize_json(other_request, rules,
                                  _context()) != normalize.normalize_json(other_request_changed, rules, _context())


def test_the_session_runs_task_spec_hash_is_masked_only_for_the_local_repository() -> None:
  first = _masked_run_item({"repo_path": WORKER_REPO, "task_spec_hash": "synthetic-hash"})
  second = _masked_run_item({"repo_path": WORKER_REPO, "task_spec_hash": "synthetic-other-hash"})
  assert first["task_spec_hash"] == "<session-runs-local-repo-task-spec-hash>"
  assert first == second
  # A hash outside the condition still compares: another repository keeps its own value.
  outside = _masked_run_item({"repo_path": ANOTHER_REPO, "task_spec_hash": "synthetic-hash"})
  outside_other = _masked_run_item({"repo_path": ANOTHER_REPO, "task_spec_hash": "synthetic-other-hash"})
  assert outside["task_spec_hash"] == "synthetic-hash"
  assert outside != outside_other


def test_the_run_metadata_task_spec_hash_is_masked_only_for_the_local_repository() -> None:
  first = _masked_run_metadata({"repo_path": WORKER_REPO, "task_spec_hash": "synthetic-hash"})
  second = _masked_run_metadata({"repo_path": WORKER_REPO, "task_spec_hash": "synthetic-other-hash"})
  assert first["task_spec_hash"] == "<run-local-repo-task-spec-hash>"
  assert first == second
  # A hash outside the condition still compares: another repository keeps its own value.
  outside = _masked_run_metadata({"repo_path": ANOTHER_REPO, "task_spec_hash": "synthetic-hash"})
  outside_other = _masked_run_metadata({"repo_path": ANOTHER_REPO, "task_spec_hash": "synthetic-other-hash"})
  assert outside["task_spec_hash"] == "synthetic-hash"
  assert outside != outside_other


STOP_REQUEST = "synthetic-stop-request"
STOPPED_RUN_METADATA = "home/sessions/synthetic-session/data/runs/synthetic-run/metadata.json"
STOPPED_RUN_EVENTS = "home/sessions/synthetic-session/data/chat_events.jsonl"


def _stopped_run_context(request_id: str, outcome: str) -> normalize.Context:
  events = [
      {
          "type": "run_stop_requested",
          "request_id": request_id,
          "run_id": "synthetic-run"
      },
      {
          "type": "run_finished",
          "run_id": "synthetic-run",
          "outcome": outcome
      },
  ]
  data = "".join(json.dumps(event) + "\n" for event in events).encode()
  ctx = _context()
  normalize.prepare_snapshot([(STOPPED_RUN_EVENTS, "file", data)], ctx)
  return ctx


def _masked_stopped_run_exit_code(exit_code: Any, request_id: str = STOP_REQUEST, outcome: str = "interrupted") -> Any:
  rules = normalize.file_rules(STOPPED_RUN_METADATA)
  record = {"id": "synthetic-run", "exit_code": exit_code}
  return normalize.normalize_json(record, rules, _stopped_run_context(request_id, outcome))["exit_code"]


def test_stopped_run_exit_code_masks_null_and_sigkill_to_one_placeholder() -> None:
  placeholder = "<interrupted-run-exit-code>"
  assert _masked_stopped_run_exit_code(None) == placeholder
  assert _masked_stopped_run_exit_code(-9) == placeholder
  # Another code, and a float that equals -9, stay visible: the rule accepts null and the integer -9 only.
  assert _masked_stopped_run_exit_code(-15) == -15
  assert _masked_stopped_run_exit_code(0) == 0
  assert _masked_stopped_run_exit_code(-9.0) == -9.0


def test_stopped_run_exit_code_stays_visible_in_another_scenario_and_for_another_outcome() -> None:
  assert _masked_stopped_run_exit_code(None, request_id="synthetic-other-stop") is None
  assert _masked_stopped_run_exit_code(-9, request_id="synthetic-other-stop") == -9
  assert _masked_stopped_run_exit_code(None, outcome="failed") is None
  assert _masked_stopped_run_exit_code(-9, outcome="success") == -9


def test_the_task_created_hash_is_masked_only_for_the_delegate_request() -> None:
  first = _masked_chat_event({"request_id": DELEGATE_REQUEST, "task_spec_hash": "synthetic-hash"})
  second = _masked_chat_event({"request_id": DELEGATE_REQUEST, "task_spec_hash": "synthetic-other-hash"})
  assert first["task_spec_hash"] == "<local-repo-task-created-spec-hash>"
  assert first == second
  # A hash outside the condition still compares: another request keeps its own value.
  outside = _masked_chat_event({"request_id": "synthetic-other-request", "task_spec_hash": "synthetic-hash"})
  other = {"request_id": "synthetic-other-request", "task_spec_hash": "synthetic-other-hash"}
  outside_other = _masked_chat_event(other)
  assert outside["task_spec_hash"] == "synthetic-hash"
  assert outside != outside_other


def test_task_close_and_child_report_mask_only_spec_hashes_in_result_refs() -> None:
  close = _masked_chat_event(
      {
          "type":
              "task_closed",
          "result_refs":
              ["run:synthetic-run", f"spec:{'a' * 64}", "review:synthetic-review", "landed:main@synthetic-commit"],
      })
  other_close = _masked_chat_event(
      {
          "type":
              "task_closed",
          "result_refs":
              ["run:synthetic-run", f"spec:{'b' * 64}", "review:synthetic-review", "landed:main@synthetic-commit"],
      })
  assert close == other_close
  assert close["result_refs"] == [
      "run:synthetic-run", "spec:<task_closed-landed-local-repo-spec-ref>", "review:synthetic-review",
      "landed:main@synthetic-commit"
  ]

  other_event = _masked_chat_event({"type": "task_opened", "result_refs": [f"spec:{'a' * 64}"]})
  changed_event = _masked_chat_event({"type": "task_opened", "result_refs": [f"spec:{'b' * 64}"]})
  assert other_event != changed_event


def test_task_close_without_a_landing_keeps_its_spec_hash_compared() -> None:
  close = _masked_chat_event(
      {
          "type": "task_closed",
          "result_refs": ["run:synthetic-run", f"spec:{'a' * 64}", "review:synthetic-review"]
      })
  other_close = _masked_chat_event(
      {
          "type": "task_closed",
          "result_refs": ["run:synthetic-run", f"spec:{'b' * 64}", "review:synthetic-review"]
      })
  assert close != other_close
  assert close["result_refs"][1] == f"spec:{'a' * 64}"


def test_a_landed_event_of_another_type_keeps_its_spec_hash_compared() -> None:
  opened = _masked_chat_event(
      {
          "type": "task_opened",
          "result_refs": ["run:synthetic-run", f"spec:{'a' * 64}", "landed:main@synthetic-commit"]
      })
  other_opened = _masked_chat_event(
      {
          "type": "task_opened",
          "result_refs": ["run:synthetic-run", f"spec:{'b' * 64}", "landed:main@synthetic-commit"]
      })
  assert opened != other_opened
  assert opened["result_refs"][1] == f"spec:{'a' * 64}"


def test_clock_derived_leaf_ids_sorting_in_opposite_order_get_the_same_placeholders(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A cron firing's leaf session id is clock-derived, so the two trees' ids sort differently.

  The spawn-order seeding must number the placeholders, so the minted ids inside the session
  directories do not renumber when the leaf directory sorts before the manager directory in one tree
  and after it in the other.
  """
  monkeypatch.delenv("CHARLIEBOT_RUN_TOKEN", raising=False)
  manager_ids = ("20000000-0000-4000-8000-000000000001", "20000000-0000-4000-8000-000000000002")
  leaf_ids = ("20000000-0000-4000-8000-000000000003", "20000000-0000-4000-8000-000000000004")
  manager_runs = ("20000000-0000-4000-8000-000000000021", "20000000-0000-4000-8000-000000000022")
  leaf_runs = ("20000000-0000-4000-8000-000000000031", "20000000-0000-4000-8000-000000000032")
  # The bound manager session is not a spawn, so its id stays minted and its raw form reorders too.
  minted_ids = ("30000000-0000-4000-8000-000000000001", "30000000-0000-4000-8000-000000000002")
  sides = []
  snapshots = []
  for label, manager_id, leaf_id, manager_run, leaf_run, minted_id in (
      ("base", manager_ids[0], leaf_ids[0], manager_runs[0], leaf_runs[0], minted_ids[0]),
      ("cand", manager_ids[1], leaf_ids[1], manager_runs[1], leaf_runs[1], minted_ids[1]),
  ):
    # Base sorts manager before leaf; candidate sorts leaf before manager.
    side = _spawn_side(
        tmp_path, monkeypatch, label, [
            _spawn_record(0, leaf_id, leaf_run, b"Manual firing prompt."),
            _spawn_record(1, manager_id, manager_run, b"Cron finish report."),
        ])
    sides.append(side)
    snapshots.append(
        {
            f"home/sessions/{manager_id}/metadata.json":
                _task_metadata(None),
            f"home/sessions/{leaf_id}/metadata.json":
                _task_metadata(manager_id),
            f"home/sessions/{manager_id}/data/chat_events.jsonl":
                compare.Entry("file", b'{"id": "%s"}\n' % manager_run.encode()),
            f"home/sessions/{leaf_id}/data/chat_events.jsonl":
                compare.Entry("file", b'{"id": "%s"}\n' % leaf_run.encode()),
            f"home/sessions/{minted_id}/data/chat_events.jsonl":
                compare.Entry("file", b'{"id": "40000000-0000-4000-8000-000000000003"}\n'),
        })

  parity._prepare_contexts(sides, snapshots)

  for side, minted_id in zip(sides, minted_ids, strict=True):
    assert side.ctx.minted_uuids[minted_id] == "<minted-1>"
    assert normalize.normalize_text("40000000-0000-4000-8000-000000000003", side.ctx) == "<minted-2>"

  for side, manager_id, leaf_id, manager_run, leaf_run in zip(sides, manager_ids, leaf_ids, manager_runs, leaf_runs,
                                                              strict=True):
    # The manager is a root spawn; the leaf is its child, so the leaf's alias is the same on both sides.
    assert side.ctx.stable_uuids[manager_id] == "<spawn-session-root-1>"
    assert side.ctx.stable_uuids[leaf_id] == sides[0].ctx.stable_uuids[leaf_ids[0]]
    assert normalize.normalize_text(manager_id, side.ctx) == "<spawn-session-root-1>"
    assert normalize.normalize_text(leaf_id, side.ctx) == side.ctx.stable_uuids[leaf_id]
    assert side.ctx.stable_uuids[manager_run] == sides[0].ctx.stable_uuids[manager_runs[0]]
    assert side.ctx.stable_uuids[leaf_run] == sides[0].ctx.stable_uuids[leaf_runs[0]]
  assert parity._spawns_diff(sides, expect_starts=2) == []
