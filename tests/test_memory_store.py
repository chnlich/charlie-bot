"""Tests for the labeled-entry memory store library (src/core/memory.py).

Fixtures are entry format v2 (frontmatter ``title``, comma-list ``audience``,
no ``created``/``source``, heading-free body); ``legacy_memory_entry_text``
(conftest) builds v1 files (``created``/``source``, ``both``, ``# <title>``
body opener) to cover the dual-read path.
"""

import io
from pathlib import Path
from types import SimpleNamespace

import pytest
from conftest import memory_entry_text as _entry_text
from conftest import write_memory_entry as _write_entry
from conftest import write_memory_staging as _write_staging
from conftest import write_memory_topics as _write_topics

from src.core import memory
from src.core.memory import (
    MemoryFormatError,
    assemble_master,
    assemble_worker,
    lint,
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


def test_parse_audience_comma_list_variants(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  e = parse_entry(_write_entry(tmp_path, "profile", "a", audience="master,worker"))
  assert e.audience == ["master", "worker"]
  e = parse_entry(_write_entry(tmp_path, "profile", "b", audience="worker , master"))
  assert e.audience == ["worker", "master"]
  e = parse_entry(_write_entry(tmp_path, "profile", "c", audience="worker"))
  assert e.audience == ["worker"]


def test_parse_body_containing_separator(tmp_path: Path) -> None:
  """A `---` line inside the body is opaque; only the first header block is parsed."""
  _write_topics(tmp_path)
  body = "---\n\nthis is not a header\n---\nstill body\n"
  p = _write_entry(tmp_path, "profile", "with-sep", body=body)
  e = parse_entry(p)
  assert e.title == "With Sep"
  assert "this is not a header" in e.body
  assert "still body" in e.body
  assert e.body.count("---") == 2


def test_parse_title_with_free_text(tmp_path: Path) -> None:
  """Frontmatter title is a free-text line (spaces, punctuation), not slug-charset."""
  _write_topics(tmp_path)
  p = _write_entry(tmp_path, "profile", "pref", title="Prefers dark UI, everywhere & always")
  e = parse_entry(p)
  assert e.title == "Prefers dark UI, everywhere & always"


def test_parse_empty_title_value(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  d = tmp_path / "entries" / "profile"
  d.mkdir(parents=True)
  p = d / "emptytitle.md"
  p.write_text("---\ntopic: profile\ntitle:    \n---\nbody\n", encoding="utf-8")
  with pytest.raises(MemoryFormatError, match="empty 'title' header value"):
    parse_entry(p)


# --- parse_entry: structural errors -------------------------------------------


def test_parse_bad_header_line(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  d = tmp_path / "entries" / "profile"
  d.mkdir(parents=True)
  p = d / "bad.md"
  p.write_text("---\nscope: user\ntopic: profile\nNot A Header\n---\n# T\n", encoding="utf-8")
  with pytest.raises(MemoryFormatError, match="malformed header line"):
    parse_entry(p)


def test_parse_non_title_fields_stay_slug_charset(tmp_path: Path) -> None:
  """Free-text values are limited to title; scope/audience values stay charset-checked."""
  _write_topics(tmp_path)
  d = tmp_path / "entries" / "profile"
  d.mkdir(parents=True)
  p = d / "bad.md"
  p.write_text("---\ntopic: profile\nscope: user x\ntitle: T\n---\nbody\n", encoding="utf-8")
  with pytest.raises(MemoryFormatError, match="malformed header line"):
    parse_entry(p)
  p.write_text("---\ntopic: profile\nscope: user\naudience: master worker\ntitle: T\n---\nbody\n", encoding="utf-8")
  with pytest.raises(MemoryFormatError, match="malformed header line"):
    parse_entry(p)


def test_parse_missing_opener(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  d = tmp_path / "entries" / "profile"
  d.mkdir(parents=True)
  p = d / "noopen.md"
  p.write_text("scope: user\n---\n# T\n", encoding="utf-8")
  with pytest.raises(MemoryFormatError, match="expected '---' front matter opener"):
    parse_entry(p)


def test_parse_missing_title_anywhere(tmp_path: Path) -> None:
  """No frontmatter title and no legacy '# <title>' body opener -> fail loud."""
  _write_topics(tmp_path)
  d = tmp_path / "entries" / "profile"
  d.mkdir(parents=True)
  p = d / "notitle.md"
  p.write_text("---\ntopic: profile\nscope: user\naudience: both\n---\nno title here\n", encoding="utf-8")
  with pytest.raises(MemoryFormatError, match="no frontmatter 'title'"):
    parse_entry(p)


def test_parse_unknown_header_field(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  d = tmp_path / "entries" / "profile"
  d.mkdir(parents=True)
  p = d / "unknown.md"
  p.write_text("---\ntopic: profile\nscope: user\nbogus: val\n---\n# T\n", encoding="utf-8")
  with pytest.raises(MemoryFormatError, match="unknown header field"):
    parse_entry(p)


# --- parse_entry: legacy (v1) dual-read ---------------------------------------


def test_parse_legacy_fallback(tmp_path: Path) -> None:
  """Legacy format: title from the body '# <title>' opener; 'both' -> [master, worker]."""
  _write_topics(tmp_path)
  p = _write_entry(tmp_path, "profile", "dark-mode", legacy=True)
  e = parse_entry(p)
  assert e.audience == ["master", "worker"]
  assert e.audience_raw == "both"
  assert e.created == "2026-07-28"
  assert e.source == "test"
  assert e.title == "Dark Mode"
  assert e.title_in_header is False
  assert e.body.startswith("# Dark Mode\n\nbody for dark-mode")


def test_parse_legacy_created_source_parseable(tmp_path: Path) -> None:
  """created/source remain parseable (rejected by lint only, and only in entries/)."""
  _write_topics(tmp_path)
  # v2 header plus legacy created/source: still parses; lint (entries/) will flag them.
  p = tmp_path / "entries" / "profile" / "withmeta.md"
  p.parent.mkdir(parents=True)
  p.write_text(
      "---\nscope: user\ntopic: profile\naudience: master, worker\ntitle: With Meta\n"
      "created: 2026-07-28\nsource: test\n---\nplain body\n",
      encoding="utf-8")
  e = parse_entry(p)
  assert e.created == "2026-07-28"
  assert e.source == "test"
  assert e.title == "With Meta"
  assert e.title_in_header is True


# --- load_store semantic validation ------------------------------------------


def test_load_unknown_topic_raises(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "nonexistent", "x")
  with pytest.raises(MemoryFormatError, match="not in topics vocabulary"):
    load_store(tmp_path)


def test_load_dir_topic_mismatch_raises(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  d = tmp_path / "entries" / "wrongdir"
  d.mkdir(parents=True)
  p = d / "slug.md"
  p.write_text(_entry_text("profile", "slug"), encoding="utf-8")
  with pytest.raises(MemoryFormatError, match="directory name 'wrongdir' != topic 'profile'"):
    load_store(tmp_path)


def test_load_bad_filename_raises(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  d = tmp_path / "entries" / "profile"
  d.mkdir(parents=True)
  p = d / "bad slug.md"  # space is outside the slug charset
  p.write_text(_entry_text("profile", "bad slug"), encoding="utf-8")
  with pytest.raises(MemoryFormatError, match="does not match slug charset"):
    load_store(tmp_path)


def test_load_revises_in_entries_rejected(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "profile", "rev", revises="old-entry")
  with pytest.raises(MemoryFormatError, match="'revises' is forbidden in entries"):
    load_store(tmp_path)


def test_load_bad_audience_element_raises(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "profile", "a", audience="master,all")
  with pytest.raises(MemoryFormatError, match="audience element 'all' not in \\{master, worker\\}"):
    load_store(tmp_path)


def test_load_missing_topics_raises(tmp_path: Path) -> None:
  # tmp_path exists but has no topics file.
  with pytest.raises(MemoryFormatError, match="topics vocabulary file not found"):
    load_store(tmp_path)


def test_load_empty_store_is_valid(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  store = load_store(tmp_path)
  assert not store.entries
  assert set(store.topics) == {"profile", "communication", "workflow", "rulings", "host", "charliebot"}


def test_load_legacy_store_still_loads(tmp_path: Path) -> None:
  """Dual-read: a fully legacy-format store loads even though lint flags it."""
  _write_topics(tmp_path)
  _write_entry(tmp_path, "profile", "dark-mode", legacy=True)
  _write_entry(tmp_path, "charliebot", "cli-flags", legacy=True, audience="master")
  store = load_store(tmp_path)
  assert {e.slug for e in store.entries} == {"dark-mode", "cli-flags"}
  assert all(e.title for e in store.entries)


# --- lint: v2 strict entries/, relaxed staging/ --------------------------------


def test_lint_clean_store(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "profile", "dark-mode")
  assert not lint(tmp_path)


def test_lint_entries_flags_missing_title(tmp_path: Path) -> None:
  """entries/ requires the frontmatter title; a legacy body title does not satisfy it."""
  _write_topics(tmp_path)
  p = tmp_path / "entries" / "profile" / "old.md"
  p.parent.mkdir(parents=True)
  p.write_text(
      "---\nscope: user\ntopic: profile\naudience: master, worker\n---\n# Old Title\n\nbody\n", encoding="utf-8")
  violations = lint(tmp_path)
  assert any("missing required header field 'title'" in v for v in violations)


def test_lint_entries_flags_created_source(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  p = tmp_path / "entries" / "profile"
  p.mkdir(parents=True)
  p.joinpath("meta.md").write_text(
      "---\nscope: user\ntopic: profile\naudience: master, worker\ntitle: Meta\n"
      "created: 2026-07-28\nsource: test\n---\nbody\n",
      encoding="utf-8")
  violations = lint(tmp_path)
  assert any("'created' is forbidden in entries/" in v for v in violations)
  assert any("'source' is forbidden in entries/" in v for v in violations)


def test_lint_entries_flags_literal_both(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  p = tmp_path / "entries" / "profile"
  p.mkdir(parents=True)
  p.joinpath("both.md").write_text(
      "---\nscope: user\ntopic: profile\naudience: both\ntitle: Both\n---\nbody\n", encoding="utf-8")
  violations = lint(tmp_path)
  assert any("literal audience 'both' is forbidden in entries/" in v for v in violations)


def test_lint_entries_flags_bad_audience_element(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "profile", "a", audience="master,all")
  violations = lint(tmp_path)
  assert any("audience element 'all' not in {master, worker}" in v for v in violations)


def test_lint_staging_legacy_candidate_stays_clean(tmp_path: Path) -> None:
  """Existing staged candidates (created/source header, both, '# ' body) stay lint-clean."""
  _write_topics(tmp_path)
  _write_staging(tmp_path, "20260728T120000Z-abcd1234-pending", "profile", "pending", legacy=True, audience="both")
  violations = lint(tmp_path)
  assert not violations, f"expected clean, got: {violations}"


def test_lint_revises_in_staging_accepted(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "profile", "existing")
  _write_staging(
      tmp_path, "20260728T120000Z-abcd1234-rev-prop", "newtopic", "rev-prop", revises="existing", audience="worker")
  violations = lint(tmp_path)
  assert not violations, f"expected clean, got: {violations}"


def test_lint_staging_comma_audience_accepted(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_staging(tmp_path, "cand", "profile", "cand", audience="master, worker")
  violations = lint(tmp_path)
  assert not violations, f"expected clean, got: {violations}"


def test_lint_revises_in_entries_flagged(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "profile", "rev", revises="old")
  violations = lint(tmp_path)
  assert any("'revises' is forbidden in entries" in v for v in violations)


def test_lint_staging_missing_topic_flagged(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  # A staging file with no topic field at all.
  memory_dir = tmp_path
  memory_dir.joinpath("staging").mkdir(parents=True, exist_ok=True)
  p = memory_dir / "staging" / "cand.md"
  p.write_text("---\nscope: user\n---\n# Cand\n", encoding="utf-8")
  violations = lint(tmp_path)
  assert any("missing required header field 'topic'" in v for v in violations)


def test_lint_staging_free_form_capture_stays_clean(tmp_path: Path) -> None:
  """A free-form capture (no frontmatter, '# <title>' first line) lints clean."""
  _write_topics(tmp_path)
  staging = tmp_path / "staging"
  staging.mkdir()
  staging.joinpath("20260810T000000Z-nosess-dark-mode.md").write_text(
      "# Dark Mode\n\nUser prefers dark UI.\n", encoding="utf-8")
  assert not lint(tmp_path)


def test_lint_staging_capture_bad_first_line_flagged(tmp_path: Path) -> None:
  """A staging file whose first line is neither '---' nor '# <title>' is a violation."""
  _write_topics(tmp_path)
  staging = tmp_path / "staging"
  staging.mkdir()
  staging.joinpath("cand.md").write_text("no title here\n", encoding="utf-8")
  violations = lint(tmp_path)
  assert any("cand.md" in v for v in violations)


def test_lint_staging_capture_empty_title_flagged(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  staging = tmp_path / "staging"
  staging.mkdir()
  staging.joinpath("cand.md").write_text("# \n\nbody\n", encoding="utf-8")
  violations = lint(tmp_path)
  assert any("cand.md" in v and "empty title" in v for v in violations)


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


def test_assemble_master_no_duplicate_heading_for_legacy_body(tmp_path: Path) -> None:
  """A legacy body already opening with '# ' keeps its own heading exactly once."""
  _write_topics(tmp_path)
  _write_entry(tmp_path, "profile", "dark-mode", legacy=True, body="# Dark Mode\n\nUser prefers dark UI.\n")
  block = assemble_master(tmp_path)
  assert block is not None
  assert block.count("# Dark Mode") == 1
  assert "User prefers dark UI." in block


def test_assemble_master_audience_filtering(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "profile", "for-master", audience="master", body="mbody\n")
  _write_entry(tmp_path, "profile", "for-worker", audience="worker", body="wbody\n")
  _write_entry(tmp_path, "profile", "for-both", audience="master, worker", body="bbody\n")
  block = assemble_master(tmp_path)
  assert block is not None
  assert "mbody" in block and "bbody" in block  # master + comma list
  assert "wbody" not in block  # worker-only excluded


def test_assemble_master_legacy_both_filters_like_master_worker(tmp_path: Path) -> None:
  """Legacy 'both' filtering is exactly equivalent to 'master, worker'."""
  _write_topics(tmp_path)
  _write_entry(tmp_path, "profile", "leg", legacy=True, audience="both", body="# Leg\n\nlegbody\n")
  _write_entry(tmp_path, "profile", "v2", audience="master, worker", body="v2body\n")
  block = assemble_master(tmp_path)
  assert block is not None
  assert "legbody" in block and "v2body" in block


def test_assemble_master_stable_order(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "charliebot", "zebra", audience="master", title="Zebra")
  _write_entry(tmp_path, "charliebot", "apple", audience="master", title="Apple")
  _write_entry(tmp_path, "charliebot", "mango", audience="master", title="Mango")
  block = assemble_master(tmp_path)
  assert block is not None
  assert block.index("Apple") < block.index("Mango") < block.index("Zebra")


def test_assemble_master_missing_dir_returns_none(tmp_path: Path) -> None:
  assert assemble_master(tmp_path / "nope") is None


def test_assemble_master_empty_store_returns_none(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  assert assemble_master(tmp_path) is None


def test_assemble_master_index_header_exactly_once_with_index(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "profile", "res", title="Res", body="r\n")
  _write_entry(tmp_path, "charliebot", "ondemand", title="On")
  block = assemble_master(tmp_path)
  assert block is not None
  assert block.count(memory.INDEX_HEADER) == 1
  # The header line sits immediately before the first index line.
  assert f"{memory.INDEX_HEADER}\ncharliebot/ondemand · On" in block


def test_assemble_master_no_index_header_without_index_entries(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "profile", "res", title="Res", body="r\n")
  block = assemble_master(tmp_path)
  assert block is not None
  assert memory.INDEX_HEADER not in block  # full-body only: no index, no header


def test_assemble_master_legacy_store_output_is_pre_change_plus_header(tmp_path: Path) -> None:
  """On a legacy-format store the output equals the pre-change output plus only the new header line."""
  _write_topics(tmp_path)
  _write_entry(tmp_path, "profile", "dark-mode", legacy=True, body="# Dark Mode\n\nUser prefers dark UI.\n")
  _write_entry(tmp_path, "charliebot", "cli-flags", legacy=True, body="# CLI Flags\n\nDetails.\n")
  block = assemble_master(tmp_path)
  pre_change = "# Dark Mode\n\nUser prefers dark UI.\n\ncharliebot/cli-flags · CLI Flags"
  assert block == pre_change.replace("charliebot/cli-flags", f"{memory.INDEX_HEADER}\ncharliebot/cli-flags")


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


def test_assemble_worker_no_match_index_only(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "profile", "pref", title="Pref", body="PBODY\n")
  block = assemble_worker(tmp_path, "charliebot")
  assert block is not None
  assert "profile/pref · Pref" in block
  assert "PBODY" not in block  # no matching topic -> index only
  assert "charliebot memory query --topic" in block
  assert block.count(memory.INDEX_HEADER) == 1


def test_assemble_worker_no_index_header_without_index_entries(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "charliebot", "cli", audience="worker", title="CLI", body="c\n")
  block = assemble_worker(tmp_path, "charliebot")
  assert block is not None
  assert memory.INDEX_HEADER not in block  # only full-body + usage line
  # And never with an entirely empty store (usage line only).
  _write_topics(tmp_path / "empty")
  block2 = assemble_worker(tmp_path / "empty", "charliebot")
  assert block2 is not None
  assert memory.INDEX_HEADER not in block2


def test_assemble_worker_usage_line_matches_free_capture_contract(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  block = assemble_worker(tmp_path, "charliebot")
  assert block is not None
  assert "`charliebot memory add [--file F]`" in block
  assert "one fact to record or one change to propose" in block
  assert "--scope" not in block
  assert "--audience" not in block
  assert "--revises" not in block


def test_assemble_worker_audience_filtering(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "charliebot", "wonly", audience="worker", title="Wonly", body="WONLYBODY\n")
  _write_entry(tmp_path, "charliebot", "monly", audience="master", title="Monly", body="MONLYBODY\n")
  _write_entry(tmp_path, "charliebot", "both", audience="master, worker", title="Both", body="BOTHBODY\n")
  block = assemble_worker(tmp_path, "charliebot")
  assert block is not None
  assert "WONLYBODY" in block  # worker entry full body
  assert "BOTHBODY" in block  # comma list includes worker
  assert "MONLYBODY" not in block  # master-only excluded from worker


def test_assemble_worker_legacy_both_included(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "charliebot", "leg", legacy=True, audience="both", body="# Leg\n\nLEGBODY\n")
  block = assemble_worker(tmp_path, "charliebot")
  assert block is not None
  assert "LEGBODY" in block
  assert block.count("# Leg") == 1  # legacy body heading not duplicated


def test_assemble_worker_missing_dir_returns_none(tmp_path: Path) -> None:
  assert assemble_worker(tmp_path / "nope", "charliebot") is None


# --- CLI add creates exactly one staging file, never touches entries/ -------

# Import-path patch target for the memory CLI's config read: src/cli/memory.py binds the name
# with `from src.core.config import get_config`, so mock setattrs the
# stand-in on the src.cli.memory module attribute and the CLI's entry points read it at call time.
_CLI_MEMORY_GET_CONFIG_PATCH_TARGET = "src.cli.memory.get_config"


def _fake_cfg(tmp_path: Path) -> SimpleNamespace:
  home = tmp_path / "home"
  home.mkdir()
  mem = home / "memory"
  _write_topics(mem)
  return SimpleNamespace(memory_dir=mem, sessions_dir=home / "sessions")


def test_cli_add_creates_one_staging_file(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Flag-less invocation writes exactly one staging file whose content is the body verbatim."""
  cfg = _fake_cfg(tmp_path)
  monkeypatch.setattr(_CLI_MEMORY_GET_CONFIG_PATCH_TARGET, lambda: cfg)
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


def test_cli_add_cjk_title_uses_capture_segment(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A title with no slug-charset character (pure CJK) falls back to the fixed 'capture' slug."""
  cfg = _fake_cfg(tmp_path)
  monkeypatch.setattr(_CLI_MEMORY_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  body = "# 本地渲染环境\n\n渲染机约束逐条列出。\n"
  monkeypatch.setattr("sys.stdin", io.StringIO(body))
  import src.cli.memory as cli
  monkeypatch.setattr("sys.argv", ["charliebot memory", "add"])
  cli.main()
  files = list((cfg.memory_dir / "staging").glob("*.md"))
  assert len(files) == 1
  assert files[0].name.endswith("-capture.md")
  assert files[0].read_text(encoding="utf-8") == body


def test_cli_add_rejects_missing_title_line(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _fake_cfg(tmp_path)
  monkeypatch.setattr(_CLI_MEMORY_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  monkeypatch.setattr("sys.stdin", io.StringIO("no title here\n"))
  import src.cli.memory as cli
  monkeypatch.setattr("sys.argv", ["charliebot memory", "add"])
  with pytest.raises(SystemExit) as exc:
    cli.main()
  assert exc.value.code == 1
  assert not (cfg.memory_dir / "staging").exists() or not list((cfg.memory_dir / "staging").glob("*.md"))


def test_cli_add_rejects_empty_title(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _fake_cfg(tmp_path)
  monkeypatch.setattr(_CLI_MEMORY_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  monkeypatch.setattr("sys.stdin", io.StringIO("# \n\nbody\n"))
  import src.cli.memory as cli
  monkeypatch.setattr("sys.argv", ["charliebot memory", "add"])
  with pytest.raises(SystemExit) as exc:
    cli.main()
  assert exc.value.code == 1
  assert not (cfg.memory_dir / "staging").exists() or not list((cfg.memory_dir / "staging").glob("*.md"))


def test_cli_add_removed_flag_fails_via_argparse(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Removed flags (--topic/--scope/--audience/--revises) hit the argparse unrecognized-argument error."""
  cfg = _fake_cfg(tmp_path)
  monkeypatch.setattr(_CLI_MEMORY_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  monkeypatch.setattr("sys.stdin", io.StringIO("# T\n\nbody\n"))
  import src.cli.memory as cli
  monkeypatch.setattr("sys.argv", ["charliebot memory", "add", "--topic", "x"])
  with pytest.raises(SystemExit) as exc:
    cli.main()
  assert exc.value.code == 2  # argparse: unrecognized arguments
  assert not (cfg.memory_dir / "staging").exists() or not list((cfg.memory_dir / "staging").glob("*.md"))


def test_cli_query_unknown_topic_exits_nonzero(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture) -> None:
  cfg = _fake_cfg(tmp_path)
  monkeypatch.setattr(_CLI_MEMORY_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  import src.cli.memory as cli
  monkeypatch.setattr("sys.argv", ["charliebot memory", "query", "--topic", "nope"])
  with pytest.raises(SystemExit) as exc:
    cli.main()
  assert exc.value.code != 0
  err = capsys.readouterr().err
  assert err == "error: unknown topic: nope\n"


def test_cli_query_unknown_topic_slash_value_known_pre_slash_hints(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture) -> None:
  """A ``topic/slug`` value whose pre-slash segment is a real topic gets a corrective hint."""
  cfg = _fake_cfg(tmp_path)
  monkeypatch.setattr(_CLI_MEMORY_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  import src.cli.memory as cli
  monkeypatch.setattr("sys.argv", ["charliebot memory", "query", "--topic", "charliebot/some-slug"])
  with pytest.raises(SystemExit) as exc:
    cli.main()
  assert exc.value.code == 1
  out, err = capsys.readouterr()
  assert out == ""
  assert err == ("error: unknown topic: charliebot/some-slug (index lines are topic/slug; try --topic charliebot)\n")


def test_cli_query_unknown_topic_slash_value_unknown_pre_slash_is_plain(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture) -> None:
  """A ``topic/slug`` value whose pre-slash segment is not a real topic gets no hint."""
  cfg = _fake_cfg(tmp_path)
  monkeypatch.setattr(_CLI_MEMORY_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  import src.cli.memory as cli
  monkeypatch.setattr("sys.argv", ["charliebot memory", "query", "--topic", "nope/some-slug"])
  with pytest.raises(SystemExit) as exc:
    cli.main()
  assert exc.value.code == 1
  out, err = capsys.readouterr()
  assert out == ""
  assert err == "error: unknown topic: nope/some-slug\n"


def test_cli_query_unknown_topic_mixed_invocation_hints_only_slash_value(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture) -> None:
  """One plain typo plus one hint-eligible value: two lines, in argument order, only the latter hinted."""
  cfg = _fake_cfg(tmp_path)
  monkeypatch.setattr(_CLI_MEMORY_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  import src.cli.memory as cli
  monkeypatch.setattr("sys.argv", ["charliebot memory", "query", "--topic", "nope", "--topic", "charliebot/some-slug"])
  with pytest.raises(SystemExit) as exc:
    cli.main()
  assert exc.value.code == 1
  out, err = capsys.readouterr()
  assert out == ""
  assert err == (
      "error: unknown topic: nope\n"
      "error: unknown topic: charliebot/some-slug (index lines are topic/slug; try --topic charliebot)\n")


def test_cli_query_index_prints_lines_full_prints_body(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture) -> None:
  cfg = _fake_cfg(tmp_path)
  _write_entry(cfg.memory_dir, "profile", "dark-mode", title="Dark Mode", body="body\n")
  monkeypatch.setattr(_CLI_MEMORY_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  import src.cli.memory as cli
  # --index: prints index lines only
  monkeypatch.setattr("sys.argv", ["charliebot memory", "query", "--topic", "profile", "--index"])
  cli.main()
  assert "profile/dark-mode · Dark Mode" in capsys.readouterr().out
  # full text: synthesizes the '# {title}' heading (v2 bodies carry none)
  monkeypatch.setattr("sys.argv", ["charliebot memory", "query", "--topic", "profile"])
  cli.main()
  out = capsys.readouterr().out
  assert out.count("# Dark Mode") == 1
  assert "body" in out


def test_cli_query_full_no_duplicate_heading_for_legacy_body(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture) -> None:
  cfg = _fake_cfg(tmp_path)
  _write_entry(cfg.memory_dir, "profile", "dark-mode", legacy=True)
  monkeypatch.setattr(_CLI_MEMORY_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  import src.cli.memory as cli
  monkeypatch.setattr("sys.argv", ["charliebot memory", "query", "--topic", "profile"])
  cli.main()
  out = capsys.readouterr().out
  assert out.count("# Dark Mode") == 1  # legacy body heading kept, none synthesized


def test_cli_query_audience_filter_is_membership(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture) -> None:
  cfg = _fake_cfg(tmp_path)
  _write_entry(cfg.memory_dir, "profile", "for-master", audience="master", body="mbody\n")
  _write_entry(cfg.memory_dir, "profile", "for-both", audience="master, worker", body="bbody\n")
  monkeypatch.setattr(_CLI_MEMORY_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  import src.cli.memory as cli
  monkeypatch.setattr("sys.argv", ["charliebot memory", "query", "--topic", "profile", "--audience", "worker"])
  cli.main()
  out = capsys.readouterr().out
  assert "bbody" in out
  assert "mbody" not in out


def test_cli_lint_nonzero_on_violations(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture) -> None:
  cfg = _fake_cfg(tmp_path)
  _write_entry(cfg.memory_dir, "profile", "bad", revises="old")  # revises forbidden in entries
  monkeypatch.setattr(_CLI_MEMORY_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  import src.cli.memory as cli
  monkeypatch.setattr("sys.argv", ["charliebot memory", "lint"])
  with pytest.raises(SystemExit) as exc:
    cli.main()
  assert exc.value.code != 0
  out = capsys.readouterr().out
  assert "revises" in out.lower() or "forbidden" in out.lower()


def test_cli_lint_clean_exits_zero(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _fake_cfg(tmp_path)
  _write_entry(cfg.memory_dir, "profile", "good")
  monkeypatch.setattr(_CLI_MEMORY_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  import src.cli.memory as cli
  monkeypatch.setattr("sys.argv", ["charliebot memory", "lint"])
  cli.main()  # exits 0
