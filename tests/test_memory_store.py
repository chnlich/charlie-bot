"""Tests for the labeled-entry memory store library (src/features/memory/memory.py)."""

import concurrent.futures
import pathlib
import threading
from collections.abc import Callable

import conftest
import pytest

from src.features.memory import memory

# --- parse_entry: v2 ----------------------------------------------------------


def test_parse_valid(tmp_path: pathlib.Path) -> None:
  conftest.write_memory_topics(tmp_path)
  p = conftest.write_memory_entry(tmp_path, "profile", "dark-mode")
  e = memory.parse_entry(p)
  assert e.topic == "profile"
  assert e.slug == "dark-mode"
  assert e.scope == "user"
  assert e.audience == ["master", "worker"]
  assert e.title == "Dark Mode"
  assert e.body == "body for dark-mode\n"
  assert e.id == "profile/dark-mode"


# --- load_store semantic validation ------------------------------------------


def _mismatched_dir_entry(tmp_path: pathlib.Path) -> None:
  d = tmp_path / "entries" / "wrongdir"
  d.mkdir(parents=True)
  (d / "slug.md").write_text(conftest.memory_entry_text("profile", "slug"), encoding="utf-8")


def _bad_filename_entry(tmp_path: pathlib.Path) -> None:
  d = tmp_path / "entries" / "profile"
  d.mkdir(parents=True)
  # space is outside the slug charset
  (d / "bad slug.md").write_text(conftest.memory_entry_text("profile", "bad slug"), encoding="utf-8")


def _retired_header_field_entry(tmp_path: pathlib.Path) -> None:
  d = tmp_path / "entries" / "profile"
  d.mkdir(parents=True)
  text = conftest.memory_entry_text("profile", "rev").replace("---\n", "---\nrevises: old-entry\n", 1)
  (d / "rev.md").write_text(text, encoding="utf-8")


_LOAD_REJECTION_CASES = [
    # (entry placement against the written topics vocabulary, the MemoryFormatError fragment
    # naming the broken field).
    pytest.param(
        lambda p: conftest.write_memory_entry(p, "nonexistent", "x"), "not in topics vocabulary", id="unknown-topic"),
    pytest.param(_mismatched_dir_entry, "directory name 'wrongdir' != topic 'profile'", id="dir-topic-mismatch"),
    pytest.param(_bad_filename_entry, "does not match slug charset", id="bad-filename"),
    pytest.param(_retired_header_field_entry, "unknown header field 'revises'", id="retired-header-field"),
    pytest.param(
        lambda p: conftest.write_memory_entry(p, "profile", "a", audience="master,all"),
        "audience element 'all' not in {master, worker}",
        id="bad-audience-element",
    ),
]


@pytest.mark.parametrize(("place_entry", "expected_fragment"), _LOAD_REJECTION_CASES)
def test_load_store_rejects_invalid_entry(
    tmp_path: pathlib.Path, place_entry: Callable[[pathlib.Path], None], expected_fragment: str) -> None:
  """An entries/ store holding one invalid entry fails the load, and the error names the broken field."""
  conftest.write_memory_topics(tmp_path)
  place_entry(tmp_path)
  with pytest.raises(memory.MemoryFormatError) as exc_info:
    memory.load_store(tmp_path)
  assert expected_fragment in str(exc_info.value)


# --- ensure_store: the store creates its own scaffold -------------------------

_SCAFFOLD_NAMES = (".git", "topics", ".gitignore", "entries", "staging")


def _assert_fresh_scaffold(memory_dir: pathlib.Path) -> None:
  for name in _SCAFFOLD_NAMES:
    assert (memory_dir / name).exists(), name
  assert (memory_dir / "topics").read_text(encoding="utf-8") == memory.DEFAULT_MEMORY_TOPICS
  assert (memory_dir / ".gitignore").read_text(encoding="utf-8") == memory.DEFAULT_MEMORY_GITIGNORE


def _snapshot(root: pathlib.Path) -> dict[str, bytes | None]:
  """Every path under *root* with its bytes (None for a directory)."""
  return {p.relative_to(root).as_posix(): (p.read_bytes() if p.is_file() else None) for p in sorted(root.rglob("*"))}


def test_select_master_memory_on_missing_dir_creates_scaffold(tmp_path: pathlib.Path) -> None:
  mem = tmp_path / "nope"
  assert memory.select_master_memory(mem) is None
  _assert_fresh_scaffold(mem)


def test_ensure_store_twice_leaves_files_unchanged(tmp_path: pathlib.Path) -> None:
  mem = tmp_path / "store"
  memory.ensure_store(mem)
  # A curated vocabulary replaces the seeded one; a repeat call must keep it.
  (mem / "topics").write_text("alpha resident\n", encoding="utf-8")
  before = _snapshot(mem)
  memory.ensure_store(mem)
  assert _snapshot(mem) == before
  assert (mem / "topics").read_text(encoding="utf-8") == "alpha resident\n"


def test_ensure_store_serves_concurrent_first_use(tmp_path: pathlib.Path) -> None:
  """Each launch's prompt assembly calls ensure_store on its own thread, so first-use calls overlap. A
  ``git init`` that starts while another runs in the same directory exits 128, which kills that launch."""
  mem = tmp_path / "store"
  callers = 8
  start = threading.Barrier(callers)

  def first_use() -> None:
    start.wait()
    memory.ensure_store(mem)

  with concurrent.futures.ThreadPoolExecutor(max_workers=callers) as pool:
    futures = [pool.submit(first_use) for _ in range(callers)]
    for future in futures:
      future.result()
  _assert_fresh_scaffold(mem)


def test_lint_reports_a_missing_topics_file_without_creating_the_scaffold(tmp_path: pathlib.Path) -> None:
  mem = tmp_path / "bare"
  mem.mkdir()
  assert any("topics" in v for v in memory.lint(mem))
  assert not (mem / "topics").exists() and not (mem / ".git").exists()
