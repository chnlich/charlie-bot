"""Normalization tests: a fake's recording compares only the headers its feature code sets explicitly, and each
fake-agent start's stdin and stdout_path normalize under the rules that name them.
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
        "plans": [{"id": plan_id, "title": "Synthetic plan", "versions": versions} for plan_id in (1, 2)],
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
