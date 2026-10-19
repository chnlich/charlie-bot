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
  assert lines == ["body: one side is a JSON object with a detail key, the other side is not"]
