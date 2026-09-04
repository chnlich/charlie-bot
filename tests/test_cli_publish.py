"""Tests for src/cli/publish.py — URL on stdout, overwrite note, preflight failure exit codes."""

import json
from pathlib import Path
from unittest.mock import patch

import pytest

from src.cli.publish import main
from src.core.config import CharlieBotConfig


def make_cfg(tmp_path: Path) -> CharlieBotConfig:
  """A deployed publish lane under tmp_path: the directory present, the base URL a test literal."""
  publish_dir = tmp_path / "publish"
  publish_dir.mkdir(parents=True, exist_ok=True)
  return CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      publish_dir=publish_dir,
      public_base_url="https://pub.example.test/charliebot_pub")


def test_publish_prints_the_url_on_stdout_and_exits_zero(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
  artifact = tmp_path / "page.html"
  artifact.write_text("<p>hello</p>", encoding="utf-8")

  with patch("sys.argv", ["publish", str(artifact)]), patch("src.cli.publish.get_config",
                                                            return_value=make_cfg(tmp_path)):
    main()

  out = capsys.readouterr()
  assert out.out == "https://pub.example.test/charliebot_pub/page.html\n"
  assert out.err == ""
  assert (tmp_path / "publish" / "page.html").read_text(encoding="utf-8") == "<p>hello</p>"


def test_publish_notes_a_differing_replaced_file(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
  artifact = tmp_path / "page.html"
  artifact.write_text("<p>new</p>", encoding="utf-8")
  cfg = make_cfg(tmp_path)
  replaced = cfg.publish_dir / "page.html"
  replaced.write_text("<p>old</p>", encoding="utf-8")

  with patch("sys.argv", ["publish", str(artifact)]), patch("src.cli.publish.get_config", return_value=cfg):
    main()

  captured = capsys.readouterr()
  assert json.loads(captured.err)["note"] == f"overwrote a differing file with the same name: {replaced}"


@pytest.mark.parametrize("missing", ["publish_dir", "public_base_url"])
def test_preflight_failure_exits_non_zero_naming_the_missing_item(
    tmp_path: Path, capsys: pytest.CaptureFixture[str], missing: str) -> None:
  artifact = tmp_path / "page.html"
  artifact.write_text("<p>hello</p>", encoding="utf-8")
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  if missing == "public_base_url":
    cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", publish_dir=tmp_path / "publish")

  with (
      patch("sys.argv", ["publish", str(artifact)]),
      patch("src.cli.publish.get_config", return_value=cfg),
      pytest.raises(SystemExit) as exc_info,
  ):
    main()

  assert exc_info.value.code == 1
  captured = capsys.readouterr()
  assert captured.out == ""
  assert missing in json.loads(captured.err)["error"]


def test_missing_artifact_exits_non_zero_naming_the_path(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
  absent = tmp_path / "gone.html"

  with (
      patch("sys.argv", ["publish", str(absent)]),
      patch("src.cli.publish.get_config", return_value=make_cfg(tmp_path)),
      pytest.raises(SystemExit) as exc_info,
  ):
    main()

  assert exc_info.value.code == 1
  captured = capsys.readouterr()
  assert captured.out == ""
  assert str(absent) in json.loads(captured.err)["error"]
