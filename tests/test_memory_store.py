"""Tests for the labeled-entry memory store library (src/core/memory.py).

Fixtures are entry format v2 (frontmatter ``title``, comma-list ``audience``,
no ``created``/``source``, heading-free body); ``legacy_memory_entry_text``
(conftest) builds v1 files (``created``/``source``, ``both``, ``# <title>``
body opener) to cover the dual-read path.
"""

import io
from collections.abc import Callable
from pathlib import Path
from types import SimpleNamespace

import pytest
from conftest import CLI_MEMORY_HOME_PATCH_TARGET, memory_entry_text
from conftest import write_memory_entry as _write_entry
from conftest import write_memory_topics as _write_topics

from src.core import memory
from src.core.memory import (
    MemoryFormatError,
    assemble_master,
    assemble_worker,
    load_store,
    parse_entry,
)

# --- parse_entry: v2 ----------------------------------------------------------


def test_parse_valid(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  p = _write_entry(tmp_path, "profile", "dark-mode")
  e = parse_entry(p)
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


def _mismatched_dir_entry(tmp_path: Path) -> None:
  d = tmp_path / "entries" / "wrongdir"
  d.mkdir(parents=True)
  (d / "slug.md").write_text(memory_entry_text("profile", "slug"), encoding="utf-8")


def _bad_filename_entry(tmp_path: Path) -> None:
  d = tmp_path / "entries" / "profile"
  d.mkdir(parents=True)
  # space is outside the slug charset
  (d / "bad slug.md").write_text(memory_entry_text("profile", "bad slug"), encoding="utf-8")


_LOAD_REJECTION_CASES = [
    # (entry placement against the written topics vocabulary, the MemoryFormatError fragment
    # naming the broken field).
    pytest.param(lambda p: _write_entry(p, "nonexistent", "x"), "not in topics vocabulary", id="unknown-topic"),
    pytest.param(_mismatched_dir_entry, "directory name 'wrongdir' != topic 'profile'", id="dir-topic-mismatch"),
    pytest.param(_bad_filename_entry, "does not match slug charset", id="bad-filename"),
    pytest.param(
        lambda p: _write_entry(p, "profile", "rev", revises="old-entry"),
        "'revises' is forbidden in entries",
        id="revises-in-entries",
    ),
    pytest.param(
        lambda p: _write_entry(p, "profile", "a", audience="master,all"),
        "audience element 'all' not in {master, worker}",
        id="bad-audience-element",
    ),
]


@pytest.mark.parametrize(("place_entry", "expected_fragment"), _LOAD_REJECTION_CASES)
def test_load_store_rejects_invalid_entry(
    tmp_path: Path, place_entry: Callable[[Path], None], expected_fragment: str) -> None:
  """An entries/ store holding one invalid entry fails the load, and the error names the broken field."""
  _write_topics(tmp_path)
  place_entry(tmp_path)
  with pytest.raises(MemoryFormatError) as exc_info:
    load_store(tmp_path)
  assert expected_fragment in str(exc_info.value)


# --- assemble_master ----------------------------------------------------------


def test_assemble_master_resident_full_and_others_index(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "profile", "dark-mode", title="Dark Mode", body="User prefers dark UI.\n")
  _write_entry(tmp_path, "charliebot", "cli-flags", title="CLI Flags", body="Details.\n")
  block = assemble_master(tmp_path)
  assert block is not None
  # v2 resident full body: the '# {title}' heading is synthesized.
  assert "# Dark Mode\n\nUser prefers dark UI." in block
  assert "charliebot/cli-flags · CLI Flags" in block  # non-resident index line
  assert "Details." not in block  # non-resident body NOT injected
  assert memory.INDEX_HEADER in block


def test_assemble_master_missing_dir_returns_none(tmp_path: Path) -> None:
  assert assemble_master(tmp_path / "nope") is None


# --- assemble_worker ----------------------------------------------------------


def test_assemble_worker_repo_topic_match(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "charliebot", "cli-flags", audience="worker", title="CLI Flags", body="FBODY\n")
  _write_entry(tmp_path, "profile", "pref", audience="worker", title="Pref", body="PBODY\n")
  block = assemble_worker(tmp_path, "charliebot")
  assert block is not None
  assert "# CLI Flags\n\nFBODY" in block  # full body for matching topic, heading synthesized
  assert "profile/pref · Pref" in block  # non-matching as index line
  assert "PBODY" not in block  # non-matching body not injected
  assert "charliebot memory query --topic" in block  # usage line present
  assert block.count(memory.INDEX_HEADER) == 1


# --- CLI add creates exactly one staging file, never touches entries/ -------


def _fake_cfg(tmp_path: Path) -> SimpleNamespace:
  home = tmp_path / "home"
  home.mkdir()
  mem = home / "memory"
  _write_topics(mem)
  return SimpleNamespace(home=home, memory_dir=mem, sessions_dir=home / "sessions")


def _patch_cli_cfg(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> SimpleNamespace:
  """Point the memory CLI's home resolution at a fresh fake store and return that config."""
  cfg = _fake_cfg(tmp_path)
  monkeypatch.setattr(CLI_MEMORY_HOME_PATCH_TARGET, lambda: cfg.home)
  return cfg


def test_cli_add_creates_one_staging_file(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Flag-less invocation writes exactly one staging file whose content is the body verbatim."""
  cfg = _patch_cli_cfg(monkeypatch, tmp_path)
  body = "# Prefers Dark Mode\n\nThe user prefers dark themes across all UIs.\n"
  monkeypatch.setattr("sys.stdin", io.StringIO(body))
  import src.cli.memory as cli
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


def test_cli_query_audience_filter_is_membership(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture) -> None:
  cfg = _patch_cli_cfg(monkeypatch, tmp_path)
  _write_entry(cfg.memory_dir, "profile", "for-master", audience="master", body="mbody\n")
  _write_entry(cfg.memory_dir, "profile", "for-both", audience="master, worker", body="bbody\n")
  import src.cli.memory as cli
  monkeypatch.setattr("sys.argv", ["charliebot memory", "query", "--topic", "profile", "--audience", "worker"])
  cli.main()
  out = capsys.readouterr().out
  assert "bbody" in out
  assert "mbody" not in out


# --- CLI --dir: lint and query read the given store root ----------------------


def test_cli_lint_dir_reads_given_root(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture) -> None:
  """Without --dir lint reads the live store; with --dir it reads the given root."""
  _patch_cli_cfg(monkeypatch, tmp_path)
  import src.cli.memory as cli
  monkeypatch.setattr("sys.argv", ["charliebot memory", "lint"])
  cli.main()
  assert capsys.readouterr().out.strip() == "clean"

  # A second store root whose entry breaks the strict v2 rules.
  other = tmp_path / "memory-proposal"
  _write_topics(other)
  _write_entry(other, "profile", "legacy", legacy=True)
  monkeypatch.setattr("sys.argv", ["charliebot memory", "lint", "--dir", str(other)])
  with pytest.raises(SystemExit) as exc_info:
    cli.main()
  assert exc_info.value.code == 1
  # The violations name the given root's entry (the CLI prints them on stdout).
  out = capsys.readouterr().out
  assert "entries/profile/legacy.md" in out and "'created' is forbidden" in out


def test_cli_query_dir_reads_given_root(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture) -> None:
  """query --dir reads entries from the given root, not the live store."""
  cfg = _patch_cli_cfg(monkeypatch, tmp_path)
  _write_entry(cfg.memory_dir, "profile", "live-only", body="live body\n")
  other = tmp_path / "memory-proposal"
  _write_topics(other)
  _write_entry(other, "profile", "pr-only", body="pr body\n")
  import src.cli.memory as cli
  monkeypatch.setattr("sys.argv", ["charliebot memory", "query", "--topic", "profile", "--dir", str(other)])
  cli.main()
  out = capsys.readouterr().out
  assert "pr body" in out
  assert "live body" not in out
