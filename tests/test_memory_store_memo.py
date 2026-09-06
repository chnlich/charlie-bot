"""Unit tests for the load_store signature-keyed memo (src/core/memory.py)."""
from __future__ import annotations

from pathlib import Path

import pytest
from conftest import count_path_read_text, write_memory_entry, write_memory_topics

import src.core.memory as memory_module
from src.core.memory import MemoryFormatError, assemble_master, load_store


@pytest.fixture(autouse=True)
def _clear_store_memo():
  memory_module._store_memo.clear()
  yield
  memory_module._store_memo.clear()


def test_steady_state_load_pays_no_file_read(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  write_memory_topics(tmp_path, ["profile resident"])
  write_memory_entry(tmp_path, "profile", "one")
  first = load_store(tmp_path)
  assert len(first.entries) == 1

  reads = count_path_read_text(monkeypatch, lambda path: True)
  for _ in range(3):
    again = load_store(tmp_path)
    assert again is first
  assert reads == []


def test_entry_rewrite_rereads_and_serves_new_content(tmp_path: Path) -> None:
  write_memory_topics(tmp_path, ["profile resident"])
  p = write_memory_entry(tmp_path, "profile", "one", title="Before")
  assert load_store(tmp_path).entries[0].title == "Before"

  p.write_text(p.read_text(encoding="utf-8").replace("Before", "After"), encoding="utf-8")
  assert load_store(tmp_path).entries[0].title == "After"


def test_new_entry_file_rereads(tmp_path: Path) -> None:
  write_memory_topics(tmp_path, ["profile resident"])
  write_memory_entry(tmp_path, "profile", "one")
  assert len(load_store(tmp_path).entries) == 1

  write_memory_entry(tmp_path, "profile", "two")
  assert len(load_store(tmp_path).entries) == 2


def test_entry_deletion_rereads(tmp_path: Path) -> None:
  write_memory_topics(tmp_path, ["profile resident"])
  write_memory_entry(tmp_path, "profile", "one")
  p = write_memory_entry(tmp_path, "profile", "two")
  assert len(load_store(tmp_path).entries) == 2

  p.unlink()
  assert len(load_store(tmp_path).entries) == 1


def test_topics_rewrite_rereads(tmp_path: Path) -> None:
  write_memory_topics(tmp_path, ["profile resident"])
  write_memory_entry(tmp_path, "profile", "one")
  assert [t.name for t in load_store(tmp_path).topics.values() if t.resident] == ["profile"]

  (tmp_path / "topics").write_text("profile\n", encoding="utf-8")
  assert not any(t.resident for t in load_store(tmp_path).topics.values())


def test_malformed_store_raises_every_call(tmp_path: Path) -> None:
  write_memory_topics(tmp_path, ["profile resident"])
  p = write_memory_entry(tmp_path, "profile", "one")
  assert load_store(tmp_path).entries[0].slug == "one"

  p.write_text("---\nnot a header\n---\nbody\n", encoding="utf-8")
  for _ in range(3):
    with pytest.raises(MemoryFormatError):
      load_store(tmp_path)
  assert tmp_path not in memory_module._store_memo


def test_missing_topics_stays_uncached(tmp_path: Path) -> None:
  (tmp_path / "entries").mkdir(parents=True)
  for _ in range(2):
    with pytest.raises(MemoryFormatError):
      load_store(tmp_path)
  assert tmp_path not in memory_module._store_memo


def test_assemble_master_serves_memoized_store_without_new_reads(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  write_memory_topics(tmp_path, ["profile resident"])
  write_memory_entry(tmp_path, "profile", "one", title="First Fact", body="the one fact\n")
  block = assemble_master(tmp_path)
  assert "the one fact" in block

  reads = count_path_read_text(monkeypatch, lambda path: True)
  assert assemble_master(tmp_path) == block
  assert reads == []
