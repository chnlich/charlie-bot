"""Tests for the labeled-entry memory store library (src/core/memory.py)."""

import io
import json
from pathlib import Path
from types import SimpleNamespace

import pytest

from src.core import memory
from src.core.memory import MemoryFormatError, assemble_master, assemble_worker, lint, load_store, parse_entry, \
    record_usage, usage_stats

_DEFAULT_TOPICS = [
    "profile resident",
    "communication resident",
    "workflow resident",
    "rulings resident",
    "host resident",
    "charliebot",
]


def _write_topics(memory_dir: Path, lines: list[str] | None = None) -> None:
  memory_dir.mkdir(parents=True, exist_ok=True)
  (memory_dir / "entries").mkdir(exist_ok=True)
  (memory_dir / "topics").write_text("".join(l + "\n" for l in (lines or _DEFAULT_TOPICS)), encoding="utf-8")


def _entry_text(
    topic: str,
    slug: str,
    *,
    scope="user",
    audience="both",
    created="2026-07-28",
    source="test",
    revises=None,
    body=None) -> str:
  header = [
      "---", f"scope: {scope}", f"topic: {topic}", f"audience: {audience}", f"created: {created}", f"source: {source}"
  ]
  if revises is not None:
    header.append(f"revises: {revises}")
  header.append("---")
  if body is None:
    body = f"# {slug.replace('-', ' ').title()}\n\nbody for {slug}\n"
  return "\n".join(header) + "\n" + body


def _write_entry(memory_dir: Path, topic: str, slug: str, **kw) -> Path:
  d = memory_dir / "entries" / topic
  d.mkdir(parents=True, exist_ok=True)
  p = d / f"{slug}.md"
  p.write_text(_entry_text(topic, slug, **kw), encoding="utf-8")
  return p


def _write_staging(memory_dir: Path, name: str, topic: str, slug: str, **kw) -> Path:
  memory_dir.joinpath("staging").mkdir(parents=True, exist_ok=True)
  p = memory_dir / "staging" / f"{name}.md"
  p.write_text(_entry_text(topic, slug, **kw), encoding="utf-8")
  return p


# --- parse_entry --------------------------------------------------------------


def test_parse_valid(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  p = _write_entry(tmp_path, "profile", "dark-mode", audience="both")
  e = parse_entry(p)
  assert e.topic == "profile"
  assert e.slug == "dark-mode"
  assert e.scope == "user"
  assert e.audience == "both"
  assert e.created == "2026-07-28"
  assert e.source == "test"
  assert e.revises is None
  assert e.title == "Dark Mode"
  assert e.body.startswith("# Dark Mode\n\nbody for dark-mode")
  assert e.id == "profile/dark-mode"


def test_parse_body_containing_separator(tmp_path: Path) -> None:
  """A `---` line inside the body is opaque; only the first header block is parsed."""
  _write_topics(tmp_path)
  body = "# Title\n\n---\n\nthis is not a header\n---\nstill body\n"
  p = _write_entry(tmp_path, "profile", "with-sep", body=body)
  e = parse_entry(p)
  assert e.title == "Title"
  assert "this is not a header" in e.body
  assert "still body" in e.body
  assert e.body.count("---") == 2


def test_parse_bad_header_line(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  d = tmp_path / "entries" / "profile"
  d.mkdir(parents=True)
  p = d / "bad.md"
  p.write_text("---\nscope: user\ntopic: profile\nNot A Header\n---\n# T\n", encoding="utf-8")
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


def test_parse_missing_title(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  d = tmp_path / "entries" / "profile"
  d.mkdir(parents=True)
  p = d / "notitle.md"
  p.write_text(
      "---\ntopic: profile\nscope: user\naudience: both\ncreated: 2026-07-28\nsource: t\n---\nno title here\n",
      encoding="utf-8")
  with pytest.raises(MemoryFormatError, match="body must start with '# <title>'"):
    parse_entry(p)


def test_parse_unknown_header_field(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  d = tmp_path / "entries" / "profile"
  d.mkdir(parents=True)
  p = d / "unknown.md"
  p.write_text("---\ntopic: profile\nscope: user\nbogus: val\n---\n# T\n", encoding="utf-8")
  with pytest.raises(MemoryFormatError, match="unknown header field"):
    parse_entry(p)


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


def test_load_missing_topics_raises(tmp_path: Path) -> None:
  # tmp_path exists but has no topics file.
  with pytest.raises(MemoryFormatError, match="topics vocabulary file not found"):
    load_store(tmp_path)


def test_load_empty_store_is_valid(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  store = load_store(tmp_path)
  assert store.entries == []
  assert set(store.topics) == {"profile", "communication", "workflow", "rulings", "host", "charliebot"}


# --- lint (staging relaxed) ---------------------------------------------------


def test_lint_clean_store(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "profile", "dark-mode")
  assert lint(tmp_path) == []


def test_lint_revises_in_staging_accepted(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "profile", "existing")
  _write_staging(
      tmp_path, "20260728T120000Z-abcd1234-rev-prop", "newtopic", "rev-prop", revises="existing", audience="worker")
  violations = lint(tmp_path)
  assert violations == [], f"expected clean, got: {violations}"


def test_lint_revises_in_entries_flagged(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "profile", "rev", revises="old")
  violations = lint(tmp_path)
  assert any("'revises' is forbidden in entries" in v for v in violations)


def test_lint_staging_missing_topic_flagged(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_staging(tmp_path, "cand", "notopic", "cand", scope=None, audience=None, created=None, source=None)
  # Construct a staging file with no topic field at all.
  p = tmp_path / "staging" / "cand.md"
  p.write_text("---\nscope: user\n---\n# Cand\n", encoding="utf-8")
  violations = lint(tmp_path)
  assert any("missing required header field 'topic'" in v for v in violations)


# --- assemble_master ----------------------------------------------------------


def test_assemble_master_resident_full_and_others_index(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "profile", "dark-mode", audience="both", body="# Dark Mode\n\nUser prefers dark UI.\n")
  _write_entry(tmp_path, "charliebot", "cli-flags", audience="both", body="# CLI Flags\n\nDetails.\n")
  block = assemble_master(tmp_path)
  assert block is not None
  assert "User prefers dark UI." in block  # resident full body
  assert "charliebot/cli-flags \u00b7 CLI Flags" in block  # non-resident index line
  assert "Details." not in block  # non-resident body NOT injected


def test_assemble_master_audience_filtering(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "profile", "for-master", audience="master", body="# Master\n\nm\n")
  _write_entry(tmp_path, "profile", "for-worker", audience="worker", body="# Worker\n\nw\n")
  _write_entry(tmp_path, "profile", "for-both", audience="both", body="# Both\n\nb\n")
  block = assemble_master(tmp_path)
  assert block is not None
  assert "m" in block and "b" in block  # master + both
  assert "w" not in block  # worker-only excluded


def test_assemble_master_stable_order(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "charliebot", "zebra", audience="master", body="# Zebra\n\nz\n")
  _write_entry(tmp_path, "charliebot", "apple", audience="master", body="# Apple\n\na\n")
  _write_entry(tmp_path, "charliebot", "mango", audience="master", body="# Mango\n\nm\n")
  block = assemble_master(tmp_path)
  assert block is not None
  assert block.index("Apple") < block.index("Mango") < block.index("Zebra")


def test_assemble_master_missing_dir_returns_none(tmp_path: Path) -> None:
  assert assemble_master(tmp_path / "nope") is None


def test_assemble_master_empty_store_returns_none(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  assert assemble_master(tmp_path) is None


def test_assemble_master_records_usage_for_full_body_only(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "profile", "res", audience="both", body="# Res\n\nr\n")
  _write_entry(tmp_path, "charliebot", "ondemand", audience="both", body="# On\n\no\n")
  assemble_master(tmp_path)
  usage_path = tmp_path / "usage.jsonl"
  assert usage_path.exists()
  rec = json.loads(usage_path.read_text(encoding="utf-8").strip())
  assert rec["caller"] == "master-spawn"
  assert rec["entries"] == ["profile/res"]  # index-only entry not recorded


# --- assemble_worker ----------------------------------------------------------


def test_assemble_worker_repo_topic_match(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "charliebot", "cli-flags", audience="worker", body="# CLI Flags\n\nFBODY\n")
  _write_entry(tmp_path, "profile", "pref", audience="worker", body="# Pref\n\nPBODY\n")
  block = assemble_worker(tmp_path, "charliebot")
  assert block is not None
  assert "FBODY" in block  # full body for matching topic
  assert "profile/pref \u00b7 Pref" in block  # non-matching as index line
  assert "PBODY" not in block  # non-matching body not injected
  assert "charliebot memory query --topic" in block  # usage line present


def test_assemble_worker_no_match_index_only(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "profile", "pref", audience="both", body="# Pref\n\nPBODY\n")
  block = assemble_worker(tmp_path, "charliebot")
  assert block is not None
  assert "profile/pref \u00b7 Pref" in block
  assert "PBODY" not in block  # no matching topic -> index only
  assert "charliebot memory query --topic" in block


def test_assemble_worker_audience_filtering(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "charliebot", "wonly", audience="worker", body="# Wonly\n\nWONLYBODY\n")
  _write_entry(tmp_path, "charliebot", "monly", audience="master", body="# Monly\n\nMONLYBODY\n")
  block = assemble_worker(tmp_path, "charliebot")
  assert block is not None
  assert "WONLYBODY" in block  # worker entry full body
  assert "MONLYBODY" not in block  # master-only excluded from worker


def test_assemble_worker_missing_dir_returns_none(tmp_path: Path) -> None:
  assert assemble_worker(tmp_path / "nope", "charliebot") is None


def test_assemble_worker_records_usage(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "charliebot", "cli", audience="worker", body="# CLI\n\nc\n")
  _write_entry(tmp_path, "profile", "pref", audience="worker", body="# Pref\n\np\n")
  assemble_worker(tmp_path, "charliebot")
  rec = json.loads((tmp_path / "usage.jsonl").read_text(encoding="utf-8").strip())
  assert rec["caller"] == "worker-spawn"
  assert rec["entries"] == ["charliebot/cli"]  # only full-body injection


# --- record_usage + usage_stats ----------------------------------------------


def test_record_usage_appends_one_line(tmp_path: Path) -> None:
  record_usage(tmp_path, "query", ["a/b", "c/d"])
  record_usage(tmp_path, "query", ["a/b"])
  lines = (tmp_path / "usage.jsonl").read_text(encoding="utf-8").strip().splitlines()
  assert len(lines) == 2
  rec = json.loads(lines[0])
  assert set(rec.keys()) == {"ts", "caller", "entries"}
  assert rec["entries"] == ["a/b", "c/d"]


def test_record_usage_empty_ids_noop(tmp_path: Path) -> None:
  record_usage(tmp_path, "query", [])
  assert not (tmp_path / "usage.jsonl").exists()


def test_record_usage_invalid_caller_raises(tmp_path: Path) -> None:
  with pytest.raises(ValueError, match="invalid caller"):
    record_usage(tmp_path, "bogus", ["a/b"])


def test_usage_stats_hits_and_idle(tmp_path: Path) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "profile", "a")
  _write_entry(tmp_path, "profile", "b")
  record_usage(tmp_path, "query", ["profile/a"])
  stats = {s.entry_id: s for s in usage_stats(tmp_path, idle_days=60)}
  assert stats["profile/a"].hits == 1
  assert stats["profile/a"].last_seen is not None
  assert stats["profile/a"].idle_days == 0
  assert stats["profile/a"].over_threshold is False
  assert stats["profile/b"].hits == 0
  assert stats["profile/b"].last_seen is None
  assert stats["profile/b"].idle_days is None
  assert stats["profile/b"].over_threshold is True  # never seen = idle


def test_usage_stats_corrupt_line_skipped(tmp_path: Path, caplog: pytest.LogCaptureFixture) -> None:
  _write_topics(tmp_path)
  _write_entry(tmp_path, "profile", "a")
  usage_path = tmp_path / "usage.jsonl"
  usage_path.write_text(
      "not json at all\n" +
      json.dumps({
          "ts": "2026-07-28T00:00:00+00:00",
          "caller": "query",
          "entries": ["profile/a"]
      }) + "\n",
      encoding="utf-8")
  stats = {s.entry_id: s for s in usage_stats(tmp_path)}
  # Corrupt line skipped (no raise), valid line tallied.
  assert stats["profile/a"].hits == 1


# --- CLI add creates exactly one staging file, never touches entries/ -------


def _fake_cfg(tmp_path: Path) -> SimpleNamespace:
  home = tmp_path / "home"
  home.mkdir()
  mem = home / "memory"
  _write_topics(mem)
  return SimpleNamespace(memory_dir=mem, sessions_dir=home / "sessions")


def test_cli_add_creates_one_staging_file(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _fake_cfg(tmp_path)
  monkeypatch.setattr("src.cli.memory.get_config", lambda: cfg)
  body = "# Prefers Dark Mode\n\nThe user prefers dark themes across all UIs.\n"
  monkeypatch.setattr("sys.stdin", io.StringIO(body))
  import src.cli.memory as cli
  monkeypatch.setattr(
      "sys.argv", ["charliebot memory", "add", "--topic", "profile", "--scope", "user", "--audience", "both"])
  cli.main()
  staging = cfg.memory_dir / "staging"
  files = list(staging.glob("*.md"))
  assert len(files) == 1
  text = files[0].read_text(encoding="utf-8")
  assert text.startswith("---\n")
  assert "topic: profile" in text
  assert "revises:" not in text
  assert "# Prefers Dark Mode" in text
  # entries/ untouched
  entries = cfg.memory_dir / "entries"
  assert not entries.exists() or not list(entries.glob("**/*.md"))


def test_cli_add_with_revises(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _fake_cfg(tmp_path)
  _write_entry(cfg.memory_dir, "profile", "old-entry")
  monkeypatch.setattr("src.cli.memory.get_config", lambda: cfg)
  body = "# New Version\n\nrevised body\n"
  monkeypatch.setattr("sys.stdin", io.StringIO(body))
  import src.cli.memory as cli
  monkeypatch.setattr(
      "sys.argv", [
          "charliebot memory", "add", "--topic", "profile", "--scope", "user", "--audience", "both", "--revises",
          "old-entry"
      ])
  cli.main()
  files = list((cfg.memory_dir / "staging").glob("*.md"))
  assert len(files) == 1
  assert "revises: old-entry" in files[0].read_text(encoding="utf-8")
  # entries/ still has exactly the one pre-existing file
  assert len(list((cfg.memory_dir / "entries").glob("**/*.md"))) == 1


def test_cli_add_rejects_bad_title(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _fake_cfg(tmp_path)
  monkeypatch.setattr("src.cli.memory.get_config", lambda: cfg)
  monkeypatch.setattr("sys.stdin", io.StringIO("no title here\n"))
  import src.cli.memory as cli
  monkeypatch.setattr(
      "sys.argv", ["charliebot memory", "add", "--topic", "profile", "--scope", "user", "--audience", "both"])
  with pytest.raises(SystemExit):
    cli.main()
  assert not (cfg.memory_dir / "staging").exists() or not list((cfg.memory_dir / "staging").glob("*.md"))


def test_cli_query_unknown_topic_exits_nonzero(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _fake_cfg(tmp_path)
  monkeypatch.setattr("src.cli.memory.get_config", lambda: cfg)
  import src.cli.memory as cli
  monkeypatch.setattr("sys.argv", ["charliebot memory", "query", "--topic", "nope"])
  with pytest.raises(SystemExit) as exc:
    cli.main()
  assert exc.value.code != 0


def test_cli_query_full_records_usage_index_does_not(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture) -> None:
  cfg = _fake_cfg(tmp_path)
  _write_entry(cfg.memory_dir, "profile", "dark-mode", audience="both", body="# Dark Mode\n\nbody\n")
  monkeypatch.setattr("src.cli.memory.get_config", lambda: cfg)
  import src.cli.memory as cli
  # --index: prints index line, records nothing
  monkeypatch.setattr("sys.argv", ["charliebot memory", "query", "--topic", "profile", "--index"])
  cli.main()
  assert not (cfg.memory_dir / "usage.jsonl").exists()
  # full text: prints body, records one usage line with caller=query
  monkeypatch.setattr("sys.argv", ["charliebot memory", "query", "--topic", "profile"])
  cli.main()
  lines = (cfg.memory_dir / "usage.jsonl").read_text(encoding="utf-8").strip().splitlines()
  assert len(lines) == 1
  assert json.loads(lines[0])["caller"] == "query"
  assert json.loads(lines[0])["entries"] == ["profile/dark-mode"]


def test_cli_lint_nonzero_on_violations(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture
) -> None:
  cfg = _fake_cfg(tmp_path)
  _write_entry(cfg.memory_dir, "profile", "bad", revises="old")  # revises forbidden in entries
  monkeypatch.setattr("src.cli.memory.get_config", lambda: cfg)
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
  monkeypatch.setattr("src.cli.memory.get_config", lambda: cfg)
  import src.cli.memory as cli
  monkeypatch.setattr("sys.argv", ["charliebot memory", "lint"])
  cli.main()  # exits 0
