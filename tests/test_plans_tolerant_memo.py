"""Unit tests for the read_plans_tolerant per-file memo."""
from __future__ import annotations

import json
import unittest.mock
from pathlib import Path

import pytest

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


def test_steady_state_read_pays_no_file_read(tmp_path: Path) -> None:
  p = tmp_path / "plans.json"
  _write_registry(p, 3)
  first = read_plans_tolerant(p, "sess")
  assert len(first["plans"]) == 3

  reads = 0
  real_read_text = Path.read_text

  def counting_read_text(self: Path, *args, **kwargs):
    nonlocal reads
    reads += 1
    return real_read_text(self, *args, **kwargs)

  with unittest.mock.patch.object(Path, "read_text", counting_read_text):
    for _ in range(3):
      again = read_plans_tolerant(p, "sess")
      assert again == first
  assert reads == 0


def test_rewrite_rereads_and_serves_new_content(tmp_path: Path) -> None:
  p = tmp_path / "plans.json"
  _write_registry(p, 1)
  assert len(read_plans_tolerant(p, "sess")["plans"]) == 1

  _write_registry(p, 2)
  result = read_plans_tolerant(p, "sess")
  assert len(result["plans"]) == 2
  assert all(plan["state"] == "awaiting approval" for plan in result["plans"])


def test_rewrite_to_corrupt_drops_the_stale_hit(tmp_path: Path) -> None:
  p = tmp_path / "plans.json"
  _write_registry(p, 1)
  assert read_plans_tolerant(p, "sess")["errors"] == []

  p.write_text("{not valid json", encoding="utf-8")
  result = read_plans_tolerant(p, "sess")
  assert result["plans"] == []
  assert len(result["errors"]) == 1


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
