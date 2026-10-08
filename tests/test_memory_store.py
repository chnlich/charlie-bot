"""Tests for the labeled-entry memory store library (src/features/memory/memory.py).

Fixtures are entry format v2 (frontmatter ``title``, comma-list ``audience``,
no ``created``/``source``, heading-free body); ``legacy_memory_entry_text``
(conftest) builds v1 files (``created``/``source``, ``both``, ``# <title>``
body opener) to cover the dual-read path.
"""

import io
import pathlib
import types
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
  assert e.audience_raw == "master, worker"
  assert e.created is None
  assert e.source is None
  assert e.revises is None
  assert e.title == "Dark Mode"
  assert e.title_in_header is True
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


_LOAD_REJECTION_CASES = [
    # (entry placement against the written topics vocabulary, the MemoryFormatError fragment
    # naming the broken field).
    pytest.param(
        lambda p: conftest.write_memory_entry(p, "nonexistent", "x"), "not in topics vocabulary", id="unknown-topic"),
    pytest.param(_mismatched_dir_entry, "directory name 'wrongdir' != topic 'profile'", id="dir-topic-mismatch"),
    pytest.param(_bad_filename_entry, "does not match slug charset", id="bad-filename"),
    pytest.param(
        lambda p: conftest.write_memory_entry(p, "profile", "rev", revises="old-entry"),
        "'revises' is forbidden in entries",
        id="revises-in-entries",
    ),
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


def test_lint_reports_a_missing_topics_file_without_creating_the_scaffold(tmp_path: pathlib.Path) -> None:
  mem = tmp_path / "bare"
  mem.mkdir()
  assert any("topics" in v for v in memory.lint(mem))
  assert not (mem / "topics").exists() and not (mem / ".git").exists()


# --- CLI add creates exactly one staging file, never touches entries/ -------


def _fake_cfg(tmp_path: pathlib.Path) -> types.SimpleNamespace:
  home = tmp_path / "home"
  home.mkdir()
  mem = home / "memory"
  conftest.write_memory_topics(mem)
  return types.SimpleNamespace(home=home, memory_dir=mem, sessions_dir=home / "sessions")


def _patch_cli_cfg(monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> types.SimpleNamespace:
  """Point the memory CLI's home resolution at a fresh fake store and return that config."""
  cfg = _fake_cfg(tmp_path)
  monkeypatch.setattr(conftest.CLI_MEMORY_HOME_PATCH_TARGET, lambda: cfg.home)
  return cfg


def test_cli_add_creates_one_staging_file(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Flag-less invocation writes exactly one staging file whose content is the body verbatim."""
  cfg = _patch_cli_cfg(monkeypatch, tmp_path)
  body = "# Prefers Dark Mode\n\nThe user prefers dark themes across all UIs.\n"
  monkeypatch.setattr("sys.stdin", io.StringIO(body))
  from src.features.memory import cli
  monkeypatch.setattr("sys.argv", ["charliebot memory", "add"])
  cli.main()
  staging = cfg.memory_dir / "staging"
  files = list(staging.glob("*.md"))
  assert len(files) == 1
  assert files[0].name.endswith("-prefers-dark-mode.md")
  text = files[0].read_text(encoding="utf-8")
  assert text == body  # verbatim: no '---' frontmatter, no header fields
  assert "---" not in text
  # entries/ untouched
  entries = cfg.memory_dir / "entries"
  assert not entries.exists() or not list(entries.glob("**/*.md"))


def _fresh_cli_home(monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> pathlib.Path:
  """Point the memory CLI at a home with no store yet and return the store root it will use."""
  home = tmp_path / "home"
  home.mkdir()
  monkeypatch.setattr(conftest.CLI_MEMORY_HOME_PATCH_TARGET, lambda: home)
  return home / "memory"


def test_cli_add_on_a_fresh_home_creates_the_scaffold_and_the_capture(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  mem = _fresh_cli_home(monkeypatch, tmp_path)
  body = "# Fresh Capture\n\nA fact to record.\n"
  monkeypatch.setattr("sys.stdin", io.StringIO(body))
  from src.features.memory import cli
  monkeypatch.setattr("sys.argv", ["charliebot memory", "add"])
  cli.main()
  _assert_fresh_scaffold(mem)
  captures = list((mem / "staging").glob("*.md"))
  assert len(captures) == 1
  assert captures[0].read_text(encoding="utf-8") == body


def test_cli_query_index_on_a_fresh_home_runs(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture) -> None:
  mem = _fresh_cli_home(monkeypatch, tmp_path)
  from src.features.memory import cli
  monkeypatch.setattr("sys.argv", ["charliebot memory", "query", "--topic", "profile", "--index"])
  cli.main()
  assert capsys.readouterr().out == ""
  _assert_fresh_scaffold(mem)


def test_cli_query_audience_filter_is_membership(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture) -> None:
  cfg = _patch_cli_cfg(monkeypatch, tmp_path)
  conftest.write_memory_entry(cfg.memory_dir, "profile", "for-master", audience="master", body="mbody\n")
  conftest.write_memory_entry(cfg.memory_dir, "profile", "for-both", audience="master, worker", body="bbody\n")
  from src.features.memory import cli
  monkeypatch.setattr("sys.argv", ["charliebot memory", "query", "--topic", "profile", "--audience", "worker"])
  cli.main()
  out = capsys.readouterr().out
  assert "bbody" in out
  assert "mbody" not in out


# --- CLI --dir: lint and query read the given store root ----------------------


def test_cli_lint_dir_reads_given_root(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture) -> None:
  """Without --dir lint reads the live store; with --dir it reads the given root."""
  _patch_cli_cfg(monkeypatch, tmp_path)
  from src.features.memory import cli
  monkeypatch.setattr("sys.argv", ["charliebot memory", "lint"])
  cli.main()
  assert capsys.readouterr().out.strip() == "clean"

  # A second store root whose entry breaks the strict v2 rules.
  other = tmp_path / "memory-proposal"
  conftest.write_memory_topics(other)
  conftest.write_memory_entry(other, "profile", "legacy", legacy=True)
  monkeypatch.setattr("sys.argv", ["charliebot memory", "lint", "--dir", str(other)])
  with pytest.raises(SystemExit) as exc_info:
    cli.main()
  assert exc_info.value.code == 1
  # The violations name the given root's entry (the CLI prints them on stdout).
  out = capsys.readouterr().out
  assert "entries/profile/legacy.md" in out and "'created' is forbidden" in out


def test_cli_query_dir_reads_given_root(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture) -> None:
  """query --dir reads entries from the given root, not the live store."""
  cfg = _patch_cli_cfg(monkeypatch, tmp_path)
  conftest.write_memory_entry(cfg.memory_dir, "profile", "live-only", body="live body\n")
  other = tmp_path / "memory-proposal"
  conftest.write_memory_topics(other)
  conftest.write_memory_entry(other, "profile", "pr-only", body="pr body\n")
  from src.features.memory import cli
  monkeypatch.setattr("sys.argv", ["charliebot memory", "query", "--topic", "profile", "--dir", str(other)])
  cli.main()
  out = capsys.readouterr().out
  assert "pr body" in out
  assert "live body" not in out
