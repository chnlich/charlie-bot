"""Unit tests for the M112 corpus builder's verify-or-rebuild contract.

The builder serves the hourly standing sweep: its verify walk is what keeps the
~5 GB synthetic corpus from being re-written every round, so a silent
always-rebuild regression costs the round's wall and the root fs's write
budget. The corpus under test is a tmp_path with the per-file targets shrunk.
"""
import json

import backup_corpus_builder as builder
import pytest


def _shrink(monkeypatch, tmp_path):
  home = tmp_path / "home"
  monkeypatch.setattr(builder, "HOME", home)
  monkeypatch.setattr(builder, "MANIFEST", tmp_path / "corpus_manifest.json")
  monkeypatch.setattr(builder, "_CHAT_TARGET", 4096)
  monkeypatch.setattr(builder, "_ARCHIVE_TARGET", 4096)
  monkeypatch.setattr(builder, "_RAW_TARGET", 4096)
  monkeypatch.setattr(builder, "_TALLY_ROWS", 10)
  return home


def _sids(home):
  return sorted(p.name for p in (home / "sessions").iterdir())


def test_verified_corpus_survives_a_rerun_untouched(tmp_path, monkeypatch, capsys):
  home = _shrink(monkeypatch, tmp_path)
  builder.main()
  assert "corpus built" in capsys.readouterr().out
  sids = _sids(home)
  builder.main()
  assert "corpus verified" in capsys.readouterr().out
  assert _sids(home) == sids  # fresh uuid4 session ids would differ on a rebuild
  manifest = json.loads(builder.MANIFEST.read_text(encoding="utf-8"))
  files, total = builder._corpus_shape(home)
  assert manifest == {"files": files, "bytes": total}


@pytest.mark.parametrize(
    "damaged_manifest",
    [
        pytest.param("{}", id="shapeless"),  # valid JSON, missing both fields
        pytest.param("{not json", id="unreadable"),
    ])
def test_damaged_manifest_rebuilds(tmp_path, monkeypatch, capsys, damaged_manifest: str) -> None:
  """A manifest _read_manifest cannot serve — unparseable, or parsed without
  both integer fields — must ride the rebuild path (_read_manifest's contract):
  the rebuild replaces every session id."""
  home = _shrink(monkeypatch, tmp_path)
  builder.main()
  sids = _sids(home)
  capsys.readouterr()
  builder.MANIFEST.write_text(damaged_manifest, encoding="utf-8")
  builder.main()
  assert "corpus built" in capsys.readouterr().out
  assert _sids(home) != sids


def test_drifted_shape_rebuilds(tmp_path, monkeypatch, capsys):
  home = _shrink(monkeypatch, tmp_path)
  builder.main()
  sids = _sids(home)
  capsys.readouterr()
  chat = next((home / "sessions").glob("*/data/chat_events.jsonl"))
  chat.write_text(chat.read_text(encoding="utf-8") + "drift\n", encoding="utf-8")
  builder.main()
  assert "corpus built" in capsys.readouterr().out
  assert _sids(home) != sids
