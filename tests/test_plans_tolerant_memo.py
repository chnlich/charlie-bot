"""Unit tests for the read_plans_tolerant per-file memo."""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from conftest import count_path_read_text

import src.core.plans as plans_module
from src.core.plans import read_plans_tolerant


@pytest.fixture(autouse=True)
def _clear_tolerant_read_memo():
  plans_module._tolerant_read_memo.clear()
  yield
  plans_module._tolerant_read_memo.clear()


def _write_registry(path: Path, count: int) -> None:
  plans = [
      {
          "id": i,
          "title": f"plan {i}",
          "versions": [{
              "v": 1,
              "file": f"plan_{i}_v1.html",
              "created_at": "2026-09-01T00:00:00+00:00",
              "trigger": "initial",
              "base": None,
          }],
          "takeoff": None,
          "closed": None,
      } for i in range(count)
  ]
  path.write_text(json.dumps({"plans": plans}), encoding="utf-8")


def test_steady_state_read_pays_no_file_read(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  p = tmp_path / "plans.json"
  _write_registry(p, 3)
  first = read_plans_tolerant(p, "sess")
  assert len(first["plans"]) == 3

  reads = count_path_read_text(monkeypatch, lambda path: True)
  for _ in range(3):
    again = read_plans_tolerant(p, "sess")
    assert again == first
  assert reads == []


def test_rewrite_rereads_and_serves_new_content(tmp_path: Path) -> None:
  p = tmp_path / "plans.json"
  _write_registry(p, 1)
  assert len(read_plans_tolerant(p, "sess")["plans"]) == 1

  _write_registry(p, 2)
  result = read_plans_tolerant(p, "sess")
  assert len(result["plans"]) == 2
  assert all(plan["state"] == "awaiting approval" for plan in result["plans"])


def test_same_size_rewrite_rereads_on_mtime(tmp_path: Path) -> None:
  p = tmp_path / "plans.json"
  _write_registry(p, 1)
  assert read_plans_tolerant(p, "sess")["plans"][0]["title"] == "plan 0"

  same_length = p.read_text(encoding="utf-8").replace('"plan 0"', '"plan X"')
  assert len(same_length.encode()) == p.stat().st_size
  p.write_text(same_length, encoding="utf-8")
  assert read_plans_tolerant(p, "sess")["plans"][0]["title"] == "plan X"


def test_rewrite_to_corrupt_drops_the_stale_hit(tmp_path: Path) -> None:
  p = tmp_path / "plans.json"
  _write_registry(p, 1)
  assert read_plans_tolerant(p, "sess")["errors"] == []

  p.write_text("{not valid json", encoding="utf-8")
  result = read_plans_tolerant(p, "sess")
  assert result["plans"] == []
  assert len(result["errors"]) == 1
  # A content-determined error is a pure function of the file bytes: memoized.
  assert p.as_posix() in plans_module._tolerant_read_memo


def test_oserror_read_is_never_memoized(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  p = tmp_path / "plans.json"
  _write_registry(p, 1)
  real_read_text = Path.read_text
  failing = {"on": True}

  def flaky_read_text(self: Path, *args, **kwargs):
    if self == p and failing["on"]:
      raise OSError("simulated transient io")
    return real_read_text(self, *args, **kwargs)

  monkeypatch.setattr(Path, "read_text", flaky_read_text)
  failed = read_plans_tolerant(p, "sess")
  assert failed["plans"] == [] and len(failed["errors"]) == 1
  assert p.as_posix() not in plans_module._tolerant_read_memo

  failing["on"] = False
  recovered = read_plans_tolerant(p, "sess")
  assert len(recovered["plans"]) == 1 and recovered["errors"] == []


def test_missing_file_stays_uncached(tmp_path: Path) -> None:
  p = tmp_path / "plans.json"
  assert read_plans_tolerant(p, "sess") == {"plans": [], "errors": []}
  assert p.as_posix() not in plans_module._tolerant_read_memo

  _write_registry(p, 1)
  assert len(read_plans_tolerant(p, "sess")["plans"]) == 1


def test_memo_lru_evicts_oldest_path(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr(plans_module, "_TOLERANT_READ_MEMO_LIMIT", 2)
  paths = []
  for name in ("a", "b", "c"):
    p = tmp_path / f"{name}.json"
    _write_registry(p, 1)
    read_plans_tolerant(p, "sess")
    paths.append(p.as_posix())

  assert list(plans_module._tolerant_read_memo) == paths[1:]
